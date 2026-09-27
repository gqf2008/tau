//! Scripted model for tests and demos — the faux-provider pattern from pi:
//! no real API, no keys, no tokens. Each `stream()` call pops the next round
//! of events from the script.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream};

use crate::model::{Model, ModelEvent, RealtimeConfig, RealtimeSession, Request, StopReason};

/// A scripted model for tests and demos: plays back pre-recorded
/// event rounds, or synthesizes a one-tool-call-then-text demo flow.
pub struct FauxModel {
    rounds: Mutex<Vec<Vec<ModelEvent>>>,
    demo: std::sync::atomic::AtomicBool,
}

impl FauxModel {
    /// `rounds[i]` is the event stream for the i-th model request.
    pub fn scripted(rounds: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            rounds: Mutex::new(rounds),
            demo: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Demo mode: exercises the tool loop when tools are registered.
    /// First model call of a run (tools present, no tool result in the
    /// history yet) emits one call to the first tool — required string
    /// parameters are filled with the last user text, numbers with 1,
    /// booleans with true; required parameters of other shapes skip the
    /// call. The follow-up call (tool result present) answers plain text.
    /// Deterministic, offline, and makes `--demo -e tool.wasm` real.
    pub fn demo() -> Self {
        let model = Self::scripted(vec![]); // rounds unused; stream() synthesizes
        model.demo.store(true, std::sync::atomic::Ordering::Relaxed);
        model
    }

    /// One round of plain text, for demos.
    pub fn echo() -> Self {
        Self::scripted(vec![vec![
            ModelEvent::TextDelta {
                text: "tau is alive. ".into(),
            },
            ModelEvent::TextDelta {
                text: "(faux model — set ANTHROPIC_API_KEY or OPENAI_API_KEY for a real one)"
                    .into(),
            },
            ModelEvent::Done {
                stop: crate::model::StopReason::Stop,
            },
        ]])
    }
}

/// The demo realtime double (realtime-av Phase 2a): a deterministic
/// full-duplex script — VAD on the first chunk of a burst, every uplink
/// chunk echoed back as an AudioDelta of the same media type (the echo
/// IS the duplex proof), Interrupted answered to interrupt(), VAD-off
/// and Done at close. Only the `demo` variant has the capability, so
/// capability discovery has a negative case to assert.
pub struct FauxRealtime {
    config: RealtimeConfig,
    tx: futures::channel::mpsc::UnboundedSender<ModelEvent>,
    rx: Arc<Mutex<Option<futures::channel::mpsc::UnboundedReceiver<ModelEvent>>>>,
    speech_active: bool,
    noted: bool,
    closed: bool,
}

impl FauxRealtime {
    fn new(config: RealtimeConfig) -> Self {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        Self {
            config,
            tx,
            rx: Arc::new(Mutex::new(Some(rx))),
            speech_active: false,
            noted: false,
            closed: false,
        }
    }

    fn emit(&self, event: ModelEvent) {
        // Unbounded send to our own channel cannot fail while the
        // receiver lives; a dropped receiver means the CLI stopped
        // listening, which is the caller's business, not a panic.
        let _ = self.tx.unbounded_send(event);
    }
}

#[async_trait]
impl RealtimeSession for FauxRealtime {
    async fn push_audio(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        if self.closed {
            return Err("realtime session is closed".into());
        }
        if !self.speech_active {
            self.speech_active = true;
            self.emit(ModelEvent::SpeechStarted);
        }
        if !self.noted {
            self.noted = true;
            self.emit(ModelEvent::TextDelta {
                text: "live echo active. ".into(),
            });
        }
        self.emit(ModelEvent::InputAudioChunk {
            data: bytes.clone(),
            media_type: self.config.input_media_type.clone(),
        });
        // The echo: every uplink byte comes back down, same media
        // type — duplex, VAD, assembly and the playback sink all
        // exercised by one deterministic rule.
        self.emit(ModelEvent::AudioDelta {
            data: bytes,
            media_type: self.config.input_media_type.clone(),
        });
        Ok(())
    }

    async fn push_image(&mut self, _jpeg: Vec<u8>) -> Result<(), String> {
        if self.closed {
            return Err("realtime session is closed".into());
        }
        Ok(()) // accepted, unanswered — the demo double has no eyes
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        if self.closed {
            return Err("realtime session is closed".into());
        }
        self.emit(ModelEvent::Interrupted);
        Ok(())
    }

