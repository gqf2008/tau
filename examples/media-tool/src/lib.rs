//! Example tau extension exercising tool media results
//! (tau:extension@0.3.0, docs/tool-media.md): the `dot_png` tool returns
//! a text block plus a real image block — a 1x1 transparent PNG. Media
//! bytes cross the ABI raw; the host persists them (inline base64 under
//! the blob threshold) and materializes them at the provider edge.
//!
//! Build:
//!   cargo build --manifest-path examples/media-tool/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use:
//!   tau --demo -e .../media_tool.wasm -p "show me a dot"
//!   # → The tool answered: a 1x1 transparent dot. [image: image/png].

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "extension",
});

use exports::tau::extension::probes::{Action, Guest as Probes, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::types::{Media, MediaSource, ResultBlock};

/// A valid 1x1 transparent PNG (68 bytes).
const DOT_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
    0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
    0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
    0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

struct MediaTool;

impl Tools for MediaTool {
    fn definitions() -> Vec<Definition> {
        vec![Definition {
            name: "dot_png".into(),
            description: "Return a 1x1 transparent PNG as an image block".into(),
            parameters_json: r#"{ "type": "object", "properties": {} }"#.into(),
        }]
    }

    fn execute(name: String, _arguments_json: String) -> ToolResult {
        if name != "dot_png" {
            return ToolResult {
                content: vec![ResultBlock::Text(format!("unknown tool: {name}"))],
                is_error: true,
            };
        }
        ToolResult {
            content: vec![
                ResultBlock::Text("a 1x1 transparent dot. ".into()),
                ResultBlock::Media(Media {
                    media_type: "image/png".into(),
                    source: MediaSource::Bytes(DOT_PNG.to_vec()),
                    name: None,
                }),
            ],
            is_error: false,
        }
    }
}

impl Probes for MediaTool {
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

export!(MediaTool);
