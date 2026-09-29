//! One turn: the prompt goes in, `session/update` notifications come out,
//! and the answer is the response to `session/prompt`.
//!
//! A turn runs on its own task — a handler may not await inside the
//! dispatch loop (see the module header of [`super`]) — and it answers for
//! itself on every path: a spawned task that returns `Err` takes the whole
//! connection down, so a failure becomes a response or a stderr line here,
//! never a `?`.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    Error, PromptResponse, SessionId, SessionNotification, SessionUpdate,
};
use agent_client_protocol::{Client, ConnectionTo, Responder};
use tau_core::agent::AgentEvent;
use tau_core::bus::EventStream;
use tau_core::session::{EntryKind, JsonlStore, SessionEntry, new_id};
use tau_core::{Content, Message, StopReason};
use tokio::sync::broadcast::error::RecvError;

use super::map;
use super::session::Session;

/// A session's one running turn, and the claim that keeps it the only one.
///
/// Held for the whole turn and released on every way out — including a
/// panic, which would otherwise leave the session refusing every later
/// prompt as concurrent with a turn that no longer exists, and would leave
/// `session/cancel` sending aborts nothing is waiting for.
pub struct InFlight(Arc<Session>);

impl InFlight {
    /// Take the session's turn slot, or `None` if a turn already holds it.
    pub fn claim(session: Arc<Session>) -> Option<Self> {
        session
            .in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(Self(session))
    }

    fn session(&self) -> &Session {
        &self.0
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.store(false, Ordering::SeqCst);
    }
}

/// Run one prompt to completion and answer it.
pub async fn run(
    turn: InFlight,
    prompt: Message,
    responder: Responder<PromptResponse>,
    connection: ConnectionTo<Client>,
) {
    let session = turn.session();
    // Claimed before anything can fail, so two turns of one session can
    // never share a number.
    let number = session.turns.fetch_add(1, Ordering::SeqCst) + 1;

    if let Some(delay) = stall() {
        tokio::time::sleep(delay).await;
    }

    // The tree is the session: the history comes from the file, and
    // reopening it per turn means a turn always runs on what is on disk,
    // including a turn another client wrote and this process never saw.
    let mut store = match JsonlStore::open(&session.path) {
        Ok(store) => store.with_blobs(tau_core::BlobStore::new(tau_core::BlobStore::default_dir())),
        Err(error) => {
            return answer(
                responder,
                Error::internal_error(),
                format!(
                    "the session file {} is unreadable: {error}",
                    session.path.display()
                ),
            );
        }
    };
    crate::warn_torn_tail(&store);
    let parent = store.head().map(|head| head.id.clone());
    let history = match &parent {
        Some(head) => match store.active_branch(head) {
            Ok(branch) => branch,
            Err(error) => {
                return answer(
                    responder,
                    Error::internal_error(),
                    format!("the session tree is unreadable: {error}"),
                );
            }
        },
        None => Vec::new(),
    };

    // The bus says what happened, the run's value says what to persist:
    // the stop reason rides one and the messages ride the other, so both
    // are driven to completion together rather than one around the other.
    // Neither future may be dropped early — the renderer is what answers
    // the client, and the run is what the session file records.
    let mut events = session.agent.events();
    let (produced, stop) = tokio::join!(
        session.agent.run(&history, prompt),
        render(&mut events, session, &connection, number),
    );

    let messages = match produced {
        Ok(messages) => messages,
        Err(error) => {
            return answer(
                responder,
                Error::internal_error(),
                format!("the run failed: {error}"),
            );
        }
    };

    // Persisted before the client is answered: a client's next prompt
    // builds its history from this file, so this turn has to be in it by
    // the time that client is told the turn is over.
    let mut parent = parent;
    for message in messages {
        let entry = SessionEntry {
            id: new_id(),
            parent,
            kind: EntryKind::Message { message },
        };
        parent = Some(entry.id.clone());
        if let Err(error) = store.append(entry) {
            return answer(
                responder,
                Error::internal_error(),
                format!("the session file is not writable: {error}"),
            );
        }
    }

    let stop = match stop {
        Some(stop) => stop,
        // `Agent::run` emits RunEnd before it returns Ok and emits RunError
        // instead of returning Ok, so a missing stop means the bus closed
        // under the renderer. Answering is better than leaving the client
        // waiting for a prompt that will never be answered.
        None => {
            eprintln!("[tau] acp: the run ended without a stop event; answering end_turn");
            StopReason::Stop
        }
    };
    let _ = responder.respond(PromptResponse::new(map::stop_reason(stop)));
}

