//! Mapping between tau's message model and the Messages API wire format.

use serde_json::{json, Value as Json};
use tau_core::model::{ModelEvent, Request, StopReason};
use tau_core::types::{Content, Media, MediaSource, Message, Role};

pub fn request_body(model: &str, max_tokens: u32, req: &Request) -> Json {
    let mut messages = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::User | Role::Tool => {
                messages.push(json!({
                    "role": "user",
                    "content": content_blocks(message),
                }));
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                for content in &message.content {
                    match content {
                        Content::Text { text } => {
                            blocks.push(json!({ "type": "text", "text": text }))
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                        } => blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": arguments,
                        })),
                        _ => {}
                    }
                }
                messages.push(json!({ "role": "assistant", "content": blocks }));
            }
        }
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true,
    });
    if let Some(system) = &req.system {
        body["system"] = json!(system);
    }
    if !req.tools.is_empty() {
        body["tools"] = Json::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect(),
        );
    }
    body
}

/// User-role blocks: text, media, and tool results (which are user-role in
/// the Messages API).
fn content_blocks(message: &Message) -> Vec<Json> {
    let mut blocks = Vec::new();
    for content in &message.content {
        match content {
            Content::Text { text } => blocks.push(json!({ "type": "text", "text": text })),
            Content::Image { media } => {
                if let Some(source) = media_source(media) {
                    blocks.push(json!({ "type": "image", "source": source }));
                }
            }
            Content::File { media, .. } => {
                if let Some(source) = media_source(media) {
                    blocks.push(json!({ "type": "document", "source": source }));
                }
            }
            // Audio/video have no Messages API block; degrade to a text
            // placeholder so context is never silently dropped.
            Content::Audio { .. } | Content::Video { .. } => blocks.push(json!({
                "type": "text",
                "text": "[media attachment omitted: unsupported by this API]"
            })),
            Content::ToolResult {
                call_id,
                content,
                is_error,
            } => blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": call_id,
                "content": content,
                "is_error": is_error,
            })),
            Content::ToolCall { .. } => {}
        }
    }
    blocks
}

fn media_source(media: &Media) -> Option<Json> {
    match &media.source {
        MediaSource::Bytes(_) => Some(json!({
            "type": "base64",
            "media_type": media.media_type,
            "data": media.source.encode_base64()?,
        })),
        MediaSource::Url(url) => Some(json!({
            "type": "url",
            "url": url,
        })),
        // The agent materializes blobs to bytes before the request; a
        // blob reaching the wire means no store was attached — omit.
        MediaSource::Blob { .. } => None,
    }
}

/// Map one SSE payload (a JSON event object) to zero or more model events.
pub fn chunk_events(data: &str) -> Vec<ModelEvent> {
    let Ok(event) = serde_json::from_str::<Json>(data) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    match event["type"].as_str().unwrap_or_default() {
        "content_block_start" => {
            let block = &event["content_block"];
            if block["type"].as_str() == Some("tool_use") {
                events.push(ModelEvent::ToolCallDelta {
                    index: event["index"].as_u64().unwrap_or(0) as u32,
                    id: block["id"].as_str().map(str::to_string),
                    name: block["name"].as_str().map(str::to_string),
                    arguments_delta: String::new(),
                });
            }
        }
        "content_block_delta" => {
            let delta = &event["delta"];
            let index = event["index"].as_u64().unwrap_or(0) as u32;
            match delta["type"].as_str().unwrap_or_default() {
                "text_delta" => {
                    if let Some(text) = delta["text"].as_str() {
                        if !text.is_empty() {
                            events.push(ModelEvent::TextDelta { text: text.into() });
                        }
                    }
                }
                "input_json_delta" => events.push(ModelEvent::ToolCallDelta {
                    index,
                    id: None,
                    name: None,
                    arguments_delta: delta["partial_json"].as_str().unwrap_or_default().into(),
                }),
                _ => {}
            }
        }
        "message_delta" => {
            if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                let stop = match reason {
                    "end_turn" | "stop_sequence" => StopReason::Stop,
                    "tool_use" => StopReason::ToolUse,
                    "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
                    _ => StopReason::Stop,
                };
                events.push(ModelEvent::Done { stop });
            }
        }
        "error" => {
            let message = event["error"]["message"]
                .as_str()
                .unwrap_or("stream error")
                .to_string();
            events.push(ModelEvent::Error { message });
            events.push(ModelEvent::Done {
                stop: StopReason::Error,
            });
        }
        _ => {}
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use tau_core::tool::ToolDef;
    use tau_core::types::Media;

    #[test]
    fn request_maps_multimodal_and_tool_turn() {
        let req = Request {
            system: Some("be brief".into()),
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![
                        Content::Text { text: "what is this?".into() },
                        Content::Image {
                            media: Media::bytes("image/png", b"hello"),
                        },
                    ],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolCall {
                        id: "t1".into(),
                        name: "describe".into(),
                        arguments: json!({}),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        call_id: "t1".into(),
                        content: "a cat".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolDef {
                name: "describe".into(),
                description: "Describe an image".into(),
                parameters: json!({"type": "object"}),
            }],
        };
        let body = request_body("claude-sonnet-4-5", 8192, &req);
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["messages"][0]["content"][1]["type"], "image");
        assert_eq!(
            body["messages"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(
            body["messages"][0]["content"][1]["source"]["data"],
            "aGVsbG8="
        );
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        // tool result becomes a user-role tool_result block
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(
            body["messages"][2]["content"][0]["type"],
            "tool_result"
        );
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn chunk_maps_anthropic_event_stream() {
        assert_eq!(
            chunk_events(
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"upper","input":{}}}"#
            ),
            vec![ModelEvent::ToolCallDelta {
                index: 1,
                id: Some("tu_1".into()),
                name: Some("upper".into()),
                arguments_delta: String::new(),
            }]
        );
        assert_eq!(
            chunk_events(
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"text\":"}}"#
            ),
            vec![ModelEvent::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments_delta: r#"{"text":"#.into(),
            }]
        );
        assert_eq!(
            chunk_events(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#
            ),
            vec![ModelEvent::TextDelta { text: "hi".into() }]
        );
        assert_eq!(
            chunk_events(r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
            vec![ModelEvent::Done {
                stop: StopReason::ToolUse
            }]
        );
        assert_eq!(
            chunk_events(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
            vec![
                ModelEvent::Error {
                    message: "Overloaded".into()
                },
                ModelEvent::Done {
                    stop: StopReason::Error
                }
            ]
        );
    }
}
