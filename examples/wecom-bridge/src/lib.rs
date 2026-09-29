//! Example tau bridge: a wecom-shaped (企业微信) IM adapter over the
//! `ingress` capability (docs/im-channels.md §企微回环协议) — the
//! webhook-platform leg WITH the cryptography the ingress design's red
//! line assigns to the component ("签名校验是组件的职责，宿主不代劳";
//! whatsapp-bridge leaves that honestly unimplemented, this one does it):
//!
//! - inbound: `session_start` reads the crypto material from the
//!   environment (`WECOM_TOKEN` / `WECOM_ENCODING_AES_KEY` /
//!   `WECOM_CORP_ID` — keys never enter config files or consent
//!   storage) and calls `ingress.listen("/wecom/callback")`. Every
//!   callback request carries `msg_signature`/`timestamp`/`nonce` in
//!   the QUERY string (which is why the ingress contract passes the
//!   raw query through — the host is a pipe):
//!   - GET (URL verification): verify sha1 signature, AES-decrypt
//!     `echostr`, check the corpid frame suffix, return the plaintext;
//!   - POST (message push): verify the signature over the XML
//!     envelope's `Encrypt` element, decrypt (AES-256-CBC, key =
//!     base64(EncodingAESKey+"="), IV = key[:16], frame = 16 random +
//!     u32be(len) + msg + corpid, PKCS#7 with wecom's 32-byte block),
//!     check corpid, steer the text into the session (consent:
//!     --allow-inject) and ack "success" — 403 when injection is not
//!     consented, same honest-ack rule as whatsapp-bridge.
//! - outbound: `after_response` posts the assembled assistant text to
//!   the send API over the origin-allowlisted `http` capability
//!   (`{TAU_MCP_URL}/cgi-bin/message/send?access_token=…` — wecom's
//!   async reply is plain JSON; the crypto only guards the inbound
//!   callback). The encrypted passive reply (5s window) is documented
//!   as not implemented: it would require ingress-handler to await a
//!   whole turn synchronously, conflicting with the push model's
//!   instance-lock semantics.
//!
//! 0.7.0 shape: the listener is a `registration` resource, so the adapter
//! HOLDS it (state lives in a `thread_local` cell because a resource is
//! not `Sync`), and the reply POST is an awaited `http.request` — a wait,
//! so it lives in `bridge-io.turn` (a synchronously lowered export cannot
//! await; docs/wit-redesign.md section 5). The crypto legs need neither:
//! `listen` is synchronous and `handle-request` is its own async export.
//!
//! Loopback only: scripts/wecom_mock.py is the platform (real crypto,
//! NIST-self-tested AES). Identity mapping stays feishu-bridge's
//! demonstration; this example's new ground is the crypto gate.
//!
//! Build:
//!   cargo build --manifest-path examples/wecom-bridge/Cargo.toml \
//!       --target wasm32-wasip2 --release
//! Use (loopback mock):
//!   WECOM_TOKEN=… WECOM_ENCODING_AES_KEY=… WECOM_CORP_ID=… \
//!   tau --allow-unsigned --mcp-bridge .../wecom_bridge.wasm \
//!       --mcp-url http://127.0.0.1:API_PORT \
//!       --ingress 127.0.0.1:HOOK_PORT --allow-inject --demo

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "bridge",
});

