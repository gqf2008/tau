//! The agent loop.
//!
//! One turn: send the active branch to the model, record the streamed
//! assistant response, execute its tool calls, record the results. If the
//! response had tool calls, start another turn; otherwise the run ends.

use std::collections::HashMap;

use thiserror::Error;

use crate::bus::EventBus;
use crate::control::{Control, ControlRx, ControlTx};
use crate::model::{Model, ModelEvent, Request, StopReason};
use crate::probe::{ProbePoint, ProbeRegistry, Verdict};
use crate::probe_payload::{
    AssembledContext, AssembledResponse, BeforeRun, Compaction, FinalRequest, Navigation,
    ProbePayload, RunEnd, ToolOutcome,
};
use crate::tool::ToolRegistry;
use crate::types::{Content, Message, ResultBlock, Role, ToolCall};

/// Lifecycle events published on the agent's bus ([`Agent::events`]):
/// the full observable trail of a run for UIs and loggers.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// A run began.
    RunStart,
    /// A fragment of assistant text.
    TextDelta(String),
    /// A provider streamed audio (realtime-style): the bytes of this
    /// chunk and the media type of the segment they belong to. The
    /// bytes ride the bus so the host renderer's playback sink can play
    /// as they arrive (realtime-av Phase 1); they also land in the
    /// assistant message as Content::Audio. The wasm subscribe path
    /// stays count-only (audio-segment) — no audio hot path to guests.
    AudioDelta {
        /// The audio bytes of this chunk.
        data: Vec<u8>,
        /// The media type of the segment they belong to.
        media_type: String,
    },
    /// Uplink fact (realtime sessions): the host pushed an audio chunk
    /// toward the provider. Count-only — the bytes are already local
    /// (the host pushed them); no renderer needs them twice on the bus.
    InputAudioChunk {
        /// How many bytes were pushed.
        bytes: usize,
        /// The uplink media type.
        media_type: String,
    },
    /// Server VAD observed the user start speaking (realtime).
    SpeechStarted,
    /// Server VAD observed the user stop speaking (realtime).
    SpeechStopped,
    /// Barge-in (realtime): the provider truncated the in-flight
    /// assistant audio — playback sinks clear their buffers.
    Interrupted,
    /// A tool call began executing.
    ToolCallStart {
        /// The call's id.
        id: String,
        /// The tool's name.
        name: String,
    },
    /// A tool call finished.
    ToolCallEnd {
        /// The call's id.
        id: String,
        /// The tool's name.
        name: String,
        /// Whether the tool reported failure.
        is_error: bool,
        /// The output the loop recorded (post-probe). Renderers should
        /// show a compact preview, not dump it verbatim.
        output: String,
    },
    /// A probe fired; observers see the full decision trail.
    Probe {
        /// The probe point's wire name.
        point: &'static str,
        /// The verdict's action (`continue`/`replace`/`block`, or
        /// `ignored` when an observe-only point got a verdict it must
        /// not honor).
        action: &'static str,
    },
    /// A steering message was injected after the current turn's tool results.
    Steer(Message),
    /// A queued follow-up became the next prompt in the same run.
    FollowUp(Message),
    /// An abort command was honored at a checkpoint.
    Abort,
    /// One model turn ended.
    TurnEnd {
        /// Why the model stopped.
        stop: StopReason,
    },
    /// The whole run ended.
    RunEnd {
        /// Why the final turn stopped.
        stop: StopReason,
    },
    /// The run failed.
    RunError {
        /// What failed.
        message: String,
    },
    /// A wasm extension pushed a user-visible notice through the host
    /// channel (`host.notify`). A fact for the UI, never model history.
    ExtensionNotice {
        /// "info" | "warn" | "error" (guest-chosen, free-form).
        level: String,
        /// The notice content; renderers draw text and media
        /// placeholders.
        content: Vec<Content>,
    },
    /// A wasm extension published an extension-defined fact through the
    /// host channel (`host.emit`). Observe-only: its schema is external
    /// to tau, so it travels as JSON.
    ExtensionFact(serde_json::Value),
}

