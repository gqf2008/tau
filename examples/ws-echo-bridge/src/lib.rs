//! Example tau bridge exercising the `ws` capability
//! (tau:extension@0.3.0, docs/im-channels.md): the `ws_echo` tool
//! connects to the consented endpoint (TAU_MCP_URL, a ws(s) URL), sends
//! the argument as one text frame, waits for one frame back (explicit
//! timeout — a recv that can block forever hides a dead connection,
//! wit-review F9), and returns it. The host is a frame pipe; this
//! component owns any protocol above frames.
//!
//! Build:
//!   cargo build --manifest-path examples/ws-echo-bridge/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use (loopback echo server on :PORT):
//!   tau --allow-unsigned --mcp-bridge .../ws_echo_bridge.wasm \
//!       --mcp-url ws://127.0.0.1:PORT/echo --demo -p "echo hi via ws_echo"

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "bridge",
});

use exports::tau::extension::ingress_handler::{Guest as IngressHandler, Request, Response};
use exports::tau::extension::probes::{Action, Guest as Probes, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::types::ResultBlock;
use tau::extension::ws::{self, Frame};

struct WsEchoBridge;

fn text_result(text: String, is_error: bool) -> ToolResult {
    ToolResult {
        content: vec![ResultBlock::Text(text)],
        is_error,
    }
}

/// Minimal flat-JSON extraction of the "text" field, same contract as
/// the other examples (escapes and nesting out of scope).
fn extract_text(json: &str) -> Option<String> {
    let key = "\"text\"";
    let start = json.find(key)? + key.len();
    let rest = json[start..].trim_start_matches([' ', ':', '\t']);
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

impl Tools for WsEchoBridge {
    fn definitions() -> Vec<Definition> {
        vec![Definition {
            name: "ws_echo".into(),
            description: "Send text over the consented WebSocket and return the echo".into(),
            parameters_json:
                r#"{ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] }"#
                    .into(),
        }]
    }

    fn execute(name: String, arguments_json: String) -> ToolResult {
        if name != "ws_echo" {
            return text_result(format!("unknown tool: {name}"), true);
        }
        let Some(text) = extract_text(&arguments_json) else {
            return text_result("missing string argument 'text'".into(), true);
        };
        match round_trip(&text) {
            Ok(echo) => text_result(format!("echo: {echo}"), false),
            Err(e) => text_result(format!("ws failed: {e}"), true),
        }
    }
}

fn round_trip(text: &str) -> Result<String, String> {
    let url = std::env::var("TAU_MCP_URL")
        .map_err(|_| "no TAU_MCP_URL granted by host (--mcp-url ws://…)".to_string())?;
    let handle = ws::connect(&url)?;
    let result = (|| {
        ws::send(handle, &Frame::Text(text.to_string()))?;
        // 5s is generous for a loopback echo and proves the timeout path
        // exists; a dead connection must surface as an error, not a hang.
        match ws::recv(handle, 5000)? {
            Frame::Text(t) => Ok(t),
            Frame::Binary(b) => Ok(format!("<{} binary bytes>", b.len())),
        }
    })();
    let _ = ws::close(handle);
    result
}

/// Nothing to observe — the bridge world exports probes since 0.3.0
/// (docs/im-channels.md); an empty points() list opts out.
impl Probes for WsEchoBridge {
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

// The 0.3.0 bridge world makes ingress-handler a mandatory export. This
// adapter has no webhook leg (it never calls ingress.listen), so nothing
// can invoke this — the stub is explicit, not dead weight.
impl IngressHandler for WsEchoBridge {
    fn handle_request(_request: Request) -> Response {
        Response {
            status: 501,
            headers: Vec::new(),
            body: b"this bridge has no webhook leg".to_vec(),
        }
    }
}

export!(WsEchoBridge);
