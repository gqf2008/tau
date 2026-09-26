//! OpenAI Responses API (`POST /responses`) wire mapping.
//!
//! Distinct from chat completions: `input` items instead of `messages`,
//! `instructions` instead of a system message, flat function tools, and a
//! typed SSE event stream (`response.*`).

use std::collections::HashMap;

use serde_json::{json, Value as Json};
use tau_core::model::{ModelEvent, Request, StopReason};
use tau_core::types::{Content, Role};

pub const PATH: &str = "/responses";

pub fn request_body(model: &str, req: &Request) -> Json {
    let mut input = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::User => {
                // Tool results ride as function_call_output items, not messages.
                let results: Vec<Json> = message
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::ToolResult {
                            call_id, content, ..
                        } => Some(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": content,
                        })),
                        _ => None,
                    })
                    .collect();
                let mut parts = Vec::new();
                for content in &message.content {
                    match content {
                        Content::Text { text } => {
                            parts.push(json!({ "type": "input_text", "text": text }))
                        }
                        Content::Image { media } => {
                            if let Some(url) = media.data_url() {
                                parts.push(json!({
                                    "type": "input_image",
                                    "image_url": url,
                                }));
                            }
                        }
                        Content::File { media, name } => {
                            if let Some(data) = media.data_url() {
                                parts.push(json!({
                                    "type": "input_file",
                                    "filename": name.clone().unwrap_or_else(|| "attachment".into()),
                                    "file_data": data,
                                }));
                            }
                        }
                        Content::Audio { .. } | Content::Video { .. } => parts.push(json!({
                            "type": "input_text",
                            "text": "[media attachment omitted: unsupported by this API]"
                        })),
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": parts,
                    }));
                }
                input.extend(results);
            }
            Role::Assistant => {
                let text = message.text();
                if !text.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for (id, name, arguments) in message.tool_calls() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": arguments.to_string(),
                    }));
                }
            }
            Role::Tool => {
                for content in &message.content {
                    if let Content::ToolResult {
                        call_id, content, ..
                    } = content
                    {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": content,
                        }));
                    }
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "input": input,
        "stream": true,
    });
    if let Some(system) = &req.system {
        body["instructions"] = json!(system);
    }
    if !req.tools.is_empty() {
        body["tools"] = Json::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect(),
        );
    }
    body
}

/// Stateful chunk mapper: function-call identity arrives on
/// `response.output_item.added`, argument fragments later, keyed by
/// `output_index`.
#[derive(Default)]
pub struct ChunkMapper {
    calls: HashMap<u32, (String, String)>,
    saw_function_call: bool,
}

impl ChunkMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&mut self, data: &str) -> Vec<ModelEvent> {
        let Ok(chunk) = serde_json::from_str::<Json>(data) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        match chunk["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                if let Some(text) = chunk["delta"].as_str()
                    && !text.is_empty() {
                        events.push(ModelEvent::TextDelta { text: text.into() });
                    }
            }
            "response.output_item.added" => {
                let item = &chunk["item"];
                if item["type"].as_str() == Some("function_call") {
                    let index = chunk["output_index"].as_u64().unwrap_or(0) as u32;
                    let id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    self.calls.insert(index, (id.clone(), name.clone()));
                    self.saw_function_call = true;
                    events.push(ModelEvent::ToolCallDelta {
                        index,
                        id: Some(id),
                        name: Some(name),
                        arguments_delta: String::new(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                let index = chunk["output_index"].as_u64().unwrap_or(0) as u32;
                events.push(ModelEvent::ToolCallDelta {
                    index,
                    id: None,
                    name: None,
                    arguments_delta: chunk["delta"].as_str().unwrap_or_default().into(),
                });
            }
            "response.completed" => {
                events.push(ModelEvent::Done {
                    stop: if self.saw_function_call {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    },
                });
            }
            "response.incomplete" => {
                events.push(ModelEvent::Done {
                    stop: StopReason::Length,
                });
            }
            "response.failed" => {
                let message = chunk["response"]["error"]["message"]
                    .as_str()
                    .unwrap_or("response failed")
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_tool_call_lifecycle() {
        let mut mapper = ChunkMapper::new();
        assert_eq!(
            mapper.events(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"fc1","name":"upper"}}"#),
            vec![ModelEvent::ToolCallDelta {
                index: 0,
                id: Some("fc1".into()),
                name: Some("upper".into()),
                arguments_delta: String::new(),
            }]
        );
        assert_eq!(
            mapper.events(r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"text\":\"hi\"}"}"#),
            vec![ModelEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: r#"{"text":"hi"}"#.into(),
            }]
        );
        assert_eq!(
            mapper.events(r#"{"type":"response.completed"}"#),
            vec![ModelEvent::Done {
                stop: StopReason::ToolUse
            }]
        );
    }

    #[test]
    fn maps_text_and_plain_completion() {
        let mut mapper = ChunkMapper::new();
        assert_eq!(
            mapper.events(r#"{"type":"response.output_text.delta","delta":"hi"}"#),
            vec![ModelEvent::TextDelta { text: "hi".into() }]
        );
        assert_eq!(
            mapper.events(r#"{"type":"response.completed"}"#),
            vec![ModelEvent::Done {
                stop: StopReason::Stop
            }]
        );
    }

    #[test]
    fn request_maps_history_with_tool_turn() {
        let req = Request {
            system: Some("be brief".into()),
            messages: vec![
                tau_core::types::Message::user("shout hi"),
                tau_core::types::Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolCall {
                        id: "fc1".into(),
                        name: "upper".into(),
                        arguments: json!({"text": "hi"}),
                    }],
                },
                tau_core::types::Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        call_id: "fc1".into(),
                        content: "HI".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
        };
        let body = request_body("gpt-5", &req);
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["input"][2]["output"], "HI");
    }
}
