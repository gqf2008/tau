//! Typed probe payloads — the Rust side of `variant payload` in the 0.7.0
//! contract (`wit/tau.wit`, `interface probes`), per the projection
//! rule in `docs/wit-redesign.md` §3.
//!
//! Before this, every firing handed the handler a `Json` object and the
//! handler pulled fields out by name (`payload["messages"]`), so a renamed
//! or missing field was a runtime no-op instead of a compile error — and
//! the same hole sat one level out at the wasm boundary. The point is
//! fixed when a handler is called, so the payload can be typed without
//! guessing: one arm per point, and [`ProbePayload::point`] is the only
//! way to ask which one it is.
//!
//! The JSON edge is a compatibility shim, and it is deliberate:
//! [`ProbePayload::to_json`] reproduces 0.6.0's payload shapes and
//! [`ProbePayload::merge_json`] reproduces 0.6.0's replacement semantics,
//! because extensions on the wire still speak `payload-json` /
//! `replace-json` (`interface probes`). Migration stage 1 must not move
//! the wire under them. Stage 2 swaps the contract for the typed payload
//! and deletes both functions.

use serde::de::DeserializeOwned;
use serde_json::{Value as Json, json};

use crate::error::HostError;
use crate::model::StopReason;
use crate::probe::ProbePoint;
use crate::tool::ToolDef;
use crate::types::{Content, Message, ResultBlock, ToolCall, tool_result_text};

/// `before_run`: the prompt the run starts from.
#[derive(Debug, Clone)]
pub struct BeforeRun {
    /// The user message that opened the run.
    pub prompt: Message,
}

/// `transform_context`: the assembled context, before the request is
/// built. Splitting it from `before_request` is what lets a component
/// rewrite history without touching the tool table.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    /// System prompt, when the agent has one.
    pub system: Option<String>,
    /// Active branch, oldest first.
    pub messages: Vec<Message>,
}

/// `before_request`: the final request, after `transform_context`, on its
/// way to the provider. The tool table is registry-owned and not
/// replaceable at this point.
#[derive(Debug, Clone)]
pub struct FinalRequest {
    /// System prompt, when the agent has one.
    pub system: Option<String>,
    /// What the provider will see.
    pub messages: Vec<Message>,
    /// The advertised tools, in registry order.
    pub tools: Vec<ToolDef>,
}

/// `after_response`: one assistant response, before its tool calls run.
#[derive(Debug, Clone)]
pub struct AssembledResponse {
    /// The assistant message as assembled from the stream.
    pub message: Message,
    /// Why the stream ended.
    pub stop: StopReason,
}

/// `after_tool`: the recorded outcome of one tool call. A replacement
/// rewrites what the model is told the tool returned — the call itself
/// (`id`/`name`/`args`) is history and stays.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// The call this outcome answers.
    pub call: ToolCall,
    /// Result blocks as recorded (`ResultBlock`, not `Content`: a tool
    /// result cannot contain a tool call).
    pub content: Vec<ResultBlock>,
    /// Whether the harness recorded this as an error result.
    pub is_error: bool,
}

/// `before_run_end`: the messages a finished run produced.
#[derive(Debug, Clone)]
pub struct RunEnd {
    /// Everything the run appended (prompt first).
    pub messages: Vec<Message>,
    /// Why the run stopped.
    pub stop: StopReason,
}

/// `before_compaction`: the message set about to be summarized.
#[derive(Debug, Clone)]
pub struct Compaction {
    /// Why compaction is happening; `manual` today (the mirror of the
    /// session entry's own reason field).
    pub reason: String,
    /// The messages the summary will replace.
    pub messages: Vec<Message>,
}

/// `before_navigation`: the entry a fork would move to.
#[derive(Debug, Clone)]
pub struct Navigation {
    /// Resolved session entry id.
    pub target: String,
    /// One-line summary of that entry.
    pub summary: String,
}

