//! Example tau extension exercising the high-frequency stream
//! subscription (tau:extension@0.8.0, docs/stream-subscribe.md — F2's
//! high-frequency observation leg): at `session_start` it subscribes to
//! `text-delta`; at `before_run_end` it polls the backlog and notifies
//! what it saw. The guest pulls — the host never calls into the
//! component asynchronously, so the drain rides the guest's own probe
//! invocations.
//!
//! Build:
//!   cargo build --manifest-path examples/streamer/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use:
//!   tau --demo -e .../streamer.wasm -p "hello"
//!   # → [tau] ext info: stream observed: N text deltas, K chars

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "extension",
});

use std::cell::RefCell;

use exports::tau::extension::probes::{Guest as Probes, Payload, Point, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::host::{self, Level, StreamEvent, Subscription, Topic};
use tau::extension::types::{Content, Error as HostError};

thread_local! {
    /// The subscription opened at `session_start`. A resource since
    /// 0.7.0: the guest owns it, and replacing it drops the old one —
    /// dropping IS the unsubscribe (0.6.0 kept a u64 handle in an atomic
    /// and closed it explicitly). Guests are single-threaded; the cell is
    /// only ever touched from a probe call.
    static SUBSCRIPTION: RefCell<Option<Subscription>> = const { RefCell::new(None) };
}

/// A host error as one line: the typed kind picks the wording (the contract
/// says the variant is what a guest branches on), the detail is the host's
/// own sentence handed through verbatim. `{e}` would print the Debug form
/// of the variant, which is not a sentence.
fn host_error(verb: &str, error: HostError) -> String {
    match error {
        HostError::Failed(detail) => format!("{verb} failed: {detail}"),
        HostError::Invalid(detail) => format!("{verb} invalid: {detail}"),
    }
}

struct Streamer;

impl Tools for Streamer {
    async fn definitions() -> Vec<Definition> {
        Vec::new()
    }

    async fn execute(name: String, _arguments_json: String) -> ToolResult {
        ToolResult {
            content: vec![tau::extension::types::ResultBlock::Text(format!(
                "unknown tool: {name}"
            ))],
            is_error: true,
        }
    }
}

impl Probes for Streamer {
    fn points() -> Vec<Point> {
        vec![Point::SessionStart, Point::BeforeRunEnd]
    }

    fn probe(_point: Point, payload: Payload) -> Verdict {
        match payload {
            Payload::SessionStart(_) => match host::subscribe(&[Topic::TextDelta]) {
                Ok(subscription) => {
                    SUBSCRIPTION.with(|cell| *cell.borrow_mut() = Some(subscription));
                }
                Err(e) => {
                    let _ = host::notify(
                        Level::Warn,
                        &[Content::Text(host_error("stream subscribe", e))],
                    );
                }
            },
            Payload::BeforeRunEnd(_) => {
                let events =
                    SUBSCRIPTION.with(|cell| cell.borrow().as_ref().map(Subscription::poll));
                if let Some(events) = events {
                    let mut deltas = 0u64;
                    let mut chars = 0usize;
                    let mut lagged = 0u64;
                    for event in &events {
                        match event {
                            StreamEvent::TextDelta(text) => {
                                deltas += 1;
                                chars += text.len();
                            }
                            StreamEvent::Lagged(n) => lagged += n,
                            StreamEvent::AudioDelta(_) => {}
                        }
                    }
                    let suffix = if lagged > 0 {
                        format!(" (+{lagged} dropped)")
                    } else {
                        String::new()
                    };
                    let _ = host::notify(
                        Level::Info,
                        &[Content::Text(format!(
                            "stream observed: {deltas} text deltas, {chars} chars{suffix}"
                        ))],
                    );
                }
            }
            _ => {}
        }
        Verdict::Continue
    }
}

export!(Streamer);
