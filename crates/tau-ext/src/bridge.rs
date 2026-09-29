//! Bridge components (world "bridge"): external tool protocols (MCP, IM
//! platforms) behind WIT. The host grants scoped capabilities, and only on
//! explicit consent: spawn-with-pipes (the caller passes the allowed
//! command argv), origin-allowlisted HTTP and WebSocket frames (the caller
//! passes the allowed origins), and session injection via the host channel
//! (`inject` consent, same gate as extensions). Bridges also export
//! probes — an IM adapter observes `after_response` to post replies
//! (docs/im-channels.md); a bridge with nothing to observe returns an
//! empty points() list. The host knows nothing about MCP or IM protocols;
//! the bridge component speaks whatever protocol it likes over the pipes.
//! (Ambient WASI follows the host's WasiPolicy — allow-all by default.)

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use async_trait::async_trait;
use tau_core::probe::{ProbeHandler, ProbePoint, Verdict};
use tau_core::probe_payload::ProbePayload;
use tau_core::tool::{Tool, ToolDef, ToolOutput};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::http::HttpRegistry;
use crate::{
    ExtError, ExtensionHost, HostChannel, LoadedExtension, StreamSubscription, WasiPolicy,
    bridge_bindings,
};

impl bridge_bindings::tau::extension::types::Host for BridgeState {}

struct BridgeState {
    ctx: WasiCtx,
    table: ResourceTable,
    /// The three blocking registries sit behind a std mutex so the host
    /// imports can hand a whole call to `spawn_blocking` and await it (the
    /// lock is taken and dropped inside the blocking body, never held
    /// across an await). Same shape as the provider and realtime worlds.
    processes: std::sync::Arc<std::sync::Mutex<ProcessRegistry>>,
    http: std::sync::Arc<std::sync::Mutex<HttpRegistry>>,
    ws: std::sync::Arc<std::sync::Mutex<crate::ws::WsRegistry>>,
    /// Host channel sinks (late-bound via wire_host_channel, same as
    /// extensions) + this bridge's session-injection consent.
    channel: Arc<HostChannel>,
    inject: bool,
    /// host.subscribe handles, per-instance like the extension world's
    /// (a trap rebuild drops them with the old state).
    subscriptions: HashMap<u64, StreamSubscription>,
    next_subscription: u64,
    /// Webhook ingress (docs/im-channels.md): consented listen
    /// addresses + this bridge's routes/servers. Arc-shared with the
    /// factory so a trap rebuild keeps the listener (and its routes)
    /// alive — the server threads dispatch into the SharedBridge, which
    /// revive() repoints at the fresh instance.
    ingress: std::sync::Arc<crate::ingress::IngressRegistry>,
}

impl WasiView for BridgeState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// How much undelivered stdin data the host holds for one child: one pipe
/// buffer's worth. A guest that writes faster than the child reads gets a
/// short count instead of unbounded host memory, and a child that has
/// stopped reading is noticed within a buffer (wit-review F12).
const STDIN_BUFFER_CAP: usize = 64 * 1024;

/// Chunks handed to the writer thread never exceed this, so the host can see
/// the pipe draining in steps instead of one all-or-nothing write.
const STDIN_CHUNK: usize = 8 * 1024;

/// What the writer thread reports after each chunk it hands to the pipe.
enum WriteReport {
    /// `n` bytes made it into the pipe.
    Delivered(usize),
    /// Nothing more can be delivered; the text says why.
    Broken(String),
}

/// One spawned child: piped stdin, stdout drained by a reader thread into a
/// channel so `read-stdout` never blocks the wasm engine on a raw pipe, and
/// stdin fed by a writer thread so `write-stdin` never does either. Taken
/// bytes are the host's from the moment the count is returned (wit-review
/// F12), which is why the counters live here and not in the guest.
struct ChildProcess {
    child: Child,
    /// Chunks queued for the writer thread, in order.
    stdin_tx: std::sync::mpsc::Sender<Vec<u8>>,
    /// Progress and failure reports from the writer thread.
    stdin_reports: std::sync::mpsc::Receiver<WriteReport>,
    /// Bytes handed to the pipe so far (the sum of the delivered reports).
    stdin_delivered: u64,
    /// Bytes accepted from the guest so far.
    stdin_taken: u64,
    /// Set once the child's stdin is known to be gone.
    stdin_broken: Option<String>,
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    pending: VecDeque<u8>,
    eof: bool,
}

impl ChildProcess {
    /// Fold every pending writer report into the counters. One report per
    /// chunk, so this is cheap; a report left unread would only make the host
    /// take fewer bytes than it could, never more.
    fn drain_stdin_reports(&mut self) {
        loop {
            match self.stdin_reports.try_recv() {
                Ok(WriteReport::Delivered(n)) => self.stdin_delivered += n as u64,
                Ok(WriteReport::Broken(why)) => {
                    self.stdin_broken.get_or_insert(why);
                }
                Err(_) => break,
            }
        }
    }

}

/// Handles pack a generation in the high 32 bits: the factory bumps the
/// generation on every (re)instantiation, so a handle a guest holds from
/// before a trap-rebuild can never alias a child of the fresh instance
/// (wit-review F8).
struct ProcessRegistry {
    generation: u32,
    next: u32,
    children: HashMap<u64, ChildProcess>,
}

impl ProcessRegistry {
    fn new(generation: u32) -> Self {
        Self {
            generation,
            next: 0,
            children: HashMap::new(),
        }
    }