/// `session_start` / `session_end`: the facts the harness knows about the
/// session. Observe-only in both directions — see
/// [`ProbePoint::observe_only`].
#[derive(Debug, Clone)]
pub struct SessionFacts {
    /// Session id.
    pub session: String,
    /// Working directory the session runs in.
    pub cwd: String,
    /// Model label, as configured.
    pub model: String,
}

/// `branch`: the fork that just happened. Observe-only.
#[derive(Debug, Clone)]
pub struct Branch {
    /// Where the session was before the fork, when it had a position.
    pub previous: Option<String>,
    /// Where it is now.
    pub to: String,
}

/// One probe firing's payload: the point and its data are the same fact.
#[derive(Debug, Clone)]
pub enum ProbePayload {
    /// [`ProbePoint::BeforeRun`].
    BeforeRun(BeforeRun),
    /// [`ProbePoint::TransformContext`].
    TransformContext(AssembledContext),
    /// [`ProbePoint::BeforeRequest`].
    BeforeRequest(FinalRequest),
    /// [`ProbePoint::AfterResponse`].
    AfterResponse(AssembledResponse),
    /// [`ProbePoint::BeforeTool`]: the call about to run. A replacement
    /// is the new arguments.
    BeforeTool(ToolCall),
    /// [`ProbePoint::AfterTool`].
    AfterTool(ToolOutcome),
    /// [`ProbePoint::BeforeRunEnd`].
    BeforeRunEnd(RunEnd),
    /// [`ProbePoint::BeforeCompaction`].
    BeforeCompaction(Compaction),
    /// [`ProbePoint::BeforeNavigation`].
    BeforeNavigation(Navigation),
    /// [`ProbePoint::SessionStart`].
    SessionStart(SessionFacts),
    /// [`ProbePoint::Branch`].
    Branch(Branch),
    /// [`ProbePoint::SessionEnd`].
    SessionEnd(SessionFacts),
}

impl ProbePayload {
    /// The point this payload belongs to. Total by construction: a
    /// handler no longer has to be told which point it is answering
    /// twice.
    pub fn point(&self) -> ProbePoint {
        match self {
            Self::BeforeRun(_) => ProbePoint::BeforeRun,
            Self::TransformContext(_) => ProbePoint::TransformContext,
            Self::BeforeRequest(_) => ProbePoint::BeforeRequest,
            Self::AfterResponse(_) => ProbePoint::AfterResponse,
            Self::BeforeTool(_) => ProbePoint::BeforeTool,
            Self::AfterTool(_) => ProbePoint::AfterTool,
            Self::BeforeRunEnd(_) => ProbePoint::BeforeRunEnd,
            Self::BeforeCompaction(_) => ProbePoint::BeforeCompaction,
            Self::BeforeNavigation(_) => ProbePoint::BeforeNavigation,
            Self::SessionStart(_) => ProbePoint::SessionStart,
            Self::Branch(_) => ProbePoint::Branch,
            Self::SessionEnd(_) => ProbePoint::SessionEnd,
        }
    }

    /// The 0.6.0 JSON shape of this firing: what the wire hands an
    /// extension as `payload-json`. Compatibility shim — stage 2 replaces
    /// it with the typed payload and deletes this function.
    pub fn to_json(&self) -> Json {
        match self {
            Self::BeforeRun(p) => json!({ "prompt": p.prompt }),
            Self::TransformContext(p) => {
                json!({ "messages": p.messages, "system": p.system })
            }
            Self::BeforeRequest(p) => {
                json!({ "system": p.system, "messages": p.messages, "tools": p.tools })
            }
            Self::AfterResponse(p) => json!({ "message": p.message, "stop": p.stop }),
            Self::BeforeTool(call) => {
                json!({ "id": call.id, "name": call.name, "args": call.arguments })
            }
            Self::AfterTool(p) => json!({
                "id": p.call.id,
                "name": p.call.name,
                "args": p.call.arguments,
                "content": rendered(&p.content),
                "isError": p.is_error,
            }),
            Self::BeforeRunEnd(p) => json!({ "messages": p.messages, "stop": p.stop }),
            Self::BeforeCompaction(p) => {
                json!({ "reason": p.reason, "messages": p.messages })
            }
            Self::BeforeNavigation(p) => json!({ "target": p.target, "summary": p.summary }),
            Self::SessionStart(p) | Self::SessionEnd(p) => {
                json!({ "session": p.session, "cwd": p.cwd, "model": p.model })
            }
            Self::Branch(p) => json!({ "from": p.previous, "to": p.to }),
        }
    }

