//! Example tau bridge: a dingtalk-shaped (钉钉 stream mode) IM adapter
//! (docs/im-channels.md §钉钉回环协议) — the feishu mechanism (ws
//! long connection inbound, http reply outbound) with the two
//! dingtalk-shaped increments, which are the reason this example
//! exists:
//!
//! 1. **in-band ack**: dingtalk stream requires the client to ack each
//!    CALLBACK frame ON THE SAME ws connection
//!    (`{"code":200,"headers":{...},"message":"OK","data":...}`) — an
//!    unacked frame is redelivered by the platform. The feishu loopback
//!    never sends; this is the first real `ws.send` user.
//! 2. **double-encoded JSON**: the frame's `data` field is a STRING
//!    holding escaped JSON — decode the outer frame, unescape `data`,
//!    then parse the inner message body (`msgtype`/`text.content`/
//!    `senderStaffId`).
//!
//! - inbound: `session_start` opens the ws long connection
//!   (TAU_MCP_URL, host config via --mcp-url); later call points drain
//!   frames, ack the CALLBACK, steer the inner text into the session
//!   (no call-time gate since 0.8.0: the install record authorizes
//!   steering).
//! - outbound: `after_response` posts the assembled assistant text to
//!   the robot send API shape (`{ws-origin}/reply`, derived from the ws
//!   URL — no origin allowlist since 0.8.0).
//!
//! 0.7.0 shape: the connection is a resource, the ack is an awaited
//! `send` (so "sent" means on the wire — print mode may exit right after
//! this call), and the whole waiting half lives in `bridge-io.turn`,
//! because a synchronously lowered export cannot await anything
//! (docs/wit-redesign.md section 5). The drain itself never waits:
//! `ws.connection.poll` returns what has arrived.
//!
//! Honestly NOT simulated (recorded in the doc): the gateway handshake
//! (POST /v1.0/gateway/connections exchanges credentials for the wss
//! endpoint — it only OBTAINS the address, the loopback connects
//! directly), the session/identity mapping config (feishu-bridge's
//! demonstration), and card/rich-text mapping (beyond a text loopback).
//!
//! Build:
//!   cargo build --manifest-path examples/dingtalk-bridge/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use (loopback mock):
//!   tau --allow-unsigned --mcp-bridge .../dingtalk_bridge.wasm \
//!       --mcp-url ws://127.0.0.1:PORT/dt --demo -p "hi"

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "bridge",
});

use std::cell::RefCell;