    fn spawn(&mut self, argv: &[String]) -> Result<u64, String> {
        let (program, args) = argv.split_first().ok_or("spawn: empty argv")?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Child stderr goes to tau's stderr: bridge servers log there and
            // it is invaluable when a handshake fails.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", program))?;
        let mut stdin = child.stdin.take().ok_or("spawn: no stdin pipe")?;
        let mut stdout = child.stdout.take().ok_or("spawn: no stdout pipe")?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        // stdin gets the mirror treatment (wit-review F12): a thread of its
        // own writes the pipe, so a child that has stopped reading can never
        // park the host. The guest is answered with how many bytes the host
        // took, and those bytes are the host's to deliver.
        let (stdin_tx, stdin_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (report_tx, stdin_reports) = std::sync::mpsc::channel::<WriteReport>();
        std::thread::spawn(move || {
            while let Ok(chunk) = stdin_rx.recv() {
                match stdin.write_all(&chunk).and_then(|()| stdin.flush()) {
                    Ok(()) => {
                        if report_tx.send(WriteReport::Delivered(chunk.len())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = report_tx.send(WriteReport::Broken(format!(
                            "the child's stdin is closed ({e})"
                        )));
                        break;
                    }
                }
            }
        });
        let handle = ((self.generation as u64) << 32) | (self.next as u64);
        self.next += 1;
        self.children.insert(
            handle,
            ChildProcess {
                child,
                stdin_tx,
                stdin_reports,
                stdin_delivered: 0,
                stdin_taken: 0,
                stdin_broken: None,
                rx,
                pending: VecDeque::new(),
                eof: false,
            },
        );
        Ok(handle)
    }

    fn get(&mut self, handle: u64) -> Result<&mut ChildProcess, String> {
        if (handle >> 32) as u32 != self.generation {
            return Err(format!(
                "stale process handle {handle} (the instance was rebuilt; respawn the child)"
            ));
        }
        self.children
            .get_mut(&handle)
            .ok_or_else(|| format!("unknown process handle {handle}"))
    }

    /// The read bound lives here (not in the `process::Host` impl) so it can
    /// be unit-tested without a wasm engine — the `spawn` shape.
    fn read_stdout(
        &mut self,
        handle: u64,
        max: u32,
        timeout_ms: u32,
    ) -> Result<(Vec<u8>, bool), String> {
        if timeout_ms == 0 {
            return Err(
                "process.read-stdout: timeout-ms must be > 0 — a read that can block forever hides a dead child (wit-review F11)"
                    .into(),
            );
        }
        let child = self.get(handle)?;
        let max = max.max(1) as usize;
        // Block only until SOMETHING is available, then return immediately:
        // waiting to fill `max` would deadlock any peer that sends a short
        // message and then waits for a reply. The wait is bounded by
        // `timeout_ms` (wit-review F11): a child that hangs without writing
        // anything surfaces as an explicit error, and the handle survives it.
        if child.pending.is_empty() && !child.eof {
            match child
                .rx
                .recv_timeout(std::time::Duration::from_millis(u64::from(timeout_ms)))
            {
                Ok(Ok(chunk)) => child.pending.extend(chunk),
                Ok(Err(e)) => return Err(format!("read-stdout: {e}")),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "process.read-stdout: no bytes within {timeout_ms}ms"
                    ));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => child.eof = true,
            }
        }
        let take = child.pending.len().min(max);
        let bytes: Vec<u8> = child.pending.drain(..take).collect();
        Ok((bytes, child.eof && child.pending.is_empty()))
    }

    /// Take up to `data.len()` bytes from the guest, waiting at most
    /// `timeout_ms` for room in the buffer. The count returned is what the
    /// host **took**: those bytes are queued in order and the host owns them
    /// from that moment, so a short count is not an error and the caller
    /// resumes at `data[taken..]` (wit-review F12). A blocking write that
    /// timed out could never report an honest count — bytes already in the
    /// pipe cannot be taken back — which is why the guest-facing shape counts
    /// bytes taken rather than bytes delivered.
    fn write_stdin(&mut self, handle: u64, data: &[u8], timeout_ms: u32) -> Result<u32, String> {
        if timeout_ms == 0 {
            return Err(
                "process.write-stdin: timeout-ms must be > 0 — a write that can block forever hides a dead child (wit-review F12)"
                    .into(),
            );
        }
        let child = self.get(handle)?;
        child.drain_stdin_reports();
        if let Some(reason) = child.stdin_broken.clone() {
            return Err(format!("process.write-stdin: {reason}"));
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(u64::from(timeout_ms));
        let mut taken = 0usize;
        while taken < data.len() {
            let outstanding = child.stdin_taken.saturating_sub(child.stdin_delivered);
            let free = (STDIN_BUFFER_CAP as u64).saturating_sub(outstanding) as usize;
            if free == 0 {
                // A cap's worth of bytes is already undelivered. Wait for room
                // — but only up to the budget: taking bytes we cannot hand
                // over would be a lie about progress.
                let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now())
                else {
                    break;
                };
                match child.stdin_reports.recv_timeout(remaining) {
                    Ok(WriteReport::Delivered(n)) => {
                        child.stdin_delivered += n as u64;
                        continue;
                    }
                    Ok(WriteReport::Broken(why)) => {
                        child.stdin_broken = Some(why);
                        break;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        child.stdin_broken = Some("the child's stdin is closed".into());
                        break;
                    }
                }
            }
            let n = free.min(data.len() - taken).min(STDIN_CHUNK);
            if child.stdin_tx.send(data[taken..taken + n].to_vec()).is_err() {
                // The writer thread is gone: the pipe is closed, and this is
                // not a "try again later".
                child.stdin_broken = Some("the child's stdin is closed".into());
                break;
            }
            child.stdin_taken += n as u64;
            taken += n;
        }
        if taken > 0 {
            // A short count is the normal outcome on a busy child; the guest
            // resumes from the offset it left off at.
            return Ok(taken.min(u32::MAX as usize) as u32);
        }
        if let Some(reason) = child.stdin_broken.clone() {
            return Err(format!("process.write-stdin: {reason}"));
        }
        // Nothing fit and nothing is broken: the child took nothing within the
        // budget. The message is intact — the caller decides whether to retry,
        // hold it back, or kill the child.
        Ok(0)
    }
}