use std::cell::RefCell;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use cbc::cipher::block_padding::NoPadding;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use exports::tau::extension::bridge_io::Guest as BridgeIo;
use exports::tau::extension::ingress_handler::{Guest as IngressHandler, Request, Response};
use exports::tau::extension::probes::{Guest as Probes, Payload, Point, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use sha1::{Digest, Sha1};
use tau::extension::host::{self, Level};
use tau::extension::types::{Content, Error as HostError, Message, Role};
use tau::extension::{http, ingress};

/// The route this adapter serves (wecom calls it the callback URL).
/// Fixed: the route is part of the adapter's protocol translation,
/// not user configuration.
const ROUTE: &str = "/wecom/callback";

/// The callback crypto material, from the environment at session_start.
/// Keys never enter config files or consent storage (docs red line).
struct WecomCrypto {
    token: String,
    key: [u8; 32],
    corpid: String,
}

/// Adapter state. A `thread_local` cell since 0.7.0: the registration is
/// a resource (not `Sync`), and the guest is single-threaded — the host
/// serializes every export call (probes, tools, turn, webhooks) on this
/// instance, so there is nothing to lock against.
struct Adapter {
    /// The callback crypto material (None until session_start).
    crypto: Option<WecomCrypto>,
    /// The registered route. Held for the component's life: dropping the
    /// registration stops serving the route.
    registration: Option<ingress::Registration>,
    /// The user an inbound message came from (echoed in the reply).
    user: Option<String>,
    /// A steered IM message is waiting for its assistant reply.
    awaiting_reply: bool,
}

thread_local! {
    static ADAPTER: RefCell<Adapter> = RefCell::new(Adapter {
        crypto: None,
        registration: None,
        user: None,
        awaiting_reply: false,
    });
}

struct WecomBridge;

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

/// Lowercase hex, no dependency.
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// wecom msg_signature: sha1 hex of the four parts sorted then
/// concatenated.
fn msg_signature(token: &str, timestamp: &str, nonce: &str, encrypt_msg: &str) -> String {
    let mut parts = [token, timestamp, nonce, encrypt_msg];
    parts.sort_unstable();
    let mut hasher = Sha1::new();
    hasher.update(parts.concat().as_bytes());
    hex_lower(&hasher.finalize())
}

/// Percent-decode a query value (%XX only; the mock encodes with
/// quote_plus so a real '+' always arrives as %2B — a literal '+' is
/// left as-is, which is unambiguous under that encoding).
fn percent_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("truncated percent escape".into());
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .map_err(|_| "bad percent escape".to_string())?;
            let byte =
                u8::from_str_radix(hex, 16).map_err(|_| "bad percent escape".to_string())?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "query value is not UTF-8".to_string())
}

/// One query parameter, percent-decoded. The query arrives raw (the
/// host is a pipe); parsing is our semantics.
fn query_get(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            return percent_decode(value).ok();
        }
    }
    None
}

