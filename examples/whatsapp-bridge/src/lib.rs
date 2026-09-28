//! Example tau bridge: a whatsapp-shaped IM adapter over the `ingress`
//! capability (docs/im-channels.md) — the webhook-platform leg, dual of
//! feishu-bridge's ws long connection:
//!
//! - inbound: `session_start` calls `ingress.listen("/im/whatsapp")`
//!   (consent: --ingress <addr:port>); the platform POSTs each message
//!   event to that route and the host pushes it into
//!   `ingress-handler.handle-request` SYNCHRONOUSLY — no pump, no idle
//!   window: the export steers the message into the session
//!   (consent: --allow-inject) and its return value is the webhook's
//!   HTTP response (the platform's ack).
//! - outbound: `after_response` posts the assembled assistant message to
//!   the platform's send API over the origin-allowlisted `http`
//!   capability (TAU_MCP_URL + "/send" — WhatsApp's Graph API messages
//!   endpoint stands behind that shape).
//!
//! Speaks the loopback protocol from docs/im-channels.md (JSON bodies
//! standing in for WhatsApp's Cloud API shapes; scripts/wa_mock.py is
//! the mock platform). Identity mapping (users.allow, chat→session) is
//! feishu-bridge's demonstration — this example keeps the webhook leg
//! minimal and says so instead of half-doing identity.
//!
//! Build:
//!   cargo build --manifest-path examples/whatsapp-bridge/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use (loopback mock):
//!   tau --allow-unsigned --mcp-bridge .../whatsapp_bridge.wasm \
//!       --mcp-url http://127.0.0.1:API_PORT \
//!       --ingress 127.0.0.1:HOOK_PORT --allow-inject --demo -p "hi"

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
use tau::extension::{host, http, ingress};

/// The route this adapter serves (WhatsApp Cloud API's callback path
/// shape). Fixed: the route is part of the adapter's protocol
/// translation, not user configuration.
const ROUTE: &str = "/im/whatsapp";

/// Budget for one network wait: a reply POST's response headers, or a ws
/// connect's handshake. The platform answers in seconds, so this is
/// generous already — it exists so a peer that accepts the connection and
/// then says nothing fails loudly instead of hanging the bridge
/// (wit-review F11).
const NET_MS: u32 = 30_000;

struct Adapter {
    /// The chat an inbound message came from (echoed in the reply).
    chat_id: Option<String>,
    /// A steered IM message is waiting for its assistant reply.
    awaiting_reply: bool,
}

static ADAPTER: Mutex<Adapter> = Mutex::new(Adapter {
    chat_id: None,
    awaiting_reply: false,
});

struct WhatsAppBridge;

/// Flat-JSON string extraction, same contract as the other examples
/// (escapes and nesting out of scope; the loopback mock owns the shape).
/// Whitespace after the colon is legal JSON — skip it.
fn json_get<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let start = json.find(&pat)? + pat.len();
    let rest = json[start..]
        .trim_start_matches(|c: char| c.is_whitespace())
        .strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn notify(level: &str, text: String) {
    let _ = host::notify(level, &[Content::Text(text)]);
}

impl Tools for WhatsAppBridge {
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

impl Probes for WhatsAppBridge {
    fn points() -> Vec<String> {
        vec!["session_start".into(), "after_response".into()]
    }

    fn probe(point: String, payload_json: String) -> Verdict {
        let continue_ = || Verdict {
            action: Action::Continue,
            payload_json: None,
            reason: None,
        };
        match point.as_str() {
            "session_start" => {
                // Open the webhook listener. Failure is a notice, not a
                // load error — without --ingress this bridge simply has
                // no inbound leg (the notice names the missing consent).
                if let Err(e) = ingress::listen(ROUTE) {
                    notify("error", format!("wa: {e}"));
                }
            }
            "after_response" => {
                // Post the assembled assistant message back to the
                // platform's send API (outbound is the http capability,
                // origin-consented via --mcp-url).
                let mut adapter = ADAPTER.lock().unwrap_or_else(|e| e.into_inner());
                if !adapter.awaiting_reply {
                    return continue_();
                }
                let (Some(chat_id), Some(base)) = (
                    adapter.chat_id.clone(),
                    std::env::var("TAU_MCP_URL").ok(),
                ) else {
                    return continue_();
                };
                // The assistant text block: the message's first "text"
                // member (Content's wire shape serializes the field
                // before the "type" tag — never assume key order).
                let Some(text) = json_get(&payload_json, "text") else {
                    return continue_();
                };
                let url = format!("{base}/send");
                let body = format!("{{\"chat_id\":\"{chat_id}\",\"text\":\"{text}\"}}");
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
                        notify("info", format!("wa: reply posted to {chat_id}"));
                    }
                    Err(e) => notify("error", format!("wa: reply POST failed: {e}")),
                }
            }
            _ => {}
        }
        continue_()
    }
}

/// The webhook itself: the platform POSTs a message event, the host
/// pushes it here synchronously, the return value is the HTTP response.
impl IngressHandler for WhatsAppBridge {
    fn handle_request(request: Request) -> Response {
        let ok = |body: &str| Response {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: body.as_bytes().to_vec(),
        };
        let bad = |status: u16, body: &str| Response {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        };
        if request.method != "POST" {
            return bad(405, "webhooks are POSTs");
        }
        let Ok(body) = String::from_utf8(request.body) else {
            return bad(400, "body is not UTF-8");
        };
        if json_get(&body, "type") != Some("message") {
            // Status verifications, delivery receipts, …: acknowledged,
            // not steered (the loopback protocol has one event kind;
            // real adapters fan out here).
            return ok("{\"ignored\":true}");
        }
        let (Some(chat_id), Some(user), Some(text)) = (
            json_get(&body, "chat_id"),
            json_get(&body, "user"),
            json_get(&body, "text"),
        ) else {
            return bad(400, "message event missing chat_id/user/text");
        };
        let message = Message {
            role: Role::User,
            content: vec![Content::Text(format!(
                "[IM chat {chat_id} from {user}] {text}"
            ))],
        };
        match host::steer(&message) {
            Ok(()) => {
                let mut adapter = ADAPTER.lock().unwrap_or_else(|e| e.into_inner());
                adapter.chat_id = Some(chat_id.to_string());
                adapter.awaiting_reply = true;
                notify(
                    "info",
                    format!("wa: inbound message from {user} steered into the session (chat {chat_id})"),
                );
                ok("{\"ok\":true}")
            }
            // The ack is honest: 200 would tell the platform the message
            // landed when it did not (no inject consent) — 403 names the
            // refusal and the platform's retry is correct behavior.
            Err(e) => {
                notify("error", format!("wa: steer refused: {e}"));
                bad(403, "session injection not consented")
            }
        }
    }
}

export!(WhatsAppBridge);