impl Drop for ProcessRegistry {
    fn drop(&mut self) {
        for (_, mut child) in self.children.drain() {
            let _ = child.child.kill();
            let _ = child.child.wait();
        }
    }
}

/// Explicit user consent for one bridge load. Passing it IS the consent UX:
/// `command` is the argv the bridge may spawn (delivered as TAU_MCP_COMMAND),
/// `origins` the scheme://host[:port] prefixes HTTP requests may target
/// (the bridge learns its endpoint via TAU_MCP_URL).
#[derive(Default, Clone)]
pub struct BridgeConsent {
    /// The argv the bridge may spawn (delivered as TAU_MCP_COMMAND).
    pub command: Option<Vec<String>>,
    /// The MCP endpoint URL (delivered as TAU_MCP_URL).
    pub mcp_url: Option<String>,
    /// `scheme://host[:port]` prefixes HTTP requests may target.
    pub origins: HashSet<String>,
    /// Session injection: the bridge may push messages into the session
    /// (host.steer / follow-up) — the IM inbound leg. Same gate and same
    /// remembered grant as extensions.
    pub inject: bool,
    /// Webhook ingress: the `addr:port` list the bridge may listen on
    /// (CLI `--ingress`; the IM webhook leg for WhatsApp/企微-class
    /// platforms — docs/im-channels.md). Empty = listen() fails naming
    /// the missing consent.
    pub ingress: Vec<String>,
}

/// Extract the consent origin ("scheme://host[:port]") from an http(s) URL.
/// Public so the CLI can build a consent allowlist from --mcp-url.
pub fn origin_of(url: &str) -> Option<String> {
    crate::http::HttpRegistry::origin_of(url)
}

impl bridge_bindings::tau::extension::http::Host for BridgeState {
    /// The network calls run on the blocking pool and are awaited; the
    /// table lookups answer inline (same shape as the provider world's
    /// http import).
    async fn request(
        &mut self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<u64, String> {
        let registry = self.http.clone();
        tokio::task::spawn_blocking(move || {
            crate::lock_registry(&registry).request(&method, &url, &headers, &body, timeout_ms)
        })
        .await
        .map_err(|e| format!("http.request: blocking task failed: {e}"))?
    }

    async fn status(&mut self, handle: u64) -> Result<u16, String> {
        crate::lock_registry(&self.http).status(handle)
    }

    async fn header(&mut self, handle: u64, name: String) -> Result<Option<String>, String> {
        crate::lock_registry(&self.http).header(handle, &name)
    }

    async fn read_body(
        &mut self,
        handle: u64,
        max: u32,
        timeout_ms: u32,
    ) -> Result<(Vec<u8>, bool), String> {
        let registry = self.http.clone();
        tokio::task::spawn_blocking(move || {
            crate::lock_registry(&registry).read_body(handle, max, timeout_ms)
        })
        .await
        .map_err(|e| format!("http.read-body: blocking task failed: {e}"))?
    }

    async fn close(&mut self, handle: u64) {
        crate::lock_registry(&self.http).close(handle);
    }
}

impl bridge_bindings::tau::extension::ws::Host for BridgeState {
    /// Every ws op waits on the connection's actor thread (handshake,
    /// write confirmation, inbound frame up to the timeout), so each one
    /// goes to the blocking pool and is awaited.
    async fn connect(&mut self, url: String, timeout_ms: u32) -> Result<u64, String> {
        let registry = self.ws.clone();
        tokio::task::spawn_blocking(move || {
            crate::lock_poisoned(&registry).connect(&url, timeout_ms)
        })
        .await
        .map_err(|e| format!("ws.connect: blocking task failed: {e}"))?
    }

    async fn send(
        &mut self,
        handle: u64,
        frame: bridge_bindings::tau::extension::ws::Frame,
    ) -> Result<(), String> {
        use bridge_bindings::tau::extension::ws::Frame;
        let frame = match frame {
            Frame::Text(t) => crate::ws::WsFrame::Text(t),
            Frame::Binary(b) => crate::ws::WsFrame::Binary(b),
        };
        let registry = self.ws.clone();
        tokio::task::spawn_blocking(move || crate::lock_poisoned(&registry).send(handle, frame))
            .await
            .map_err(|e| format!("ws.send: blocking task failed: {e}"))?
    }

    async fn recv(
        &mut self,
        handle: u64,
        timeout_ms: u32,
    ) -> Result<bridge_bindings::tau::extension::ws::Frame, String> {
        use bridge_bindings::tau::extension::ws::Frame;
        let registry = self.ws.clone();
        let frame = tokio::task::spawn_blocking(move || {
            crate::lock_poisoned(&registry).recv(handle, timeout_ms)
        })
        .await
        .map_err(|e| format!("ws.recv: blocking task failed: {e}"))??;
        match frame {
            crate::ws::WsFrame::Text(t) => Ok(Frame::Text(t)),
            crate::ws::WsFrame::Binary(b) => Ok(Frame::Binary(b)),
        }
    }

    async fn close(&mut self, handle: u64) -> Result<(), String> {
        let registry = self.ws.clone();
        tokio::task::spawn_blocking(move || crate::lock_poisoned(&registry).close(handle))
            .await
            .map_err(|e| format!("ws.close: blocking task failed: {e}"))?
    }
}

/// The bridge world's host channel (docs/im-channels.md contract
/// amendment): the same ops as the extension world's, with the bridge
/// bindgen's own copies of the types converted field-by-field into the
/// host bindings' shapes (identical by construction, like
/// bridge_block_to_host below).
impl bridge_bindings::tau::extension::host::Host for BridgeState {
    async fn notify(
        &mut self,
        level: String,
        content: Vec<bridge_bindings::tau::extension::types::Content>,
    ) -> Result<(), String> {
        crate::channel_notify(
            &self.channel,
            level,
            content.into_iter().map(bridge_content_to_host).collect(),
        )
    }

