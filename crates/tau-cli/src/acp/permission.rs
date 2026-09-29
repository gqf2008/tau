//! The permission gate: the built-in tools that change the machine ask the
//! client before they run.
//!
//! This exists only in ACP mode. Outside it, tau's posture is unchanged —
//! the built-ins are host code and the consent tau asks for is at load
//! time (a component's signature, a bridge's argv, `--deny-wasi`). What
//! changes here is that a client — an editor with a user in front of it —
//! has a protocol for exactly this question, so tau asks it.
//!
//! The gate is a probe, and it is registered *after* everything else. That
//! order is the point: an extension's `replace` has already rewritten the
//! arguments by the time the user is asked, so what they approve is what
//! will run, and an extension's `block` has already stopped the call, so
//! the user is not asked about something that will not happen. tau's fold
//! returns on the first block, which is why registering last is enough.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SessionId, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use serde_json::Value as Json;
use tau_core::probe::{ProbeHandler, ProbePoint, Verdict};
use tau_core::probe_payload::ProbePayload;

use super::map;

/// The built-ins that write to the machine or run something on it — the
/// four a user should be asked about. The names are tau-tools'; the test
/// below fails if one stops existing there.
///
/// The gate is by *name*, so a component tool that took one of these names
/// would be gated too. That is the conservative direction: erring towards
/// asking is the safe way for this to be wrong.
pub const GATED_TOOLS: [&str; 4] = ["bash", "edit", "powershell", "write"];

/// Whether a tool is one the gate asks about.
pub fn gated(name: &str) -> bool {
    GATED_TOOLS.contains(&name)
}

/// The option ids, which are also the whole vocabulary of answers the gate
/// understands. Kept as constants so the options offered and the decisions
/// read cannot drift apart — the test at the bottom fires if they do.
const ALLOW_ONCE: &str = "allow_once";
const ALLOW_ALWAYS: &str = "allow_always";
const REJECT_ONCE: &str = "reject_once";
const REJECT_ALWAYS: &str = "reject_always";

/// What the user decided about one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Allow,
    Reject,
}

