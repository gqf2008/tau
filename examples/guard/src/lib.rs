//! Example tau extension: a probe that actually decides. Contributes no
//! tools; its `before_tool` probe blocks any call whose arguments carry
//! the word "forbidden" — the block reason goes back to the model as
//! the tool result, so the run continues instead of dying.
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

use exports::tau::extension::hooks::{Action, Guest as Hooks, Verdict};
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

impl Hooks for Guard {
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
        let forbidden = parsed
            .ok()
            .and_then(|p| p["args"]["text"].as_str().map(str::to_string))
            .is_some_and(|text| text.contains("forbidden"));
        if forbidden {
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
