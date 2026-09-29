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

use std::collections::HashSet;
use std::io;
use std::net::TcpStream;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// The inbound frame queue: the receiver half of the connection actor's
/// channel. The stream handed to the guest's `receive` and the synchronous
/// `poll` drain are two readings of this one queue, so both name it -- and
/// naming it once keeps `clippy::type_complexity` out of every signature
/// that carries it.
pub(crate) type InboundQueue =
    Arc<std::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Result<WsFrame, String>>>>;

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

pub(crate) enum WsCommand {
    /// Send carries a confirmation channel: the caller awaits it until
    /// the actor has WRITTEN the frame to the socket (Ok = on the wire,
    /// not
    /// merely queued — the dingtalk ack lesson: in print mode the
    /// process can exit within one actor read tick, and a queued-only
    /// ack provably never reaches the platform).
    Send(WsFrame, tokio::sync::oneshot::Sender<Result<(), String>>),
    Close,
}

/// Why a ws call failed, in the contract's own three-way split
/// (`types.error`) -- same shape and same reason as `http::HttpError`: the
/// variant is what a guest branches on, the detail is for the log.
#[derive(Debug)]
pub(crate) enum WsError {
    /// No consent covers this origin: the URL is not in the allowlist.
    Refused(String),
    /// Consent covered it and the call failed: no handshake within the
    /// host's budget, the peer is gone, the actor stopped.
    Failed(String),
    /// The call is not valid here: not a ws(s) URL, or a connection whose
    /// actor is already gone.
    Invalid(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(detail) => write!(f, "refused: {detail}"),
            Self::Failed(detail) => write!(f, "failed: {detail}"),
            Self::Invalid(detail) => write!(f, "invalid: {detail}"),
        }
    }
}

/// One open connection, owned by the guest as `ws.connection` since
/// 0.7.0: the actor thread owns the socket, this handle holds the command
/// channel plus the inbound frame queue the guest's `receive` stream
/// drains. Dropping it closes the connection -- 0.6.0's handle table, its
/// generation fence and its explicit `close` all collapse into ownership
/// (the table dies with the store, so a rebuilt instance cannot alias a
/// stale handle).
#[derive(Debug)]
pub struct HostConnection {
    cmd: mpsc::Sender<WsCommand>,
    frames: InboundQueue,
    /// Who owns the inbound queue. The contract allows exactly one consumer:
    /// the `receive` stream (async guests) or `poll` (the sync pump a
    /// bridge's probe runs). Nothing is ever split between them.
    inbound: Inbound,
    /// The connection's terminal reason, recorded by `poll` when it meets
    /// the end of the queue and reported on the NEXT call -- so the frames
    /// that arrived before the end are delivered instead of being traded
    /// away for the error.
    ended: Option<String>,
}

/// The inbound queue's owner, decided by whichever method was called first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inbound {
    Free,
    Stream,
    Poll,
}

/// Why an inbound call was refused the frame queue. The two answers are
/// different promises to the guest, so they are different variants here:
/// `AlreadyOwned` is the contract's `invalid` (one consumer per
/// connection -- a misuse of the interface, and `invalid` is what a guest
/// matching the typed variant looks for), while `Ended` is the
/// connection's terminal reason, which the interface reports as `failed`
/// after the frames it follows. Folding both into one string is what let a
/// refused `receive` show up as a clean end.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InboundError {
    /// The other consumer already owns this connection's frames.
    AlreadyOwned,
    /// The connection's terminal reason (peer closed, actor gone).
    Ended(String),
}

impl HostConnection {
    /// The inbound frame queue, handed out once, to `receive`'s stream.
    /// `Err(AlreadyOwned)` when the sync drain already has it -- the
    /// contract's `invalid`, which `receive` reports through the future it
    /// hands back (the stream it also returns ends at once: there is
    /// nothing left to hand you).
    pub(crate) fn take_inbound(&mut self) -> Result<InboundQueue, InboundError> {
        if self.inbound != Inbound::Free {
            return Err(InboundError::AlreadyOwned);
        }
        self.inbound = Inbound::Stream;
        Ok(Arc::clone(&self.frames))
    }

