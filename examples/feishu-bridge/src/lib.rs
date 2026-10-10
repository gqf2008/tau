//! Example tau bridge: a feishu-shaped IM adapter (docs/im-channels.md),
//! exercising all three legs of the bridge world:
//!
//! - inbound: `session_start` opens the ws long connection (TAU_MCP_URL,
//!   a ws(s) URL); later calls drain the frames the platform sent and
//!   `host::steer` the IM message into the session (no call-time gate
//!   since 0.8.0: the install record authorizes steering). Components
//!   only run when called — the pump rides existing call points, and the
//!   host's ws actor keeps the connection alive between them.
//! - outbound: `after_response` posts the assembled assistant message
//!   back over the `http` capability (the reply API, derived from the ws
//!   URL: same authority, `/reply`) — no origin allowlist since 0.8.0:
//!   the request goes where the component says.
//! - session/identity mapping: the channel config file
//!   (docs/im-channels.md 会话/身份映射配置文件格式) read from
//!   TAU_IM_CONFIG via ambient WASI — endpoint cross-checked against
//!   TAU_MCP_URL, unknown chats ignored, users.allow is fail-closed
//!   (identity is the component's own allowlist decision).
//!
//! 0.7.0 shape, and the shape this adapter is made of is now split in two
//! on purpose: both of its obligations WAIT (`ws.connect` and
//! `http.request` are `async func`s since 0.7.0), and a synchronously
//! lowered export cannot await them — nor appoint a task to do it (a task
//! spawned from a sync export is never polled; docs/wit-redesign.md
//! section 5). So the waiting half lives in `bridge-io.turn`, the host
//! calls it right after `probe` for the same point, and the sync probe
//! stays what the contract says it is: the decision point. This adapter
//! makes no decisions at either point, so its probe is `continue` — the
//! points are declared to make the host call `turn`.
//!
//! The pump itself never waits: `ws.connection.poll` drains what has
//! arrived and returns, which is what a pump must do (an awaited frame
//! read would hold the run open on the platform's silence).
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
//!       --mcp-url ws://127.0.0.1:PORT/im --demo -p "hi"

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

/// The channel's slice of the mapping config (the channel whose
/// `endpoint` equals TAU_MCP_URL governs this run).
struct ChannelConfig {
    /// chat_id → session file path (echoed in notices; a single-session
    /// run never switches sessions — the path serves supervisors and
    /// restart recovery).
    chats: std::collections::HashMap<String, String>,
    /// Identity allowlist — fail-closed: absent/empty admits nobody.
    users_allow: Vec<String>,
}

/// Adapter state. A `thread_local` cell since 0.7.0 rather than the 0.6.0
/// `Mutex` in a static: the connection is a resource now (not `Sync`), and
/// the guest is single-threaded anyway — the host serializes every export
/// call on this instance, so the lock would only ever be uncontended.
struct Adapter {
    /// Open connection to the platform (None until session_start).
    connection: Option<ws::Connection>,
    /// The chat an inbound message came from (echoed in the reply).
    chat_id: Option<String>,
    /// A steered IM message is waiting for its assistant reply.
    awaiting_reply: bool,
    /// The drain already consumed its one demo message.
    steered: bool,
    /// Channel config: None = not loaded yet; Some governs; a failed
    /// load is fail-closed (config_error set, nothing is ever steered).
    config: Option<ChannelConfig>,
    config_error: bool,
}

thread_local! {
    static ADAPTER: RefCell<Adapter> = RefCell::new(Adapter {
        connection: None,
        chat_id: None,
        awaiting_reply: false,
        steered: false,
        config: None,
        config_error: false,
    });
}

struct FeishuBridge;

/// One user-visible line (renderer draws it; never enters model history).
fn say(level: Level, text: String) {
    let _ = host::notify(level, &[Content::Text(text)]);
}

/// A host error as one notice line. The typed variant is what a guest
/// branches on (never the string); the detail is the host's own sentence,
/// handed through verbatim.
fn host_error(verb: &str, error: HostError) -> String {
    match error {
        HostError::Failed(detail) => format!("{verb} failed: {detail}"),
        HostError::Invalid(detail) => format!("{verb} invalid: {detail}"),
    }
}

/// Flat-JSON string extraction, same contract as the other examples
/// (escapes and nesting out of scope; the loopback mock owns the shape).
fn json_get<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    // Whitespace after the colon is legal JSON (python's json.dumps
    // defaults to it); skip it rather than demand compact encoding.
    let pat = format!("\"{key}\":");
    let start = json.find(&pat)? + pat.len();
    let rest = json[start..]
        .trim_start_matches(|c: char| c.is_whitespace())
        .strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// ws(s) URL → the reply API URL: same authority (the reply API sits
