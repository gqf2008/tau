//! Minimal SSE parser: `data:` lines, blank line terminates an event,
//! `data: [DONE]` ends the stream.

use futures::StreamExt;
use futures::stream::{BoxStream, Stream};

use crate::model::{ModelEvent, StopReason};

/// Parse a byte stream into parsed SSE payloads (the part after `data: `).
pub fn parse<S, B, E>(bytes: S) -> BoxStream<'static, Result<String, String>>
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send,
    E: std::error::Error + Send + 'static,
{
    let stream = async_stream::stream! {
        let mut buffer = String::new();
        let mut data_lines: Vec<String> = Vec::new();
        futures::pin_mut!(bytes);
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    yield Err(format!("stream error: {e}"));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(chunk.as_ref()));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if line.is_empty() {
                    if !data_lines.is_empty() {
                        let payload = data_lines.join("\n");
                        data_lines.clear();
                        if payload == "[DONE]" {
                            return;
                        }
                        yield Ok(payload);
                    }
                } else if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.strip_prefix(' ').unwrap_or(data).to_string());
                }
                // comment/field lines (event:, id:, :keep-alive) are ignored
            }
        }
        if !data_lines.is_empty() {
            let payload = data_lines.join("\n");
            if payload != "[DONE]" {
                yield Ok(payload);
            }
        }
    };
    stream.boxed()
}

/// The last word on why a provider stream ended.
///
/// A provider appends a fallback [`ModelEvent::Done`] when its byte stream
/// runs out, so a stream that simply stops still closes. What that fallback
/// must never do is overwrite a stop the provider already reported: the
/// chat-completions wire says `finish_reason: "tool_calls"` in its last
/// chunk and then `[DONE]`, and a trailing `stop` on top of it reads to the
/// agent loop as "the model was done talking" (it executes tool calls only
/// on `ToolUse`) — the call the model asked for is assembled, persisted, and
/// never run.
///
/// So the fallback is emitted by the streams that never spoke, and only by
/// them:
///
/// ```
/// use tau_core::model::{ModelEvent, StopReason};
/// use tau_core::sse::Closing;
///
/// let mut closing = Closing::default();
/// assert_eq!(
///     closing.observe(vec![ModelEvent::Done {
///         stop: StopReason::ToolUse
///     }]),
///     vec![ModelEvent::Done {
///         stop: StopReason::ToolUse
///     }]
/// );
/// assert!(closing.fallback().is_none());
/// ```
#[derive(Default)]
pub struct Closing {
    spoke: bool,
}

impl Closing {
    /// Note one chunk's events. They pass through unchanged; this only
    /// remembers whether the stream has said why it stopped.
    pub fn observe(&mut self, events: Vec<ModelEvent>) -> Vec<ModelEvent> {
        if events
            .iter()
            .any(|event| matches!(event, ModelEvent::Done { .. }))
        {
            self.spoke = true;
        }
        events
    }

    /// The closing event a stream that ended without a word still owes its
    /// reader, or `None` when the provider already named the stop.
    pub fn fallback(&self) -> Option<ModelEvent> {
        (!self.spoke).then_some(ModelEvent::Done {
            stop: StopReason::Stop,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(chunks: &[&str]) -> Vec<Result<String, String>> {
        let owned: Vec<Vec<u8>> = chunks.iter().map(|c| c.as_bytes().to_vec()).collect();
        let stream = futures::stream::iter(owned.into_iter().map(Ok::<_, std::io::Error>));
        futures::executor::block_on_stream(parse(stream)).collect()
    }

    #[test]
    fn parses_events_across_chunk_boundaries() {
        let events = feed(&[
            "data: {\"a\":1",
            "}\n\ndata: {\"b\"",
            ":2}\n",
            "\ndata: [DONE]\n",
        ]);
        assert_eq!(
            events,
            vec![Ok(r#"{"a":1}"#.to_string()), Ok(r#"{"b":2}"#.to_string()),]
        );
    }

    #[test]
    fn ignores_comments_and_crlf() {
        let events = feed(&[": keep-alive\r\n\r\ndata: hello\r\n\r\n"]);
        assert_eq!(events, vec![Ok("hello".to_string())]);
    }

    #[test]
    fn joins_multiline_data() {
        let events = feed(&["data: one\ndata: two\n\n"]);
        assert_eq!(events, vec![Ok("one\ntwo".to_string())]);
    }

    #[test]
    fn a_reported_stop_survives_the_end_of_the_stream() {
        // The bug this type exists for: `finish_reason: "tool_calls"` is
        // followed by `[DONE]`, the byte stream ends, and a trailing `stop`
        // used to overwrite the `tool_use` — silently discarding the call.
        let mut closing = Closing::default();
        closing.observe(vec![ModelEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("ls".into()),
            arguments_delta: "{}".into(),
        }]);
        closing.observe(vec![ModelEvent::Done {
            stop: StopReason::ToolUse,
        }]);
        assert!(closing.fallback().is_none());
    }

    #[test]
    fn a_silent_stream_still_closes() {
        let mut closing = Closing::default();
        closing.observe(vec![ModelEvent::TextDelta { text: "hi".into() }]);
        assert_eq!(
            closing.fallback(),
            Some(ModelEvent::Done {
                stop: StopReason::Stop
            })
        );
    }

    #[test]
    fn observe_passes_events_through_unchanged() {
        let mut closing = Closing::default();
        let events = vec![
            ModelEvent::TextDelta { text: "a".into() },
            ModelEvent::Done {
                stop: StopReason::Length,
            },
        ];
        assert_eq!(closing.observe(events.clone()), events);
    }
}
