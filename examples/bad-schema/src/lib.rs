//! Negative fixture, never shipped: declares one tool whose
//! `parameters-json` is not valid JSON. The host must refuse to load
//! the component and name the tool (wit-review F5 — a broken schema must
//! not silently degrade to an open one).
//!
//! Build:
//!   cargo build --manifest-path examples/bad-schema/Cargo.toml \
//!       --target wasm32-wasip2 --release

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "extension",
});

use exports::tau::extension::probes::{Action, Guest as Probes, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::types::ResultBlock;

struct BadSchema;

impl Tools for BadSchema {
    fn definitions() -> Vec<Definition> {
        vec![Definition {
            name: "bad_schema".into(),
            description: "Declares an invalid parameters-json on purpose".into(),
            parameters_json: "this is not json {".into(),
        }]
    }

    fn execute(_name: String, _arguments_json: String) -> ToolResult {
        ToolResult {
            content: vec![ResultBlock::Text("unreachable: the host refuses to load this component".into())],
            is_error: true,
        }
    }
}

impl Probes for BadSchema {
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

export!(BadSchema);