    /// Drain what has arrived, without waiting -- the sync pump's shape.
    /// `Err(AlreadyOwned)` when the queue already belongs to the
    /// `receive` stream (the contract's `invalid`); otherwise the frames
    /// so far plus, on a later call, the connection's terminal reason.
    pub(crate) fn poll_inbound(&mut self) -> Result<Vec<WsFrame>, InboundError> {
        if self.inbound == Inbound::Stream {
            return Err(InboundError::AlreadyOwned);
        }
        self.inbound = Inbound::Poll;
        if let Some(why) = self.ended.take() {
            return Err(InboundError::Ended(why));
        }
        let frames = Arc::clone(&self.frames);
        let mut queue = frames.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        loop {
            match queue.try_recv() {
                Ok(Ok(frame)) => out.push(frame),
                // The actor reported a failure: the frames drained so far
                // are still delivered, and the reason surfaces next call.
                Ok(Err(why)) => {
                    self.ended = Some(why);
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.ended = Some("the connection's actor is gone".into());
                    break;
                }
            }
        }
        Ok(out)
    }

    /// A connection with no socket behind it, for tests: the returned
    /// sender scripts what the actor would have queued (frames, then the
    /// terminal reason).
    #[cfg(test)]
    pub(crate) fn scripted() -> (
        Self,
        tokio::sync::mpsc::UnboundedSender<Result<WsFrame, String>>,
    ) {
        let (cmd, _commands) = mpsc::channel::<WsCommand>();
        let (frames, queue) = tokio::sync::mpsc::unbounded_channel();
        (
            HostConnection {
                cmd,
                frames: Arc::new(std::sync::Mutex::new(queue)),
                inbound: Inbound::Free,
                ended: None,
            },
            frames,
        )
    }

    /// The command channel to the actor. The guest path clones this out of
    /// the resource table and awaits [`send_frame`]; holding the sender is
    /// what keeps the actor's command loop alive, so it never leaves the
    /// handle.
    pub(crate) fn commands(&self) -> mpsc::Sender<WsCommand> {
        self.cmd.clone()
    }
}

/// How long [`send_frame`] waits for the actor's write confirmation.
/// Bounded by one read tick in the worst case; the generous margin is for
/// a wedged actor, which must surface as an error and never as a hang.
const SEND_CONFIRM_BUDGET: Duration = Duration::from_secs(5);

/// Send one frame and await the actor's write confirmation -- the dingtalk
/// ack lesson: an awaited send means the frame was WRITTEN to the socket,
/// not merely queued.
pub(crate) async fn send_frame(
    commands: mpsc::Sender<WsCommand>,
    frame: WsFrame,
) -> Result<(), WsError> {
    let (confirm_tx, confirm_rx) = tokio::sync::oneshot::channel();
    commands
        .send(WsCommand::Send(frame, confirm_tx))
        .map_err(|_| WsError::Invalid("ws.send: connection closed".into()))?;
    match tokio::time::timeout(SEND_CONFIRM_BUDGET, confirm_rx).await {
        Ok(Ok(result)) => result.map_err(WsError::Failed),
        Ok(Err(_)) => Err(WsError::Failed(
            "ws.send: the actor stopped without confirming (connection closed)".into(),
        )),
        Err(_) => Err(WsError::Failed(format!(
            "ws.send: no write confirmation within {}s (wedged actor)",
            SEND_CONFIRM_BUDGET.as_secs()
        ))),
    }
}

impl Drop for HostConnection {
    fn drop(&mut self) {
        let _ = self.cmd.send(WsCommand::Close);
    }
}

/// Connect budget when the contract has none: since 0.7.0 the handshake
/// wait is not a parameter any more ("waiting is no longer a parameter"),
/// so how long a connect may take is the host's own policy -- and there is
/// no way for a caller to ask for "forever".
pub(crate) const CONNECT_TIMEOUT_MS: u32 = 30_000;

/// The connect budget in force: [`CONNECT_TIMEOUT_MS`], or the test knob
/// `TAU_WS_CONNECT_TIMEOUT_MS`.
pub(crate) fn connect_timeout_ms() -> u32 {
    let default = Duration::from_millis(u64::from(CONNECT_TIMEOUT_MS));
    crate::budget("TAU_WS_CONNECT_TIMEOUT_MS", default).as_millis() as u32
}