    async fn close(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(()); // closing twice is a no-op, not an error
        }
        self.closed = true;
        if self.speech_active {
            self.speech_active = false;
            self.emit(ModelEvent::SpeechStopped);
        }
        self.emit(ModelEvent::Done {
            stop: StopReason::Stop,
        });
        self.tx.close_channel();
        Ok(())
    }

    fn events(&self) -> BoxStream<'static, ModelEvent> {
        // Taken once, at open; a second take is an empty stream (the
        // trait documents this).
        match self.rx.lock().unwrap().take() {
            Some(rx) => Box::pin(rx),
            None => Box::pin(stream::empty()),
        }
    }
}

#[async_trait]
impl Model for FauxModel {
    fn realtime(&self, config: RealtimeConfig) -> Option<Box<dyn RealtimeSession>> {
        if self.demo.load(std::sync::atomic::Ordering::Relaxed) {
            Some(Box::new(FauxRealtime::new(config)))
        } else {
            None
        }
    }

    async fn stream(&self, req: &Request) -> BoxStream<'static, ModelEvent> {
        // Demo rounds are synthesized from the request, not scripted.
        if self.demo.load(std::sync::atomic::Ordering::Relaxed) {
            return stream::iter(demo_round(req)).boxed();
        }
        // The Model contract forbids panicking: once the script is
        // exhausted (interactive demo use runs past it), answer with a
        // fallback round instead of remove(0) on an empty vec.
        let round = {
            let mut rounds = self.rounds.lock().unwrap();
            if rounds.is_empty() {
                vec![
                    ModelEvent::TextDelta {
                        text: "tau is alive. ".into(),
                    },
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

/// One synthesized demo round: a scripted tool call when the run has
/// not seen a tool result yet, else the plain alive-text answer.
fn demo_round(req: &Request) -> Vec<ModelEvent> {
    use crate::model::StopReason;
    use crate::types::{Content, MediaSource, Role};

    // Voice message (docs/realtime-av.md Phase 0): echo the clip back
    // as three AudioDelta chunks of one media_type — the downlink
    // assembly (concatenate → Content::Audio) and the host playback
    // path get exercised for real, offline. A voice turn never reaches
    // the tool-call branch.
    let voice = req.messages.iter().rev().find_map(|m| {
        if m.role != Role::User {
            return None;
        }
        m.content.iter().find_map(|c| match c {
            Content::Audio { media } => match &media.source {
                MediaSource::Bytes(bytes) => Some(bytes.clone()),
                _ => None,
            },
            _ => None,
        })
    });
    if let Some(wav) = voice {
        let third = wav.len() / 3;
        return vec![
            ModelEvent::TextDelta {
                text: "echoing your voice clip. ".into(),
            },
            ModelEvent::AudioDelta {
                data: wav[..third].to_vec(),
                media_type: "audio/wav".into(),
            },
            ModelEvent::AudioDelta {
                data: wav[third..2 * third].to_vec(),
                media_type: "audio/wav".into(),
            },
            ModelEvent::AudioDelta {
                data: wav[2 * third..].to_vec(),
                media_type: "audio/wav".into(),
            },
            ModelEvent::Done {
                stop: StopReason::Stop,
            },
        ];
    }

    let answered = req.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|c| matches!(c, Content::ToolResult { .. }))
    });
    if !answered && let Some(call) = demo_tool_call(req) {
        return vec![
            ModelEvent::ToolCallDelta {
                index: 0,
                id: Some(call.0),
                name: Some(call.1),
                arguments_delta: call.2,
            },
            ModelEvent::Done {
                stop: StopReason::ToolUse,
            },
        ];
    }
    // A tool ran: say what it returned, so the demo shows the loop
    // closing (result → model), not just the call.
    let outcome = req
        .messages
        .iter()
        .rev()
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        });
    let middle = match outcome {
        Some((content, false)) => {
            format!("The tool answered: {}. ", crate::types::tool_result_text(&content))
        }
        Some((content, true)) => {
            format!("The tool failed: {}. ", crate::types::tool_result_text(&content))
        }
        None => String::new(),
    };
    vec![
        ModelEvent::TextDelta {
            text: "tau is alive. ".into(),
        },
        ModelEvent::TextDelta { text: middle },
        ModelEvent::TextDelta {
            text: "(faux model — set ANTHROPIC_API_KEY or OPENAI_API_KEY for a real one)".into(),
        },
        ModelEvent::Done {
            stop: StopReason::Stop,
        },
    ]
}