    async fn emit(&mut self, event_json: String) -> Result<(), String> {
        crate::channel_emit(&self.channel, event_json)
    }

    async fn steer(
        &mut self,
        message: bridge_bindings::tau::extension::types::Message,
    ) -> Result<(), String> {
        crate::inject_message(
            &self.channel,
            self.inject,
            bridge_message_to_host(message),
            true,
        )
    }

    async fn follow_up(
        &mut self,
        message: bridge_bindings::tau::extension::types::Message,
    ) -> Result<(), String> {
        crate::inject_message(
            &self.channel,
            self.inject,
            bridge_message_to_host(message),
            false,
        )
    }

    async fn subscribe(&mut self, topics: Vec<String>) -> Result<u64, String> {
        crate::subscribe_topics(
            &self.channel,
            &mut self.subscriptions,
            &mut self.next_subscription,
            &topics,
        )
    }

    async fn poll(
        &mut self,
        subscription: u64,
    ) -> Result<Vec<bridge_bindings::tau::extension::host::StreamEvent>, String> {
        crate::poll_subscription(&mut self.subscriptions, subscription)
            .map(|events| events.into_iter().map(host_event_to_bridge).collect())
    }

    async fn unsubscribe(&mut self, subscription: u64) -> Result<(), String> {
        crate::unsubscribe_subscription(&mut self.subscriptions, subscription)
    }
}

/// Webhook ingress: consent is the listen address (CLI --ingress);
/// the registry owns routes/servers and the push dispatch
/// (docs/im-channels.md). Host stays a pipe.
impl bridge_bindings::tau::extension::ingress::Host for BridgeState {
    async fn listen(&mut self, route: String) -> Result<(), String> {
        // Arc<S: listen takes &Arc<Self> for server spawning; clone the
        // Arc out of the state (the registry outlives any one instance).
        // First use binds the socket, so the call goes to the blocking
        // pool like the other network imports.
        let registry = self.ingress.clone();
        tokio::task::spawn_blocking(move || registry.listen(&route))
            .await
            .map_err(|e| format!("ingress.listen: blocking task failed: {e}"))?
    }

    async fn close(&mut self, route: String) -> Result<(), String> {
        self.ingress.close(&route)
    }
}

/// Push one inbound webhook request into the component's
/// ingress-handler export, under the instance lock. A trap poisons the
/// guest: revive so the NEXT request lands on a fresh instance, and
/// answer this one 502 (the platform retries — a retried webhook is a
/// platform fact, not a loss).
///
/// The callers are the ingress server's plain threads, and the export is
/// awaited now, so the call is driven to completion here — the one place
/// in this world that still crosses the runtime boundary (and the reason
/// `block_on_component` exists).
pub(crate) fn ingress_dispatch(
    shared: &SharedBridge,
    request: bridge_bindings::exports::tau::extension::ingress_handler::Request,
) -> Result<bridge_bindings::exports::tau::extension::ingress_handler::Response, String> {
    let shared = shared.clone();
    crate::block_on_component(async move {
        let mut guard = shared.lock().await;
        let BridgeInstance { store, bindings } = &mut guard.instance;
        let result = bindings
            .tau_extension_ingress_handler()
            .call_handle_request(store, &request)
            .await;
        match result {
            Ok(response) => Ok(response),
            Err(e) => {
                guard.revive().await;
                Err(format!(
                    "component trapped handling the webhook: {}",
                    crate::compact_wasm_error(&e)
                ))
            }
        }
    })
}

impl bridge_bindings::tau::extension::process::Host for BridgeState {
    /// `spawn` is a fork/exec under the lock; the other three wait on the
    /// child (pipe drains, timeout polls, wait), so they go to the
    /// blocking pool like the other waiting imports.
    async fn spawn(&mut self, argv: Vec<String>) -> Result<u64, String> {
        crate::lock_poisoned(&self.processes).spawn(&argv)
    }

