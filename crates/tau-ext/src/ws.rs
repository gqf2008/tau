//! WebSocket frame-pipe capability for bridge components
//! (docs/im-channels.md — IM stream modes). The host is a frame pipe
//! only: it never parses payloads. Origin consent shares the `http`
//! allowlist (ws: matches http:, wss: matches https:), granted empty by
//! default — every call fails at call time without consent.
//!
//! Keepalive / idle semantics (wit-review F9, contract-commented in
//! `wit/tau.wit`): the actor thread pings every 30s; 60s without any
//! inbound frame (a pong counts) closes the connection and surfaces an
//! explicit error from `recv` — a recv that could block forever would
//! make a dead connection indistinguishable from a quiet one.
//! Reconnect and catch-up are the component's job.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Ping cadence and death verdict (see the module docs).
const PING_INTERVAL: Duration = Duration::from_secs(30);
const DEAD_AFTER: Duration = Duration::from_secs(60);
/// The socket's read timeout: how often the actor loop services outgoing
/// commands and keepalive between inbound frames.
const READ_TICK: Duration = Duration::from_millis(250);

/// The guest-visible frame (mirrors the WIT `ws.frame` variant).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WsFrame {
    Text(String),
    Binary(Vec<u8>),
}

enum WsCommand {
    Send(WsFrame),
    Close,
}

/// One open connection: an actor thread owns the socket; commands go in,
/// frames (or the terminal error) come out.
struct WsConnection {
    cmd: mpsc::Sender<WsCommand>,
    rx: mpsc::Receiver<Result<WsFrame, String>>,
}

/// Handle-issuing registry, generation-fenced like `ProcessRegistry`
/// (wit-review F8): a handle from before a trap rebuild errors instead
/// of aliasing a newer connection.
pub(crate) struct WsRegistry {
    generation: u32,
    next: u32,
    connections: HashMap<u64, WsConnection>,
    /// Consented origins in http(s) form (ws:→http:, wss:→https:).
    /// Empty = deny all.
    origins: HashSet<String>,
}

/// Consent origin of a ws(s) URL, in the http(s) form the allowlist
/// holds (ws:→http:, wss:→https:). Public so the CLI can build a
/// consent allowlist from ws URLs.
pub fn origin_of(url: &str) -> Option<String> {
    WsRegistry::origin_of(url)
}

impl WsRegistry {
    pub(crate) fn new(generation: u32, origins: HashSet<String>) -> Self {
        Self {
            generation,
            next: 0,
            connections: HashMap::new(),
            origins,
        }
    }

    /// Consent origin of a ws(s) URL, in the http(s) form the allowlist
    /// holds: ws:→http:, wss:→https:, then the same authority parsing as
    /// http (userinfo stripped, authority ends at the first delimiter —
    /// the check must see the same host the client will dial).
    pub fn origin_of(url: &str) -> Option<String> {
        let (scheme, rest) = url.split_once("://")?;
        let httpish = match scheme {
            "ws" => "http",
            "wss" => "https",
            _ => return None,
        };
        crate::http::HttpRegistry::origin_of(&format!("{httpish}://{rest}"))
    }

    pub(crate) fn connect(&mut self, url: &str) -> Result<u64, String> {
        let origin =
            Self::origin_of(url).ok_or_else(|| format!("ws.connect: not a ws(s) URL: {url:?}"))?;
        if !self.origins.contains(&origin) {
            return Err(format!(
                "ws.connect: origin {origin} not consented (bridge endpoints are consented \
                 like http origins — the host CLI passes them, e.g. --mcp-url)"
            ));
        }
        let (mut socket, _response) =
            tungstenite::connect(url).map_err(|e| format!("ws.connect {origin}: {e}"))?;
        set_read_tick(&mut socket);
        let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>();
        let (frame_tx, frame_rx) = mpsc::channel();
        std::thread::spawn(move || actor(socket, cmd_rx, frame_tx));
        let handle = ((self.generation as u64) << 32) | (self.next as u64);
        self.next += 1;
        self.connections.insert(
            handle,
            WsConnection {
                cmd: cmd_tx,
                rx: frame_rx,
            },
        );
        Ok(handle)
    }

    fn get(&self, handle: u64) -> Result<&WsConnection, String> {
        if (handle >> 32) as u32 != self.generation {
            return Err(format!(
                "stale ws handle {handle} (the instance was rebuilt; reconnect)"
            ));
        }
        self.connections
            .get(&handle)
            .ok_or_else(|| format!("unknown ws handle {handle}"))
    }

    pub(crate) fn send(&mut self, handle: u64, frame: WsFrame) -> Result<(), String> {
        self.get(handle)?
            .cmd
            .send(WsCommand::Send(frame))
            .map_err(|_| "ws.send: connection closed".to_string())
    }

    pub(crate) fn recv(&mut self, handle: u64, timeout_ms: u32) -> Result<WsFrame, String> {
        if timeout_ms == 0 {
            return Err(
                "ws.recv: timeout-ms must be > 0 — a recv that can block forever \
                 hides a dead connection (wit-review F9)"
                    .into(),
            );
        }
        let conn = self.get(handle)?;
        match conn.rx.recv_timeout(Duration::from_millis(u64::from(timeout_ms))) {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("ws.recv: no frame within {timeout_ms}ms"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("ws.recv: connection closed".into())
            }
        }
    }