/// Failures of the agent loop and its session operations.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The model reported an error event.
    #[error("model error: {0}")]
    Model(String),
    /// A `before_navigation` probe vetoed the navigation.
    #[error("navigation blocked: {0}")]
    NavigationBlocked(String),
    /// Session store failure.
    #[error("session: {0}")]
    Session(#[from] crate::session::SessionError),
}

/// The agent: a [`Model`], a tool set, probes, and the event bus, run as
/// the loop described at the module level. Build with [`Agent::new`] and
/// the `with`-style setters, then [`Agent::run`].
pub struct Agent {
    model: Box<dyn Model>,
    tools: ToolRegistry,
    probes: ProbeRegistry,
    bus: EventBus,
    system: Option<String>,
    /// Safety bound on consecutive model turns in one run.
    max_turns: usize,
    control_tx: ControlTx,
    control_rx: tokio::sync::Mutex<ControlRx>,
    /// Blob store for materializing externalized media at the request
    /// edge; sessions may carry `MediaSource::Blob` references.
    blobs: Option<crate::blobs::BlobStore>,
    /// Durable run progress (frames.rs): the harness attaches a sink so
    /// a crash mid-run salvages what committed. Set post-construction —
    /// the harness owns the sidecar path, the agent just writes through.
    frame_sink: std::sync::Mutex<Option<crate::frames::FrameSink>>,
}

impl Agent {
    /// An agent over the given model and tools (probes empty, defaults
    /// for everything else).
    pub fn new(model: Box<dyn Model>, tools: ToolRegistry) -> Self {
        let (control_tx, control_rx) = crate::control::channel();
        Self {
            model,
            tools,
            probes: ProbeRegistry::new(),
            bus: crate::bus::new_bus(),
            system: None,
            max_turns: 64,
            control_tx,
            control_rx: tokio::sync::Mutex::new(control_rx),
            blobs: None,
            frame_sink: std::sync::Mutex::new(None),
        }
    }

    /// Attach the blob store used to resolve `MediaSource::Blob` media
    /// back to bytes before each model request.
    /// Attach a blob store for materializing `MediaSource::Blob` media
    /// at the request edge.
    /// Attach (or detach, with None) the frame sink — post-construction
    /// because the harness owns the sidecar path, and a swapped-in agent
    /// (/reload) needs the same sink re-attached by the REPL.
    pub fn set_frame_sink(&self, sink: Option<crate::frames::FrameSink>) {
        *self.frame_sink.lock().unwrap() = sink;
    }

    /// Write one durable progress frame (no-op without a sink).
    fn frame(&self, frame: crate::frames::Frame) {
        if let Some(sink) = &*self.frame_sink.lock().unwrap() {
            sink(&frame);
        }
    }

    pub fn blobs(mut self, store: crate::blobs::BlobStore) -> Self {
        self.blobs = Some(store);
        self
    }

    /// Subscribe to the event stream. Call before `run`.
    /// Subscribe to this agent's [`AgentEvent`] stream.
    pub fn events(&self) -> crate::bus::EventStream {
        self.bus.subscribe()
    }

    /// The bus itself (sending half). For composition layers wiring an
    /// external publisher — tau-ext's host channel publishes
    /// [`AgentEvent::ExtensionNotice`]/[`AgentEvent::ExtensionFact`] here
    /// so extension output joins the same observable trail.
    pub fn bus(&self) -> EventBus {
        self.bus.clone()
    }

    /// The control channel into the loop: steer, follow-up, abort.
    /// Clone freely; safe to use from any task (see `control` module docs).
    /// A handle for sending control commands (steer, follow-up, abort)
    /// into a running loop.
    /// The model's realtime capability, if any (docs/realtime-av.md).
    /// Discovery IS `Model::realtime`; the agent forwards it untouched.
    pub fn realtime(
        &self,
        config: crate::model::RealtimeConfig,
    ) -> Option<Box<dyn crate::model::RealtimeSession>> {
        self.model.realtime(config)
    }

