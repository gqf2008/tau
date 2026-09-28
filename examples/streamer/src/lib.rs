//! Example tau extension exercising the high-frequency stream
//! subscription (tau:extension@0.6.0, docs/stream-subscribe.md — F2's
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

use std::sync::atomic::{AtomicU64, Ordering};

use exports::tau::extension::probes::{Action, Guest as Probes, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::host::{self, StreamEvent};
use tau::extension::types::Content;

/// The subscription handle, or u64::MAX before `session_start` wires it.
/// Guests are single-threaded; an atomic is the simplest shared cell.
static SUBSCRIPTION: AtomicU64 = AtomicU64::new(u64::MAX);

struct Streamer;

impl Tools for Streamer {
    fn definitions() -> Vec<Definition> {
        Vec::new()
    }

    fn execute(name: String, _arguments_json: String) -> ToolResult {
        ToolResult {
            content: vec![tau::extension::types::ResultBlock::Text(format!(
                "unknown tool: {name}"
            ))],
            is_error: true,
        }
    }
}

impl Probes for Streamer {
    fn points() -> Vec<String> {
        vec!["session_start".into(), "before_run_end".into()]
    }

    fn probe(point: String, _payload_json: String) -> Verdict {
        match point.as_str() {
            "session_start" => match host::subscribe(&["text-delta".to_string()]) {
                Ok(handle) => SUBSCRIPTION.store(handle, Ordering::Relaxed),
                Err(e) => {
                    let _ = host::notify(
                        "warn",
                        &[Content::Text(format!("stream subscribe refused: {e}"))],
                    );
                }
            },
            "before_run_end" => {
                let handle = SUBSCRIPTION.load(Ordering::Relaxed);
                if handle != u64::MAX
                    && let Ok(events) = host::poll(handle)
                {
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
                        "info",
                        &[Content::Text(format!(
                            "stream observed: {deltas} text deltas, {chars} chars{suffix}"
                        ))],
                    );
                }
            }
            _ => {}
        }
        Verdict {
            action: Action::Continue,
            payload_json: None,
            reason: None,
        }
    }
}

export!(Streamer);
