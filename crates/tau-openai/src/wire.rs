//! Mapping between tau's message model and the chat-completions wire format.

use serde_json::{Value as Json, json};
use tau_core::model::{ModelEvent, Request, StopReason};
use tau_core::types::{Content, Media, Message, Role};

/// Chat-completions content parts for one user/tool-adjacent message.
fn user_parts(message: &Message) -> Vec<Json> {
    let mut parts = Vec::new();
    for content in &message.content {
        match content {
            Content::Text { text } => parts.push(json!({ "type": "text", "text": text })),
            Content::Image { media } => {
                if let Some(url) = media.data_url() {
                    parts.push(json!({
                        "type": "image_url",
                        "image_url": { "url": url },
                    }));
                }
            }
            Content::Audio { media } => {
                if let Some(part) = audio_part(media) {
                    parts.push(part);
                }
            }
            // Video and generic files have no chat-completions part; the
            // provider layer degrades them to a text placeholder so context
            // is never silently dropped.
            Content::Video { .. } => parts.push(json!({
                "type": "text",
                "text": "[video attachment omitted: unsupported by this API]"
            })),
            Content::File { media, name } => parts.push(json!({
                "type": "file",
                "file": {
                    "filename": name.clone().unwrap_or_else(|| "attachment".into()),
                    "file_data": media.data_url(),
                }
            })),
            _ => {}
        }
    }
    parts
}

fn audio_part(media: &Media) -> Option<Json> {
    let tau_core::types::MediaSource::Bytes(_) = &media.source else {
        return None;
    };
    let format = match media.media_type.as_str() {
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        _ => return None,
    };
    Some(json!({
        "type": "input_audio",
        "input_audio": { "data": media.source.encode_base64()?, "format": format },
    }))
}

pub fn request_body(model: &str, req: &Request) -> Json {
    let mut messages = Vec::new();
    if let Some(system) = &req.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for message in &req.messages {
        match message.role {
            Role::User => {
                let parts = user_parts(message);
                let text = message.text();
                let has_media = message.media().next().is_some();
                if has_media {
                    messages.push(json!({ "role": "user", "content": parts }));
                } else {
                    // Text-only: plain string for maximal proxy compatibility.
                    messages.push(json!({ "role": "user", "content": text }));
                }
            }
            Role::Assistant => {
                let text = message.text();
                let tool_calls: Vec<Json> = message
                    .tool_calls()
                    .map(|(id, name, arguments)| {
                        json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments.to_string(),
                            }
                        })
                    })
                    .collect();
                let mut m = json!({ "role": "assistant", "content": text });
                if !tool_calls.is_empty() {
                    m["tool_calls"] = Json::Array(tool_calls);
                }
                messages.push(m);
            }
            Role::Tool => {
                // Chat-completions tool messages are text-only: images ride
                // as a trailing user message of image_url parts (pi's
                // pattern); audio/video/file degrade to text placeholders
                // (docs/tool-media.md).
                let mut images = Vec::new();
                for content in &message.content {
                    if let Content::ToolResult {
                        call_id, content, ..
                    } = content
                    {
                        let images_before = images.len();
                        let mut text = String::new();
                        for block in content {
                            match block {
                                Content::Text { text: t } => text.push_str(t),
                                Content::Image { media } => {
                                    if let Some(data) = media.source.encode_base64() {
                                        images.push(json!({
                                            "type": "image_url",
                                            "image_url": {
                                                "url": format!("data:{};base64,{data}", media.media_type),
                                            },
                                        }));
                                    }
                                }
                                other => text.push_str(
                                    &tau_core::types::tool_result_text(
                                        std::slice::from_ref(other),
                                    ),
                                ),
                            }
                        }
                        if text.is_empty() {
                            text = if images.len() > images_before {
                                "(see attached image)".into()
                            } else {
                                "(no tool output)".into()
                            };
                        }
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": text,
                        }));
                    }
                }
                if !images.is_empty() {
                    let mut parts =
                        vec![json!({ "type": "text", "text": "Attached image(s) from tool result:" })];
                    parts.extend(images);
                    messages.push(json!({ "role": "user", "content": parts }));
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });
    if !req.tools.is_empty() {
        body["tools"] = Json::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect(),
        );
    }
    body
}