/// Consent origin of a ws(s) URL, in the http(s) form the allowlist
/// holds (ws:->http:, wss:->https:). Public so the CLI can build a
/// consent allowlist from ws URLs.
pub fn origin_of(url: &str) -> Option<String> {
    WsRegistry::origin_of(url)
}

/// The consent allowlist. Since 0.7.0 the connections themselves live in
/// the guest's resource table, not here: what is left is the one thing the
/// registry could never delegate -- who is allowed to connect at all.
pub(crate) struct WsRegistry {
    /// Consented origins in http(s) form (ws:->http:, wss:->https:).
    /// Empty = deny all.
    origins: HashSet<String>,
}

impl WsRegistry {
    pub(crate) fn new(origins: HashSet<String>) -> Self {
        Self { origins }
    }

    /// Consent origin of a ws(s) URL, in the http(s) form the allowlist
    /// holds: ws:->http:, wss:->https:, then the same authority parsing as
    /// http (userinfo stripped, authority ends at the first delimiter --
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

    /// Connect with the host's own budget (see `CONNECT_TIMEOUT_MS`), or
    /// the test knob `TAU_WS_CONNECT_TIMEOUT_MS`.
    pub(crate) fn connect(&self, url: &str) -> Result<HostConnection, WsError> {
        self.connect_with_timeout(url, connect_timeout_ms())
    }