    async fn write_stdin(
        &mut self,
        handle: u64,
        data: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<u32, String> {
        let processes = self.processes.clone();
        tokio::task::spawn_blocking(move || {
            crate::lock_poisoned(&processes).write_stdin(handle, &data, timeout_ms)
        })
        .await
        .map_err(|e| format!("process.write-stdin: blocking task failed: {e}"))?
    }

    async fn read_stdout(
        &mut self,
        handle: u64,
        max: u32,
        timeout_ms: u32,
    ) -> Result<(Vec<u8>, bool), String> {
        let processes = self.processes.clone();
        tokio::task::spawn_blocking(move || {
            crate::lock_poisoned(&processes).read_stdout(handle, max, timeout_ms)
        })
        .await
        .map_err(|e| format!("process.read-stdout: blocking task failed: {e}"))?
    }

    async fn kill(&mut self, handle: u64) -> Result<(), String> {
        let processes = self.processes.clone();
        tokio::task::spawn_blocking(move || {
            let mut registry = crate::lock_poisoned(&processes);
            // Validates generation and existence (0.2.0: kill failures are
            // reported, not swallowed — wit-review F8).
            registry.get(handle)?;
            let mut child = registry.children.remove(&handle).expect("checked above");
            child.child.kill().map_err(|e| format!("kill: {e}"))?;
            child.child.wait().map_err(|e| format!("kill: wait: {e}"))?;
            Ok(())
        })
        .await
        .map_err(|e| format!("process.kill: blocking task failed: {e}"))?
    }
}

struct BridgeInstance {
    store: Store<BridgeState>,
    bindings: bridge_bindings::Bridge,
}

/// Everything needed to (re)create a bridge instance. A trapped guest
/// poisons its instance, so the host re-instantiates after a trap: the
/// fresh guest respawns its server on first use (its connection cache
/// starts empty), and dropping the poisoned instance kills its leftover
/// child processes (ProcessRegistry::drop).
struct BridgeFactory {
    engine: Engine,
    component: Component,
    linker: Linker<BridgeState>,
    wasi: WasiPolicy,
    consent: BridgeConsent,
    /// Shared with every other component loaded from the same host —
    /// late-bound sinks, see [`HostChannel`].
    channel: Arc<HostChannel>,
    /// Session-injection consent for this bridge (steer/follow-up).
    inject: bool,
    /// Bumped per instantiation; baked into process handles (see
    /// [`ProcessRegistry`]). An atomic, not a `Cell`: the factory now rides
    /// `&self` across awaits, which needs `Sync`.
    generation: std::sync::atomic::AtomicU32,
    /// One per load_bridge; the listener survives instance revivals and
    /// dies with the factory (see Drop).
    ingress: std::sync::Arc<crate::ingress::IngressRegistry>,
}

impl Drop for BridgeFactory {
    fn drop(&mut self) {
        // The factory is the registry owner whose lifetime tracks the
        // bridge's: when every tool/probe/CLI handle to this bridge is
        // gone, the accept loops stop (threads exit within a tick and
        // drop their Arcs; the registry itself drops with the last).
        self.ingress.shutdown();
    }
}

impl BridgeFactory {
    async fn instantiate(&self) -> Result<BridgeInstance, wasmtime::Error> {
        let mut ctx = self.wasi.ctx_builder();
        if let Some(command) = &self.consent.command {
            let command_json = serde_json::to_string(command).unwrap_or_else(|_| "[]".into());
            ctx.env("TAU_MCP_COMMAND", &command_json);
        }
        if let Some(url) = &self.consent.mcp_url {
            ctx.env("TAU_MCP_URL", url);
        }
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        let state = BridgeState {
            ctx: ctx.build(),
            table: ResourceTable::new(),
            processes: std::sync::Arc::new(std::sync::Mutex::new(ProcessRegistry::new(
                generation,
            ))),
            http: std::sync::Arc::new(std::sync::Mutex::new(HttpRegistry::new(
                self.consent.origins.clone(),
            ))),
            ws: std::sync::Arc::new(std::sync::Mutex::new(crate::ws::WsRegistry::new(
                generation,
                self.consent.origins.clone(),
            ))),
            channel: self.channel.clone(),
            inject: self.inject,
            subscriptions: HashMap::new(),
            next_subscription: 0,
            ingress: self.ingress.clone(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings =
            bridge_bindings::Bridge::instantiate_async(&mut store, &self.component, &self.linker)
                .await?;
        Ok(BridgeInstance { store, bindings })
    }
}

pub(crate) struct SharedBridgeInstance {
    instance: BridgeInstance,
    factory: BridgeFactory,
}

impl SharedBridgeInstance {
    /// Drop a poisoned instance and build a fresh one. Best-effort: if
    /// re-instantiation somehow fails, the poisoned instance stays and
    /// calls keep surfacing trap errors.
    async fn revive(&mut self) {
        if let Ok(fresh) = self.factory.instantiate().await {
            self.instance = fresh;
        }
    }
}

/// The interior mutex alone, for holders that keep a `Weak` (the ingress
/// registry: a strong ref there would cycle through the factory).
pub(crate) type SharedBridgeInner = tokio::sync::Mutex<SharedBridgeInstance>;

pub(crate) type SharedBridge = Arc<SharedBridgeInner>;

impl ExtensionHost {
    /// Load a bridge component. `consent` carries everything the bridge is
    /// allowed to touch: the spawn argv (delivered via TAU_MCP_COMMAND),
    /// the HTTP/WS origins it may reach (its endpoint via TAU_MCP_URL) and
    /// session injection (`inject` — the IM inbound leg). Passing the
    /// consent IS the consent; with everything empty the bridge loads but
    /// every capability call fails permission-denied.
    ///
    /// Returns the tools AND the probes the bridge contributed (the IM
    /// outbound leg observes `after_response`; a bridge with nothing to
    /// observe declares an empty points() list and contributes none).
    pub fn load_bridge(
        &self,
        path: impl AsRef<Path>,
        consent: BridgeConsent,
    ) -> Result<LoadedExtension, ExtError> {
        let path = path.as_ref().to_path_buf();
        crate::block_on_component(self.load_bridge_inner(&path, consent))
    }

    async fn load_bridge_inner(
        &self,
        path: &Path,
        consent: BridgeConsent,
    ) -> Result<LoadedExtension, ExtError> {
        let bytes = self.read_verified(path)?;
        let component =
            Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        let mut linker: Linker<BridgeState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        bridge_bindings::Bridge::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let ingress = std::sync::Arc::new(crate::ingress::IngressRegistry::new(
            consent.ingress.clone(),
        ));
        let factory = BridgeFactory {
            engine: self.engine.clone(),
            component,
            linker,
            wasi: self.wasi,
            channel: self.channel.clone(),
            inject: consent.inject,
            consent,
            generation: std::sync::atomic::AtomicU32::new(0),
            ingress,
        };
        let mut instance = factory.instantiate().await.map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: format!(
                "bridge instantiation failed (the bridge world since 0.3.0 also \
                 imports the host channel and exports probes — rebuild against \
                 wit/tau.wit {}; a bridge with nothing to observe returns an empty \
                 points() list): {}",
                crate::CONTRACT_VERSION,
                crate::compact_wasm_error(&e)
            ),
        })?;

        // definitions() performs the protocol handshake (MCP initialize +
        // tools/list); failure here means the server is unusable.
        let definitions = instance
            .bindings
            .tau_extension_tools()
            .call_definitions(&mut instance.store)
            .await
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("bridge handshake failed: {}", crate::compact_wasm_error(&e)),
            })?;

