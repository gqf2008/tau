//! Example tau extension exercising the host channel (tau:extension@0.2.0).
//!
//! Tool `poke` does all three host calls in one shot, and its result
//! reports each outcome — so the consent gate is observable in the tool
//! transcript itself:
//!
//! - `host.notify("info", …)` — a user-visible fact. Always allowed.
//! - `host.emit({"poke": …})` — an extension-defined fact on the event
//!   bus. Always allowed.
//! - `host.steer(user message)` — a decision: inject the text into the
//!   run. Consent-gated: without `--allow-inject` the call fails and the
//!   tool result carries the refusal; with it, the steer lands after the
//!   current turn's tool results (enqueue-only, never re-entrant).
//!
//! Build:
//!   cargo build --manifest-path examples/notifier/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use:
//!   tau --demo -e .../notifier.wasm -p "hello"                    # steer refused
//!   tau --demo -e .../notifier.wasm --allow-inject -p "hello"     # steer lands

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "extension",
});

use exports::tau::extension::probes::{Action, Guest as Probes, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::host;
use tau::extension::types::{Content, Message, Role};

struct Notifier;

impl Tools for Notifier {
    fn definitions() -> Vec<Definition> {
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

    fn execute(name: String, arguments_json: String) -> ToolResult {
        if name != "poke" {
            return ToolResult {
                content: format!("unknown tool: {name}"),
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
        // enters model history).
        match host::notify("info", &[Content::Text(format!("poke: {text}"))]) {
            Ok(()) => report.push("notify ok".to_string()),
            Err(e) => report.push(format!("notify refused: {e}")),
        }

        // Fact 2: extension-defined event on the bus (observe-only).
        match host::emit(&serde_json::json!({"poke": text}).to_string()) {
            Ok(()) => report.push("emit ok".to_string()),
            Err(e) => report.push(format!("emit refused: {e}")),
        }

        // Decision: steer the run. Consent-gated — the refusal text is
        // part of the demo (it proves the gate fails closed).
        let message = Message {
            role: Role::User,
            content: vec![Content::Text(text)],
        };
        match host::steer(&message) {
            Ok(()) => report.push("steer queued".to_string()),
            Err(e) => report.push(format!("steer refused: {e}")),
        }

        ToolResult {
            content: report.join("; "),
            is_error: false,
        }
    }
}

impl Probes for Notifier {
    fn points() -> Vec<String> {
        Vec::new()
    }

    fn probe(_point: String, _payload_json: String) -> Verdict {
        Verdict {
            action: Action::Continue,
            payload_json: None,
            reason: None,
        }
    }
}

export!(Notifier);
