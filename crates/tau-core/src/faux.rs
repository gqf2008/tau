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
        // The Model contract forbids panicking: once the script is
        // exhausted (interactive demo use runs past it), answer with a
        // fallback round instead of remove(0) on an empty vec.
        let round = {
            let mut rounds = self.rounds.lock().unwrap();
            if rounds.is_empty() {
                vec![
                    ModelEvent::TextDelta { text: "tau is alive. ".into() },
                    ModelEvent::TextDelta {
                        text: "(faux model — script exhausted)".into(),
                    },
                    ModelEvent::Done {
                        stop: crate::model::StopReason::Stop,
                    },
                ]
            } else {
                rounds.remove(0)
            }
        };
        stream::iter(round).boxed()
    }
}

use futures::StreamExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exhausted_script_falls_back_instead_of_panicking() {
        use futures::StreamExt;
        let model = FauxModel::echo();
        let request = Request::default();
        let first: Vec<_> = model.stream(&request).await.collect().await;
        let second: Vec<_> = model.stream(&request).await.collect().await;
        assert!(matches!(first.last(), Some(ModelEvent::Done { .. })));
        assert!(matches!(second.last(), Some(ModelEvent::Done { .. })));
        assert!(second.iter().any(|e| matches!(
            e,
            ModelEvent::TextDelta { text } if text.contains("script exhausted")
        )));
    }

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

#[cfg(test)]
mod probe_point_tests {
    #![allow(clippy::module_inception)]
    use crate::model::{Model, ModelEvent, Request, StopReason};
    use crate::probe::{ProbeHandler, ProbePoint, ProbeRegistry, Verdict};
    use crate::{Agent, Message, ToolRegistry};
    use async_trait::async_trait;

    /// A real echo: answers with the last user message's text, so tests can
    /// observe what actually reached the model.
    struct EchoLast;

    #[async_trait]
    impl Model for EchoLast {
        async fn stream(
            &self,
            req: &Request,
        ) -> futures::stream::BoxStream<'static, ModelEvent> {
            use futures::StreamExt;
            let text = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == crate::types::Role::User)
                .map(|m| m.text())
                .unwrap_or_default();
            futures::stream::iter([
                ModelEvent::TextDelta { text },
                ModelEvent::Done {
                    stop: StopReason::Stop,
                },
            ])
            .boxed()
        }
    }

    /// Records every probe firing (point + payload) for assertion.
    struct Recorder(std::sync::Mutex<Vec<(ProbePoint, serde_json::Value)>>);

    #[async_trait]
    impl ProbeHandler for Recorder {
        fn points(&self) -> &[ProbePoint] {
            &[
                ProbePoint::BeforeRequest,
                ProbePoint::AfterResponse,
                ProbePoint::BeforeRunEnd,
            ]
        }

        async fn probe(&self, point: ProbePoint, payload: serde_json::Value) -> Verdict {
            self.0.lock().unwrap().push((point, payload));
            Verdict::Continue
        }
    }

    #[tokio::test]
    async fn request_response_and_run_end_probes_fire_in_order() {
        let model = EchoLast;
        let recorder = std::sync::Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
        struct Shared(std::sync::Arc<Recorder>);
        #[async_trait]
        impl ProbeHandler for Shared {
            fn points(&self) -> &[ProbePoint] {
                self.0.points()
            }
            async fn probe(&self, point: ProbePoint, payload: serde_json::Value) -> Verdict {
                self.0.probe(point, payload).await
            }
        }
        let mut probes = ProbeRegistry::new();
        probes.register(Box::new(Shared(recorder.clone())));
        let agent = Agent::new(Box::new(model), ToolRegistry::new()).probes(probes);
        agent.run(&[], Message::user("hello probes")).await.unwrap();

        let fired = recorder.0.lock().unwrap();
        let points: Vec<ProbePoint> = fired.iter().map(|(p, _)| *p).collect();
        assert_eq!(
            points,
            [
                ProbePoint::BeforeRequest,
                ProbePoint::AfterResponse,
                ProbePoint::BeforeRunEnd
            ]
        );
        // before_request sees the final request incl. the user message.
        assert!(fired[0].1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["content"][0]["text"] == "hello probes"));
        // after_response sees the assembled assistant message.
        assert!(fired[1].1["message"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hello probes"));
        // before_run_end sees everything the run produced.
        assert!(fired[2].1["messages"].as_array().unwrap().len() >= 2);
    }

    struct RewriteRequest;

    #[async_trait]
    impl ProbeHandler for RewriteRequest {
        fn points(&self) -> &[ProbePoint] {
            &[ProbePoint::BeforeRequest]
        }

        async fn probe(&self, _point: ProbePoint, mut payload: serde_json::Value) -> Verdict {
            // Rewrite the user text before it reaches the model.
            payload["messages"][0]["content"][0]["text"] =
                serde_json::Value::String("rewritten by probe".into());
            Verdict::Replace(payload)
        }
    }

    #[tokio::test]
    async fn before_request_can_rewrite_the_wire_request() {
        let model = EchoLast;
        let mut probes = ProbeRegistry::new();
        probes.register(Box::new(RewriteRequest));
        let agent = Agent::new(Box::new(model), ToolRegistry::new()).probes(probes);
        let produced = agent.run(&[], Message::user("original")).await.unwrap();
        // The echo model answers with what it saw: the rewritten request.
        assert_eq!(produced[1].text(), "rewritten by probe");
    }
}