        // The IM outbound leg (docs/im-channels.md): which probe points
        // this bridge observes. Empty = none, same opt-in semantics as
        // extensions.
        let points = instance
            .bindings
            .tau_extension_probes()
            .call_points(&mut instance.store)
            .await
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("bridge points() trapped: {}", crate::compact_wasm_error(&e)),
            })?;

        let shared: SharedBridge =
            Arc::new(tokio::sync::Mutex::new(SharedBridgeInstance { instance, factory }));
        // Late-bind the dispatch target: requests arriving between the
        // listener's first accept and this line answer 503, never
        // dispatch into a half-built bridge.
        shared.lock().await.factory.ingress.bind(&shared);
        let mut tools: Vec<Box<dyn Tool>> = Vec::with_capacity(definitions.len());
        for def in definitions {
            let tool_def = crate::tool_def_strict(def.name, def.description, &def.parameters_json)
                .map_err(|reason| ExtError::Load {
                    path: path.display().to_string(),
                    reason,
                })?;
            tools.push(Box::new(BridgeTool {
                def: tool_def,
                shared: shared.clone(),
            }) as Box<dyn Tool>);
        }

        let points: Vec<ProbePoint> = points
            .iter()
            .filter_map(|name| ProbePoint::from_name(name))
            .collect();
        let probes: Vec<Box<dyn ProbeHandler>> = if points.is_empty() {
            Vec::new()
        } else {
            vec![Box::new(BridgeProbes {
                points,
                shared: shared.clone(),
            })]
        };

        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "bridge".into());
        Ok(LoadedExtension::new(name, tools, probes))
    }
}

/// Bridge-world result block → host-bindings result block. The two
/// bindgen invocations generate distinct types for the same WIT shapes.
fn bridge_block_to_host(
    block: bridge_bindings::tau::extension::types::ResultBlock,
) -> crate::bindings::tau::extension::types::ResultBlock {
    use bridge_bindings::tau::extension::types as bt;
    use crate::bindings::tau::extension::types as ht;
    match block {
        bt::ResultBlock::Text(text) => ht::ResultBlock::Text(text),
        bt::ResultBlock::Media(media) => ht::ResultBlock::Media(ht::Media {
            media_type: media.media_type,
            source: match media.source {
                bt::MediaSource::Bytes(bytes) => ht::MediaSource::Bytes(bytes),
                bt::MediaSource::Url(url) => ht::MediaSource::Url(url),
                bt::MediaSource::Blob(hash) => ht::MediaSource::Blob(hash),
            },
            name: media.name,
        }),
    }
}

/// Bridge-world media → host-bindings media.
fn bridge_media_to_host(
    media: bridge_bindings::tau::extension::types::Media,
) -> crate::bindings::tau::extension::types::Media {
    use bridge_bindings::tau::extension::types as bt;
    use crate::bindings::tau::extension::types as ht;
    ht::Media {
        media_type: media.media_type,
        source: match media.source {
            bt::MediaSource::Bytes(bytes) => ht::MediaSource::Bytes(bytes),
            bt::MediaSource::Url(url) => ht::MediaSource::Url(url),
            bt::MediaSource::Blob(hash) => ht::MediaSource::Blob(hash),
        },
        name: media.name,
    }
}

/// Bridge-world message content block → host-bindings content block.
fn bridge_content_to_host(
    content: bridge_bindings::tau::extension::types::Content,
) -> crate::bindings::tau::extension::types::Content {
    use bridge_bindings::tau::extension::types as bt;
    use crate::bindings::tau::extension::types as ht;
    match content {
        bt::Content::Text(text) => ht::Content::Text(text),
        bt::Content::Media(media) => ht::Content::Media(bridge_media_to_host(media)),
        bt::Content::ToolCall(call) => ht::Content::ToolCall(ht::ToolCall {
            id: call.id,
            name: call.name,
            arguments_json: call.arguments_json,
        }),
        bt::Content::ToolResult(result) => ht::Content::ToolResult(ht::ToolResult {
            call_id: result.call_id,
            content: result
                .content
                .into_iter()
                .map(bridge_block_to_host)
                .collect(),
            is_error: result.is_error,
        }),
    }
}

/// Bridge-world message → host-bindings message (for steer/follow-up).
fn bridge_message_to_host(
    message: bridge_bindings::tau::extension::types::Message,
) -> crate::bindings::tau::extension::types::Message {
    use bridge_bindings::tau::extension::types as bt;
    use crate::bindings::tau::extension::types as ht;
    ht::Message {
        role: match message.role {
            bt::Role::User => ht::Role::User,
            bt::Role::Assistant => ht::Role::Assistant,
            bt::Role::Tool => ht::Role::Tool,
        },
        content: message
            .content
            .into_iter()
            .map(bridge_content_to_host)
            .collect(),
    }
}

/// Host-bindings stream event → bridge-world stream event (poll's
/// return crosses the other way).
fn host_event_to_bridge(
    event: crate::bindings::tau::extension::host::StreamEvent,
) -> bridge_bindings::tau::extension::host::StreamEvent {
    use bridge_bindings::tau::extension::host as bh;
    use crate::bindings::tau::extension::host as hh;
    match event {
        hh::StreamEvent::Lagged(n) => bh::StreamEvent::Lagged(n),
        hh::StreamEvent::TextDelta(text) => bh::StreamEvent::TextDelta(text),
        hh::StreamEvent::AudioDelta(segment) => bh::StreamEvent::AudioDelta(bh::AudioSegment {
            bytes: segment.bytes,
            media_type: segment.media_type,
        }),
    }
}

