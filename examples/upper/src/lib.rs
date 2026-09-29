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

use exports::tau::extension::probes::{Guest as Probes, Payload, Point, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::types::ResultBlock;

struct Upper;

impl Tools for Upper {
    async fn definitions() -> Vec<Definition> {
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

    /// `async` since 0.7.0: a tool that talks to the host awaits those calls
    /// in place. This one has nothing to wait for, so the body is unchanged.
    async fn execute(name: String, arguments_json: String) -> ToolResult {
        if name != "upper" {
            return ToolResult {
                content: vec![ResultBlock::Text(format!("unknown tool: {name}"))],
                is_error: true,
            };
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&arguments_json);
        match parsed.ok().and_then(|v| v["text"].as_str().map(str::to_string)) {
            Some(text) => ToolResult {
                content: vec![ResultBlock::Text(text.to_uppercase())],
                is_error: false,
            },
            None => ToolResult {
                content: vec![ResultBlock::Text("missing string argument 'text'".into())],
                is_error: true,
            },
        }
    }
}

impl Probes for Upper {
    fn points() -> Vec<Point> {
        // This extension only provides a tool; it probes nothing.
        Vec::new()
    }

    /// Points and payloads are typed since 0.7.0: a probe that handles
    /// nothing cannot be handed an unknown point, and "no opinion" is the
    /// `continue` arm rather than a record with three loose fields.
    fn probe(_point: Point, _payload: Payload) -> Verdict {
        Verdict::Continue
    }
}

export!(Upper);