    pub(crate) fn close(&mut self, handle: u64) -> Result<(), String> {
        self.get(handle)?;
        let conn = self.connections.remove(&handle).expect("checked above");
        let _ = conn.cmd.send(WsCommand::Close);
        Ok(())
    }
}

fn set_read_tick(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    let result = match socket.get_mut() {
        MaybeTlsStream::Plain(tcp) => tcp.set_read_timeout(Some(READ_TICK)),
        MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(Some(READ_TICK)),
        _ => Ok(()), // other transports: no tick (compile-time coverage decides)
    };
    if result.is_err() {
        // A missing tick only delays keepalive, it never blocks the guest:
        // recv's timeout is enforced on the channel, not the socket.
    }
}

/// The connection actor: owns the socket, drains outbound commands,
/// keeps the connection alive (and kills it when it provably died),
/// forwards inbound frames. Runs until close/error; the terminal error
/// is delivered once on the frame channel.
fn actor(
    mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
    cmd: mpsc::Receiver<WsCommand>,
    frames: mpsc::Sender<Result<WsFrame, String>>,
) {
    let mut last_inbound = Instant::now();
    let mut last_ping = Instant::now();
    loop {
        // Outbound commands first: close wins over a pending frame.
        let mut close = false;
        while let Ok(command) = cmd.try_recv() {
            match command {
                WsCommand::Send(frame) => {
                    let message = match frame {
                        WsFrame::Text(t) => Message::Text(t.into()),
                        WsFrame::Binary(b) => Message::Binary(b.into()),
                    };
                    if socket.send(message).is_err() {
                        return;
                    }
                }
                WsCommand::Close => close = true,
            }
        }
        if close {
            let _ = socket.close(None);
            return;
        }
        // Keepalive (wit-review F9): ping on cadence; without inbound
        // signs of life, close — never let recv hang on a dead socket.
        if last_ping.elapsed() >= PING_INTERVAL {
            if socket.send(Message::Ping(Vec::new().into())).is_err() {
                return;
            }
            last_ping = Instant::now();
        }
        if last_inbound.elapsed() >= DEAD_AFTER {
            let _ = frames.send(Err(format!(
                "ws: no inbound frame or pong for {}s — connection dead, closed by host",
                DEAD_AFTER.as_secs()
            )));
            let _ = socket.close(None);
            return;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                last_inbound = Instant::now();
                if frames.send(Ok(WsFrame::Text(text.to_string()))).is_err() {
                    return;
                }
            }
            Ok(Message::Binary(bytes)) => {
                last_inbound = Instant::now();
                if frames.send(Ok(WsFrame::Binary(bytes.to_vec()))).is_err() {
                    return;
                }
            }
            Ok(Message::Ping(_) | Message::Pong(_)) => {
                last_inbound = Instant::now();
                // Flush the auto-queued pong reply promptly.
                let _ = socket.flush();
            }
            Ok(Message::Close(_)) => {
                let _ = frames.send(Err("ws: peer closed the connection".into()));
                return;
            }
            Ok(Message::Frame(_)) => {} // raw frames never surface from read()
            Err(tungstenite::Error::Io(e))
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut => {}
            Err(e) => {
                let _ = frames.send(Err(format!("ws: {e}")));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_origin_maps_to_http_form() {
        assert_eq!(
            WsRegistry::origin_of("ws://127.0.0.1:9000/im"),
            Some("http://127.0.0.1:9000".to_string())
        );
        assert_eq!(
            WsRegistry::origin_of("wss://open.feishu.cn/callback"),
            Some("https://open.feishu.cn".to_string())
        );
        assert_eq!(WsRegistry::origin_of("http://x.test/"), None);
        assert_eq!(WsRegistry::origin_of("garbage"), None);
        // Same parser as http: userinfo cannot smuggle the authority.
        assert_eq!(
            WsRegistry::origin_of("ws://consented.test@evil.test/"),
            Some("http://evil.test".to_string())
        );
    }

    #[test]
    fn unconsented_origin_and_bad_url_fail_loud() {
        let mut registry = WsRegistry::new(1, HashSet::new());
        let err = registry.connect("ws://127.0.0.1:9/").unwrap_err();
        assert!(err.contains("not consented"), "{err}");
        let err = registry.connect("ftp://x.test/").unwrap_err();
        assert!(err.contains("not a ws(s) URL"), "{err}");
    }

    #[test]
    fn recv_requires_a_timeout_and_fences_stale_handles() {
        let mut registry = WsRegistry::new(1, HashSet::new());
        let err = registry.recv(0, 0).unwrap_err();
        assert!(err.contains("must be > 0"), "{err}");
        let handle = (9u64 << 32) | 3; // generation 9, this registry is 1
        let err = registry.recv(handle, 100).unwrap_err();
        assert!(err.contains("stale ws handle"), "{err}");
        let err = registry.recv((1 << 32) | 3, 100).unwrap_err();
        assert!(err.contains("unknown ws handle"), "{err}");
    }
}