/// Push what the loop reports as `session/update`, and return the stop it
/// ended on. `None` means no stop was seen: either the run failed — which
/// its own value reports — or the bus closed.
async fn render(
    events: &mut EventStream,
    session: &Session,
    connection: &ConnectionTo<Client>,
    number: u64,
) -> Option<StopReason> {
    // The calls the client has been told about, by wire id. ACP wants a
    // call announced before its update: a client that never saw the call
    // has nothing to draw the update on.
    let mut rendered = Rendered::default();
    loop {
        match events.recv().await {
            Ok(AgentEvent::RunEnd { stop }) => return Some(stop),
            Ok(AgentEvent::RunError { message }) => {
                eprintln!("[tau] acp: the run failed: {message}");
                return None;
            }
            Ok(event) => {
                for update in updates(&event, number, &mut rendered) {
                    let notification = SessionNotification::new(session.id.clone(), update);
                    if let Err(error) = connection.send_notification(notification) {
                        eprintln!("[tau] acp: a session/update did not reach the client: {error}");
                    }
                }
            }
            // The client is slower than the bus is busy. It loses updates,
            // not the turn: the answer and the session file are unaffected.
            Err(RecvError::Lagged(missed)) => {
                eprintln!("[tau] acp: the renderer fell behind by {missed} event(s)");
            }
            Err(RecvError::Closed) => return None,
        }
    }
}

/// What one turn's renderer remembers between events: the calls the
/// client has been told about (ACP wants a call announced before its
/// update, and the loop reports both ends of every call), and whether the
/// one-line notice about realtime audio has been said.
#[derive(Default)]
struct Rendered {
    announced: HashSet<String>,
    audio_noted: bool,
}

impl Rendered {
    /// Whether this is the first realtime event of the turn — the only one
    /// worth a line. The first chunk is news; the next thousand are not.
    fn first_audio(&mut self) -> bool {
        let first = !self.audio_noted;
        self.audio_noted = true;
        first
    }
}

/// What one event becomes, if anything.
fn updates(event: &AgentEvent, number: u64, rendered: &mut Rendered) -> Vec<SessionUpdate> {
    match event {
        AgentEvent::TextDelta(text) => vec![map::text_chunk(text)],
        AgentEvent::ToolCallStart { id, name } => {
            let id = wire_id(number, id);
            rendered.announced.insert(id.clone());
            vec![map::tool_call(&id, name)]
        }
        AgentEvent::ToolCallEnd {
            id,
            name,
            is_error,
            output,
        } => {
            let id = wire_id(number, id);
            let mut updates = Vec::new();
            // First sight: announce, so the update below has a call to
            // attach to. The loop emits both events for every call it
            // runs, so this is the catch-up path, not the common one.
            if rendered.announced.insert(id.clone()) {
                updates.push(map::tool_call(&id, name));
            }
            updates.push(map::tool_result(&id, *is_error, output));
            updates
        }
        // Everything else is tau's own business and stable v1 has no
        // update for it (the notice and compaction updates are unstable
        // and need client capabilities tau does not ask for). It goes to
        // stderr, where the operator can see it.
        other => {
            note(other, rendered);
            Vec::new()
        }
    }
}

/// The id a client sees: the turn, then the loop's own id.
///
/// The loop's ids come from the provider, and a scripted model reuses them
/// — `demo-call-1` on every turn — which a client drawing two turns would
/// show as one call that never ends. The session file keeps the provider's
/// ids untouched: this is a wire detail, not a record.
fn wire_id(number: u64, id: &str) -> String {
    format!("{number}:{id}")
}

/// The events no client sees, said out loud on stderr. The phrasings are
/// print mode's, so an operator reading either mode reads the same words.
fn note(event: &AgentEvent, rendered: &mut Rendered) {
    match event {
        AgentEvent::Probe { point, action } => eprintln!("[tau] probe {point}: {action}"),
        AgentEvent::Steer(message) => eprintln!("[tau] steer: {}", message.text()),
        AgentEvent::FollowUp(message) => eprintln!("[tau] follow-up: {}", message.text()),
        AgentEvent::ExtensionFact(fact) => {
            eprintln!(
                "[tau] ext fact: {}",
                crate::repl::compact_preview(&fact.to_string())
            );
        }
        AgentEvent::ExtensionNotice { level, content } => {
            eprintln!(
                "[tau] ext {level}: {}",
                crate::repl::compact_preview(&text_of(content))
            );
        }
        // A realtime provider's audio, VAD, and barge-in have no stable v1
        // update, and this mode carries none of them (docs/acp.md). The
        // guard is what makes the line once per turn rather than once per
        // chunk — a line per chunk is noise, and a user watching an editor
        // should still be told why nothing plays. Everything past the first
        // chunk falls through to the arm below. The audio itself is kept:
        // the loop assembles contiguous chunks into the assistant message,
        // which the session file holds.
        AgentEvent::AudioDelta { .. }
        | AgentEvent::InputAudioChunk { .. }
        | AgentEvent::SpeechStarted
        | AgentEvent::SpeechStopped
        | AgentEvent::Interrupted
            if rendered.first_audio() =>
        {
            eprintln!(
                "[tau] acp: realtime audio is not carried in this mode; the session file has it"
            );
        }
        // The rest — run and turn boundaries, an honored abort — is not a
        // log: the boundaries are visible in the turn's answer.
        _ => {}
    }
}