use exports::tau::extension::bridge_io::Guest as BridgeIo;
use exports::tau::extension::ingress_handler::{Guest as IngressHandler, Request, Response};
use exports::tau::extension::probes::{Guest as Probes, Payload, Point, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::host::{self, Level};
use tau::extension::types::{Content, Error as HostError, Message, Role};
use tau::extension::{http, ws};

/// Adapter state. A `thread_local` cell since 0.7.0 (the connection is a
/// resource, and the guest is single-threaded — the host serializes every
/// export call on this instance, so a lock would only be uncontended).
struct Adapter {
    /// Open connection to the platform (None until session_start).
    connection: Option<ws::Connection>,
    /// The user an inbound message came from (echoed in the reply).
    user: Option<String>,
    /// A steered IM message is waiting for its assistant reply.
    awaiting_reply: bool,
    /// The one loopback message was already steered (the mock pushes
    /// once; without this every call point would re-steer it).
    steered: bool,
}

thread_local! {
    static ADAPTER: RefCell<Adapter> = RefCell::new(Adapter {
        connection: None,
        user: None,
        awaiting_reply: false,
        steered: false,
    });
}

struct DingtalkBridge;

/// One user-visible line (renderer draws it; never enters model history).
fn say(level: Level, text: String) {
    let _ = host::notify(level, &[Content::Text(text)]);
}

/// A host error as one notice line: the typed kind picks the wording, the
/// detail is the host's own sentence handed through verbatim (the contract
/// says never to match on it, so this never does).
fn host_error(verb: &str, error: HostError) -> String {
    match error {
        HostError::Failed(detail) => format!("{verb} failed: {detail}"),
        HostError::Invalid(detail) => format!("{verb} invalid: {detail}"),
    }
}

/// Flat-JSON string extraction, escape-UNaware (fine for flat values —
/// same contract as the other examples).
fn json_get<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let start = json.find(&pat)? + pat.len();
    let rest = json[start..]
        .trim_start_matches(|c: char| c.is_whitespace())
        .strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Extract a JSON string that may contain ESCAPED quotes — the
/// dingtalk frame's `data` is a string holding an entire escaped JSON
/// document, so a naive first-quote search stops at the first `\"`.
/// Scans honoring backslash escapes and unescapes (\", \\, \/, \n, \t,
/// \r, \b, \f; \uXXXX is out of the loopback's scope, documented).
fn json_get_escaped(json: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let start = json.find(&pat)? + pat.len();
    let rest = json[start..]
        .trim_start_matches(|c: char| c.is_whitespace())
        .strip_prefix('"')?;
    let bytes = rest.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some(out),
            b'\\' if i + 1 < bytes.len() => {
                let c = bytes[i + 1] as char;
                out.push(match c {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    other => other, // \" \\ \/ and anything else: itself
                });
                i += 2;
            }
            _ => {
                // UTF-8 safe: the source is &str, escapes are ASCII.
                let ch_len = utf8_len(bytes[i]);
                out.push_str(&rest[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    None
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// ws(s) URL → the reply API URL: same authority (the reply API sits
/// next to the ws endpoint), path replaced with /reply — the feishu
/// pattern.
fn reply_url(ws_url: &str) -> Option<String> {
    let (httpish, rest) = if let Some(rest) = ws_url.strip_prefix("ws://") {
        ("http", rest)
    } else {
        ("https", ws_url.strip_prefix("wss://")?)
    };
    let authority = rest.split('/').next()?;
    Some(format!("{httpish}://{authority}/reply"))
}

/// Open the platform's long connection, once. The gateway handshake
/// (endpoint+ticket exchange) only OBTAINS this URL — the loopback
/// connects directly.
async fn connect() {
    if ADAPTER.with(|cell| cell.borrow().connection.is_some()) {
        return;
    }
    let url = std::env::var("TAU_MCP_URL").unwrap_or_default();
    match ws::Connection::connect(url).await {
        Ok(connection) => ADAPTER.with(|cell| cell.borrow_mut().connection = Some(connection)),
        Err(error) => say(Level::Error, host_error("dingtalk: ws connect", error)),
    }
}

/// Drain what the platform sent since the last call point: ack each
/// CALLBACK frame and steer the inner text into the session. Never waits
/// (`poll` returns what has arrived); the ack's `send` IS awaited, so
/// "acked" means written to the socket — an unacked frame is redelivered
/// by the platform, and redelivery would re-steer the message.
async fn pump_inbound(connection: &ws::Connection) {
    if ADAPTER.with(|cell| cell.borrow().steered) {
        return;
    }
    let frames = match connection.poll() {
        Ok(frames) => frames,
        Err(error) => return say(Level::Warn, host_error("dingtalk: ws poll", error)),
    };
    for frame in frames {
        if handle_frame(connection, frame).await {
            return;
        }
    }
}

/// Handle one drained frame. Returns true when it was a CALLBACK this pump
/// is done with (acked and steered, or noted and dropped).
async fn handle_frame(connection: &ws::Connection, frame: ws::Frame) -> bool {
    let ws::Frame::Text(text) = frame else {
        return false;
    };
    if json_get(&text, "type") != Some("CALLBACK") {
        return false;
    }
    // Ack FIRST, on the same connection — an unacked frame is
    // redelivered, and redelivery would re-steer the message.
    let ack = "{\"code\":200,\"headers\":{\"contentType\":\"application/json\"},\"message\":\"OK\",\"data\":\"{}\"}";
    if let Err(error) = connection.send(ws::Frame::Text(ack.to_string())).await {
        say(Level::Error, host_error("dingtalk: ack send", error));
        return false;
    }
    // `data` is a string holding escaped JSON — decode the second layer.
    let Some(inner) = json_get_escaped(&text, "data") else {
        return false;
    };
    if json_get(&inner, "msgtype") != Some("text") {
        return true; // cards/rich media: beyond the text loopback (doc says so)
    }
    // The inner body is flat JSON once `data` is unescaped;
    // "content" (inside the text object) appears exactly once — the
    // loopback owns the shape, no quotes inside the text.
    let (Some(user), Some(body)) = (
        json_get(&inner, "senderStaffId"),
        json_get(&inner, "content"),
    ) else {
        return false;
    };
    let notice = ADAPTER.with(|cell| {
        let mut adapter = cell.borrow_mut();
        let message = Message {
            role: Role::User,
            content: vec![Content::Text(format!("[IM dingtalk {user}] {body}"))],
        };
        match host::steer(&message) {
            Ok(()) => {
                adapter.user = Some(user.to_string());
                adapter.awaiting_reply = true;
                adapter.steered = true;
                (
                    Level::Info,
                    format!(
                        "dingtalk: inbound message from {user} acked and steered into the session"
                    ),
                )
            }
            // A failed steer leaves `awaiting_reply` false, so no reply
            // leaves for a message that never entered the session.
            Err(error) => (Level::Error, host_error("dingtalk: steer", error)),
        }
    });
    let (level, line) = notice;
    say(level, line);
    true
}

/// Post the assembled assistant text back over the robot send API shape
/// (same authority as the ws URL). The text is the typed payload's first
/// text block — 0.6.0 scraped the JSON payload for one.
async fn post_reply(payload: &Payload) {
    let pending = ADAPTER.with(|cell| {
        let adapter = cell.borrow();
        adapter
            .awaiting_reply
            .then(|| adapter.user.clone())
            .flatten()
    });
    let Some(user) = pending else { return };
    let text = match payload {
        Payload::AfterResponse(response) => {
            response
                .message
                .content
                .iter()
                .find_map(|block| match block {
                    Content::Text(text) => Some(text.clone()),
                    _ => None,
                })
        }
        _ => None,
    };
    let Some(text) = text else { return };
    let Some(ws_url) = std::env::var("TAU_MCP_URL").ok() else {
        return;
    };
    let Some(url) = reply_url(&ws_url) else {
        return say(
            Level::Error,
            format!("dingtalk: cannot derive reply URL from {ws_url}"),
        );
    };
    // The body is built as JSON rather than interpolated: assistant text
    // routinely contains quotes and newlines.
    let body = serde_json::json!({ "user": user, "text": text }).to_string();
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    match http::request("POST".to_string(), url, headers, body.into_bytes()).await {
        Ok(response) => {
            let _ = response.status();
            drop(response); // dropping closes the connection
            ADAPTER.with(|cell| cell.borrow_mut().awaiting_reply = false);
            say(Level::Info, format!("dingtalk: reply posted to {user}"));
        }
        Err(error) => say(Level::Error, host_error("dingtalk: reply POST", error)),
    }
}

impl Tools for DingtalkBridge {
    async fn definitions() -> Vec<Definition> {
        Vec::new()
    }

    async fn execute(name: String, _arguments_json: String) -> ToolResult {
        ToolResult {
            content: vec![tau::extension::types::ResultBlock::Text(format!(
                "unknown tool: {name}"
            ))],
            is_error: true,
        }
    }
}

impl Probes for DingtalkBridge {
    fn points() -> Vec<Point> {
        vec![Point::SessionStart, Point::AfterResponse]
    }

    /// Neither point is a decision for this adapter — everything it does
    /// at them is I/O that waits (connect, ack, post), which no
    /// synchronously lowered export can do. The probe answers `continue`
    /// and the work rides `turn`; declaring the points is what makes the
    /// host call it.
    fn probe(_point: Point, _payload: Payload) -> Verdict {
        Verdict::Continue
    }
}

impl BridgeIo for DingtalkBridge {
    async fn turn(point: Point, payload: Payload) {
        match point {
            Point::SessionStart => connect().await,
            Point::AfterResponse => {
                // The connection is taken out for the awaits (a borrow
                // cannot be held across them) and put back whatever
                // happens — dropping it would close the connection.
                let connection = ADAPTER.with(|cell| cell.borrow_mut().connection.take());
                if let Some(connection) = connection.as_ref() {
                    // Drain first (an inbound frame may be waiting), then
                    // reply if a steered message is outstanding — the pump
                    // order that makes the loopback close within one turn.
                    pump_inbound(connection).await;
                    post_reply(&payload).await;
                }
                ADAPTER.with(|cell| cell.borrow_mut().connection = connection);
            }
            _ => {}
        }
    }
}

/// No webhook leg (钉钉 stream is a client connection, not a callback)
/// — the contract makes the export mandatory, so it stubs 501.
impl IngressHandler for DingtalkBridge {
    async fn handle_request(_request: Request) -> Response {
        Response {
            status: 501,
            headers: Vec::new(),
            body: b"this bridge has no webhook leg (ws stream mode)".to_vec(),
        }
    }
}

export!(DingtalkBridge);
