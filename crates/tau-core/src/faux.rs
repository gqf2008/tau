//! Scripted model for tests and demos — the faux-provider pattern from pi:
//! no real API, no keys, no tokens. Each `stream()` call pops the next round
//! of events from the script.

use std::sync::Mutex;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};

use crate::model::{Model, ModelEvent, Request};

pub struct FauxModel {
    rounds: Mutex<Vec<Vec<ModelEvent>>>,
}

impl FauxModel {
    /// `rounds[i]` is the event stream for the i-th model request.
    pub fn scripted(rounds: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            rounds: Mutex::new(rounds),
        }
    }

    /// One round of plain text, for demos.
    pub fn echo() -> Self {
        Self::scripted(vec![vec![
            ModelEvent::TextDelta { text: "tau is alive. ".into() },
            ModelEvent::TextDelta { text: "(faux model — set OPENAI_API_KEY for a real one)".into() },
            ModelEvent::Done {
                stop: crate::model::StopReason::Stop,
            },
        ]])
    }
}

#[async_trait]
impl Model for FauxModel {
    async fn stream(&self, _req: &Request) -> BoxStream<'static, ModelEvent> {
        let round = self.rounds.lock().unwrap().remove(0);
        stream::iter(round).boxed()
    }
}

use futures::StreamExt;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentEvent};
    use crate::model::StopReason;
    use crate::tool::{Tool, ToolDef, ToolOutput, ToolRegistry};
    use crate::types::{Content, Message, Role};

    struct Reverse;

    #[async_trait]
    impl Tool for Reverse {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "reverse".into(),
                description: "Reverse a string".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
            }
        }

        async fn execute(&self, arguments: serde_json::Value) -> ToolOutput {
            match arguments["text"].as_str() {
                Some(text) => ToolOutput::ok(text.chars().rev().collect::<String>()),
                None => ToolOutput::err("missing 'text'"),
            }
        }
    }

    fn tool_call_round() -> Vec<ModelEvent> {
        vec![
            ModelEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".into()),
                name: Some("reverse".into()),
                arguments_delta: String::new(),
            },
            ModelEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: r#"{"text":"#.into(),
            },
            ModelEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: r#""abc"}"#.into(),
            },
            ModelEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]
    }

    #[tokio::test]
    async fn loop_executes_tool_then_answers() {
        let model = FauxModel::scripted(vec![
            tool_call_round(),
            vec![
                ModelEvent::TextDelta { text: "cba reversed is abc".into() },
                ModelEvent::Done {
                    stop: StopReason::Stop,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Reverse));
        let agent = Agent::new(Box::new(model), tools);

        let mut events = Vec::new();
        let mut stream = agent.events();
        let produced = agent
            .run(&[], Message::user("reverse abc"))
            .await
            .unwrap();
        while let Ok(e) = stream.try_recv() {
            events.push(e);
        }

        // prompt + assistant(toolcall) + tool result + assistant(text)
        assert_eq!(produced.len(), 4);
        assert_eq!(produced[1].role, Role::Assistant);
        assert_eq!(produced[2].role, Role::Tool);
        assert_eq!(
            produced[2].content[0],
            Content::ToolResult {
                call_id: "call-1".into(),
                content: "cba".into(),
                is_error: false,
            }
        );
        assert_eq!(produced[3].text(), "cba reversed is abc");

        // The second model request saw the tool result in its context.
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallEnd {
                is_error: false,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_not_fatal() {
        let model = FauxModel::scripted(vec![
            tool_call_round(), // asks for "reverse", registry is empty
            vec![ModelEvent::Done {
                stop: StopReason::Stop,
            }],
        ]);
        let agent = Agent::new(Box::new(model), ToolRegistry::new());
        let produced = agent.run(&[], Message::user("x")).await.unwrap();
        assert_eq!(
            produced[2].content[0],
            Content::ToolResult {
                call_id: "call-1".into(),
                content: "unknown tool: reverse".into(),
                is_error: true,
            }
        );
    }

    #[tokio::test]
    async fn model_error_stops_run() {
        let model = FauxModel::scripted(vec![vec![
            ModelEvent::Error {
                message: "boom".into(),
            },
            ModelEvent::Done {
                stop: StopReason::Error,
            },
        ]]);
        let agent = Agent::new(Box::new(model), ToolRegistry::new());
        let err = agent.run(&[], Message::user("x")).await.unwrap_err();
        assert_eq!(err.to_string(), "model error: boom");
    }

    #[tokio::test]
    async fn max_turns_bounds_runaway_tool_loop() {
        let rounds = (0..10).map(|_| tool_call_round()).collect();
        let model = FauxModel::scripted(rounds);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Reverse));
        let agent = Agent::new(Box::new(model), tools).max_turns(3);
        let err = agent.run(&[], Message::user("x")).await.unwrap_err();
        assert!(err.to_string().contains("max turns"));
    }

    struct BlockDangerous;

    #[async_trait]
    impl crate::probe::ProbeHandler for BlockDangerous {
        fn points(&self) -> &[crate::probe::ProbePoint] {
            &[crate::probe::ProbePoint::BeforeTool]
        }

        async fn probe(
            &self,
            _point: crate::probe::ProbePoint,
            payload: serde_json::Value,
        ) -> crate::probe::Verdict {
            // jev-shaped verdict: typed judgment over the payload.
            if payload["args"]["text"].as_str() == Some("abc") {
                crate::probe::Verdict::Block {
                    reason: "policy: 'abc' is on the deny list".into(),
                }
            } else {
                crate::probe::Verdict::Continue
            }
        }
    }

    #[tokio::test]
    async fn probe_blocks_tool_call() {
        let model = FauxModel::scripted(vec![
            tool_call_round(),
            vec![ModelEvent::Done {
                stop: StopReason::Stop,
            }],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Reverse));
        let mut probes = crate::probe::ProbeRegistry::new();
        probes.register(Box::new(BlockDangerous));
        let agent = Agent::new(Box::new(model), tools).probes(probes);

        let produced = agent
            .run(&[], Message::user("reverse abc"))
            .await
            .unwrap();

        // The tool never ran; the model got a blocked tool result instead.
        assert_eq!(
            produced[2].content[0],
            Content::ToolResult {
                call_id: "call-1".into(),
                content: "blocked: policy: 'abc' is on the deny list".into(),
                is_error: true,
            }
        );
    }
}
