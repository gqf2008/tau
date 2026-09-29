//! Example tau bridge: a whatsapp-shaped IM adapter over the `ingress`
//! capability (docs/im-channels.md) — the webhook-platform leg, dual of
//! feishu-bridge's ws long connection:
//!
//! - inbound: `session_start` calls `ingress.listen("/im/whatsapp")`
//!   (consent: --ingress <addr:port>); the platform POSTs each message
//!   event to that route and the host pushes it into
//!   `ingress-handler.handle-request` — no pump, no idle window: the
//!   export steers the message into the session (consent:
//!   --allow-inject) and its return value is the webhook's HTTP response
//!   (the platform's ack).
//! - outbound: `after_response` posts the assembled assistant message to
//!   the platform's send API over the origin-allowlisted `http`
//!   capability (TAU_MCP_URL + "/send" — WhatsApp's Graph API messages
//!   endpoint stands behind that shape).
//!
//! 0.7.0 shape: the listener is a `registration` resource — dropping it
//! stops serving the route, so the adapter HOLDS it (the state cell is a
//! `thread_local` for exactly that reason: a resource is not `Sync`) —
//! and the reply POST is an awaited `http.request`, which is why it lives
//! in `bridge-io.turn` (a synchronously lowered export cannot await;
//! docs/wit-redesign.md section 5). Inbound needs neither: `listen` is
//! synchronous and `handle-request` is its own async export.
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

use std::cell::RefCell;

use exports::tau::extension::bridge_io::Guest as BridgeIo;
use exports::tau::extension::ingress_handler::{Guest as IngressHandler, Request, Response};
use exports::tau::extension::probes::{Guest as Probes, Payload, Point, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::host::{self, Level};
use tau::extension::types::{Content, Error as HostError, Message, Role};
use tau::extension::{http, ingress};

/// The route this adapter serves (WhatsApp Cloud API's callback path
/// shape). Fixed: the route is part of the adapter's protocol
/// translation, not user configuration.
const ROUTE: &str = "/im/whatsapp";

/// Adapter state. A `thread_local` cell because the registration is a
/// resource (not `Sync`) and the guest is single-threaded — the host
/// serializes every export call (probes, tools, turn, webhooks) on this
/// instance, so there is nothing to lock against.
struct Adapter {
    /// The registered route. Held for the component's life: dropping the
    /// registration stops serving the route.
    registration: Option<ingress::Registration>,
    /// The chat an inbound message came from (echoed in the reply).
    chat_id: Option<String>,
    /// A steered IM message is waiting for its assistant reply.
    awaiting_reply: bool,
}

thread_local! {
    static ADAPTER: RefCell<Adapter> = RefCell::new(Adapter {
        registration: None,
        chat_id: None,
        awaiting_reply: false,
    });
}

struct WhatsAppBridge;

/// One user-visible line (renderer draws it; never enters model history).
fn say(level: Level, text: String) {
    let _ = host::notify(level, &[Content::Text(text)]);
}

/// A host error as one notice line: the typed kind picks the wording, the
/// detail is the host's own sentence handed through verbatim (the contract
/// says never to match on it, so this never does).
fn host_error(verb: &str, error: HostError) -> String {
    match error {
        HostError::Refused(detail) => format!("{verb} refused: {detail}"),
        HostError::Failed(detail) => format!("{verb} failed: {detail}"),
        HostError::Invalid(detail) => format!("{verb} invalid: {detail}"),
    }
}

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

impl Tools for WhatsAppBridge {
    async fn definitions() -> Vec<Definition> {
        // An IM adapter contributes no tools; the conversation IS the
        // interface.
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

impl Probes for WhatsAppBridge {
    fn points() -> Vec<Point> {
        vec![Point::SessionStart, Point::AfterResponse]
    }

    /// The one thing this adapter does at a point that does not wait:
    /// registering its route (`listen` is synchronous, and the
    /// registration must be held). The outbound POST waits, so it rides
    /// `turn` below.
    fn probe(point: Point, _payload: Payload) -> Verdict {
        if point == Point::SessionStart {
            // Failure is a notice, not a load error — without --ingress
            // this bridge simply has no inbound leg (the notice names the
            // missing consent).
            match ingress::listen(ROUTE) {
                Ok(registration) => {
                    ADAPTER.with(|cell| cell.borrow_mut().registration = Some(registration))
                }
                Err(error) => say(Level::Error, host_error("wa: ingress", error)),
            }
        }
        Verdict::Continue
    }
}

impl BridgeIo for WhatsAppBridge {
    async fn turn(point: Point, payload: Payload) {
        if point == Point::AfterResponse {
            post_reply(&payload).await;
        }
    }
}

/// Post the assembled assistant message back to the platform's send API
/// (outbound is the http capability, origin-consented via --mcp-url). The
/// text is the typed payload's first text block — 0.6.0 scraped the JSON
/// payload for one.
async fn post_reply(payload: &Payload) {
    let pending = ADAPTER.with(|cell| {
        let adapter = cell.borrow();
        adapter.awaiting_reply.then(|| adapter.chat_id.clone()).flatten()
    });
    let Some(chat_id) = pending else { return };
    let Some(base) = std::env::var("TAU_MCP_URL").ok() else { return };
    let text = match payload {
        Payload::AfterResponse(response) => {
            response.message.content.iter().find_map(|block| match block {
                Content::Text(text) => Some(text.clone()),
                _ => None,
            })
        }
        _ => None,
    };
    let Some(text) = text else { return };
    let url = format!("{base}/send");
    // The body is built as JSON rather than interpolated: assistant text
    // routinely contains quotes and newlines.
    let body = serde_json::json!({ "chat_id": chat_id, "text": text }).to_string();
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    match http::request("POST".to_string(), url, headers, body.into_bytes()).await {
        Ok(response) => {
            let _ = response.status();
            drop(response); // dropping closes the connection
            ADAPTER.with(|cell| cell.borrow_mut().awaiting_reply = false);
            say(Level::Info, format!("wa: reply posted to {chat_id}"));
        }
        Err(error) => say(Level::Error, host_error("wa: reply POST", error)),
    }
}

/// The webhook itself: the platform POSTs a message event, the host
/// pushes it here, the return value is the HTTP response.
impl IngressHandler for WhatsAppBridge {
    async fn handle_request(request: Request) -> Response {
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
                ADAPTER.with(|cell| {
                    let mut adapter = cell.borrow_mut();
                    adapter.chat_id = Some(chat_id.to_string());
                    adapter.awaiting_reply = true;
                });
                say(
                    Level::Info,
                    format!("wa: inbound message from {user} steered into the session (chat {chat_id})"),
                );
                ok("{\"ok\":true}")
            }
            // The ack is honest: 200 would tell the platform the message
            // landed when it did not (no inject consent) — 403 names the
            // refusal and the platform's retry is correct behavior.
            Err(error) => {
                say(Level::Error, host_error("wa: steer", error));
                bad(403, "session injection not consented")
            }
        }
    }
}

export!(WhatsAppBridge);