    pub fn control(&self) -> ControlTx {
        self.control_tx.clone()
    }

    fn emit(&self, event: AgentEvent) {
        // No subscribers is fine; a full channel is the subscriber's problem.
        let _ = self.bus.send(event);
    }

    /// Fire a probe, publish its outcome, return the verdict.
    async fn probe(&self, point: ProbePoint, payload: ProbePayload) -> Verdict {
        if self.probes.is_empty() {
            return Verdict::Continue;
        }
        let verdict = self.probes.probe(point, payload).await;
        let action = match &verdict {
            Verdict::Continue => "continue",
            Verdict::Replace(_) => "replace",
            Verdict::Block { .. } => "block",
        };
        if action != "continue" {
            self.emit(AgentEvent::Probe {
                point: point.name(),
                action,
            });
        }
        verdict
    }

    /// Fire an observe-only point (session lifecycle: `session_start`,
    /// `branch`, `session_end` — probes.md). Every handler registered
    /// for the point sees the payload; verdicts are ignored by contract.
    /// A non-continue verdict is reported on the bus as a `Probe` event
    /// with action `ignored` — visible, never honored.
    pub async fn observe(&self, point: ProbePoint, payload: ProbePayload) {
        debug_assert!(
            point.observe_only(),
            "observe() with an influence point: {point:?}"
        );
        if self.probes.is_empty() {
            return;
        }
        let verdict = self.probes.probe(point, payload).await;
        if !matches!(verdict, Verdict::Continue) {
            self.emit(AgentEvent::Probe {
                point: point.name(),
                action: "ignored",
            });
        }
    }

    /// Set the system prompt for requests built by this agent.
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Attach probe handlers (extension hooks observing/influencing runs).
    pub fn probes(mut self, probes: ProbeRegistry) -> Self {
        self.probes = probes;
        self
    }

    /// Bound the consecutive model turns in one run (runaway-loop guard).
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Run the loop to completion. `history` is the active branch so far;
    /// returns the new messages (assistant responses and tool results) to
    /// append to the session, in order. All lifecycle events are published
    /// on the bus; subscribe with [`Agent::events`].
    pub async fn run(
        &self,
        history: &[Message],
        prompt: Message,
    ) -> Result<Vec<Message>, AgentError> {
        self.emit(AgentEvent::RunStart);
        let result = self.run_inner(history, prompt).await;
        match &result {
            Ok((_, stop)) => self.emit(AgentEvent::RunEnd { stop: *stop }),
            Err(e) => self.emit(AgentEvent::RunError {
                message: e.to_string(),
            }),
        }
        result.map(|(messages, _)| messages)
    }

    /// Summarize `history` into one replacement message (pi-style
    /// compaction): the caller appends it as a `Compaction` entry and
    /// future branch walks yield it instead of the covered messages;
    /// originals stay in the session tree. The `before_compaction` probe
    /// may substitute the message set or veto the compaction.
    pub async fn compact(&self, history: &[Message]) -> Result<Message, AgentError> {
        self.compact_guided(history, None).await
    }

