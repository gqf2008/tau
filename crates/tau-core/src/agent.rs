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
use crate::tool::ToolRegistry;
use crate::types::{Content, Message, Role};

/// Lifecycle events published on the agent's bus ([`Agent::events`]):
/// the full observable trail of a run for UIs and loggers.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// A run began.
    RunStart,
    /// A fragment of assistant text.
    TextDelta(String),
    /// A provider streamed audio (realtime-style): how many bytes just
    /// arrived and of what media type. The bytes themselves are not on
    /// the bus — they land in the assistant message as Content::Audio.
    AudioDelta {
        /// How many bytes just arrived.
        bytes: usize,
        /// The media type of the segment they belong to.
        media_type: String,
    },
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
        /// The verdict's action (`continue`/`replace`/`block`).
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
        }
    }

    /// Attach the blob store used to resolve `MediaSource::Blob` media
    /// back to bytes before each model request.
    /// Attach a blob store for materializing `MediaSource::Blob` media
    /// at the request edge.
    pub fn blobs(mut self, store: crate::blobs::BlobStore) -> Self {
        self.blobs = Some(store);
        self
    }

    /// Subscribe to the event stream. Call before `run`.
    /// Subscribe to this agent's [`AgentEvent`] stream.
    pub fn events(&self) -> crate::bus::EventStream {
        self.bus.subscribe()
    }

    /// The control channel into the loop: steer, follow-up, abort.
    /// Clone freely; safe to use from any task (see `control` module docs).
    /// A handle for sending control commands (steer, follow-up, abort)
    /// into a running loop.
    pub fn control(&self) -> ControlTx {
        self.control_tx.clone()
    }

    fn emit(&self, event: AgentEvent) {
        // No subscribers is fine; a full channel is the subscriber's problem.
        let _ = self.bus.send(event);
    }

    /// Fire a probe, publish its outcome, return the verdict.
    async fn probe(&self, point: ProbePoint, payload: serde_json::Value) -> Verdict {
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
        use futures::StreamExt;

        let messages = match self
            .probe(
                ProbePoint::BeforeCompaction,
                serde_json::json!({ "reason": "manual", "messages": history }),
            )
            .await
        {
            Verdict::Replace(payload) => serde_json::from_value(payload["messages"].clone())
                .map_err(|e| AgentError::Model(format!("bad before_compaction payload: {e}")))?,
            Verdict::Block { reason } => return Err(AgentError::Model(reason)),
            Verdict::Continue => history.to_vec(),
        };

        let request = Request {
            system: Some(
                "You condense conversation history into a compact continuation brief.".into(),
            ),
            messages: [
                messages,
                vec![Message::user(concat!(
                    "Summarize the conversation so far for continuation: ",
                    "the goal, decisions made, open tasks, and key facts. ",
                    "Terse, plain text, no preamble."
                ))],
            ]
            .concat(),
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
                serde_json::json!({ "target": target, "summary": summary }),
            )
            .await
        {
            Verdict::Replace(payload) => {
                let redirected = payload["target"].as_str().ok_or_else(|| {
                    AgentError::Model("bad before_navigation payload: no target".into())
                })?;
                target = store.resolve_id(redirected)?;
            }
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
                serde_json::json!({ "prompt": prompt }),
            )
            .await
        {
            Verdict::Replace(payload) => serde_json::from_value(payload["prompt"].clone())
                .map_err(|e| AgentError::Model(format!("bad before_run payload: {e}")))?,
            Verdict::Block { reason } => return Err(AgentError::Model(reason)),
            Verdict::Continue => prompt,
        };

        let mut control_rx = self.control_rx.lock().await;
        let mut pending: Vec<Control> = Vec::new();
        let mut produced = vec![prompt];
        for _ in 0..self.max_turns {
            let mut request = Request {
                system: self.system.clone(),
                messages: [history, &produced].concat(),
                tools: self.tools.defs(),
            };
            if let Verdict::Replace(payload) = self
                .probe(
                    ProbePoint::TransformContext,
                    serde_json::json!({
                        "messages": request.messages,
                        "system": request.system,
                    }),
                )
                .await
            {
                if let Some(messages) = payload.get("messages") {
                    request.messages = serde_json::from_value(messages.clone())
                        .map_err(|e| AgentError::Model(format!("bad context payload: {e}")))?;
                }
                if let Some(system) = payload.get("system") {
                    request.system = serde_json::from_value(system.clone())
                        .map_err(|e| AgentError::Model(format!("bad context payload: {e}")))?;
                }
            }
            let request_json = serde_json::json!({
                "system": request.system,
                "messages": request.messages,
                "tools": request.tools,
            });
            let request = match self.probe(ProbePoint::BeforeRequest, request_json).await {
                Verdict::Replace(payload) => Request {
                    system: serde_json::from_value(payload["system"].clone()).map_err(|e| {
                        AgentError::Model(format!("bad before_request payload: {e}"))
                    })?,
                    messages: serde_json::from_value(payload["messages"].clone()).map_err(|e| {
                        AgentError::Model(format!("bad before_request payload: {e}"))
                    })?,
                    tools: request.tools, // tools are registry-owned; not replaceable here
                },
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
                    serde_json::json!({ "message": assistant, "stop": stop }),
                )
                .await
            {
                Verdict::Replace(payload) => {
                    let message =
                        serde_json::from_value(payload["message"].clone()).map_err(|e| {
                            AgentError::Model(format!("bad after_response payload: {e}"))
                        })?;
                    let stop = serde_json::from_value(payload["stop"].clone()).unwrap_or(stop);
                    (message, stop)
                }
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
                        serde_json::json!({ "messages": produced, "stop": stop }),
                    )
                    .await
                {
                    Verdict::Replace(payload) => {
                        serde_json::from_value(payload["messages"].clone()).map_err(|e| {
                            AgentError::Model(format!("bad before_run_end payload: {e}"))
                        })?
                    }
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
                let args = match self
                    .probe(
                        ProbePoint::BeforeTool,
                        serde_json::json!({ "id": id, "name": name, "args": args }),
                    )
                    .await
                {
                    Verdict::Replace(args) => args,
                    Verdict::Block { reason } => {
                        self.emit(AgentEvent::ToolCallEnd {
                            id: id.clone(),
                            name: name.clone(),
                            is_error: true,
                            output: format!("blocked: {reason}"),
                        });
                        results.push(Content::ToolResult {
                            call_id: id,
                            content: format!("blocked: {reason}"),
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
                let output = match self
                    .probe(
                        ProbePoint::AfterTool,
                        serde_json::json!({
                            "id": id,
                            "name": name,
                            "args": args,
                            "content": output.content,
                            "isError": output.is_error,
                        }),
                    )
                    .await
                {
                    Verdict::Replace(payload) => crate::tool::ToolOutput {
                        content: payload["content"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or(output.content),
                        is_error: payload["isError"].as_bool().unwrap_or(output.is_error),
                    },
                    _ => output,
                };
                self.emit(AgentEvent::ToolCallEnd {
                    id: id.clone(),
                    name,
                    is_error: output.is_error,
                    output: output.content.clone(),
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
                    self.emit(AgentEvent::TextDelta(delta));
                }
                ModelEvent::AudioDelta { data, media_type } => {
                    self.emit(AgentEvent::AudioDelta {
                        bytes: data.len(),
                        media_type: media_type.clone(),
                    });
                    match audio.last_mut() {
                        Some((ty, bytes)) if *ty == media_type => bytes.extend_from_slice(&data),
                        _ => audio.push((media_type, data)),
                    }
                }
                ModelEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => {
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