/// Map one SSE payload (a JSON chunk) to zero or more model events.
/// Malformed chunks are skipped — a single bad frame must not kill the turn.
pub fn chunk_events(data: &str) -> Vec<ModelEvent> {
    let Ok(chunk) = serde_json::from_str::<Json>(data) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for choice in chunk["choices"].as_array().into_iter().flatten() {
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str()
            && !text.is_empty()
        {
            events.push(ModelEvent::TextDelta {
                text: text.to_string(),
            });
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or(0) as u32;
            events.push(ModelEvent::ToolCallDelta {
                index,
                id: call["id"].as_str().map(str::to_string),
                name: call["function"]["name"].as_str().map(str::to_string),
                arguments_delta: call["function"]["arguments"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            let stop = match reason {
                "stop" => StopReason::Stop,
                "tool_calls" => StopReason::ToolUse,
                "length" | "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
                _ => StopReason::Stop,
            };
            events.push(ModelEvent::Done { stop });
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use tau_core::tool::ToolDef;
    use tau_core::types::Message;

    #[test]
    fn request_maps_tool_turn() {
        let req = Request {
            system: Some("be brief".into()),
            messages: vec![
                Message::user("reverse abc"),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolCall {
                        id: "c1".into(),
                        name: "reverse".into(),
                        arguments: json!({"text": "abc"}),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        call_id: "c1".into(),
                        content: vec![Content::Text { text: "cba".into() }],
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolDef {
                name: "reverse".into(),
                description: "Reverse a string".into(),
                parameters: json!({"type": "object"}),
            }],
        };
        let body = request_body("test-model", &req);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["arguments"],
            r#"{"text":"abc"}"#
        );
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
        assert_eq!(body["tools"][0]["function"]["name"], "reverse");
    }

    #[test]
    fn tool_result_image_rides_trailing_user_message() {
        // 0.3.0 (docs/tool-media.md): chat-completions tool messages are
        // text-only — an image block becomes an image_url part on a
        // trailing user message; audio degrades to a text placeholder.
        let req = Request {
            system: None,
            messages: vec![
                Message::user("dot please"),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolCall {
                        id: "c1".into(),
                        name: "dot_png".into(),
                        arguments: json!({}),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        call_id: "c1".into(),
                        content: vec![
                            Content::Text {
                                text: "a dot. ".into(),
                            },
                            Content::Image {
                                media: tau_core::types::Media::bytes("image/png", b"hello"),
                            },
                            Content::Audio {
                                media: tau_core::types::Media::bytes("audio/pcm", b"\x00"),
                            },
                        ],
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
        };
        let body = request_body("test-model", &req);
        let msgs = &body["messages"];
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "c1");
        assert_eq!(msgs[2]["content"], "a dot. [audio: audio/pcm]");
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(
            msgs[3]["content"][0],
            json!({ "type": "text", "text": "Attached image(s) from tool result:" })
        );
        assert_eq!(
            msgs[3]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn chunk_maps_text_tool_call_and_finish() {
        let events = chunk_events(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"reverse","arguments":"{\"tex"}}]}}]}"#,
        );
        assert_eq!(
            events,
            vec![ModelEvent::ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name: Some("reverse".into()),
                arguments_delta: r#"{"tex"#.into(),
            }]
        );

        let events =
            chunk_events(r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#);
        assert_eq!(
            events,
            vec![
                ModelEvent::TextDelta { text: "hi".into() },
                ModelEvent::Done {
                    stop: StopReason::Stop
                }
            ]
        );

        let events = chunk_events(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);
        assert_eq!(
            events,
            vec![ModelEvent::Done {
                stop: StopReason::ToolUse
            }]
        );
    }

    #[test]
    fn a_tool_call_stream_closes_as_tool_use() {
        // The exact chunk sequence a provider sends for a tool call, run
        // through the same mapping the stream uses: the deltas, then the
        // `finish_reason`, then the end of the byte stream. The last stop
        // the agent loop sees must be `ToolUse` — a trailing fallback
        // `stop` here is the difference between the call running and the
        // call being persisted and dropped.
        let mut closing = tau_core::sse::Closing::default();
        let mut last = None;
        for payload in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"ls","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\".\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ] {
            for event in closing.observe(chunk_events(payload)) {
                if let ModelEvent::Done { stop } = event {
                    last = Some(stop);
                }
            }
        }
        if let Some(ModelEvent::Done { stop }) = closing.fallback() {
            last = Some(stop);
        }
        assert_eq!(last, Some(StopReason::ToolUse));
    }

    #[test]
    fn malformed_chunk_is_skipped() {
        assert!(chunk_events("not json").is_empty());
    }
}