    /// Connect, bounded by `timeout_ms`. The tests drive this directly;
    /// the contract itself no longer carries a timeout.
    pub(crate) fn connect_with_timeout(
        &self,
        url: &str,
        timeout_ms: u32,
    ) -> Result<HostConnection, WsError> {
        let origin =
            Self::origin_of(url)
                .ok_or_else(|| WsError::Invalid(format!("ws.connect: not a ws(s) URL: {url:?}")))?;
        if !self.origins.contains(&origin) {
            return Err(WsError::Refused(format!(
                "ws.connect: origin {origin} not consented (bridge endpoints are consented \
                 like http origins — the host CLI passes them, e.g. --mcp-url)"
            )));
        }
        // tungstenite::connect does TCP + TLS + the upgrade handshake in one
        // blocking call with no bound of its own: a peer that accepts and then
        // stalls would park the host thread forever (wit-review F11). Run it
        // on a helper thread and bound the wait here; the abandoned socket
        // dies with the peer or the OS connect timeout. The read tick is set
        // only after the handshake, so it cannot bound this phase.
        let (tx, rx) = mpsc::channel();
        let target = url.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(tungstenite::connect(target.as_str()));
        });
        let (mut socket, _response) =
            match rx.recv_timeout(Duration::from_millis(u64::from(timeout_ms))) {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    return Err(WsError::Failed(format!("ws.connect {origin}: {e}")));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(WsError::Failed(format!(
                        "ws.connect: no handshake within {timeout_ms}ms"
                    )));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(WsError::Failed(format!(
                        "ws.connect {origin}: the connect thread died"
                    )));
                }
            };
        set_read_tick(&mut socket);
        let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>();
        // Unbounded on purpose: the actor thread must never block on a guest
        // that stopped reading `receive` (the actor still has commands and
        // keepalives to service); a guest that does not read grows this
        // queue until it drops the connection.
        let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || actor(socket, cmd_rx, frame_tx));
        Ok(HostConnection {
            cmd: cmd_tx,
            frames: Arc::new(std::sync::Mutex::new(frame_rx)),
            inbound: Inbound::Free,
            ended: None,
        })
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
    frames: tokio::sync::mpsc::UnboundedSender<Result<WsFrame, String>>,
) {
    let mut last_inbound = Instant::now();
    let mut last_ping = Instant::now();
    loop {
        // Outbound commands first: close wins over a pending frame.
        let mut close = false;
        while let Ok(command) = cmd.try_recv() {
            match command {
                WsCommand::Send(frame, confirm) => {
                    let message = match frame {
                        WsFrame::Text(t) => Message::Text(t.into()),
                        WsFrame::Binary(b) => Message::Binary(b.into()),
                    };
                    let result = socket
                        .send(message)
                        .map_err(|e| format!("ws.send: {e}"));
                    let failed = result.is_err();
                    // The caller waits on this even when the write
                    // failed — answer first, then die.
                    let _ = confirm.send(result);
                    if failed {
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

    /// One open connection's inbox, no socket needed: the frames' owner is
    /// exclusive, and the sync drain loses nothing -- the frames that
    /// arrived before the end are delivered, and the end surfaces on the
    /// next call.
    #[test]
    fn the_inbox_has_one_consumer_and_the_drain_is_lossless() {
        // The async stream takes the inbox: `receive` is handed out once,
        // and a `poll` after it is refused rather than silently splitting
        // the frames between two consumers. The refusal is the contract's
        // `invalid`, not the connection's terminal reason.
        let (mut stream_owner, _frames) = HostConnection::scripted();
        assert!(stream_owner.take_inbound().is_ok(), "the first take wins");
        assert_eq!(
            stream_owner.take_inbound().unwrap_err(),
            InboundError::AlreadyOwned,
            "handed out twice"
        );
        assert_eq!(
            stream_owner.poll_inbound().unwrap_err(),
            InboundError::AlreadyOwned,
            "poll took a taken inbox"
        );

        // The other order is the other half of the same rule, and the one
        // the guest meets as a refused `receive`: the sync pump owns the
        // queue, so the stream's take is refused instead of ending as if
        // the connection were over (the receipt carries the reason).
        let (mut poll_first, _frames) = HostConnection::scripted();
        assert!(poll_first.poll_inbound().unwrap().is_empty(), "nothing yet is not an end");
        assert_eq!(
            poll_first.take_inbound().unwrap_err(),
            InboundError::AlreadyOwned,
            "receive after poll is the same misuse, in the other direction"
        );

        // The sync drain: everything queued, nothing when the peer is
        // quiet, and the terminal reason only after the frames it follows.
        let (mut poller, frames) = HostConnection::scripted();
        frames.send(Ok(WsFrame::Text("one".into()))).unwrap();
        frames.send(Ok(WsFrame::Binary(vec![2]))).unwrap();
        assert_eq!(
            poller.poll_inbound().unwrap(),
            vec![WsFrame::Text("one".into()), WsFrame::Binary(vec![2])]
        );
        assert!(poller.poll_inbound().unwrap().is_empty(), "an empty inbox is not an end");
        frames.send(Ok(WsFrame::Text("three".into()))).unwrap();
        frames.send(Err("ws: peer closed the connection".into())).unwrap();
        assert_eq!(
            poller.poll_inbound().unwrap(),
            vec![WsFrame::Text("three".into())],
            "the frames before the end must not be traded away for the error"
        );
        assert_eq!(
            poller.poll_inbound().unwrap_err(),
            InboundError::Ended("ws: peer closed the connection".into())
        );
    }

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
        let registry = WsRegistry::new(HashSet::new());
        let err = registry
            .connect_with_timeout("ws://127.0.0.1:9/", 1_000)
            .unwrap_err();
        assert!(err.to_string().contains("not consented"), "{err}");
        let err = registry
            .connect_with_timeout("ftp://x.test/", 1_000)
            .unwrap_err();
        assert!(err.to_string().contains("not a ws(s) URL"), "{err}");
    }

    #[test]
    fn connect_times_out_when_the_peer_never_handshakes() {
        // TCP accepts, the upgrade never comes. tungstenite's connect does
        // transport + handshake in one unbounded blocking call, and the read
        // tick is only set after it returns — so before F11 this shape parked
        // the host thread until the peer or the OS gave up.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Hold the socket open, silent, far past the guest's budget.
            std::thread::sleep(std::time::Duration::from_millis(1500));
            drop(stream);
        });
        let origin = format!("http://127.0.0.1:{port}");
        let registry = WsRegistry::new([origin].into_iter().collect());
        let started = std::time::Instant::now();
        let err = registry
            .connect_with_timeout(&format!("ws://127.0.0.1:{port}/x"), 300)
            .unwrap_err();
        assert!(err.to_string().contains("no handshake within 300ms"), "{err}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1200),
            "connect outlived its budget: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_connect_budget_is_always_bounded() {
        // The contract has no timeout parameter any more, so the host's own
        // budget is the only bound there is: it has to be a real one
        // (wit-review F11 -- a connect that can wait forever hides a dead
        // peer).
        const { assert!(CONNECT_TIMEOUT_MS > 0) };
        const { assert!(CONNECT_TIMEOUT_MS <= 60_000) };
    }

}