/// Build one deterministic tool call from the first tool's schema:
/// required strings get the last user text, numbers 1, booleans true;
/// anything else unfillable skips the call (None).
fn demo_tool_call(req: &Request) -> Option<(String, String, String)> {
    use crate::types::Content;
    let tool = req.tools.first()?;
    let prompt = req
        .messages
        .iter()
        .rev()
        .find_map(|m| {
            m.content.iter().find_map(|c| match c {
                Content::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .unwrap_or_else(|| "hello".into());
    let schema = &tool.parameters;
    let mut args = serde_json::Map::new();
    for name in schema["required"].as_array().into_iter().flatten() {
        let Some(name) = name.as_str() else { continue };
        let ty = schema["properties"][name]["type"]
            .as_str()
            .unwrap_or("string");
        let value = match ty {
            "string" => serde_json::Value::String(prompt.clone()),
            "number" | "integer" => serde_json::json!(1),
            "boolean" => serde_json::json!(true),
            _ => return None, // cannot fabricate — no demo call
        };
        args.insert(name.to_string(), value);
    }
    Some((
        "demo-call-1".to_string(),
        tool.name.clone(),
        serde_json::Value::Object(args).to_string(),
    ))
}

use futures::StreamExt;

#[cfg(test)]
mod realtime_tests {
    use super::*;
    use futures::StreamExt;

    fn config() -> RealtimeConfig {
        RealtimeConfig {
            input_media_type: "audio/pcm;rate=16000".into(),
            ..Default::default()
        }
    }

    #[test]
    fn realtime_capability_is_demo_only() {
        assert!(FauxModel::echo().realtime(config()).is_none());
        assert!(FauxModel::demo().realtime(config()).is_some());
    }

    #[tokio::test]
    async fn faux_realtime_vad_echo_and_close_script() {
        let mut session = FauxModel::demo().realtime(config()).unwrap();
        let mut events = session.events();
        session.push_audio(vec![1, 2, 3, 4]).await.unwrap();
        session.push_audio(vec![5, 6]).await.unwrap();
        session.interrupt().await.unwrap();
        session.close().await.unwrap();

        let mut kinds = Vec::new();
        while let Some(event) = events.next().await {
            kinds.push(event);
        }
        assert_eq!(
            kinds,
            vec![
                ModelEvent::SpeechStarted,
                ModelEvent::TextDelta { text: "live echo active. ".into() },
                ModelEvent::InputAudioChunk {
                    data: vec![1, 2, 3, 4],
                    media_type: "audio/pcm;rate=16000".into()
                },
                ModelEvent::AudioDelta {
                    data: vec![1, 2, 3, 4],
                    media_type: "audio/pcm;rate=16000".into()
                },
                ModelEvent::InputAudioChunk {
                    data: vec![5, 6],
                    media_type: "audio/pcm;rate=16000".into()
                },
                ModelEvent::AudioDelta {
                    data: vec![5, 6],
                    media_type: "audio/pcm;rate=16000".into()
                },
                ModelEvent::Interrupted,
                ModelEvent::SpeechStopped,
                ModelEvent::Done { stop: StopReason::Stop },
            ]
        );
    }

    #[tokio::test]
    async fn interrupted_freezes_the_segment_and_splits_assembly() {
        use crate::types::Content;
        // Barge-in mid-stream: the chunks before Interrupted stay as
        // one frozen segment; the SAME-media_type chunk after it must
        // open a new block (what played is what the user heard — no
        // silent concatenation across the truncation).
        let model = FauxModel::scripted(vec![vec![
            ModelEvent::AudioDelta { data: vec![1, 2], media_type: "audio/pcm".into() },
            ModelEvent::Interrupted,
            ModelEvent::AudioDelta { data: vec![3], media_type: "audio/pcm".into() },
            ModelEvent::AudioDelta { data: vec![4], media_type: "audio/pcm".into() },
            ModelEvent::Done { stop: StopReason::Stop },
        ]]);
        let agent = crate::Agent::new(Box::new(model), crate::ToolRegistry::new());
        let produced = agent.run(&[], crate::Message::user("talk")).await.unwrap();
        let assistant = produced
            .iter()
            .find(|m| m.role == crate::types::Role::Assistant)
            .expect("assistant message");
        let blocks: Vec<&Vec<u8>> = assistant
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Audio { media } => match &media.source {
                    crate::types::MediaSource::Bytes(b) => Some(b),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(blocks, [&vec![1, 2], &vec![3, 4]]);
    }

    #[tokio::test]
    async fn input_audio_chunks_are_facts_not_assistant_content() {
        use crate::types::Content;
        // Uplink kinds flowing through the loop (a realtime driver
        // shape) must not pollute the assistant message.
        let model = FauxModel::scripted(vec![vec![
            ModelEvent::InputAudioChunk { data: vec![9; 8], media_type: "audio/pcm".into() },
            ModelEvent::SpeechStarted,
            ModelEvent::SpeechStopped,
            ModelEvent::TextDelta { text: "heard you. ".into() },
            ModelEvent::Done { stop: StopReason::Stop },
        ]]);
        let agent = crate::Agent::new(Box::new(model), crate::ToolRegistry::new());
        let produced = agent.run(&[], crate::Message::user("talk")).await.unwrap();
        let assistant = produced
            .iter()
            .find(|m| m.role == crate::types::Role::Assistant)
            .expect("assistant message");
        assert_eq!(
            assistant.content,
            vec![Content::Text { text: "heard you. ".into() }]
        );
    }

    #[tokio::test]
    async fn closed_session_refuses_chunks_at_the_door() {
        let mut session = FauxModel::demo().realtime(config()).unwrap();
        session.close().await.unwrap();
        assert!(session.push_audio(vec![1]).await.is_err());
        assert!(session.interrupt().await.is_err());
        assert!(session.close().await.is_ok()); // twice is a no-op
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn audio_deltas_assemble_into_content_blocks() {
        use crate::model::StopReason;
        use crate::types::{Content, MediaSource};
        // Two chunks of pcm, then one of opus: two blocks, the pcm pair
        // concatenated. Plus text, to check block order.
        let model = FauxModel::scripted(vec![vec![
            ModelEvent::TextDelta {
                text: "listen. ".into(),
            },
            ModelEvent::AudioDelta {
                data: vec![1, 2],
                media_type: "audio/pcm".into(),
            },
            ModelEvent::AudioDelta {
                data: vec![3],
                media_type: "audio/pcm".into(),
            },
            ModelEvent::AudioDelta {
                data: vec![9, 9],
                media_type: "audio/opus".into(),
            },
            ModelEvent::Done {
                stop: StopReason::Stop,
            },
        ]]);
        let agent = crate::Agent::new(Box::new(model), crate::ToolRegistry::new());
        let produced = agent
            .run(&[], crate::Message::user("play something"))
            .await
            .unwrap();
        let assistant = produced
            .iter()
            .find(|m| m.role == crate::types::Role::Assistant)
            .expect("assistant message");
        assert!(matches!(&assistant.content[0], Content::Text { text } if text == "listen. "));
        match &assistant.content[1] {
            Content::Audio { media } => {
                assert_eq!(media.media_type, "audio/pcm");
                assert!(matches!(&media.source, MediaSource::Bytes(b) if b == &vec![1, 2, 3]));
            }
            other => panic!("expected audio, got {other:?}"),
        }
        match &assistant.content[2] {
            Content::Audio { media } => {
                assert_eq!(media.media_type, "audio/opus");
                assert!(matches!(&media.source, MediaSource::Bytes(b) if b == &vec![9, 9]));
            }
            other => panic!("expected audio, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn audio_delta_wire_round_trip_is_base64() {
        // The events.emit JSON channel carries base64, not a byte array.
        let event = ModelEvent::AudioDelta {
            data: vec![1, 2, 3],
            media_type: "audio/pcm".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""kind":"audio-delta""#), "{json}");
        assert!(json.contains(r#""data":"AQID""#), "{json}");
        assert!(json.contains(r#""media_type":"audio/pcm""#), "{json}");
        let back: ModelEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[tokio::test]
    async fn demo_calls_the_first_tool_then_answers() {
        use crate::model::StopReason;
        use crate::tool::ToolDef;
        let model = FauxModel::demo();
        let tool = ToolDef {
            name: "upper".into(),
            description: "shout".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }),
        };
        // First call of the run: a tool call for upper with the prompt.
        let req = Request {
            system: None,
            messages: vec![crate::Message::user("say hi")],
            tools: vec![tool.clone()],
        };
        let events: Vec<_> = model.stream(&req).await.collect().await;
        let call = events.iter().find_map(|e| match e {
            ModelEvent::ToolCallDelta {
                name,
                arguments_delta,
                ..
            } => Some((name.clone().unwrap(), arguments_delta.clone())),
            _ => None,
        });
        let (name, args) = call.expect("demo emits a tool call");
        assert_eq!(name, "upper");
        assert_eq!(args, r#"{"text":"say hi"}"#);
        assert!(matches!(
            events.last(),
            Some(ModelEvent::Done {
                stop: StopReason::ToolUse
            })
        ));

        // After the tool result lands, the demo answers plain text.
        let mut history = req.messages.clone();
        history.push(crate::Message {
            role: crate::types::Role::Assistant,
            content: vec![crate::types::Content::ToolResult {
                call_id: "demo-call-1".into(),
                content: vec![crate::types::Content::Text { text: "SAY HI".into() }],
                is_error: false,
            }],
        });
        let req2 = Request {
            system: None,
            messages: history,
            tools: vec![tool],
        };
        let events: Vec<_> = model.stream(&req2).await.collect().await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ModelEvent::TextDelta { .. }))
        );
        // The answer cites the tool result — the demo shows the loop
        // closing, not just the call.
        let answer: String = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(answer.contains("The tool answered: SAY HI."), "{answer}");
        assert!(matches!(
            events.last(),
            Some(ModelEvent::Done {
                stop: StopReason::Stop
            })
        ));
    }

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
                ModelEvent::TextDelta {
                    text: "cba reversed is abc".into(),
                },
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
        let produced = agent.run(&[], Message::user("reverse abc")).await.unwrap();
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
                content: vec![crate::types::Content::Text { text: "cba".into() }],
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
                content: vec![crate::types::Content::Text { text: "unknown tool: reverse".into() }],
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

        let produced = agent.run(&[], Message::user("reverse abc")).await.unwrap();

        // The tool never ran; the model got a blocked tool result instead.
        assert_eq!(
            produced[2].content[0],
            Content::ToolResult {
                call_id: "call-1".into(),
                content: vec![crate::types::Content::Text { text: "blocked: policy: 'abc' is on the deny list".into() }],
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
        async fn stream(&self, req: &Request) -> futures::stream::BoxStream<'static, ModelEvent> {
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
        assert!(
            fired[0].1["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["content"][0]["text"] == "hello probes")
        );
        // after_response sees the assembled assistant message.
        assert!(
            fired[1].1["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("hello probes")
        );
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
                content: vec![crate::types::Content::Text { text: "done".into() }],
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
        assert!(matches!(produced[1].content[0], Content::ToolCall { .. }));
        assert!(matches!(produced[2].content[0], Content::ToolResult { .. }));
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
        let produced = agent
            .run(&[], Message::user("first question"))
            .await
            .unwrap();
        assert_eq!(produced.len(), 4);
        assert_eq!(produced[1].text(), "answer one");
        assert_eq!(produced[2].text(), "follow-up question");
        assert_eq!(produced[3].text(), "answer two");
    }
}

#[cfg(test)]
mod blob_edge_tests {
    use crate::blobs::{BlobStore, INLINE_LIMIT, externalize};
    use crate::model::{Model, ModelEvent, Request, StopReason};
    use crate::types::{Content, Media, MediaSource};
    use crate::{Agent, Message, ToolRegistry};
    use async_trait::async_trait;

    /// Records the media-source kinds the request arrived with.
    struct Capture(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl Model for Capture {
        async fn stream(&self, req: &Request) -> futures::stream::BoxStream<'static, ModelEvent> {
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

#[cfg(test)]
mod compaction_tests {
    use crate::model::{Model, ModelEvent, Request, StopReason};
    use crate::probe::{ProbeHandler, ProbePoint, Verdict};
    use crate::{Agent, Message, ToolRegistry};
    use async_trait::async_trait;

    /// Answers with a fixed summary; records the request it saw.
    struct Summarizer(std::sync::Mutex<Option<Request>>);

    #[async_trait]
    impl Model for Summarizer {
        async fn stream(&self, req: &Request) -> futures::stream::BoxStream<'static, ModelEvent> {
            use futures::StreamExt;
            *self.0.lock().unwrap() = Some(Request {
                system: req.system.clone(),
                messages: req.messages.clone(),
                tools: vec![],
            });
            futures::stream::iter([
                ModelEvent::TextDelta {
                    text: "goal: demo; open: nothing".into(),
                },
                ModelEvent::Done {
                    stop: StopReason::Stop,
                },
            ])
            .boxed()
        }
    }

    struct Shared(std::sync::Arc<Summarizer>);
    #[async_trait]
    impl Model for Shared {
        async fn stream(&self, req: &Request) -> futures::stream::BoxStream<'static, ModelEvent> {
            self.0.stream(req).await
        }
    }

    fn agent() -> (Agent, std::sync::Arc<Summarizer>) {
        let model = std::sync::Arc::new(Summarizer(std::sync::Mutex::new(None)));
        (
            Agent::new(Box::new(Shared(model.clone())), ToolRegistry::new()),
            model,
        )
    }

    #[tokio::test]
    async fn compact_returns_summary_message() {
        let (agent, model) = agent();
        let history = vec![Message::user("hello"), Message::user("hi")];
        let summary = agent.compact(&history).await.unwrap();
        assert!(
            summary
                .text()
                .contains("[summary of the earlier conversation]")
        );
        assert!(summary.text().contains("goal: demo"));
        // The summarization request carried the history plus the ask.
        let request = model.0.lock().unwrap().take().unwrap();
        assert_eq!(request.messages.len(), 3);
        assert!(request.system.is_some());
        assert!(request.tools.is_empty());
    }

    #[tokio::test]
    async fn before_compaction_block_vetoes() {
        struct Veto;
        #[async_trait]
        impl ProbeHandler for Veto {
            fn points(&self) -> &[ProbePoint] {
                &[ProbePoint::BeforeCompaction]
            }
            async fn probe(&self, _point: ProbePoint, _payload: serde_json::Value) -> Verdict {
                Verdict::Block {
                    reason: "not now".into(),
                }
            }
        }
        let (agent, _model) = agent();
        let mut probes = crate::ProbeRegistry::new();
        probes.register(Box::new(Veto));
        let agent = agent.probes(probes);
        let err = agent.compact(&[Message::user("hello")]).await.unwrap_err();
        assert!(err.to_string().contains("not now"));
    }

    #[tokio::test]
    async fn before_compaction_replace_substitutes_the_message_set() {
        struct Replace;
        #[async_trait]
        impl ProbeHandler for Replace {
            fn points(&self) -> &[ProbePoint] {
                &[ProbePoint::BeforeCompaction]
            }
            async fn probe(&self, _point: ProbePoint, _payload: serde_json::Value) -> Verdict {
                Verdict::Replace(serde_json::json!({
                    "messages": [Message::user("only this")]
                }))
            }
        }
        let (agent, model) = agent();
        let mut probes = crate::ProbeRegistry::new();
        probes.register(Box::new(Replace));
        let agent = agent.probes(probes);
        agent
            .compact(&[Message::user("hello"), Message::user("hi")])
            .await
            .unwrap();
        let request = model.0.lock().unwrap().take().unwrap();
        // Replaced set + the summarization ask = 2 messages.
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].text(), "only this");
    }
}

#[cfg(test)]
mod navigation_tests {
    use crate::probe::{ProbeHandler, ProbePoint, Verdict};
    use crate::session::{EntryKind, JsonlStore, SessionEntry};
    use crate::{Agent, Message, ToolRegistry};
    use async_trait::async_trait;

    fn entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            parent: parent.map(str::to_string),
            kind: EntryKind::Message {
                message: Message::user(text),
            },
        }
    }

    fn store() -> JsonlStore {
        // Unique per call: the navigation tests run in parallel in one
        // process, and a pid-only path made them race the same file.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tau-nav-test-{}-{unique}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut store = JsonlStore::open(&path).unwrap();
        store.append(entry("aaa", None, "first")).unwrap();
        store.append(entry("bbb", Some("aaa"), "second")).unwrap();
        store.append(entry("ccc", Some("bbb"), "third")).unwrap();
        store
    }

    fn agent() -> Agent {
        Agent::new(
            Box::new(crate::faux::FauxModel::echo()),
            ToolRegistry::new(),
        )
    }

    #[tokio::test]
    async fn navigate_returns_the_branch_at_the_target() {
        let store = store();
        let (target, branch) = agent().navigate(&store, "bb").await.unwrap();
        assert_eq!(target, "bbb");
        let texts: Vec<String> = branch.iter().map(|m| m.text()).collect();
        assert_eq!(texts, vec!["first", "second"]);

        // A bare number is the append-order index `tau tree` displays.
        let (target, _) = agent().navigate(&store, "1").await.unwrap();
        assert_eq!(target, "bbb");
        assert!(agent().navigate(&store, "9").await.is_err());
    }

    #[tokio::test]
    async fn before_navigation_block_vetoes() {
        struct Veto;
        #[async_trait]
        impl ProbeHandler for Veto {
            fn points(&self) -> &[ProbePoint] {
                &[ProbePoint::BeforeNavigation]
            }
            async fn probe(&self, _point: ProbePoint, _payload: serde_json::Value) -> Verdict {
                Verdict::Block {
                    reason: "critical phase".into(),
                }
            }
        }
        let mut probes = crate::ProbeRegistry::new();
        probes.register(Box::new(Veto));
        let err = agent()
            .probes(probes)
            .navigate(&store(), "aaa")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("critical phase"));
    }

    #[tokio::test]
    async fn before_navigation_replace_redirects_the_target() {
        struct Redirect;
        #[async_trait]
        impl ProbeHandler for Redirect {
            fn points(&self) -> &[ProbePoint] {
                &[ProbePoint::BeforeNavigation]
            }
            async fn probe(&self, _point: ProbePoint, payload: serde_json::Value) -> Verdict {
                // Asked to go to bbb; redirect to aaa instead.
                assert_eq!(payload["target"], "bbb");
                Verdict::Replace(serde_json::json!({ "target": "aaa" }))
            }
        }
        let mut probes = crate::ProbeRegistry::new();
        probes.register(Box::new(Redirect));
        let (target, branch) = agent()
            .probes(probes)
            .navigate(&store(), "bbb")
            .await
            .unwrap();
        assert_eq!(target, "aaa");
        assert_eq!(branch.len(), 1);
        assert_eq!(branch[0].text(), "first");
    }

    #[tokio::test]
    async fn navigate_unknown_and_ambiguous_prefixes_are_errors() {
        let store = store();
        assert!(agent().navigate(&store, "zzz").await.is_err());
        // "a".."c" all start with different letters here, so reuse one
        // ambiguous prefix by adding another entry that shares it.
        let mut store = store;
        store.append(entry("aad", Some("ccc"), "fourth")).unwrap();
        assert!(agent().navigate(&store, "aa").await.is_err());
    }
}

#[cfg(test)]
mod observe_tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use crate::agent::AgentEvent;
    use crate::probe::{ProbeHandler, Verdict};
    use crate::{Agent, ProbePoint, ProbeRegistry, ToolRegistry};

    fn agent() -> Agent {
        Agent::new(
            Box::new(crate::faux::FauxModel::echo()),
            ToolRegistry::new(),
        )
    }

    /// Observe-only points: handlers see the payload; a misbehaving
    /// handler's verdict is reported on the bus as `ignored`, never
    /// honored.
    #[tokio::test]
    async fn observe_fires_handlers_and_ignores_verdicts() {
        struct Recorder {
            seen: Arc<Mutex<Vec<(ProbePoint, serde_json::Value)>>>,
        }
        #[async_trait]
        impl ProbeHandler for Recorder {
            fn points(&self) -> &[ProbePoint] {
                &[ProbePoint::SessionStart]
            }
            async fn probe(&self, point: ProbePoint, payload: serde_json::Value) -> Verdict {
                self.seen.lock().unwrap().push((point, payload));
                // Misuse: observe-only points must not honor this.
                Verdict::Block {
                    reason: "must be ignored".into(),
                }
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut probes = ProbeRegistry::new();
        probes.register(Box::new(Recorder { seen: seen.clone() }));
        let agent = agent().probes(probes);
        let mut events = agent.events();
        agent
            .observe(
                ProbePoint::SessionStart,
                serde_json::json!({ "session": "s.jsonl" }),
            )
            .await;
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[(
                ProbePoint::SessionStart,
                serde_json::json!({ "session": "s.jsonl" })
            )]
        );
        let mut saw_ignored = false;
        while let Ok(event) = events.try_recv() {
            if let AgentEvent::Probe { point, action } = event {
                assert_eq!(point, "session_start");
                assert_eq!(action, "ignored");
                saw_ignored = true;
            }
        }
        assert!(saw_ignored, "the misused verdict was not reported");
    }

    #[tokio::test]
    async fn observe_without_probes_is_a_noop() {
        agent()
            .observe(ProbePoint::SessionEnd, serde_json::json!({}))
            .await;
    }
}