struct BridgeTool {
    def: ToolDef,
    shared: SharedBridge,
}

#[async_trait]
impl Tool for BridgeTool {
    fn def(&self) -> ToolDef {
        self.def.clone()
    }

    async fn execute(&self, arguments: serde_json::Value) -> ToolOutput {
        let name = self.def.name.clone();
        let mut guard = self.shared.lock().await;
        let BridgeInstance { store, bindings } = &mut guard.instance;
        let result = bindings
            .tau_extension_tools()
            .call_execute(store, &name, &arguments.to_string())
            .await;
        if result.is_err() {
            // The trap poisoned the guest; rebuild so the next call
            // reaches a fresh instance (which respawns its server)
            // instead of trapping forever.
            guard.revive().await;
        }
        match result {
            Ok(r) => match crate::convert::tool_result_blocks_to_core(
                // The bridge world bindgen has its own copies of the types
                // interface; translate field-by-field into the host-side
                // bindings' shapes (identical by construction).
                r.content.into_iter().map(bridge_block_to_host).collect(),
            ) {
                Ok(content) => ToolOutput {
                    content,
                    is_error: r.is_error,
                },
                Err(e) => ToolOutput::err(format!("invalid tool result: {e}")),
            },
            Err(e) => {
                ToolOutput::err(format!("bridge trap: {}", crate::compact_wasm_error(&e)))
            }
        }
    }
}

/// A bridge's probes (the IM outbound leg, docs/im-channels.md): same
/// dispatch as extension probes — synchronous call, trap revives the
/// instance, a broken bridge degrades to Continue and never wedges the
/// run.
struct BridgeProbes {
    points: Vec<ProbePoint>,
    shared: SharedBridge,
}

#[async_trait]
impl ProbeHandler for BridgeProbes {
    fn points(&self) -> &[ProbePoint] {
        &self.points
    }