    /// Fold a 0.6.0 replacement object into this payload, field by field.
    /// Compatibility shim with the point's own 0.6.0 semantics preserved:
    ///
    /// - a field the replacement does not name keeps its current value;
    /// - a field it names but that does not fit is `invalid`, not a
    ///   silent no-op;
    /// - `before_tool` is the exception: the replacement *is* the new
    ///   argument object, not an object of named fields;
    /// - `before_request` cannot replace `tools` (registry-owned), and
    ///   saying so is ignored the way 0.6.0 ignored it;
    /// - `after_tool`'s `content` is a string on the wire; a replacement
    ///   becomes a single text block.
    pub fn merge_json(self, value: Json) -> Result<Self, HostError> {
        let point = self.point();
        if let Self::BeforeTool(call) = self {
            return Ok(Self::BeforeTool(ToolCall {
                arguments: value,
                ..call
            }));
        }
        if !value.is_object() {
            return Err(HostError::invalid(format!(
                "{}: a replacement must be a JSON object of named fields",
                point.name()
            )));
        }
        Ok(match self {
            Self::BeforeTool(_) => unreachable!("handled above"),
            Self::BeforeRun(mut p) => {
                if let Some(prompt) = present(point, &value, "prompt")? {
                    p.prompt = prompt;
                }
                Self::BeforeRun(p)
            }
            Self::TransformContext(mut p) => {
                if let Some(messages) = present(point, &value, "messages")? {
                    p.messages = messages;
                }
                if let Some(system) = present(point, &value, "system")? {
                    p.system = system;
                }
                Self::TransformContext(p)
            }
            Self::BeforeRequest(mut p) => {
                if let Some(system) = present(point, &value, "system")? {
                    p.system = system;
                }
                if let Some(messages) = present(point, &value, "messages")? {
                    p.messages = messages;
                }
                Self::BeforeRequest(p)
            }
            Self::AfterResponse(mut p) => {
                if let Some(message) = present(point, &value, "message")? {
                    p.message = message;
                }
                if let Some(stop) = present(point, &value, "stop")? {
                    p.stop = stop;
                }
                Self::AfterResponse(p)
            }
            Self::AfterTool(mut p) => {
                if let Some(Some(text)) = present::<Option<String>>(point, &value, "content")? {
                    p.content = vec![ResultBlock::Text { text }];
                }
                if let Some(is_error) = present(point, &value, "isError")? {
                    p.is_error = is_error;
                }
                Self::AfterTool(p)
            }
            Self::BeforeRunEnd(mut p) => {
                if let Some(messages) = present(point, &value, "messages")? {
                    p.messages = messages;
                }
                if let Some(stop) = present(point, &value, "stop")? {
                    p.stop = stop;
                }
                Self::BeforeRunEnd(p)
            }
            Self::BeforeCompaction(mut p) => {
                if let Some(reason) = present(point, &value, "reason")? {
                    p.reason = reason;
                }
                if let Some(messages) = present(point, &value, "messages")? {
                    p.messages = messages;
                }
                Self::BeforeCompaction(p)
            }
            Self::BeforeNavigation(mut p) => {
                if let Some(target) = present(point, &value, "target")? {
                    p.target = target;
                }
                if let Some(summary) = present(point, &value, "summary")? {
                    p.summary = summary;
                }
                Self::BeforeNavigation(p)
            }
            Self::SessionStart(p) => Self::SessionStart(merge_facts(point, p, &value)?),
            Self::SessionEnd(p) => Self::SessionEnd(merge_facts(point, p, &value)?),
            Self::Branch(mut p) => {
                if let Some(previous) = present(point, &value, "from")? {
                    p.previous = previous;
                }
                if let Some(to) = present(point, &value, "to")? {
                    p.to = to;
                }
                Self::Branch(p)
            }
        })
    }
}