/// next to the ws endpoint), path replaced with /reply.
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

/// Load the channel config (TAU_IM_CONFIG → JSON file, ambient WASI
/// fs). Every failure is a notice + fail-closed: no config, no steering.
fn load_config(adapter: &mut Adapter) {
    if adapter.config.is_some() || adapter.config_error {
        return;
    }
    let fail = |adapter: &mut Adapter, reason: String| {
        adapter.config_error = true;
        let _ = host::notify(
            Level::Error,
            &[Content::Text(format!("feishu: config: {reason}"))],
        );
    };
    let Ok(path) = std::env::var("TAU_IM_CONFIG") else {
        return fail(
            adapter,
            "TAU_IM_CONFIG not set — identity is fail-closed".into(),
        );
    };
    // The host preopens each drive as /<letter> (Windows) / `/`
    // (elsewhere), but the env var arrives in the host's own spelling
    // ("C:/..." — MSYS converts it on the way in). Translate a
    // drive-absolute path into the preopen namespace.
    let path = {
        let path = path.replace('\\', "/");
        let bytes = path.as_bytes();
        if bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            let letter = (bytes[0] as char).to_ascii_lowercase();
            format!("/{}/{}", letter, path[2..].trim_start_matches('/'))
        } else {
            path
        }
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return fail(adapter, format!("cannot read {path}: {e}")),
    };
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return fail(adapter, format!("{path} is not valid JSON: {e}")),
    };
    if parsed["version"] != 1 {
        return fail(
            adapter,
            format!("unsupported config version: {}", parsed["version"]),
        );
    }
    let endpoint = std::env::var("TAU_MCP_URL").unwrap_or_default();
    let channels = parsed["channels"].as_array().cloned().unwrap_or_default();
    // The channel whose endpoint equals TAU_MCP_URL governs — the run's
    // endpoint is host config (--mcp-url), never a value the config file
    // gets to pick.
    let channel = channels
        .iter()
        .find(|c| c["endpoint"].as_str() == Some(endpoint.as_str()));
    let Some(channel) = channel else {
        return fail(
            adapter,
            format!("no channel matches the configured endpoint {endpoint}"),
        );
    };
    let mut chats = std::collections::HashMap::new();
    if let Some(map) = channel["chats"].as_object() {
        for (chat_id, spec) in map {
            chats.insert(
                chat_id.clone(),
                spec["session"].as_str().unwrap_or_default().to_string(),
            );
        }
    }
    let users_allow = channel["users"]["allow"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    adapter.config = Some(ChannelConfig { chats, users_allow });
}

/// Open the platform's long connection, once. Failure is a notice, not a
/// load error — the platform may simply be down.
async fn connect() {
    if ADAPTER.with(|cell| cell.borrow().connection.is_some()) {
        return;
    }
    let Ok(url) = std::env::var("TAU_MCP_URL") else {
        return;
    };
    match ws::Connection::connect(url).await {
        Ok(connection) => ADAPTER.with(|cell| cell.borrow_mut().connection = Some(connection)),
        Err(error) => say(Level::Error, host_error("feishu: ws connect", error)),
    }
}

/// Drain what the platform sent since the last call point and steer the
/// first IM message in the batch. Never waits: `poll` is the
/// drain-without-waiting shape (an awaited frame read would hold the run
/// open until the platform spoke or the host closed the connection).
fn pump_inbound() {
    if ADAPTER.with(|cell| cell.borrow().steered) {
        return;
    }
    ADAPTER.with(|cell| load_config(&mut cell.borrow_mut()));
    let drained = ADAPTER.with(|cell| cell.borrow().connection.as_ref().map(ws::Connection::poll));
    let frames = match drained {
        // An error here is the connection's end (reported once by the
        // actor and repeated by `poll` until the resource is dropped), or
        // the contract's `invalid` if something else already owns the
        // frames — this adapter never calls `receive`, so that cannot be it.
        Some(Err(error)) => return say(Level::Warn, host_error("feishu: ws poll", error)),
        Some(Ok(frames)) => frames,
        None => return,
    };
    for frame in frames {
        if handle_frame(frame) {
            return;
        }
    }
}