#[cfg(test)]
mod control_tests {
    use super::FauxModel;
    use crate::model::{ModelEvent, StopReason};
    use crate::tool::{Tool, ToolOutput, ToolRegistry};
    use crate::types::{Content, Role};
    use crate::{Agent, AgentEvent, Control, Message};
    use async_trait::async_trait;

    fn text_round(text: &str) -> Vec<ModelEvent> {
        vec![
            ModelEvent::TextDelta { text: text.into() },
            ModelEvent::Done {
                stop: StopReason::Stop,
            },
        ]
    }

    fn tool_call_round() -> Vec<ModelEvent> {
        vec![
            ModelEvent::ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name: Some("noop".into()),
                arguments_delta: "{}".into(),
            },
            ModelEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]
    }

    struct Noop;

    #[async_trait]
    impl Tool for Noop {
        fn def(&self) -> crate::tool::ToolDef {
            crate::tool::ToolDef {
                name: "noop".into(),
                description: "does nothing".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(&self, _arguments: serde_json::Value) -> ToolOutput {
            ToolOutput {
                content: "done".into(),
                is_error: false,
            }
        }
    }

    /// A model that yields to the executor between deltas, so a concurrent
    /// task's Abort can actually land mid-stream (stream::iter never yields).
    struct SlowModel;

    #[async_trait]
    impl crate::model::Model for SlowModel {
        async fn stream(
            &self,
            _req: &crate::model::Request,
        ) -> futures::stream::BoxStream<'static, ModelEvent> {
            use futures::StreamExt;
            async_stream::stream! {
                for piece in ["first ", "second ", "third"] {
                    tokio::task::yield_now().await;
                    yield ModelEvent::TextDelta { text: piece.into() };
                }
                yield ModelEvent::Done { stop: StopReason::Stop };
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn abort_mid_stream_keeps_partial_text_and_stops() {
        let model = SlowModel;
        let agent = Agent::new(Box::new(model), ToolRegistry::new());
        let control = agent.control();
        let mut events = agent.events();
        // Abort as soon as the first delta is out.
        let run = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if matches!(event, AgentEvent::TextDelta(_)) {
                    control.send(Control::Abort).unwrap();
                    break;
                }
            }
        });
        let produced = agent.run(&[], Message::user("x")).await.unwrap();
        run.await.unwrap();
        let assistant = &produced[1];
        assert_eq!(assistant.role, Role::Assistant);
        // Partial text kept, stream cut short.
        let text = assistant.text();
        assert!(text.starts_with("first"));
        assert!(!text.contains("third"));
        assert_eq!(produced.len(), 2); // prompt + partial assistant, nothing else
    }

    #[tokio::test]
    async fn abort_during_stream_reports_aborted_on_the_bus() {
        let model = FauxModel::scripted(vec![vec![
            ModelEvent::TextDelta { text: "x".into() },
            ModelEvent::Done {
                stop: StopReason::Stop,
            },
        ]]);
        let agent = Agent::new(Box::new(model), ToolRegistry::new());
        agent.control().send(Control::Abort).unwrap();
        let mut events = agent.events();
        agent.run(&[], Message::user("x")).await.unwrap();
        let mut saw_abort = false;
        let mut saw_run_end_aborted = false;
        while let Ok(event) = events.try_recv() {
            saw_abort |= matches!(event, AgentEvent::Abort);
            saw_run_end_aborted |=
                matches!(event, AgentEvent::RunEnd { stop } if stop == StopReason::Aborted);
        }
        assert!(saw_abort && saw_run_end_aborted);
    }

    #[tokio::test]
    async fn steer_lands_after_tool_results_never_between() {
        let model = FauxModel::scripted(vec![tool_call_round(), text_round("after steer")]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let agent = Agent::new(Box::new(model), tools);
        agent
            .control()
            .send(Control::Steer(Message::user("steer message")))
            .unwrap();
        let produced = agent.run(&[], Message::user("start")).await.unwrap();
        // prompt, assistant(tool_call), tool result, steer, assistant(text)
        assert_eq!(produced.len(), 5);
        assert!(matches!(
            produced[1].content[0],
            Content::ToolCall { .. }
        ));
        assert!(matches!(
            produced[2].content[0],
            Content::ToolResult { .. }
        ));
        assert_eq!(produced[3].text(), "steer message");
        assert_eq!(produced[3].role, Role::User);
        assert_eq!(produced[4].text(), "after steer");
    }

    #[tokio::test]
    async fn followup_continues_the_same_run() {
        let model = FauxModel::scripted(vec![text_round("answer one"), text_round("answer two")]);
        let agent = Agent::new(Box::new(model), ToolRegistry::new());
        agent
            .control()
            .send(Control::FollowUp(Message::user("follow-up question")))
            .unwrap();
        let produced = agent.run(&[], Message::user("first question")).await.unwrap();
        assert_eq!(produced.len(), 4);
        assert_eq!(produced[1].text(), "answer one");
        assert_eq!(produced[2].text(), "follow-up question");
        assert_eq!(produced[3].text(), "answer two");
    }
}

#[cfg(test)]
mod blob_edge_tests {
    use crate::blobs::{externalize, BlobStore, INLINE_LIMIT};
    use crate::model::{Model, ModelEvent, Request, StopReason};
    use crate::types::{Content, Media, MediaSource};
    use crate::{Agent, Message, ToolRegistry};
    use async_trait::async_trait;

    /// Records the media-source kinds the request arrived with.
    struct Capture(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl Model for Capture {
        async fn stream(
            &self,
            req: &Request,
        ) -> futures::stream::BoxStream<'static, ModelEvent> {
            use futures::StreamExt;
            for message in &req.messages {
                for content in &message.content {
                    let kind = match content {
                        Content::Image { media } => match &media.source {
                            MediaSource::Bytes(_) => "bytes",
                            MediaSource::Url(_) => "url",
                            MediaSource::Blob { .. } => "blob",
                        },
                        _ => continue,
                    };
                    self.0.lock().unwrap().push(kind.to_string());
                }
            }
            futures::stream::iter([ModelEvent::Done {
                stop: StopReason::Stop,
            }])
            .boxed()
        }
    }

    #[tokio::test]
    async fn agent_materializes_blobs_before_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs"));
        // A history message as it comes back from a session read: large
        // media externalized to a blob reference.
        let mut history_message = Message::user("what is in this image");
        history_message.content.push(Content::Image {
            media: Media::bytes("image/png", vec![3u8; INLINE_LIMIT + 1]),
        });
        externalize(&mut history_message, &store).unwrap();
        assert!(matches!(
            history_message.content[1],
            Content::Image {
                media: Media {
                    source: MediaSource::Blob { .. },
                    ..
                }
            }
        ));

        let capture = std::sync::Arc::new(Capture(std::sync::Mutex::new(Vec::new())));
        struct Shared(std::sync::Arc<Capture>);
        #[async_trait]
        impl Model for Shared {
            async fn stream(
                &self,
                req: &Request,
            ) -> futures::stream::BoxStream<'static, ModelEvent> {
                self.0.stream(req).await
            }
        }
        let agent = Agent::new(Box::new(Shared(capture.clone())), ToolRegistry::new())
            .blobs(BlobStore::new(dir.path().join("blobs")));
        agent
            .run(&[history_message], Message::user("well?"))
            .await
            .unwrap();

        let kinds = capture.0.lock().unwrap();
        assert!(
            kinds.iter().all(|k| k == "bytes"),
            "model must see bytes, got: {kinds:?}"
        );
        assert_eq!(kinds.len(), 1);
    }
}