/// The 0.6.0 text projection of result blocks, reused so the compatibility
/// edge renders exactly what `ToolOutput::text()` rendered.
fn rendered(blocks: &[ResultBlock]) -> String {
    let content: Vec<Content> = blocks.iter().cloned().map(Content::from).collect();
    tool_result_text(&content)
}

/// One field of a replacement object: `None` when the key is absent (keep
/// what we have), `Some` when present. A JSON `null` is a value here only
/// where the field's type can take one (`Option<String>` clears it).
fn present<T: DeserializeOwned>(
    point: ProbePoint,
    value: &Json,
    key: &str,
) -> Result<Option<T>, HostError> {
    match value.get(key) {
        None => Ok(None),
        Some(raw) => serde_json::from_value(raw.clone())
            .map(Some)
            .map_err(|error| {
                HostError::invalid(format!(
                    "{}: field `{key}` does not fit the point's payload: {error}",
                    point.name()
                ))
            }),
    }
}

fn merge_facts(
    point: ProbePoint,
    mut facts: SessionFacts,
    value: &Json,
) -> Result<SessionFacts, HostError> {
    if let Some(session) = present(point, value, "session")? {
        facts.session = session;
    }
    if let Some(cwd) = present(point, value, "cwd")? {
        facts.cwd = cwd;
    }
    if let Some(model) = present(point, value, "model")? {
        facts.model = model;
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "upper".into(),
            arguments: json!({ "text": "hi" }),
        }
    }

    fn message() -> Message {
        Message::user("hello")
    }

    fn facts() -> SessionFacts {
        SessionFacts {
            session: "s1".into(),
            cwd: "/tmp".into(),
            model: "demo".into(),
        }
    }

    /// One payload per point, so the round-trip test is total.
    fn one_of_each() -> Vec<ProbePayload> {
        vec![
            ProbePayload::BeforeRun(BeforeRun { prompt: message() }),
            ProbePayload::TransformContext(AssembledContext {
                system: Some("be brief".into()),
                messages: vec![message()],
            }),
            ProbePayload::BeforeRequest(FinalRequest {
                system: None,
                messages: vec![message()],
                tools: Vec::new(),
            }),
            ProbePayload::AfterResponse(AssembledResponse {
                message: message(),
                stop: StopReason::ToolUse,
            }),
            ProbePayload::BeforeTool(call()),
            ProbePayload::AfterTool(ToolOutcome {
                call: call(),
                content: vec![ResultBlock::Text { text: "HI".into() }],
                is_error: false,
            }),
            ProbePayload::BeforeRunEnd(RunEnd {
                messages: vec![message()],
                stop: StopReason::Stop,
            }),
            ProbePayload::BeforeCompaction(Compaction {
                reason: "manual".into(),
                messages: vec![message()],
            }),
            ProbePayload::BeforeNavigation(Navigation {
                target: "e1".into(),
                summary: "one message".into(),
            }),
            ProbePayload::SessionStart(facts()),
            ProbePayload::Branch(Branch {
                previous: Some("e1".into()),
                to: "e2".into(),
            }),
            ProbePayload::SessionEnd(facts()),
        ]
    }

    #[test]
    fn every_arm_knows_its_point_and_the_set_is_complete() {
        let payloads = one_of_each();
        let mut points: Vec<ProbePoint> = payloads.iter().map(ProbePayload::point).collect();
        let mut all: Vec<ProbePoint> = ProbePoint::ALL.to_vec();
        points.sort_by_key(|point| point.name());
        all.sort_by_key(|point| point.name());
        assert_eq!(points, all, "one arm per point, and no point twice");
    }

    #[test]
    fn the_json_edge_round_trips_every_point() {
        for payload in one_of_each() {
            let json = payload.to_json();
            // `before_tool`'s replacement is the arguments value, not the
            // `{id, name, args}` envelope the payload carries (0.6.0's own
            // asymmetry: a handler replaces args, never the call's name).
            let replacement = match payload {
                ProbePayload::BeforeTool(_) => json["args"].clone(),
                _ => json.clone(),
            };
            let back = payload
                .clone()
                .merge_json(replacement)
                .expect("a payload's own json must merge back");
            assert_eq!(
                back.to_json(),
                json,
                "round trip changed {:?}",
                payload.point()
            );
        }
    }

    #[test]
    fn absent_fields_keep_their_value_and_null_clears_an_optional_one() {
        let start = ProbePayload::TransformContext(AssembledContext {
            system: Some("be brief".into()),
            messages: vec![message()],
        });
        let untouched = start.clone().merge_json(json!({})).unwrap();
        assert_eq!(untouched.to_json(), start.to_json());

        let cleared = start.merge_json(json!({ "system": null })).unwrap();
        assert_eq!(cleared.to_json()["system"], Json::Null);
    }

    #[test]
    fn a_field_that_does_not_fit_is_invalid_not_a_no_op() {
        let payload = ProbePayload::TransformContext(AssembledContext {
            system: None,
            messages: vec![message()],
        });
        let error = payload.merge_json(json!({ "messages": 5 })).unwrap_err();
        assert_eq!(error.kind(), "invalid");
        assert!(error.detail().contains("transform_context"), "{error}");
        assert!(error.detail().contains("messages"), "{error}");

        let not_an_object = ProbePayload::BeforeRun(BeforeRun { prompt: message() });
        assert!(not_an_object.merge_json(json!("junk")).is_err());
    }

    #[test]
    fn before_tool_takes_the_whole_replacement_as_arguments() {
        let payload = ProbePayload::BeforeTool(call());
        let replaced = payload
            .merge_json(json!([1, 2, 3]))
            .expect("arguments need not be an object");
        assert_eq!(replaced.to_json()["args"], json!([1, 2, 3]));
        assert_eq!(replaced.to_json()["name"], json!("upper"));
    }

    #[test]
    fn after_tool_upgrades_a_legacy_content_string_to_one_text_block() {
        let payload = ProbePayload::AfterTool(ToolOutcome {
            call: call(),
            content: vec![ResultBlock::Text { text: "HI".into() }],
            is_error: false,
        });
        let replaced = payload
            .merge_json(json!({ "content": "rewritten", "isError": true }))
            .unwrap();
        let ProbePayload::AfterTool(outcome) = replaced else {
            panic!("arm changed");
        };
        assert_eq!(
            outcome.content,
            vec![ResultBlock::Text {
                text: "rewritten".into()
            }]
        );
        assert!(outcome.is_error);

        // A replacement that only flips `isError` keeps the recorded blocks.
        let kept = ProbePayload::AfterTool(ToolOutcome {
            call: call(),
            content: vec![ResultBlock::Text { text: "HI".into() }],
            is_error: false,
        })
        .merge_json(json!({ "isError": true }))
        .unwrap();
        let ProbePayload::AfterTool(outcome) = kept else {
            panic!("arm changed");
        };
        assert_eq!(
            outcome.content,
            vec![ResultBlock::Text { text: "HI".into() }]
        );
        assert!(outcome.is_error);
    }
}
