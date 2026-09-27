//! Example tau extension: a probe that actually decides. Contributes no
//! tools; its `before_tool` probe blocks any call whose arguments carry
//! the word "forbidden" — the block reason goes back to the model as
//! the tool result, so the run continues instead of dying. Arguments
//! carrying "crash" make the probe panic instead: the host degrades a
//! broken probe to continue, so the call still goes through. Arguments
//! carrying "wasicheck" make the probe read the ambient environment:
//! under the default allow-all WASI it sees the host's TAU_AMBIENT and
//! blocks (proving the leak); under --deny-wasi the guest env is empty
//! and the call passes (proving the sandbox).
//!
//! Build:
//!   cargo build --manifest-path examples/guard/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use (with any tool extension, e.g. upper):
//!   tau --allow-unsigned \
//!     -e examples/upper/target/wasm32-wasip2/release/upper.wasm \
//!     -e examples/guard/target/wasm32-wasip2/release/guard.wasm \
//!     --demo -p "shout forbidden"
//!   # → [tau] tool ← upper (error): blocked: the guard said no

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "extension",
});

use exports::tau::extension::probes::{Action, Guest as Probes, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};

struct Guard;

impl Tools for Guard {
    fn definitions() -> Vec<Definition> {
        // This extension only probes; it provides no tools.
        Vec::new()
    }

    fn execute(name: String, _arguments_json: String) -> ToolResult {
        ToolResult {
            content: format!("guard provides no tools (called: {name})"),
            is_error: true,
        }
    }
}

impl Probes for Guard {
    fn points() -> Vec<String> {
        vec!["before_tool".into()]
    }

    fn probe(point: String, payload_json: String) -> Verdict {
        let continue_ = || Verdict {
            action: Action::Continue,
            payload_json: None,
            reason: None,
        };
        if point != "before_tool" {
            return continue_();
        }
        // before_tool payload: {"id": ..., "name": ..., "args": {...}}.
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&payload_json);
        let text = parsed
            .ok()
            .and_then(|p| p["args"]["text"].as_str().map(str::to_string));
        // A broken probe must degrade to continue, never wedge the run:
        // this panic is the fixture that proves it end to end.
        if text.as_deref().is_some_and(|t| t.contains("crash")) {
            panic!("the guard blew up");
        }
        // The sandbox boundary, observable from inside: read an ambient
        // env var. Default ambient WASI inherits the host env (block to
        // prove the leak); --deny-wasi leaves the guest env empty.
        if text.as_deref().is_some_and(|t| t.contains("wasicheck")) {
            if std::env::var("TAU_AMBIENT").is_ok() {
                return Verdict {
                    action: Action::Block,
                    payload_json: None,
                    reason: Some("ambient env leaked into the guest".into()),
                };
            }
            return continue_();
        }
        if text.is_some_and(|text| text.contains("forbidden")) {
            Verdict {
                action: Action::Block,
                payload_json: None,
                reason: Some("the guard said no".into()),
            }
        } else {
            continue_()
        }
    }
}

export!(Guard);