/// The text of a notice's content blocks, for a one-line summary.
fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .map(|block| match block {
            Content::Text { text } => text.clone(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `TAU_ACP_STALL_MS`: how long a turn waits before starting its run.
///
/// A test knob and nothing else — the same species as `TAU_MCP_PAD`: it
/// gives a scripted client a window in which a turn is in flight but has
/// not begun, which is the only way to test `session/cancel` without
/// racing a model that answers instantly.
fn stall() -> Option<Duration> {
    parse_stall(std::env::var("TAU_ACP_STALL_MS").ok().as_deref())
}

/// The knob's rule, kept apart from the environment it reads so it can be
/// tested: anything that is not a number means no stall, not a panic. A
/// client that exported the variable for something else is not a reason to
/// refuse to run a turn.
fn parse_stall(value: Option<&str>) -> Option<Duration> {
    value?.parse().ok().map(Duration::from_millis)
}

/// Answer `session/prompt` for a session this process is not serving.
pub fn answer_unknown_session(responder: Responder<PromptResponse>, id: &SessionId) {
    answer(
        responder,
        Error::invalid_params(),
        format!("no session {id}: this process never created it, or it has been ended"),
    );
}

/// Answer `session/prompt` for a session already running a turn.
pub fn answer_busy(responder: Responder<PromptResponse>, id: &SessionId) {
    answer(
        responder,
        Error::invalid_params(),
        format!("session {id} is already running a turn; wait for it or cancel it"),
    );
}

/// Answer a request with an error, and say the same thing on stderr.
///
/// The message is the error's own, not the generic one the code carries:
/// "Invalid params" tells a user nothing, where "the session file is not
/// writable" tells them what to fix. Clients that log only `data` get it
/// there too.
fn answer(responder: Responder<PromptResponse>, error: Error, message: String) {
    eprintln!("[tau] acp: {message}");
    let mut error = error;
    error.data = Some(serde_json::json!(message));
    error.message = message;
    let _ = responder.respond_with_error(error);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loop's ids come from the provider and repeat across turns of a
    /// session; what a client sees must not.
    #[test]
    fn a_wire_id_is_unique_across_turns_of_a_session() {
        assert_eq!(wire_id(1, "demo-call-1"), "1:demo-call-1");
        assert_ne!(wire_id(1, "demo-call-1"), wire_id(2, "demo-call-1"));
        // The provider's own id is still readable in it — an operator
        // matching the wire against the session file can find it.
        assert!(wire_id(7, "call_abc").ends_with("call_abc"));
    }

    /// `TAU_ACP_STALL_MS` is read from the environment, where anything at
    /// all can appear: a number is a stall, and everything else — a typo, a
    /// value meant for another program, an empty string — is no stall
    /// rather than a failed turn.
    #[test]
    fn the_stall_knob_reads_a_number_or_nothing() {
        assert_eq!(parse_stall(Some("250")), Some(Duration::from_millis(250)));
        assert_eq!(parse_stall(Some("0")), Some(Duration::from_millis(0)));
        assert_eq!(parse_stall(Some("nope")), None);
        assert_eq!(parse_stall(Some("")), None);
        assert_eq!(
            parse_stall(Some("-5")),
            None,
            "a negative delay is not a delay"
        );
        assert_eq!(parse_stall(None), None);
    }

    /// The notice about uncarried audio is said once a turn, however many
    /// chunks arrive after it.
    #[test]
    fn the_audio_notice_is_said_once_a_turn() {
        let mut rendered = Rendered::default();
        assert!(rendered.first_audio(), "the first chunk is news");
        assert!(!rendered.first_audio(), "the second is not");
        assert!(!rendered.first_audio());
    }
}
