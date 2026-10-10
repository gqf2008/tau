//! Example tau bridge exercising the `ws` capability
//! (tau:extension@0.8.0, docs/im-channels.md): the `ws_echo` tool
//! connects to the endpoint TAU_MCP_URL names (host config via
//! --mcp-url, a ws(s) URL), sends the argument as one text frame and
//! returns the first frame back.
//!
//! Waiting is host policy since 0.7.0 — the guest passes no timeout: the
//! connect budget is the host's (`TAU_WS_CONNECT_TIMEOUT_MS` in tests,
//! `CONNECT_TIMEOUT_MS` in production), and a peer that goes silent is
//! closed by the host (60s without an inbound frame or pong), which ends
//! the frame stream. The connection is a resource: dropping it closes the
//! connection, so there is no `close` call to forget.
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

use exports::tau::extension::bridge_io::Guest as BridgeIo;
use exports::tau::extension::ingress_handler::{Guest as IngressHandler, Request, Response};
use exports::tau::extension::probes::{Guest as Probes, Payload, Point, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::types::ResultBlock;
use tau::extension::ws::{self, Frame};
use wit_bindgen::rt::async_support::StreamReader;

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
    async fn definitions() -> Vec<Definition> {
        vec![Definition {
            name: "ws_echo".into(),
            description: "Send text over the WebSocket at TAU_MCP_URL and return the echo".into(),
            parameters_json:
                r#"{ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] }"#
                    .into(),
        }]
    }

    async fn execute(name: String, arguments_json: String) -> ToolResult {
        if name != "ws_echo" {
            return text_result(format!("unknown tool: {name}"), true);
        }
        let Some(text) = extract_text(&arguments_json) else {
            return text_result("missing string argument 'text'".into(), true);
        };
        match round_trip(&text).await {
            Ok(echo) => text_result(format!("echo: {echo}"), false),
            Err(e) => text_result(format!("ws failed: {e}"), true),
        }
    }
}

async fn round_trip(text: &str) -> Result<String, String> {
    let url = std::env::var("TAU_MCP_URL")
        .map_err(|_| "no TAU_MCP_URL granted by host (--mcp-url ws://…)".to_string())?;
    // The handshake's bound is the host's since 0.7.0: a peer that accepts
    // the TCP connection and then never upgrades fails the connect, loudly
    // and without the guest naming a number.
    let connection = ws::Connection::connect(url).await.map_err(describe)?;
    // The frames the peer sends. Reading them needs no timeout parameter
    // either: the host pings and closes a silent peer, and that close ends
    // this stream.
    let (mut frames, verdict) = connection.receive();
    let echoed = echo(&mut frames, &connection, text).await;
    // The verdict is diagnostic (it says why the connection ended); the
    // frame stream's end is the answer the tool acts on.
    drop(verdict);
    // Dropping the connection closes it.
    drop(connection);
    echoed
}

async fn echo(
    frames: &mut StreamReader<Frame>,
    connection: &ws::Connection,
    text: &str,
) -> Result<String, String> {
    connection
        .send(Frame::Text(text.to_string()))
        .await
        .map_err(describe)?;
    match frames.next().await {
        Some(Frame::Text(t)) => Ok(t),
        Some(Frame::Binary(b)) => Ok(format!("<{} binary bytes>", b.len())),
        None => Err("the peer closed before answering".to_string()),
    }
}

/// The contract's three-way error, spelled the way the host spells it.
fn describe(error: ws::Error) -> String {
    match error {
        ws::Error::Failed(detail) => format!("failed: {detail}"),
        ws::Error::Invalid(detail) => format!("invalid: {detail}"),
    }
}

/// The bridge world exports `bridge-io` since 0.7.0: the asynchronous
/// half of a probe point, for a bridge whose I/O legs must wait (connect
/// at session start, post a reply after a response). This adapter does
/// all of its I/O inside tool calls, which are async already — so there
/// is nothing to do around a probe, and the empty turn says so.
impl BridgeIo for WsEchoBridge {
    async fn turn(_point: Point, _payload: Payload) {}
}

/// Nothing to observe — the bridge world exports probes since 0.3.0
/// (docs/im-channels.md); an empty points() list opts out.
impl Probes for WsEchoBridge {
    fn points() -> Vec<Point> {
        Vec::new()
    }

    fn probe(_point: Point, _payload: Payload) -> Verdict {
        Verdict::Continue
    }
}

// The 0.3.0 bridge world makes ingress-handler a mandatory export. This
// adapter has no webhook leg (it never calls ingress.listen), so nothing
// can invoke this — the stub is explicit, not dead weight.
impl IngressHandler for WsEchoBridge {
    async fn handle_request(_request: Request) -> Response {
        Response {
            status: 501,
            headers: Vec::new(),
            body: b"this bridge has no webhook leg".to_vec(),
        }
    }
}

export!(WsEchoBridge);
