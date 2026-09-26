//! Minimal SSE parser: `data:` lines, blank line terminates an event,
//! `data: [DONE]` ends the stream.

use futures::StreamExt;
use futures::stream::{BoxStream, Stream};

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
}