    async fn probe(&self, point: ProbePoint, payload: ProbePayload) -> Verdict {
        let point_name = point.name().to_string();
        let payload_json = payload.to_json().to_string();
        let mut guard = self.shared.lock().await;
        let BridgeInstance { store, bindings } = &mut guard.instance;
        let result = bindings
            .tau_extension_probes()
            .call_probe(store, &point_name, &payload_json)
            .await;
        if result.is_err() {
            // The trap poisoned the guest; rebuild so the next probe
            // still decides instead of degrading forever.
            guard.revive().await;
        }
        use bridge_bindings::exports::tau::extension::probes::Action;
        match result {
            Ok(verdict) => match verdict.action {
                Action::Continue => Verdict::Continue,
                Action::Replace => crate::replace_probe_payload(point, payload, verdict.payload_json),
                Action::Block => Verdict::Block {
                    reason: verdict.reason.unwrap_or_else(|| "blocked".into()),
                },
            },
            // A broken bridge degrades to Continue, never wedges the run.
            Err(_) => Verdict::Continue,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A child that stays alive without writing a byte to stdout — the
    /// blocked-on-nothing process that read-stdout has to bound. It has to
    /// outlive the budget below; a command that exits early would close the
    /// pipe instead and look like EOF.
    fn quiet_child_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            // ping with its output discarded runs for ~5s.
            vec!["cmd".into(), "/c".into(), "ping -n 6 127.0.0.1 >nul".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["sleep".into(), "5".into()]
        }
    }

    /// A child that says nothing for ~1s and then speaks: the retry case.
    /// On Windows the redirection silences only the first command — the
    /// `echo` after it writes to the pipe.
    fn late_child_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            vec![
                "cmd".into(),
                "/c".into(),
                "ping -n 2 127.0.0.1 >nul & echo late".into(),
            ]
        }
        #[cfg(not(windows))]
        {
            vec!["sh".into(), "-c".into(), "sleep 1; echo late".into()]
        }
    }

    #[test]
    fn read_stdout_rejects_zero_timeout() {
        // 0 would mean "block until the child says something" — refused
        // before the handle is even looked up (the read-body shape).
        let mut registry = ProcessRegistry::new(1);
        let err = registry.read_stdout(7, 4096, 0).unwrap_err();
        assert!(err.contains("must be > 0"), "unexpected error: {err}");
        assert!(err.contains("block forever"), "reason missing: {err}");
    }

    #[test]
    fn read_stdout_times_out_on_a_silent_child() {
        // A live child that has not written yet is indistinguishable from a
        // wedged one. Before F11 this call parked the host thread until the
        // child exited.
        let mut registry = ProcessRegistry::new(1);
        let argv = quiet_child_argv();
        let handle = registry.spawn(&argv).expect("spawn quiet child");
        let started = std::time::Instant::now();
        let err = registry.read_stdout(handle, 4096, 300).unwrap_err();
        assert!(
            err.contains("no bytes within 300ms"),
            "unexpected error: {err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1200),
            "read-stdout outlived its budget: {:?}",
            started.elapsed()
        );
        // A timeout is neither EOF nor a dead handle: the same handle times
        // out again rather than reporting a stale or unknown child. Dropping
        // the registry reaps the child (ProcessRegistry::drop).
        let err = registry.read_stdout(handle, 4096, 300).unwrap_err();
        assert!(
            err.contains("no bytes within 300ms"),
            "handle lost after a timeout: {err}"
        );
    }

    #[test]
    fn read_stdout_returns_what_a_child_writes_after_a_timeout() {
        // The other half of the contract: timing out does not eat the child's
        // later output, exactly as read-body keeps reading after its own
        // timeout.
        let mut registry = ProcessRegistry::new(1);
        let argv = late_child_argv();
        let handle = registry.spawn(&argv).expect("spawn late child");
        let err = registry.read_stdout(handle, 4096, 300).unwrap_err();
        assert!(
            err.contains("no bytes within 300ms"),
            "unexpected error: {err}"
        );
        let (bytes, _eof) = registry.read_stdout(handle, 4096, 5000).expect("late read");
        assert_eq!(
            String::from_utf8_lossy(&bytes).trim(),
            "late",
            "the retry lost the bytes"
        );
    }

    /// A child that reads stdin and echoes it — the write path's counterpart
    /// to `late_child_argv`. It stays quiet for ~1s first, so the test can
    /// hand it bytes before it ever looks at the pipe: exactly the case the
    /// host must still account for. `more` echoes line by line; `findstr`,
    /// the obvious alternative, buffers to 4 KiB and would read as mute.
    fn echo_child_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            vec![
                "cmd".into(),
                "/c".into(),
                "ping -n 2 127.0.0.1 >nul & more".into(),
            ]
        }
        #[cfg(not(windows))]
        {
            vec!["sh".into(), "-c".into(), "sleep 1; cat".into()]
        }
    }

    /// A child that exits at once: its stdin is closed while the handle is
    /// still known to the host.
    fn exiting_child_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/c".into(), "exit".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["true".into()]
        }
    }

    #[test]
    fn write_stdin_rejects_zero_timeout() {
        // 0 would mean "wait however long the child takes" — refused before
        // the handle is even looked up (the same shape as the reads).
        let mut registry = ProcessRegistry::new(1);
        let err = registry.write_stdin(7, b"hi", 0).unwrap_err();
        assert!(err.contains("must be > 0"), "unexpected error: {err}");
        assert!(err.contains("block forever"), "reason missing: {err}");
    }

    #[test]
    fn write_stdin_takes_everything_the_buffer_holds() {
        // Under the cap the call is a hand-over, not a wait: the bytes are the
        // host's and the guest advances its whole offset. A child that never
        // reads must not change that.
        let mut registry = ProcessRegistry::new(1);
        let argv = quiet_child_argv();
        let handle = registry.spawn(&argv).expect("spawn quiet child");
        let data = vec![b'x'; 4096];
        let started = std::time::Instant::now();
        let taken = registry
            .write_stdin(handle, &data, 300)
            .expect("small write");
        assert_eq!(taken as usize, data.len(), "a small message was cut short");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(300),
            "a hand-over waited: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn write_stdin_bounds_a_child_that_never_reads() {
        // Before F12 this call parked the host until the child drained the
        // pipe: a server that never reads its stdin hung the whole run. The
        // host now takes what it can hold and says so, and the rest stays the
        // guest's to resend. The final 0 is the caller's cue — the host never
        // claims progress it did not take.
        let mut registry = ProcessRegistry::new(1);
        let argv = quiet_child_argv();
        let handle = registry.spawn(&argv).expect("spawn quiet child");
        let data = vec![b'x'; STDIN_BUFFER_CAP + 128 * 1024];
        let started = std::time::Instant::now();
        let mut sent = 0usize;
        let mut calls = 0;
        loop {
            match registry.write_stdin(handle, &data[sent..], 300) {
                Ok(0) => break,
                Ok(n) => {
                    sent += n as usize;
                    calls += 1;
                    assert!(calls < 16, "the host kept claiming room: {sent} bytes");
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(sent > 0, "the first buffer was refused");
        assert!(
            sent < data.len(),
            "the host swallowed {} bytes from a child that reads nothing",
            sent
        );
        // The bound is per call, not per message: two budgets' worth of
        // waiting still lands well inside the test's patience.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1200),
            "write-stdin outlived its budget: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn write_stdin_hands_over_bytes_the_child_then_reads() {
        // What the guest handed over is really delivered, in order, even
        // though the call returned before the child looked at the pipe.
        let mut registry = ProcessRegistry::new(1);
        let argv = echo_child_argv();
        let handle = registry.spawn(&argv).expect("spawn echo child");
        let data = b"tau-f12-marker\n".to_vec();
        let taken = registry.write_stdin(handle, &data, 1000).expect("write");
        assert_eq!(taken as usize, data.len(), "the marker was cut short");
        let mut seen = Vec::new();
        for _ in 0..20 {
            match registry.read_stdout(handle, 4096, 1000) {
                Ok((bytes, _)) => seen.extend_from_slice(&bytes),
                Err(_) => continue,
            }
            if String::from_utf8_lossy(&seen).contains("tau-f12-marker") {
                return;
            }
        }
        panic!(
            "the child never echoed what was taken: {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    #[test]
    fn write_stdin_reports_a_stdin_that_is_gone() {
        // A dead child is an error, not a 0: "nothing fit within the budget"
        // and "there is nothing left to write to" are different answers, and
        // the guest must not retry the second one. The handle stays usable —
        // read-stdout is where the exit shows up.
        let mut registry = ProcessRegistry::new(1);
        let argv = exiting_child_argv();
        let handle = registry.spawn(&argv).expect("spawn exiting child");
        // Wait until the exit is observable: stdout closing is the signal.
        let mut eof = false;
        for _ in 0..20 {
            if let Ok((_, true)) = registry.read_stdout(handle, 1, 500) {
                eof = true;
                break;
            }
        }
        assert!(eof, "the child never closed its stdout");
        // Over the cap, so a single call both fills the buffer and waits for
        // room — which is when the writer's report lands. The first call may
        // still hand back what it took before the pipe broke; the next one has
        // to refuse on its own.
        let data = vec![b'x'; STDIN_BUFFER_CAP + 4096];
        let mut err = None;
        for _ in 0..3 {
            match registry.write_stdin(handle, &data, 300) {
                Ok(_) => continue,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let err = err.expect("a child that exited still took bytes");
        assert!(err.contains("stdin"), "unexpected error: {err}");
    }
}