/// `<tag><![CDATA[v]]></tag>` or `<tag>v</tag>` — CDATA first (wecom
/// wraps everything in CDATA), plain text as the fallback.
fn xml_get(xml: &str, tag: &str) -> Option<String> {
    let cdata_open = format!("<{tag}><![CDATA[");
    if let Some(start) = xml.find(&cdata_open) {
        let rest = &xml[start + cdata_open.len()..];
        let end = rest.find("]]>")?;
        return Some(rest[..end].to_string());
    }
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let close = format!("</{tag}>");
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// Decrypt one wecom frame: base64 → AES-256-CBC (IV = key[:16]) →
/// strip PKCS#7 padding (wecom's block size is 32, so cbc's Pkcs7
/// unwrapper — which caps at 16 — is wrong here; strip manually) →
/// 16 random bytes + u32be(len) + msg + receiveid.
fn decrypt_frame(key: &[u8; 32], b64: &str) -> Result<(Vec<u8>, String), String> {
    let mut buf = B64
        .decode(b64)
        .map_err(|e| format!("ciphertext is not base64: {e}"))?;
    if buf.is_empty() || buf.len() % 16 != 0 {
        return Err("ciphertext is not block-aligned".into());
    }
    let plain = cbc::Decryptor::<aes::Aes256>::new(
        cbc::cipher::generic_array::GenericArray::from_slice(key),
        cbc::cipher::generic_array::GenericArray::from_slice(&key[..16]),
    )
    .decrypt_padded_mut::<NoPadding>(&mut buf)
    .map_err(|e| format!("decrypt failed: {e}"))?;
    let pad = *plain.last().ok_or("empty plaintext")? as usize;
    if pad == 0 || pad > 32 || pad > plain.len() {
        return Err("bad frame padding".into());
    }
    let frame = &plain[..plain.len() - pad];
    if frame.len() < 20 {
        return Err("frame too short".into());
    }
    let len = u32::from_be_bytes([frame[16], frame[17], frame[18], frame[19]]) as usize;
    if frame.len() < 20 + len {
        return Err("frame length overruns the plaintext".into());
    }
    let msg = frame[20..20 + len].to_vec();
    let receiveid = String::from_utf8_lossy(&frame[20 + len..]).to_string();
    Ok((msg, receiveid))
}

impl Tools for WecomBridge {
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

impl Probes for WecomBridge {
    fn points() -> Vec<Point> {
        vec![Point::SessionStart, Point::AfterResponse]
    }

    /// The one thing this adapter does at a point that does not wait:
    /// reading the crypto material and registering its route (`listen` is
    /// synchronous, and the registration must be held). The outbound POST
    /// waits, so it rides `turn` below.
    fn probe(point: Point, _payload: Payload) -> Verdict {
        if point != Point::SessionStart {
            return Verdict::Continue;
        }
        // Failure is a notice, not a load error — without --ingress (or
        // without the env) this bridge simply has no inbound leg.
        let crypto = match (
            std::env::var("WECOM_TOKEN"),
            std::env::var("WECOM_ENCODING_AES_KEY"),
            std::env::var("WECOM_CORP_ID"),
        ) {
            (Ok(token), Ok(aes_key), Ok(corpid)) => match B64.decode(format!("{aes_key}=")) {
                Ok(k) if k.len() == 32 => {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&k);
                    Ok(WecomCrypto { token, key, corpid })
                }
                _ => Err("wecom: WECOM_ENCODING_AES_KEY is not a 43-char base64 key"),
            },
            _ => Err(
                "wecom: WECOM_TOKEN/WECOM_ENCODING_AES_KEY/WECOM_CORP_ID env required; not listening",
            ),
        };
        let crypto = match crypto {
            Ok(crypto) => crypto,
            Err(reason) => {
                say(Level::Error, reason.into());
                return Verdict::Continue;
            }
        };
        ADAPTER.with(|cell| cell.borrow_mut().crypto = Some(crypto));
        match ingress::listen(ROUTE) {
            Ok(registration) => {
                ADAPTER.with(|cell| cell.borrow_mut().registration = Some(registration))
            }
            Err(error) => say(Level::Error, host_error("wecom: ingress", error)),
        }
        Verdict::Continue
    }
}

impl BridgeIo for WecomBridge {
    async fn turn(point: Point, payload: Payload) {
        if point == Point::AfterResponse {
            post_reply(&payload).await;
        }
    }
}

/// Post the assembled assistant text to the send API (plain JSON with
/// access_token — the crypto only guards the inbound callback). The text
/// is the typed payload's first text block — 0.6.0 scraped the JSON
/// payload for one.
async fn post_reply(payload: &Payload) {
    let pending = ADAPTER.with(|cell| {
        let adapter = cell.borrow();
        adapter.awaiting_reply.then(|| adapter.user.clone()).flatten()
    });
    let Some(user) = pending else { return };
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
    let url = format!("{base}/cgi-bin/message/send?access_token=loopback-access-token");
    // The body is built as JSON rather than interpolated: assistant text
    // routinely contains quotes and newlines.
    let body = serde_json::json!({
        "touser": user,
        "msgtype": "text",
        "agentid": "1000002",
        "text": { "content": text },
    })
    .to_string();
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    match http::request("POST".to_string(), url, headers, body.into_bytes()).await {
        Ok(response) => {
            let _ = response.status();
            drop(response); // dropping closes the connection
            ADAPTER.with(|cell| cell.borrow_mut().awaiting_reply = false);
            say(Level::Info, format!("wecom: reply posted to {user}"));
        }
        Err(error) => say(Level::Error, host_error("wecom: reply POST", error)),
    }
}

/// The webhook itself: every request is signature-verified BEFORE any
/// body trust — a bad msg_signature is a 403 and nothing is decrypted,
/// steered, or replied.
impl IngressHandler for WecomBridge {
    async fn handle_request(request: Request) -> Response {
        let ok = |body: &str| Response {
            status: 200,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: body.as_bytes().to_vec(),
        };
        let ok_bytes = |body: Vec<u8>| Response {
            status: 200,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body,
        };
        let bad = |status: u16, body: &str| Response {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        };
        // The crypto material is snapshotted out of the state cell rather
        // than borrowed: the handler makes host calls below, and no borrow
        // should span one.
        let crypto = ADAPTER.with(|cell| {
            let adapter = cell.borrow();
            adapter
                .crypto
                .as_ref()
                .map(|c| (c.token.clone(), c.key, c.corpid.clone()))
        });
        let Some((token, key, corpid)) = crypto else {
            return bad(503, "wecom crypto not configured (session_start never ran)");
        };
        let (Some(sig), Some(timestamp), Some(nonce)) = (
            query_get(&request.query, "msg_signature"),
            query_get(&request.query, "timestamp"),
            query_get(&request.query, "nonce"),
        ) else {
            return bad(400, "missing msg_signature/timestamp/nonce");
        };
        match request.method.as_str() {
            "GET" => {
                // URL verification: sign over the ENCRYPTED echostr,
                // decrypt, corpid-check, return the plaintext.
                let Some(echostr) = query_get(&request.query, "echostr") else {
                    return bad(400, "missing echostr");
                };
                if msg_signature(&token, &timestamp, &nonce, &echostr) != sig {
                    return bad(403, "invalid signature");
                }
                match decrypt_frame(&key, &echostr) {
                    Ok((msg, receiveid)) if receiveid == corpid => ok_bytes(msg),
                    Ok(_) => bad(403, "receiveid is not this corp"),
                    Err(e) => bad(400, &format!("echostr: {e}")),
                }
            }
            "POST" => {
                let Ok(body) = String::from_utf8(request.body) else {
                    return bad(400, "body is not UTF-8");
                };
                let Some(encrypt) = xml_get(&body, "Encrypt") else {
                    return bad(400, "envelope has no Encrypt element");
                };
                if msg_signature(&token, &timestamp, &nonce, &encrypt) != sig {
                    return bad(403, "invalid signature");
                }
                let (msg, receiveid) = match decrypt_frame(&key, &encrypt) {
                    Ok(pair) => pair,
                    Err(e) => return bad(400, &format!("message: {e}")),
                };
                if receiveid != corpid {
                    return bad(403, "receiveid is not this corp");
                }
                let Ok(inner) = String::from_utf8(msg) else {
                    return bad(400, "decrypted message is not UTF-8");
                };
                if xml_get(&inner, "MsgType").as_deref() != Some("text") {
                    // Events, images, …: acknowledged, not steered (the
                    // loopback protocol has text only; real adapters
                    // fan out here).
                    return ok("success");
                }
                let (Some(user), Some(text)) = (
                    xml_get(&inner, "FromUserName"),
                    xml_get(&inner, "Content"),
                ) else {
                    return bad(400, "text message missing FromUserName/Content");
                };
                let message = Message {
                    role: Role::User,
                    content: vec![Content::Text(format!("[IM wecom {user}] {text}"))],
                };
                match host::steer(&message) {
                    Ok(()) => {
                        ADAPTER.with(|cell| {
                            let mut adapter = cell.borrow_mut();
                            adapter.user = Some(user.clone());
                            adapter.awaiting_reply = true;
                        });
                        say(
                            Level::Info,
                            format!("wecom: inbound message from {user} steered into the session"),
                        );
                        ok("success")
                    }
                    // Honest ack, same rule as whatsapp-bridge: 200
                    // would tell the platform the message landed when
                    // it did not.
                    Err(error) => {
                        say(Level::Error, host_error("wecom: steer", error));
                        bad(403, "session injection not consented")
                    }
                }
            }
            _ => bad(405, "callback is GET (url verify) or POST (message)"),
        }
    }
}

export!(WecomBridge);
