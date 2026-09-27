//! Example tau bridge: a feishu-shaped IM adapter (docs/im-channels.md),
//! exercising all three legs of the bridge world since 0.3.0:
//!
//! - inbound: `session_start` opens the ws long connection (TAU_MCP_URL,
//!   a ws(s) URL); later probe calls drain frames and `host::steer` the
//!   IM message into the session (consent: --allow-inject). Components
//!   only run when called — the pump rides existing probe call points,
//!   the host's ws actor keeps the connection alive between them.
//! - outbound: `after_response` posts the assembled assistant message
//!   back over the origin-allowlisted `http` capability (the reply API,
//!   derived from the ws URL: same origin, `/reply`).
//! - session mapping: minimal in-memory chat_id tracking (the config
//!   file format is a separate backlog item).
//!
//! Speaks the loopback protocol from docs/im-channels.md (JSON frames
//! standing in for feishu's proprietary binary frames — the translation
//! layer's shape is the same; scripts/im_mock.py is the mock platform).
//!
//! Build:
//!   cargo build --manifest-path examples/feishu-bridge/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use (loopback mock on :PORT):
//!   tau --allow-unsigned --mcp-bridge .../feishu_bridge.wasm \
//!       --mcp-url ws://127.0.0.1:PORT/im --allow-inject --demo -p "hi"

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "bridge",
});

use std::sync::Mutex;

use exports::tau::extension::probes::{Action, Guest as Probes, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::types::{Content, Message, Role};
use tau::extension::{host, http, ws};

/// Adapter state. Lives in statics because probe calls are plain
/// function calls — there is no per-instance object on the guest side.
struct Adapter {
    /// Open ws handle to the platform (None until session_start).
    ws: Option<u64>,
    /// The chat an inbound message came from (echoed in the reply).
    chat_id: Option<String>,
    /// A steered IM message is waiting for its assistant reply.
    awaiting_reply: bool,
    /// The ws drain already injected once (loopback demo: one message).
    steered: bool,
}

static ADAPTER: Mutex<Adapter> = Mutex::new(Adapter {
    ws: None,
    chat_id: None,
    awaiting_reply: false,
    steered: false,
});

struct FeishuBridge;

/// Flat-JSON string extraction, same contract as the other examples
/// (escapes and nesting out of scope; the loopback mock owns the shape).
fn json_get<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    // Whitespace after the colon is legal JSON (python's json.dumps
    // defaults to it); skip it rather than demand compact encoding.
    let pat = format!("\"{key}\":");
    let start = json.find(&pat)? + pat.len();
    let rest = json[start..].trim_start_matches(|c: char| c.is_whitespace()).strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// ws(s) URL → the reply API URL: same origin (so the ws consent origin
/// covers it), path replaced with /reply.
fn reply_url(ws_url: &str) -> Option<String> {
    let (scheme, rest) = ws_url.split_once("://")?;
    let httpish = match scheme {
        "ws" => "http",
        "wss" => "https",
        _ => return None,
    };
    let authority = rest.split('/').next()?;
    Some(format!("{httpish}://{authority}/reply"))
}

/// Drain one inbound frame; on an IM message event, steer it into the
/// session. Runs at probe call points — the synchronous guest model
/// means the pump only moves when the host calls us (docs/im-channels.md
/// 同步 guest 模型下的入站泵).
fn pump_inbound(adapter: &mut Adapter) {
    if adapter.steered {
        return;
    }
    let Some(handle) = adapter.ws else { return };
    // One short-timeout recv per call point: no frame is normal (the
    // platform is just quiet), a frame is drained and injected.
    let frame = match ws::recv(handle, 1000) {
        Ok(frame) => frame,
        Err(_) => return,
    };
    let ws::Frame::Text(text) = frame else { return };
    if json_get(&text, "type") != Some("message") {
        return;
    }
    let (Some(chat_id), Some(user), Some(body)) = (
        json_get(&text, "chat_id"),
        json_get(&text, "user"),
        json_get(&text, "text"),
    ) else {
        return;
    };
    let message = Message {
        role: Role::User,
        content: vec![Content::Text(format!(
            "[IM chat {chat_id} from {user}] {body}"
        ))],
    };
    match host::steer(&message) {
        Ok(()) => {
            adapter.chat_id = Some(chat_id.to_string());
            adapter.awaiting_reply = true;
            adapter.steered = true;
            let _ = host::notify(
                "info",
                &[Content::Text(format!(
                    "feishu: inbound message from {user} steered into the session"
                ))],
            );
        }
        Err(e) => {
            let _ = host::notify(
                "error",
                &[Content::Text(format!("feishu: steer refused: {e}"))],
            );
        }
    }
}

/// Post the assembled assistant message back to the platform's reply API.
fn post_reply(adapter: &mut Adapter, payload_json: &str) {
    if !adapter.awaiting_reply {
        return;
    }
    let (Some(chat_id), Some(handle_ws_url)) = (
        adapter.chat_id.clone(),
        std::env::var("TAU_MCP_URL").ok(),
    ) else {
        return;
    };
    // The assistant text block: the message's first "text" member (the
    // Content wire shape serializes the field before the "type" tag).
    let Some(text) = json_get(payload_json, "text") else {
        return;
    };
    let Some(url) = reply_url(&handle_ws_url) else { return };
    let body = format!("{{\"chat_id\":\"{chat_id}\",\"text\":\"{text}\"}}");
    let result = http::request(
        "POST",
        &url,
        &[("content-type".to_string(), "application/json".to_string())],
        &body.into_bytes(),
    );
    match result {
        Ok(h) => {
            let _ = http::status(h);
            http::close(h);
            adapter.awaiting_reply = false;
            let _ = host::notify(
                "info",
                &[Content::Text(format!("feishu: reply posted to {chat_id}"))],
            );
        }
        Err(e) => {
            let _ = host::notify(
                "error",
                &[Content::Text(format!("feishu: reply POST failed: {e}"))],
            );
        }
    }
}

impl Tools for FeishuBridge {
    fn definitions() -> Vec<Definition> {
        // An IM adapter contributes no tools; the conversation IS the
        // interface.
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

impl Probes for FeishuBridge {
    fn points() -> Vec<String> {
        vec!["session_start".into(), "after_response".into()]
    }

    fn probe(point: String, payload_json: String) -> Verdict {
        let continue_ = || Verdict {
            action: Action::Continue,
            payload_json: None,
            reason: None,
        };
        let mut adapter = ADAPTER.lock().unwrap_or_else(|e| e.into_inner());
        match point.as_str() {
            "session_start" => {
                // Open the long connection. Failure is a notice, not a
                // load error — the platform may simply be down.
                if adapter.ws.is_none()
                    && let Ok(url) = std::env::var("TAU_MCP_URL")
                {
                    match ws::connect(&url) {
                        Ok(handle) => adapter.ws = Some(handle),
                        Err(e) => {
                            let _ = host::notify(
                                "error",
                                &[Content::Text(format!("feishu: ws connect failed: {e}"))],
                            );
                        }
                    }
                }
            }
            "after_response" => {
                // Reply first: the response to a steered IM message is
                // the turn AFTER the injection. Pumping after the post
                // means this call's payload answers the previous
                // injection, not the one this call just made.
                post_reply(&mut adapter, &payload_json);
                pump_inbound(&mut adapter);
            }
            _ => {}
        }
        continue_()
    }
}

export!(FeishuBridge);