/// The four answers a client may give, with the label it shows for each.
fn options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(ALLOW_ONCE, "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            ALLOW_ALWAYS,
            "Allow for this session",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(REJECT_ONCE, "Reject once", PermissionOptionKind::RejectOnce),
        PermissionOption::new(
            REJECT_ALWAYS,
            "Reject for this session",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// What an answered option stands for: the decision, and whether it is for
/// the rest of the session or just this call. An option id the gate did not
/// offer — a client sending something else — is `None`, and the caller
/// turns that into a refusal: answering a permission question with an
/// answer nobody asked for is not consent.
fn decide(option_id: &str) -> Option<(Decision, bool)> {
    Some(match option_id {
        ALLOW_ONCE => (Decision::Allow, false),
        ALLOW_ALWAYS => (Decision::Allow, true),
        REJECT_ONCE => (Decision::Reject, false),
        REJECT_ALWAYS => (Decision::Reject, true),
        _ => return None,
    })
}

/// How the gate reaches the user.
///
/// A seam, not an abstraction for its own sake: on the wire this is
/// `ConnectionTo<Client>`, and the decisions worth testing — what a refusal
/// does, what is remembered, what happens when nobody answers — are tested
/// against scripted outcomes instead of a live connection.
#[async_trait::async_trait]
pub trait Asker: Send + Sync {
    async fn ask(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionOutcome, String>;
}

#[async_trait::async_trait]
impl Asker for ConnectionTo<Client> {
    async fn ask(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionOutcome, String> {
        // `block_task` is the consumption mode meant for a task running
        // outside the dispatch loop, which is where a turn runs. Dropping
        // the sent request instead would tell the client to cancel it.
        self.send_request(request)
            .block_task()
            .await
            .map(|response| response.outcome)
            .map_err(|error| error.to_string())
    }
}

/// The probe that asks. One per session: what the user allows is remembered
/// for the session they allowed it in, and a new session asks again — the
/// choice was made watching one conversation's work.
pub struct PermissionGate {
    session: SessionId,
    /// The session's turn counter, shared with it. The id a client sees for
    /// a tool call is namespaced by turn, and a permission request has to
    /// name the same call the client was already shown.
    turns: Arc<AtomicU64>,
    asker: Box<dyn Asker>,
    /// `allow_always`/`reject_always`, by tool name, for this session.
    remembered: Mutex<HashMap<String, Decision>>,
}

impl PermissionGate {
    pub fn new(session: SessionId, turns: Arc<AtomicU64>, asker: Box<dyn Asker>) -> Self {
        Self {
            session,
            turns,
            asker,
            remembered: Mutex::new(HashMap::new()),
        }
    }

    /// The request one call turns into — separate from the asking so the
    /// tests can read what the user would be shown.
    fn request(&self, name: &str, id: &str, args: &Json) -> RequestPermissionRequest {
        let turn = self.turns.load(Ordering::SeqCst);
        RequestPermissionRequest::new(
            self.session.clone(),
            ToolCallUpdate::new(
                ToolCallId::new(format!("{turn}:{id}")),
                // The arguments are the whole point of asking: a prompt
                // that does not say what is about to be written or run is
                // a prompt the user can only guess at.
                ToolCallUpdateFields::new()
                    .title(name.to_string())
                    .kind(map::tool_kind(name))
                    .raw_input(args.clone()),
            ),
            options(),
        )
    }

    fn recall(&self, name: &str) -> Option<Decision> {
        self.remembered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .copied()
    }

    fn remember(&self, name: &str, decision: Decision) {
        self.remembered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.to_string(), decision);
    }
}

#[async_trait::async_trait]
impl ProbeHandler for PermissionGate {
    fn points(&self) -> &[ProbePoint] {
        &[ProbePoint::BeforeTool]
    }

    async fn probe(&self, _point: ProbePoint, payload: ProbePayload) -> Verdict {
        let ProbePayload::BeforeTool(call) = payload else {
            // Not a firing this gate can reason about. Nothing else routes
            // one here, and a gate that blocked on a point it did not
            // expect would be a gate that stops work for no reason.
            return Verdict::Continue;
        };
        let name = call.name.as_str();
        if !gated(name) {
            return Verdict::Continue;
        }
        if let Some(decision) = self.recall(name) {
            return verdict(name, decision, "for the rest of this session");
        }

        let id = call.id.as_str();
        match self.asker.ask(self.request(name, id, &call.arguments)).await {
            Ok(RequestPermissionOutcome::Selected(selected)) => {
                match decide(selected.option_id.0.as_ref()) {
                    Some((decision, remember)) => {
                        if remember {
                            self.remember(name, decision);
                        }
                        verdict(name, decision, "once")
                    }
                    None => Verdict::Block {
                        reason: format!(
                            "the client answered the permission request for {name} with an option that was never offered"
                        ),
                    },
                }
            }
            // The spec has a client answer every pending permission
            // request with this when it cancels the turn, which is how a
            // cancel reaches a turn that is waiting here: an abort cannot.
            Ok(RequestPermissionOutcome::Cancelled) => Verdict::Block {
                reason: format!("the client cancelled the turn while {name} waited for permission"),
            },
            Ok(other) => Verdict::Block {
                reason: format!("the client answered the permission request for {name} with {other:?}"),
            },
            // Fail closed. An unanswered request is not consent, and this
            // is the one place where being wrong the other way would run
            // something the user never saw.
            Err(error) => Verdict::Block {
                reason: format!("the permission request for {name} did not reach the client: {error}"),
            },
        }
    }
}

/// A decision, as the loop reads it: the reason is what the model is told
/// when a call is refused.
fn verdict(name: &str, decision: Decision, scope: &str) -> Verdict {
    match decision {
        Decision::Allow => Verdict::Continue,
        Decision::Reject => Verdict::Block {
            reason: format!("the user rejected {name} {scope}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use agent_client_protocol::schema::v1::SelectedPermissionOutcome;

    use super::*;
    use tau_core::types::ToolCall;

    /// An asker that answers from a script, and remembers what it was
    /// asked, so a test can read both the decisions and the question.
    struct Scripted {
        answers: Mutex<VecDeque<Result<RequestPermissionOutcome, String>>>,
        asked: Arc<Mutex<Vec<RequestPermissionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Asker for Scripted {
        async fn ask(
            &self,
            request: RequestPermissionRequest,
        ) -> Result<RequestPermissionOutcome, String> {
            self.asked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            self.answers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front()
                .expect("the script has an answer for every question asked of it")
        }
    }

    type Asked = Arc<Mutex<Vec<RequestPermissionRequest>>>;

    /// A gate on turn 3 of session `s1`, answering `answers` in order.
    fn gate_with(answers: Vec<Result<RequestPermissionOutcome, String>>) -> (PermissionGate, Asked) {
        let asked: Asked = Arc::new(Mutex::new(Vec::new()));
        let asker = Scripted {
            answers: Mutex::new(answers.into()),
            asked: Arc::clone(&asked),
        };
        (
            PermissionGate::new(
                SessionId::new("s1"),
                Arc::new(AtomicU64::new(3)),
                Box::new(asker),
            ),
            asked,
        )
    }

    fn selected(option: &'static str) -> Result<RequestPermissionOutcome, String> {
        Ok(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(option),
        ))
    }

    fn call(name: &str) -> ProbePayload {
        ProbePayload::BeforeTool(ToolCall {
            id: "demo-call-1".into(),
            name: name.into(),
            arguments: serde_json::json!({"command": "rm -rf everything"}),
        })
    }

    async fn fire(gate: &PermissionGate, name: &str) -> Verdict {
        gate.probe(ProbePoint::BeforeTool, call(name)).await
    }

    fn questions(asked: &Asked) -> Vec<RequestPermissionRequest> {
        asked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    #[tokio::test]
    async fn a_call_the_gate_does_not_cover_is_never_asked_about() {
        let (gate, asked) = gate_with(vec![]);
        for name in ["read", "ls", "grep", "find", "upper"] {
            assert!(matches!(fire(&gate, name).await, Verdict::Continue), "{name}");
        }
        assert!(questions(&asked).is_empty(), "the script answered nothing");
    }

    #[tokio::test]
    async fn the_question_names_the_call_the_client_already_saw() {
        let (gate, asked) = gate_with(vec![selected(ALLOW_ONCE)]);
        assert!(matches!(fire(&gate, "bash").await, Verdict::Continue));

        let questions = questions(&asked);
        assert_eq!(questions.len(), 1);
        let question = &questions[0];
        assert_eq!(question.session_id.0.as_ref(), "s1");
        // The turn is part of the id, exactly as the announcement was: a
        // client that cannot match the two cannot attach the prompt to the
        // call it is about.
        assert_eq!(question.tool_call.tool_call_id.0.as_ref(), "3:demo-call-1");
        assert_eq!(question.tool_call.fields.title.as_deref(), Some("bash"));
        assert_eq!(question.tool_call.fields.kind, Some(map::tool_kind("bash")));
        assert_eq!(
            question.tool_call.fields.raw_input,
            Some(serde_json::json!({"command": "rm -rf everything"})),
            "what is about to run is the whole point of asking"
        );
        let offered: Vec<&str> = question
            .options
            .iter()
            .map(|option| option.option_id.0.as_ref())
            .collect();
        assert_eq!(
            offered,
            vec![ALLOW_ONCE, ALLOW_ALWAYS, REJECT_ONCE, REJECT_ALWAYS]
        );
    }

    #[tokio::test]
    async fn once_is_asked_again_and_always_is_not() {
        let (gate, asked) = gate_with(vec![selected(ALLOW_ONCE), selected(ALLOW_ONCE)]);
        assert!(matches!(fire(&gate, "write").await, Verdict::Continue));
        assert!(matches!(fire(&gate, "write").await, Verdict::Continue));
        assert_eq!(questions(&asked).len(), 2, "once means once");

        let (gate, asked) = gate_with(vec![selected(ALLOW_ALWAYS)]);
        assert!(matches!(fire(&gate, "write").await, Verdict::Continue));
        assert!(matches!(fire(&gate, "write").await, Verdict::Continue));
        assert_eq!(questions(&asked).len(), 1, "always means the session");
    }

    #[tokio::test]
    async fn a_rejection_blocks_the_call_and_says_why() {
        let (gate, _) = gate_with(vec![selected(REJECT_ONCE)]);
        match fire(&gate, "bash").await {
            Verdict::Block { reason } => {
                assert!(reason.contains("bash"), "{reason}");
                assert!(reason.contains("rejected"), "{reason}");
            }
            other => panic!("a rejection must block: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rejection_for_the_session_is_not_asked_about_again() {
        let (gate, asked) = gate_with(vec![selected(REJECT_ALWAYS)]);
        assert!(matches!(fire(&gate, "edit").await, Verdict::Block { .. }));
        assert!(
            matches!(fire(&gate, "edit").await, Verdict::Block { .. }),
            "the second call is refused from memory"
        );
        assert_eq!(questions(&asked).len(), 1);
    }

    #[tokio::test]
    async fn what_is_remembered_is_remembered_per_tool() {
        let (gate, asked) = gate_with(vec![selected(ALLOW_ALWAYS), selected(REJECT_ONCE)]);
        assert!(matches!(fire(&gate, "bash").await, Verdict::Continue));
        // A different tool is a different question: allowing a command is
        // not allowing a file to be overwritten. It is asked — the second
        // scripted answer is what decides it — and refused on its own.
        assert!(matches!(fire(&gate, "write").await, Verdict::Block { .. }));
        assert_eq!(questions(&asked).len(), 2, "the second tool was asked about");
    }

    #[tokio::test]
    async fn an_option_that_was_never_offered_is_not_consent() {
        let (gate, _) = gate_with(vec![selected("yolo")]);
        match fire(&gate, "bash").await {
            Verdict::Block { reason } => assert!(reason.contains("never offered"), "{reason}"),
            other => panic!("an unknown answer must not run the call: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_cancelled_question_is_a_refusal() {
        let (gate, _) = gate_with(vec![Ok(RequestPermissionOutcome::Cancelled)]);
        match fire(&gate, "bash").await {
            Verdict::Block { reason } => assert!(reason.contains("cancelled"), "{reason}"),
            other => panic!("a cancelled turn must not run the call: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_question_that_never_arrives_is_a_refusal() {
        let (gate, _) = gate_with(vec![Err("the connection is closed".to_string())]);
        match fire(&gate, "bash").await {
            Verdict::Block { reason } => assert!(reason.contains("did not reach"), "{reason}"),
            other => panic!("fail closed: {other:?}"),
        }
    }

    #[test]
    fn the_gate_covers_the_built_ins_that_change_the_machine() {
        // The one coupling point to tau-tools: a tool renamed there would
        // quietly drop out of the gate, so the names are checked against
        // tau-tools' own list. `powershell` is Windows-only — it is not
        // registered elsewhere — so on other platforms it is checked
        // against the implementation list rather than this platform's.
        let known = tau_tools::names();
        for name in GATED_TOOLS {
            assert!(
                known.contains(&name) || (name == "powershell" && !cfg!(windows)),
                "{name} is gated but is not a built-in: {known:?}"
            );
        }
        for name in ["read", "ls", "grep", "find", "upper"] {
            assert!(!gated(name), "{name} would be asked about");
        }
    }

    #[test]
    fn every_offered_option_has_a_decision() {
        for option in options() {
            let id = option.option_id.0.as_ref();
            let (decision, remember) = decide(id).unwrap_or_else(|| panic!("{id} has no decision"));
            let expected = match option.kind {
                PermissionOptionKind::AllowOnce => (Decision::Allow, false),
                PermissionOptionKind::AllowAlways => (Decision::Allow, true),
                PermissionOptionKind::RejectOnce => (Decision::Reject, false),
                PermissionOptionKind::RejectAlways => (Decision::Reject, true),
                // A kind a later protocol version adds is not an option
                // this gate offered, so the test fails rather than guesses.
                other => panic!("the gate offers an option kind it cannot read: {other:?}"),
            };
            assert_eq!(
                (decision, remember),
                expected,
                "{id} is offered as one thing and read as another"
            );
        }
        assert_eq!(decide("allow_sometimes"), None);
    }
}
