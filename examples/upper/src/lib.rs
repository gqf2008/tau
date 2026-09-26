//! Example tau extension compiled to a wasm component.
//!
//! Build:
//!   cargo build --manifest-path examples/upper/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use:
//!   tau -e examples/upper/target/wasm32-wasip2/release/upper.wasm -p "..."

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "extension",
});

use exports::tau::extension::hooks::{Action, Guest as Hooks, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};

struct Upper;

impl Tools for Upper {
    fn definitions() -> Vec<Definition> {
        vec![Definition {
            name: "upper".into(),
            description: "Convert text to UPPERCASE".into(),
            parameters_json: r#"{
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }"#
            .into(),
        }]
    }

    fn execute(name: String, arguments_json: String) -> ToolResult {
        if name != "upper" {
            return ToolResult {
                content: format!("unknown tool: {name}"),
                is_error: true,
            };
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&arguments_json);
        match parsed.ok().and_then(|v| v["text"].as_str().map(str::to_string)) {
            Some(text) => ToolResult {
                content: text.to_uppercase(),
                is_error: false,
            },
            None => ToolResult {
                content: "missing string argument 'text'".into(),
                is_error: true,
            },
        }
    }
}

impl Hooks for Upper {
    fn points() -> Vec<String> {
        // This extension only provides a tool; it probes nothing.
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

export!(Upper);
