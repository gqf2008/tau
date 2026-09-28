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
//!    never sends; this is the first real `ws::send` user.
//! 2. **double-encoded JSON**: the frame's `data` field is a STRING
//!    holding escaped JSON — decode the outer frame, unescape `data`,
//!    then parse the inner message body (`msgtype`/`text.content`/
//!    `senderStaffId`).
//!
//! - inbound: `session_start` opens the ws long connection
//!   (TAU_MCP_URL; origin consent via --mcp-url); later probe call
//!   points drain frames (the synchronous-guest pump, same semantics
//!   as feishu-bridge), ack the CALLBACK, steer the inner text into
//!   the session (consent: --allow-inject).
//! - outbound: `after_response` posts the assembled assistant text to
//!   the robot send API shape (`{ws-origin}/reply`, same origin as the
//!   ws consent).
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
//!       --mcp-url ws://127.0.0.1:PORT/dt --allow-inject --demo -p "hi"

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "bridge",
});

use std::sync::Mutex;

use exports::tau::extension::ingress_handler::{
    Guest as IngressHandler, Request, Response,
};
use exports::tau::extension::probes::{Action, Guest as Probes, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::types::{Content, Message, Role};
use tau::extension::{host, http, ws};

/// Budget for one network wait: a reply POST's response headers, or a ws
/// connect's handshake. The platform answers in seconds, so this is
/// generous already — it exists so a peer that accepts the connection and
/// then says nothing fails loudly instead of hanging the bridge
/// (wit-review F11).
const NET_MS: u32 = 30_000;

struct Adapter {
    /// Open ws handle to the platform (None until session_start).
    ws: Option<u64>,
    /// The user an inbound message came from (echoed in the reply).
    user: Option<String>,
    /// A steered IM message is waiting for its assistant reply.
    awaiting_reply: bool,
    /// The one loopback message was already steered (the mock pushes
    /// once; without this every call point would re-steer it).
    steered: bool,
}

static ADAPTER: Mutex<Adapter> = Mutex::new(Adapter {
    ws: None,
    user: None,
    awaiting_reply: false,
    steered: false,
});

struct DingtalkBridge;

fn notify(level: &str, text: String) {
    let _ = host::notify(level, &[Content::Text(text)]);
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

/// ws(s) URL → the reply API URL: same origin (so the ws consent
/// covers it), path replaced with /reply — the feishu pattern.
fn reply_url(ws_url: &str) -> Option<String> {
    let (httpish, rest) = if let Some(rest) = ws_url.strip_prefix("ws://") {
        ("http", rest)
    } else {
        ("https", ws_url.strip_prefix("wss://")?)
    };
    let authority = rest.split('/').next()?;
    Some(format!("{httpish}://{authority}/reply"))
}

/// Drain one inbound frame: ack the CALLBACK on the same connection
/// (the dingtalk increment #1), decode the double-encoded `data` (the
/// increment #2), steer the inner text into the session. Runs at probe
/// call points — the synchronous guest model means the pump only moves
/// when the host calls us (docs/im-channels.md 同步 guest 模型下的入站泵).
fn pump_inbound(adapter: &mut Adapter) {
    if adapter.steered {
        return;
    }
    let Some(handle) = adapter.ws else { return };
    // One short-timeout recv per call point: no frame is normal (the
    // platform is just quiet), a frame is drained and handled.
    let frame = match ws::recv(handle, 1000) {
        Ok(frame) => frame,
        Err(_) => return,
    };
    let ws::Frame::Text(text) = frame else { return };
    if json_get(&text, "type") != Some("CALLBACK") {
        return;
    }
    // Ack FIRST, on the same connection — an unacked frame is
    // redelivered, and redelivery would re-steer the message.
    let ack = "{\"code\":200,\"headers\":{\"contentType\":\"application/json\"},\"message\":\"OK\",\"data\":\"{}\"}";
    if let Err(e) = ws::send(handle, &ws::Frame::Text(ack.to_string())) {
        notify("error", format!("dingtalk: ack send failed: {e}"));
        return;
    }
    // `data` is a string holding escaped JSON — decode the second layer.
    let Some(inner) = json_get_escaped(&text, "data") else { return };
    if json_get(&inner, "msgtype") != Some("text") {
        return; // cards/rich media: beyond the text loopback (doc says so)
    }
    // The inner body is flat JSON once `data` is unescaped;
    // "content" (inside the text object) appears exactly once — the
    // loopback owns the shape, no quotes inside the text.
    let (Some(user), Some(body)) = (
        json_get(&inner, "senderStaffId"),
        json_get(&inner, "content"),
    ) else {
        return;
    };
    let message = Message {
        role: Role::User,
        content: vec![Content::Text(format!("[IM dingtalk {user}] {body}"))],
    };
    match host::steer(&message) {
        Ok(()) => {
            adapter.user = Some(user.to_string());
            adapter.awaiting_reply = true;
            adapter.steered = true;
            notify(
                "info",
                format!("dingtalk: inbound message from {user} acked and steered into the session"),
            );
        }
        Err(e) => notify("error", format!("dingtalk: steer refused: {e}")),
    }
}

/// Post the assembled assistant message back over the robot send API
/// shape (same origin as the ws consent).
fn post_reply(adapter: &mut Adapter, payload_json: &str) {
    if !adapter.awaiting_reply {
        return;
    }
    let (Some(user), Some(ws_url)) = (
        adapter.user.clone(),
        std::env::var("TAU_MCP_URL").ok(),
    ) else {
        return;
    };
    // The assistant text block: the message's first "text" member (the
    // Content wire shape serializes the field before the "type" tag).
    let Some(text) = json_get(payload_json, "text") else {
        return;
    };
    let Some(url) = reply_url(&ws_url) else {
        notify("error", format!("dingtalk: cannot derive reply URL from {ws_url}"));
        return;
    };
    let body = format!("{{\"user\":\"{user}\",\"text\":\"{text}\"}}");
    match http::request(
        "POST",
        &url,
        &[("content-type".to_string(), "application/json".to_string())],
        &body.into_bytes(),
        NET_MS,
    ) {
        Ok(h) => {
            let _ = http::status(h);
            http::close(h);
            adapter.awaiting_reply = false;
            notify("info", format!("dingtalk: reply posted to {user}"));
        }
        Err(e) => notify("error", format!("dingtalk: reply POST failed: {e}")),
    }
}

impl Tools for DingtalkBridge {
    fn definitions() -> Vec<Definition> {
        Vec::new()
    }

    fn execute(name: String, _arguments_json: String) -> ToolResult {
        ToolResult {
            content: vec![tau::extension::types::ResultBlock::Text(format!(
                "unknown tool: {name}"
            ))],
            is_error: true,
        }
    }
}

impl Probes for DingtalkBridge {
    fn points() -> Vec<String> {
        vec!["session_start".into(), "after_response".into()]
    }

    fn probe(point: String, payload_json: String) -> Verdict {
        let mut adapter = ADAPTER.lock().unwrap_or_else(|e| e.into_inner());
        match point.as_str() {
            "session_start" => {
                // Open the ws long connection to the platform. The
                // gateway handshake (endpoint+ticket exchange) only
                // OBTAINS this URL — the loopback connects directly.
                let url = std::env::var("TAU_MCP_URL").unwrap_or_default();
                match ws::connect(&url, NET_MS) {
                    Ok(handle) => adapter.ws = Some(handle),
                    Err(e) => notify("error", format!("dingtalk: ws connect failed: {e}")),
                }
            }
            "after_response" => {
                // Drain first (an inbound frame may be waiting), then
                // reply if a steered message is outstanding — the pump
                // order that makes the loopback close within one turn.
                pump_inbound(&mut adapter);
                post_reply(&mut adapter, &payload_json);
            }
            _ => {}
        }
        Verdict {
            action: Action::Continue,
            payload_json: None,
            reason: None,
        }
    }
}

/// No webhook leg (钉钉 stream is a client connection, not a callback)
/// — the contract makes the export mandatory, so it stubs 501.
impl IngressHandler for DingtalkBridge {
    fn handle_request(_request: Request) -> Response {
        Response {
            status: 501,
            headers: Vec::new(),
            body: b"this bridge has no webhook leg (ws stream mode)".to_vec(),
        }
    }
}

export!(DingtalkBridge);