/// Handle one drained frame. Returns true when it was a message-typed
/// frame this pump is done with (steered, noted and dropped, or refused) —
/// a platform that sends anything else (a pong the actor already answered,
/// a binary frame, an unknown event) simply gets drained past.
fn handle_frame(frame: ws::Frame) -> bool {
    let ws::Frame::Text(text) = frame else {
        return false;
    };
    if json_get(&text, "type") != Some("message") {
        return false;
    }
    let (Some(chat_id), Some(user), Some(body)) = (
        json_get(&text, "chat_id"),
        json_get(&text, "user"),
        json_get(&text, "text"),
    ) else {
        return false;
    };
    // Identity + mapping gate (fail-closed, docs/im-channels.md): the
    // chat must be configured and the user allowlisted; anything else is
    // consumed, noted, and never steered. The notice is built inside the
    // borrow and printed outside it (the host import must not run with the
    // adapter borrowed — `say` is a call back into the host).
    let notice = ADAPTER.with(|cell| {
        let mut adapter = cell.borrow_mut();
        let (configured, allowed, session) = match adapter.config.as_ref() {
            Some(config) => (
                config.chats.contains_key(chat_id),
                config.users_allow.iter().any(|u| u == user),
                config.chats.get(chat_id).cloned(),
            ),
            None => (false, false, None),
        };
        let Some(session) = session.filter(|_| allowed) else {
            adapter.steered = true; // consumed — do not re-litigate
            return (
                Level::Info,
                format!(
                    "feishu: ignored message (chat {chat_id} configured: {configured}, user {user} allowed: {allowed})"
                ),
            );
        };
        let message = Message {
            role: Role::User,
            content: vec![Content::Text(format!("[IM chat {chat_id} from {user}] {body}"))],
        };
        match host::steer(&message) {
            Ok(()) => {
                adapter.chat_id = Some(chat_id.to_string());
                adapter.awaiting_reply = true;
                adapter.steered = true;
                (
                    Level::Info,
                    format!(
                        "feishu: inbound message from {user} steered into the session (chat {chat_id} → {session})"
                    ),
                )
            }
            // A failed steer leaves `awaiting_reply` false, so no reply
            // leaves for a message that never entered the session.
            Err(error) => (Level::Error, host_error("feishu: steer", error)),
        }
    });
    let (level, text) = notice;
    say(level, text);
    true
}

/// Post the assembled assistant message back to the platform's reply API.
/// The text is the typed payload's first text block — 0.6.0 scraped the
/// JSON payload for one; `after-response` now carries the message itself.
async fn post_reply(payload: &Payload) {
    let pending = ADAPTER.with(|cell| {
        let adapter = cell.borrow();
        adapter
            .awaiting_reply
            .then(|| adapter.chat_id.clone())
            .flatten()
    });
    let Some(chat_id) = pending else { return };
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
    let Some(url) = std::env::var("TAU_MCP_URL")
        .ok()
        .and_then(|url| reply_url(&url))
    else {
        return;
    };
    // The body is built as JSON rather than interpolated: assistant text
    // routinely contains quotes and newlines.
    let body = serde_json::json!({ "chat_id": chat_id, "text": text }).to_string();
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    match http::request("POST".to_string(), url, headers, body.into_bytes()).await {
        Ok(response) => {
            // The host returns once the response headers are in, which is
            // all this POST needs to have landed; dropping the response
            // closes the connection.
            let _ = response.status();
            drop(response);
            ADAPTER.with(|cell| cell.borrow_mut().awaiting_reply = false);
            say(Level::Info, format!("feishu: reply posted to {chat_id}"));
        }
        Err(error) => say(Level::Error, host_error("feishu: reply POST", error)),
    }
}

impl Tools for FeishuBridge {
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

impl Probes for FeishuBridge {
    fn points() -> Vec<Point> {
        vec![Point::SessionStart, Point::AfterResponse]
    }

    /// The sync half of the two points this adapter declares. Neither is a
    /// decision — everything this adapter does at them is I/O that waits —
    /// so the probe answers `continue` and the work rides `turn` below.
    /// Declaring the points is what makes the host call it.
    fn probe(_point: Point, _payload: Payload) -> Verdict {
        Verdict::Continue
    }
}

impl BridgeIo for FeishuBridge {
    async fn turn(point: Point, payload: Payload) {
        match point {
            Point::SessionStart => connect().await,
            Point::AfterResponse => {
                // Reply first: the response to a steered IM message is the
                // turn AFTER the injection, so this call's payload answers
                // the previous injection. Pumping after the post also
                // keeps the reply from being sent to a chat the pump has
                // just switched the adapter to.
                post_reply(&payload).await;
                pump_inbound();
            }
            _ => {}
        }
    }
}

// The bridge world makes ingress-handler a mandatory export. This adapter
// has no webhook leg (it never calls ingress.listen), so nothing can
// invoke this — the stub is explicit, not dead weight.
impl IngressHandler for FeishuBridge {
    async fn handle_request(_request: Request) -> Response {
        Response {
            status: 501,
            headers: Vec::new(),
            body: b"this bridge has no webhook leg".to_vec(),
        }
    }
}

export!(FeishuBridge);
