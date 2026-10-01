//! Example tau extension exercising the host channel (tau:extension@0.8.0).
//!
//! Tool `poke` does all three host calls in one shot, and its result
//! reports each outcome — so all three legs show up in the tool
//! transcript itself:
//!
//! - `host.notify(info, …)` — a user-visible fact. Always allowed.
//! - `host.emit({"poke": …})` — an extension-defined fact on the event
//!   bus. Always allowed.
//! - `host.steer(user message)` — a decision: inject the text into the
//!   run. No call-time gate since 0.8.0 — the install record (signed
//!   bytes, trusted fingerprint) is the one authorization act, so the
//!   steer is queued and lands after the current turn's tool results
//!   (enqueue-only, never re-entrant).
//!
//! Build:
//!   cargo build --manifest-path examples/notifier/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use:
//!   tau --demo -e .../notifier.wasm -p "hello"    # notify, emit and steer all land

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "extension",
});

use exports::tau::extension::probes::{Guest as Probes, Payload, Point, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::host::{self, Level};
use tau::extension::types::{Content, Error as HostError, Message, ResultBlock, Role};

/// A host error as one report line: the typed variant is what a guest
/// branches on (never the string), and the detail is the host's own
/// sentence handed through verbatim.
fn host_error(verb: &str, error: HostError) -> String {
    match error {
        HostError::Failed(detail) => format!("{verb} failed: {detail}"),
        HostError::Invalid(detail) => format!("{verb} invalid: {detail}"),
    }
}

struct Notifier;

impl Tools for Notifier {
    async fn definitions() -> Vec<Definition> {
        vec![Definition {
            name: "poke".into(),
            description: "Notify the user, emit a fact, and steer the run with the given text"
                .into(),
            parameters_json: r#"{
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }"#
            .into(),
        }]
    }

    async fn execute(name: String, arguments_json: String) -> ToolResult {
        if name != "poke" {
            return ToolResult {
                content: vec![ResultBlock::Text(format!("unknown tool: {name}"))],
                is_error: true,
            };
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&arguments_json);
        let text = parsed
            .ok()
            .and_then(|v| v["text"].as_str().map(str::to_string))
            .unwrap_or_default();

        let mut report = Vec::new();

        // Fact 1: user-visible notification (renderer draws it; never
        // enters model history). The level is an enum since 0.7.0.
        match host::notify(Level::Info, &[Content::Text(format!("poke: {text}"))]) {
            Ok(()) => report.push("notify ok".to_string()),
            Err(error) => report.push(host_error("notify", error)),
        }

        // Fact 2: extension-defined event on the bus (observe-only).
        match host::emit(&serde_json::json!({"poke": text}).to_string()) {
            Ok(()) => report.push("emit ok".to_string()),
            Err(error) => report.push(host_error("emit", error)),
        }

        // Decision: steer the run. No call-time gate since 0.8.0 — the
        // install record authorizes it, so the call only reports contract
        // misuse (wrong role, size limit) or a host-side failure.
        let message = Message {
            role: Role::User,
            content: vec![Content::Text(text)],
        };
        match host::steer(&message) {
            Ok(()) => report.push("steer queued".to_string()),
            Err(error) => report.push(host_error("steer", error)),
        }

        ToolResult {
            content: vec![ResultBlock::Text(report.join("; "))],
            is_error: false,
        }
    }
}

impl Probes for Notifier {
    fn points() -> Vec<Point> {
        Vec::new()
    }

    fn probe(_point: Point, _payload: Payload) -> Verdict {
        Verdict::Continue
    }
}

export!(Notifier);