    /// `compact` with optional user instructions steering the summary
    /// (pi's `/compact [instructions]`): the notes are appended to the
    /// summarization request, so the brief emphasizes what the user asked
    /// to keep.
    pub async fn compact_guided(
        &self,
        history: &[Message],
        instructions: Option<&str>,
    ) -> Result<Message, AgentError> {
        use futures::StreamExt;

        let messages = match self
            .probe(
                ProbePoint::BeforeCompaction,
                ProbePayload::BeforeCompaction(Compaction {
                    reason: "manual".into(),
                    messages: history.to_vec(),
                }),
            )
            .await
        {
            Verdict::Replace(ProbePayload::BeforeCompaction(replaced)) => replaced.messages,
            // A replacement aimed at another point: the fold drops it, so
            // keep the history we already have.
            Verdict::Replace(_) => history.to_vec(),
            Verdict::Block { reason } => return Err(AgentError::Model(reason)),
            Verdict::Continue => history.to_vec(),
        };

        let mut ask = concat!(
            "Summarize the conversation so far for continuation: ",
            "the goal, decisions made, open tasks, and key facts. ",
            "Terse, plain text, no preamble."
        )
        .to_string();
        if let Some(notes) = instructions.map(str::trim).filter(|n| !n.is_empty()) {
            ask.push_str(&format!(" Additional instructions from the user: {notes}"));
        }
        let request = Request {
            system: Some(
                "You condense conversation history into a compact continuation brief.".into(),
            ),
            messages: [messages, vec![Message::user(ask)]].concat(),
            tools: vec![],
        };
        let mut stream = self.model.stream(&request).await;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                ModelEvent::TextDelta { text: delta } => text.push_str(&delta),
                ModelEvent::Error { message } => return Err(AgentError::Model(message)),
                ModelEvent::Done { .. } => break,
                _ => {}
            }
        }
        let summary = text.trim();
        if summary.is_empty() {
            return Err(AgentError::Model(
                "compaction produced an empty summary".into(),
            ));
        }
        Ok(Message::user(format!(
            "[summary of the earlier conversation]
{summary}"
        )))
    }

    /// Fork the session at `prefix_or_id`: probe `before_navigation`
    /// (block vetoes, replace redirects the target), then return the
    /// resolved entry id and the branch that becomes the new history.
    /// The caller appends the next entry with the id as parent — the
    /// fork materializes on write, pi-style.
    pub async fn navigate(
        &self,
        store: &crate::session::JsonlStore,
        prefix_or_id: &str,
    ) -> Result<(String, Vec<Message>), AgentError> {
        let mut target = store.resolve_id(prefix_or_id)?;
        let entry = store
            .get(&target)
            .ok_or_else(|| crate::session::SessionError::NotFound(target.clone()))?;
        let summary = crate::session::entry_summary(entry);

        match self
            .probe(
                ProbePoint::BeforeNavigation,
                ProbePayload::BeforeNavigation(Navigation {
                    target: target.clone(),
                    summary,
                }),
            )
            .await
        {
            Verdict::Replace(ProbePayload::BeforeNavigation(replaced)) => {
                target = store.resolve_id(&replaced.target)?;
            }
            Verdict::Replace(_) => {}
            Verdict::Block { reason } => return Err(AgentError::NavigationBlocked(reason)),
            Verdict::Continue => {}
        }

        let branch = store.active_branch(&target)?;
        Ok((target, branch))
    }

    async fn run_inner(
        &self,
        history: &[Message],
        prompt: Message,
    ) -> Result<(Vec<Message>, StopReason), AgentError> {
        let prompt = match self
            .probe(
                ProbePoint::BeforeRun,
                ProbePayload::BeforeRun(BeforeRun {
                    prompt: prompt.clone(),
                }),
            )
            .await
        {
            Verdict::Replace(ProbePayload::BeforeRun(replaced)) => replaced.prompt,
            Verdict::Replace(_) => prompt,
            Verdict::Block { reason } => return Err(AgentError::Model(reason)),
            Verdict::Continue => prompt,
        };

        let mut control_rx = self.control_rx.lock().await;
        let mut pending: Vec<Control> = Vec::new();
        let mut produced = vec![prompt];
        for _ in 0..self.max_turns {
            self.frame(crate::frames::Frame::TurnStart);
            let mut request = Request {
                system: self.system.clone(),
                messages: [history, &produced].concat(),
                tools: self.tools.defs(),
            };
            if let Verdict::Replace(ProbePayload::TransformContext(replaced)) = self
                .probe(
                    ProbePoint::TransformContext,
                    ProbePayload::TransformContext(AssembledContext {
                        system: request.system.clone(),
                        messages: request.messages.clone(),
                    }),
                )
                .await
            {
                request.messages = replaced.messages;
                request.system = replaced.system;
            }
            let final_request = ProbePayload::BeforeRequest(FinalRequest {
                system: request.system.clone(),
                messages: request.messages.clone(),
                tools: request.tools.clone(),
            });
            let request = match self.probe(ProbePoint::BeforeRequest, final_request).await {
                Verdict::Replace(ProbePayload::BeforeRequest(replaced)) => Request {
                    system: replaced.system,
                    messages: replaced.messages,
                    tools: request.tools, // tools are registry-owned; not replaceable here
                },
                Verdict::Replace(_) => request,
                Verdict::Block { reason } => return Err(AgentError::Model(reason)),
                Verdict::Continue => request,
            };
            // Materialize externalized media at the request edge: the
            // model contract is bytes-only (missing blobs degrade to text
            // notes, they do not fail the run).
            let mut request = request;
            if let Some(blobs) = &self.blobs {
                for message in &mut request.messages {
                    crate::blobs::materialize(message, blobs);
                }
            }
            let (assistant, stop) = self
                .stream_turn(request, &mut control_rx, &mut pending)
                .await?;
            if stop == StopReason::Aborted {
                if !assistant.content.is_empty() {
                    produced.push(assistant);
                }
                return Ok((produced, StopReason::Aborted));
            }
            let (assistant, stop) = match self
                .probe(
                    ProbePoint::AfterResponse,
                    ProbePayload::AfterResponse(AssembledResponse {
                        message: assistant.clone(),
                        stop,
                    }),
                )
                .await
            {
                Verdict::Replace(ProbePayload::AfterResponse(replaced)) => {
                    (replaced.message, replaced.stop)
                }
                Verdict::Replace(_) => (assistant, stop),
                Verdict::Block { reason } => return Err(AgentError::Model(reason)),
                Verdict::Continue => (assistant, stop),
            };
            self.emit(AgentEvent::TurnEnd { stop });

            let calls: Vec<(String, String, serde_json::Value)> = assistant
                .tool_calls()
                .map(|(id, name, args)| (id.to_string(), name.to_string(), args.clone()))
                .collect();
            produced.push(assistant);

            if calls.is_empty() || stop != StopReason::ToolUse {
                drain_control(&mut control_rx, &mut pending);
                if take_abort(&mut pending) {
                    self.emit(AgentEvent::Abort);
                    return Ok((produced, StopReason::Aborted));
                }
                // Follow-ups continue the same run: one trail, one RunEnd.
                // A steer with no tool calls in flight is indistinguishable
                // from a follow-up at the natural end — take both (steers
                // first: they were sent as the more urgent correction)
                // rather than dropping the steer on the floor.
                let steers = take_steers(&mut pending);
                let followups = take_followups(&mut pending);
                if !steers.is_empty() || !followups.is_empty() {
                    for message in &steers {
                        self.emit(AgentEvent::Steer(message.clone()));
                    }
                    for message in &followups {
                        self.emit(AgentEvent::FollowUp(message.clone()));
                    }
                    produced.extend(steers);
                    produced.extend(followups);
                    continue;
                }
                let produced = match self
                    .probe(
                        ProbePoint::BeforeRunEnd,
                        ProbePayload::BeforeRunEnd(RunEnd {
                            messages: produced.clone(),
                            stop,
                        }),
                    )
                    .await
                {
                    Verdict::Replace(ProbePayload::BeforeRunEnd(replaced)) => replaced.messages,
                    Verdict::Replace(_) => produced,
                    Verdict::Block { reason } => return Err(AgentError::Model(reason)),
                    Verdict::Continue => produced,
                };
                return Ok((produced, stop));
            }

            let mut results = Vec::with_capacity(calls.len());
            for (id, name, args) in calls {
                self.emit(AgentEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                });
                let call = ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: args.clone(),
                };
                let args = match self
                    .probe(ProbePoint::BeforeTool, ProbePayload::BeforeTool(call))
                    .await
                {
                    Verdict::Replace(ProbePayload::BeforeTool(replaced)) => replaced.arguments,
                    Verdict::Replace(_) => args,
                    Verdict::Block { reason } => {
                        self.emit(AgentEvent::ToolCallEnd {
                            id: id.clone(),
                            name: name.clone(),
                            is_error: true,
                            output: format!("blocked: {reason}"),
                        });
                        results.push(Content::ToolResult {
                            call_id: id,
                            content: vec![Content::Text {
                                text: format!("blocked: {reason}"),
                            }],
                            is_error: true,
                        });
                        continue;
                    }
                    Verdict::Continue => args,
                };
                let output = match self.tools.get(&name) {
                    Some(tool) => tool.execute(args.clone()).await,
                    None => crate::tool::ToolOutput::err(format!("unknown tool: {name}")),
                };
                let blocks = output
                    .content
                    .iter()
                    .cloned()
                    .map(ResultBlock::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap_or_else(|_| {
                        // A result carrying a block the ABI has no arm for
                        // (a nested tool call): fall back to the flattened
                        // text, which is the shape 0.6.0 always sent.
                        vec![ResultBlock::Text {
                            text: output.text(),
                        }]
                    });
                let outcome = ProbePayload::AfterTool(ToolOutcome {
                    call: ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: args.clone(),
                    },
                    content: blocks,
                    is_error: output.is_error,
                });
                let output = match self.probe(ProbePoint::AfterTool, outcome).await {
                    // A replacement rewrites what the model is told the tool
                    // returned; the call itself is history and stays.
                    Verdict::Replace(ProbePayload::AfterTool(replaced)) => crate::tool::ToolOutput {
                        content: replaced.content.into_iter().map(Content::from).collect(),
                        is_error: replaced.is_error,
                    },
                    _ => output,
                };
                self.frame(crate::frames::Frame::ToolResult {
                    call_id: id.clone(),
                    text: output.text(),
                    is_error: output.is_error,
                });
                self.emit(AgentEvent::ToolCallEnd {
                    id: id.clone(),
                    name,
                    is_error: output.is_error,
                    output: output.text(),
                });
                results.push(Content::ToolResult {
                    call_id: id,
                    content: output.content,
                    is_error: output.is_error,
                });
            }
            produced.push(Message {
                role: Role::Tool,
                content: results,
            });
            // Steer lands here — after the turn's tool results, never
            // between a tool_use and its tool_result.
            drain_control(&mut control_rx, &mut pending);
            if take_abort(&mut pending) {
                self.emit(AgentEvent::Abort);
                return Ok((produced, StopReason::Aborted));
            }
            for message in take_steers(&mut pending) {
                self.emit(AgentEvent::Steer(message.clone()));
                produced.push(message);
            }
        }
        Err(AgentError::Model(format!(
            "exceeded max turns ({})",
            self.max_turns
        )))
    }

    /// Stream one assistant response, reassembling tool-call deltas.
    /// Drains the control channel per model event: an Abort stops
    /// consumption immediately — partial tool-call deltas are dropped
    /// (their arguments are by definition incomplete), text is kept.
    async fn stream_turn(
        &self,
        request: Request,
        control_rx: &mut ControlRx,
        pending: &mut Vec<Control>,
    ) -> Result<(Message, StopReason), AgentError> {
        use futures::StreamExt;

        let mut stream = self.model.stream(&request).await;
        let mut text = String::new();
        // Contiguous audio segments: a new block starts when the media
        // type changes; chunks within a block concatenate.
        let mut audio: Vec<(String, Vec<u8>)> = Vec::new();
        // Barge-in boundary: an Interrupted freezes the current
        // segment (what played is what the user heard); the next
        // delta opens a NEW segment even at the same media type.
        let mut segment_frozen = false;
        let mut calls: HashMap<u32, (String, String, String)> = HashMap::new();
        let mut error = None;
        let mut stop = StopReason::Stop;

        while let Some(event) = stream.next().await {
            drain_control(control_rx, pending);
            if take_abort(pending) {
                self.emit(AgentEvent::Abort);
                let content = if text.is_empty() {
                    Vec::new()
                } else {
                    vec![Content::Text { text }]
                };
                return Ok((
                    Message {
                        role: Role::Assistant,
                        content,
                    },
                    StopReason::Aborted,
                ));
            }
            match event {
                ModelEvent::TextDelta { text: delta } => {
                    text.push_str(&delta);
                    self.frame(crate::frames::Frame::TextDelta {
                        text: delta.clone(),
                    });
                    self.emit(AgentEvent::TextDelta(delta));
                }
                ModelEvent::AudioDelta { data, media_type } => {
                    self.emit(AgentEvent::AudioDelta {
                        data: data.clone(),
                        media_type: media_type.clone(),
                    });
                    match audio.last_mut() {
                        Some((ty, bytes)) if *ty == media_type && !segment_frozen => {
                            bytes.extend_from_slice(&data)
                        }
                        _ => audio.push((media_type, data)),
                    }
                    segment_frozen = false;
                }
                ModelEvent::InputAudioChunk { data, media_type } => {
                    // Uplink fact: the loop does not assemble these into
                    // assistant content — they record what the model
                    // HEARD (the CLI's realtime driver writes the user
                    // message; the bus carries the count for renderers).
                    self.emit(AgentEvent::InputAudioChunk {
                        bytes: data.len(),
                        media_type,
                    });
                }
                ModelEvent::SpeechStarted => self.emit(AgentEvent::SpeechStarted),
                ModelEvent::SpeechStopped => self.emit(AgentEvent::SpeechStopped),
                ModelEvent::Interrupted => {
                    self.emit(AgentEvent::Interrupted);
                    segment_frozen = true;
                }
                ModelEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => {
                    self.frame(crate::frames::Frame::ToolCallDelta {
                        index,
                        id: id.clone(),
                        name: name.clone(),
                        arguments_delta: arguments_delta.clone(),
                    });
                    let call = calls.entry(index).or_default();
                    if let Some(id) = id {
                        call.0 = id;
                    }
                    if let Some(name) = name {
                        call.1 = name;
                    }
                    call.2.push_str(&arguments_delta);
                }
                ModelEvent::Done { stop: s } => stop = s,
                ModelEvent::Error { message } => error = Some(message),
            }
        }

        if stop == StopReason::Error {
            return Err(AgentError::Model(
                error.unwrap_or_else(|| "unknown model error".into()),
            ));
        }

        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(Content::Text { text });
        }
        for (media_type, bytes) in audio {
            content.push(Content::Audio {
                media: crate::types::Media::bytes(media_type, bytes),
            });
        }
        let mut ordered: Vec<_> = calls.into_iter().collect();
        ordered.sort_by_key(|(index, _)| *index);
        for (_, (id, name, arguments)) in ordered {
            let arguments = if arguments.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "__invalidJson": arguments }))
            };
            content.push(Content::ToolCall {
                id,
                name,
                arguments,
            });
        }
        Ok((
            Message {
                role: Role::Assistant,
                content,
            },
            stop,
        ))
    }
}

/// Move every queued control command into `pending` without blocking.
fn drain_control(rx: &mut ControlRx, pending: &mut Vec<Control>) {
    while let Ok(control) = rx.try_recv() {
        pending.push(control);
    }
}

fn take_abort(pending: &mut Vec<Control>) -> bool {
    if let Some(pos) = pending.iter().position(|c| matches!(c, Control::Abort)) {
        pending.remove(pos);
        true
    } else {
        false
    }
}

fn take_steers(pending: &mut Vec<Control>) -> Vec<Message> {
    let mut taken = Vec::new();
    let mut rest = Vec::with_capacity(pending.len());
    for control in pending.drain(..) {
        match control {
            Control::Steer(m) => taken.push(m),
            other => rest.push(other),
        }
    }
    *pending = rest;
    taken
}

fn take_followups(pending: &mut Vec<Control>) -> Vec<Message> {
    let mut taken = Vec::new();
    let mut rest = Vec::with_capacity(pending.len());
    for control in pending.drain(..) {
        match control {
            Control::FollowUp(m) => taken.push(m),
            other => rest.push(other),
        }
    }
    *pending = rest;
    taken
}
