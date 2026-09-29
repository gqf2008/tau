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

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tau_core::probe::{ProbeHandler, ProbePoint, Verdict};
use tau_core::probe_payload::ProbePayload;
use tau_core::tool::{Tool, ToolDef, ToolOutput};
use wasmtime::component::{
    Access, Accessor, Component, Destination, FutureReader, HasSelf, Linker, Resource,
    ResourceTable, Source, StreamConsumer, StreamProducer, StreamReader, StreamResult, VecBuffer,
};
use wasmtime::{AsContextMut, Engine, Store, StoreContextMut};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::http::HttpRegistry;
use crate::{
    ExtError, ExtensionHost, HostChannel, LoadedExtension, WasiPolicy,
    bridge_bindings,
};
use bridge_bindings::exports::tau::extension::probes as bridge_probes;

impl bridge_bindings::tau::extension::types::Host for BridgeState {}

struct BridgeState {
    ctx: WasiCtx,
    table: ResourceTable,
    /// What the guest's resources are made of. Since 0.7.0 the children,
    /// connections, responses, routes and subscriptions themselves live in
    /// `table` (they are the guest's to own); what is left here are the
    /// consent registries, behind std mutexes so a blocking call can be
    /// handed to `spawn_blocking` and awaited (the lock is taken and
    /// dropped inside the blocking body, never held across an await).
    http: std::sync::Arc<std::sync::Mutex<HttpRegistry>>,
    ws: std::sync::Arc<std::sync::Mutex<crate::ws::WsRegistry>>,
    /// Host channel sinks (late-bound via wire_host_channel, same as
    /// extensions) + this bridge's session-injection consent.
    channel: Arc<HostChannel>,
    inject: bool,
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

// ---------------------------------------------------------------------------
// The host side of the guest's resources (0.7.0). Everything the guest owns
// -- a response, a socket, a child, a route, a subscription -- is a resource
// now, and the `with:` mappings in `bridge_bindings` point each one at the
// type below that actually holds it.
// ---------------------------------------------------------------------------

use bridge_bindings::tau::extension::types as bridge_types;
use bridge_bindings::tau::extension::{http as bridge_http, ingress as bridge_ingress};

/// How many chunks may sit in the writer thread's queue before
/// `child.stdin` applies backpressure: one pipe buffer's worth (wit-review
/// F12 -- the guest learns the host is behind instead of host memory
/// growing without bound).
const STDIN_QUEUE: usize = 8;

/// Chunks handed to the writer thread never exceed this, so the host sees
/// the pipe draining in steps instead of one all-or-nothing write.
const STDIN_CHUNK: usize = 8 * 1024;

/// How much of one pipe read goes to the guest in one round.
const PIPE_DIRECT_CAPACITY: usize = 8192;

/// How often `wait`'s future polls the child's exit state. The wait is on an
/// OS process, so it is a poll, not a reactor event; 25ms is well under any
/// human-visible latency and costs nothing while the guest awaits.
const WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Idle budget for `child.stdin`: how long the guest's bytes may make NO
/// progress before the host stops consuming them. Progress resets it -- only
/// a queue that stays full reaches it -- and the guest then sees its own
/// write return the unwritten remainder.
///
/// The deadline lives HERE because the guest cannot keep one: 0.6.0's
/// `write-stdin(.., timeout-ms) -> taken` was the guest's own bound, and a
/// wasm32-wasip2 component has no clock it can await (wit-bindgen's async
/// support drives the component model, not `wasi:clocks`). The guarantee is
/// unchanged -- a server that never reads is named, never waited on
/// (wit-review F12) -- but the only place left that can hold a timer is the
/// host. Same shape as the http body's idle policy, same knob style.
pub(crate) const STDIN_IDLE_DEFAULT: std::time::Duration = std::time::Duration::from_secs(30);

/// Idle budget for `child.stdout`/`child.stderr`: an outstanding read that
/// sees no bytes for this long ends the stream (0.6.0 passed `timeout-ms` per
/// read; the stream carries it now). Armed only while a read is actually
/// outstanding, so a quiet server between requests is not a stall.
pub(crate) const PIPE_IDLE_DEFAULT: std::time::Duration = std::time::Duration::from_secs(120);

/// The two knobs, in the house style (`TAU_HTTP_IDLE_TIMEOUT_MS`, ...): a
/// gate leg or a user with a slower server moves them without a rebuild.
pub(crate) fn stdin_idle_timeout() -> std::time::Duration {
    crate::budget("TAU_PROCESS_STDIN_IDLE_TIMEOUT_MS", STDIN_IDLE_DEFAULT)
}

pub(crate) fn pipe_idle_timeout() -> std::time::Duration {
    crate::budget("TAU_PROCESS_STDOUT_IDLE_TIMEOUT_MS", PIPE_IDLE_DEFAULT)
}

/// The one place a pipe wait's deadline is decided, shared by the outbound
/// sink and the inbound stream: poll the wait's timer, arming it on the
/// first silent round, and answer whether the silence has outlived `idle`.
/// `None` disarms (progress happened, or nothing is waiting at all).
///
/// Polling a `tokio::time::Sleep` needs a runtime context; every host entry
/// into a component provides one (`block_on_component` drives a
/// current-thread runtime with `enable_all` when the caller is not already
/// on a reactor).
fn idle_expired(
    timer: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    cx: &mut Context<'_>,
    idle: std::time::Duration,
) -> bool {
    let sleep = timer.get_or_insert_with(|| Box::pin(tokio::time::sleep(idle)));
    matches!(std::future::Future::poll(sleep.as_mut(), cx), Poll::Ready(()))
}

/// One child's stdout or stderr: what the reader thread has queued. Shared
/// with the stream producer that hands it to the guest (a producer must be
/// `'static`, so it cannot borrow the resource table entry).
type PipeQueue =
    Arc<std::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Result<Vec<u8>, String>>>>;

/// One spawned child, owned by the guest as `process.child`. The child
/// handle sits behind an `Arc<Mutex<..>>` so `wait`'s future can poll the
/// exit state while the resource table still owns the handle.
pub struct HostChild {
    child: Arc<std::sync::Mutex<Child>>,
    /// stdout/stderr, handed over on the first call: the contract has one
    /// stream per child and no error to report for a second, so a second
    /// call gets an empty stream.
    stdout: Option<PipeQueue>,
    stderr: Option<PipeQueue>,
    /// Where the guest's stdin stream lands.
    stdin: Option<StdinPipe>,
}

/// Releasing the child kills it. The resource table is the owner now, so
/// the duty 0.6.0's `ProcessRegistry::drop` had -- a bridge's leftover
/// server must not outlive the instance that spawned it, trap-rebuild
/// included -- hangs on this drop.
impl Drop for HostChild {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        // Best-effort and idempotent: an already-exited child has nothing
        // to kill (`try_wait` reaps it, which is what wait() would do).
        if let Ok(None) = child.try_wait() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The host end of `child.stdin`: the bounded queue into the writer thread,
/// the waker that queue's room wakes, and the pipe's terminal state (which
/// the future `stdin` returns reports).
struct StdinPipe {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    room: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    done: Arc<std::sync::Mutex<Option<Result<(), String>>>>,
    done_waker: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    /// [`stdin_idle_timeout`], read at spawn: the sink's deadline.
    idle: std::time::Duration,
}

/// Wake whoever parked on a slot (the consumer on `room`, the future on
/// `done_waker`).
fn wake(slot: &std::sync::Mutex<Option<std::task::Waker>>) {
    if let Some(waker) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
        waker.wake();
    }
}

/// Spawn a child with piped stdio. `argv` is taken verbatim: the load-time
/// consent (and the `TAU_MCP_COMMAND` env the host hands the component) is
/// what makes a spawn legitimate -- the host does not second-guess which
/// program a consented bridge starts.
fn spawn_child(argv: &[String]) -> Result<HostChild, String> {
    let (program, args) = argv.split_first().ok_or("spawn: empty argv")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // stderr is a stream now (0.7.0 gave `child.stderr` one). The reader
        // thread also echoes it to tau's stderr: bridge servers log there,
        // and "the logs are gone unless the guest reads them" would be a
        // regression in exactly the debugging case the pipes exist for.
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", program))?;
    let stdin = child.stdin.take().ok_or("spawn: no stdin pipe")?;
    let stdout = child.stdout.take().ok_or("spawn: no stdout pipe")?;
    let stderr = child.stderr.take().ok_or("spawn: no stderr pipe")?;
    let stdout = pipe_queue(stdout, false);
    let stderr = pipe_queue(stderr, true);
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(STDIN_QUEUE);
    let room = Arc::new(std::sync::Mutex::new(None));
    let done = Arc::new(std::sync::Mutex::new(None));
    let done_waker = Arc::new(std::sync::Mutex::new(None));
    {
        let mut stdin = stdin;
        let (room, done, done_waker) =
            (Arc::clone(&room), Arc::clone(&done), Arc::clone(&done_waker));
        std::thread::spawn(move || {
            // A thread of its own writes the pipe, so a child that has
            // stopped reading can never park the host (wit-review F12).
            while let Ok(chunk) = rx.recv() {
                if let Err(e) = stdin.write_all(&chunk).and_then(|()| stdin.flush()) {
                    *done.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Err(format!("the child's stdin is closed ({e})")));
                    wake(&room);
                    wake(&done_waker);
                    return;
                }
                // Room appeared: wake a guest parked on a full queue.
                wake(&room);
            }
            // Every sender is gone: the guest ended (or dropped) its stream.
            // Closing the pipe here is the EOF the child sees.
            drop(stdin);
            let mut slot = done.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(Ok(()));
            }
            drop(slot);
            wake(&done_waker);
        });
    }
    let idle = stdin_idle_timeout();
    Ok(HostChild {
        child: Arc::new(std::sync::Mutex::new(child)),
        stdout: Some(stdout),
        stderr: Some(stderr),
        stdin: Some(StdinPipe {
            tx,
            room,
            done,
            done_waker,
            idle,
        }),
    })
}

/// Drain one OS pipe on a thread of its own into a queue. Unbounded on
/// purpose: the reader must never block on a guest that stopped reading
/// (the child would deadlock on a full pipe); a guest that stops reading
/// grows this queue and the pipe keeps draining.
fn pipe_queue(mut pipe: impl Read + Send + 'static, echo: bool) -> PipeQueue {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if echo {
                        // The tee: the guest gets the stream, the terminal
                        // keeps the logs.
                        eprint!("{}", String::from_utf8_lossy(&buf[..n]));
                    }
                    if tx.send(Ok(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("{e}")));
                    break;
                }
            }
        }
    });
    Arc::new(std::sync::Mutex::new(rx))
}

/// The guest's `child.stdout`/`child.stderr`: what the reader thread has
/// queued. The stream ends at EOF -- and on the read error that ended it
/// (logged: the contract's stream has one terminal state, and the exit
/// status comes from `wait`, not from how a stream ended).
struct PipeStream {
    /// `None` when the stream was handed out already (or the resource is
    /// not in the table): the stream ends immediately instead of
    /// panicking in a host import.
    queue: Option<PipeQueue>,
    pending: Vec<u8>,
    /// Which pipe this is, for the stall line on stderr.
    name: &'static str,
    /// [`pipe_idle_timeout`]: silence while a read is outstanding ends the
    /// stream. 0.6.0 passed the same number as `timeout-ms` on every read;
    /// the stream owns it now, which is one policy instead of one per call.
    idle: std::time::Duration,
    /// The armed deadline, present only while a read waits with nothing to
    /// give it.
    timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<D> StreamProducer<D> for PipeStream {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // An empty destination buffer is the guest asking to WAIT until this
        // stream is readable; answer "maybe later" (same shape as the http
        // body stream).
        if dst.remaining(store.as_context_mut()) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let this = self.as_mut().get_mut();
        let Some(queue) = this.queue.as_ref().map(Arc::clone) else {
            return Poll::Ready(Ok(StreamResult::Dropped));
        };
        if this.pending.is_empty() {
            let next = {
                let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.poll_recv(cx)
            };
            match next {
                Poll::Ready(Some(Ok(chunk))) => this.pending = chunk,
                Poll::Ready(Some(Err(why))) => {
                    eprintln!("tau process pipe: {why}");
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                Poll::Ready(None) => return Poll::Ready(Ok(StreamResult::Dropped)),
                Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
                Poll::Pending => {
                    // A read is outstanding and the pipe is silent: this is
                    // the one place the idle budget may fire (a stream
                    // nobody is reading must never be timed out).
                    if idle_expired(&mut this.timer, cx, this.idle) {
                        eprintln!(
                            "tau {}: no bytes within {}ms -- ending the read",
                            this.name,
                            this.idle.as_millis()
                        );
                        return Poll::Ready(Ok(StreamResult::Dropped));
                    }
                    return Poll::Pending;
                }
            }
        }
        // Bytes are here (or were already buffered): the deadline moves out
        // of the way until the next silence.
        this.timer = None;
        let mut dst = dst.as_direct(store, PIPE_DIRECT_CAPACITY);
        let buf = dst.remaining();
        if buf.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let n = buf.len().min(this.pending.len());
        buf[..n].copy_from_slice(&this.pending[..n]);
        this.pending.drain(..n);
        dst.mark_written(n);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// The consumer at the end of the guest's `child.stdin` stream: it queues
/// what the guest wrote for the writer thread. A full queue is `Pending`,
/// so the guest's own writes carry the backpressure, and the writer thread
/// wakes the consumer when room appears.
struct StdinSink {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    room: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    /// A chunk the queue had no room for, kept for the next round.
    pending: Vec<u8>,
    /// [`stdin_idle_timeout`]: a full queue that stays full is the stall.
    idle: std::time::Duration,
    /// The armed deadline, present only while a write is waiting for room.
    timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<D> StreamConsumer<D> for StdinSink {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'_, D>,
        mut source: Source<'_, Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let this = self.get_mut();
        if !this.pending.is_empty() {
            match this.tx.try_send(std::mem::take(&mut this.pending)) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(chunk)) => {
                    this.pending = chunk;
                    *this.room.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
                // The guest has bytes the child is not taking: the only
                // stall this budget covers.
                if idle_expired(&mut this.timer, cx, this.idle) {
                    eprintln!(
                        "tau process.stdin: the child took nothing for {}ms -- is it reading?",
                        this.idle.as_millis()
                    );
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                return Poll::Pending;
                }
                // The writer thread is gone: the pipe broke (the future
                // says why) -- stop reading the guest's stream.
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
            }
        }
        // `Source::read` copies at most `Vec::remaining_capacity()` items, so
        // the buffer must be sized before the read (a `Vec::new()` reads
        // nothing -- the spike's two false verdicts came from exactly this).
        let mut buf = Vec::with_capacity(STDIN_CHUNK);
        source.read(store, &mut buf)?;
        if buf.is_empty() {
            return if finish {
                Poll::Ready(Ok(StreamResult::Dropped))
            } else {
                // Nothing offered this round: the guest is not writing,
                // so there is no write to be stalled -- the budget is
                // disarmed rather than started.
                this.timer = None;
                Poll::Pending
            };
        }
        match this.tx.try_send(buf) {
            Ok(()) => {
                this.timer = None;
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Err(std::sync::mpsc::TrySendError::Full(chunk)) => {
                this.pending = chunk;
                *this.room.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
                // The guest has bytes the child is not taking: the only
                // stall this budget covers.
                if idle_expired(&mut this.timer, cx, this.idle) {
                    eprintln!(
                        "tau process.stdin: the child took nothing for {}ms -- is it reading?",
                        this.idle.as_millis()
                    );
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                Poll::Pending
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

/// The exit status in the contract's shape. Windows has no signals, so
/// `signal` stays `none` there and a killed child reports no code (the OS
/// has none to give).
fn exit_status(status: std::process::ExitStatus) -> bridge_bindings::tau::extension::process::ExitStatus {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|n| n.to_string())
    };
    #[cfg(not(unix))]
    let signal = None;
    bridge_bindings::tau::extension::process::ExitStatus {
        code: status.code().map(|code| code as u32),
        signal,
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

/// Push one inbound webhook request into the component's
/// ingress-handler export, under the instance lock. A trap poisons the
/// guest: revive so the NEXT request lands on a fresh instance, and
/// answer this one 502 (the platform retries — a retried webhook is a
/// platform fact, not a loss).
///
/// The callers are the ingress server's plain threads, and the export is
/// awaited now, so the call is driven to completion here — the one place
/// in this world that still crosses the runtime boundary (and the reason
/// `block_on_component` exists). The export is async since 0.7.0, so the
/// request runs inside one drive: a handler that calls out (http,
/// process) awaits in place instead of parking a server thread for the
/// length of the platform's timeout.
pub(crate) fn ingress_dispatch(
    shared: &SharedBridge,
    request: bridge_bindings::exports::tau::extension::ingress_handler::Request,
) -> Result<bridge_bindings::exports::tau::extension::ingress_handler::Response, String> {
    let shared = shared.clone();
    crate::block_on_component(async move {
        let mut guard = shared.lock().await;
        let result = {
            let BridgeInstance { store, bindings } = &mut guard.instance;
            store
                .run_concurrent(async |acc| {
                    bindings
                        .tau_extension_ingress_handler()
                        .call_handle_request(acc, request)
                        .await
                })
                .await
        };
        match result {
            Ok(Ok(response)) => Ok(response),
            // Two error layers: the drive's own failure and the guest's
            // trap. Either way the platform gets a 502 it will retry, and
            // the next request lands on a fresh instance.
            Ok(Err(e)) | Err(e) => {
                guard.revive().await;
                Err(format!(
                    "component trapped handling the webhook: {}",
                    crate::compact_wasm_error(&e)
                ))
            }
        }
    })
}

use bridge_bindings::tau::extension::{host as bridge_host, process as bridge_process};
use bridge_bindings::exports::tau::extension::tools as bridge_tools;
use bridge_bindings::tau::extension::ws as bridge_ws;

// ---- http -----------------------------------------------------------------

/// Lifts an HTTP failure into this world's `types.error` (the
/// classification is made where the failure happens -- `http::HttpError`
/// -- never by matching on a message).
fn http_error_to_bridge(error: crate::http::HttpError) -> bridge_types::Error {
    use bridge_types::Error;
    match error {
        crate::http::HttpError::Refused(detail) => Error::Refused(detail),
        crate::http::HttpError::Failed(detail) => Error::Failed(detail),
        crate::http::HttpError::Invalid(detail) => Error::Invalid(detail),
    }
}

/// Lifts an ingress failure into this world's `types.error`.
fn ingress_error(error: crate::ingress::IngressError) -> bridge_types::Error {
    use bridge_types::Error;
    match error {
        crate::ingress::IngressError::Refused(detail) => Error::Refused(detail),
        crate::ingress::IngressError::Failed(detail) => Error::Failed(detail),
        crate::ingress::IngressError::Invalid(detail) => Error::Invalid(detail),
    }
}

/// Lifts a ws failure into this world's `types.error`.
fn ws_error_to_bridge(error: crate::ws::WsError) -> bridge_types::Error {
    use bridge_types::Error;
    match error {
        crate::ws::WsError::Refused(detail) => Error::Refused(detail),
        crate::ws::WsError::Failed(detail) => Error::Failed(detail),
        crate::ws::WsError::Invalid(detail) => Error::Invalid(detail),
    }
}

/// The host channel's own failures in this world's `types.error`: same
/// three-way classification as the extension world's `From` impl (bindgen
/// generates the error type once per world, so the lift is written twice).
fn channel_error(error: tau_core::error::HostError) -> bridge_types::Error {
    use tau_core::error::HostError;
    match error {
        HostError::Refused(detail) => bridge_types::Error::Refused(detail),
        HostError::Failed(detail) => bridge_types::Error::Failed(detail),
        HostError::Invalid(detail) => bridge_types::Error::Invalid(detail),
    }
}

/// `http.request` is an `async func` in the contract, so it takes the
/// store through an accessor: consent is checked synchronously (the
/// registry lock is scoped to the gate, never held across the await), the
/// request is awaited on the runtime, and the response goes into the
/// guest-owned resource table.
impl<U> bridge_http::HostWithStore<U> for HasSelf<BridgeState> {
    async fn request(
        accessor: &Accessor<U, Self>,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Resource<crate::http::HostResponse>, bridge_types::Error> {
        let client = accessor
            .with(|mut access| crate::lock_registry(&access.get().http).start(&url))
            .map_err(http_error_to_bridge)?;
        let response = crate::http::send(
            &client,
            &method,
            &url,
            &headers,
            &body,
            crate::http::request_timeout(),
            crate::http::idle_timeout(),
        )
        .await
        .map_err(http_error_to_bridge)?;
        accessor.with(|mut access| {
            access.get().table.push(response).map_err(|_| {
                bridge_types::Error::Invalid("http.request: the resource table is full".into())
            })
        })
    }
}

/// The marker the linker asks for even when the interface's own functions
/// all take the store.
impl bridge_http::Host for BridgeState {}

/// The response's own methods: it lives in this state's resource table, so
/// `&mut self` is enough (`async` only because every import of this world
/// is lowered async).
impl bridge_http::HostResponse for BridgeState {
    async fn status(&mut self, response: Resource<crate::http::HostResponse>) -> u16 {
        self.table
            .get(&response)
            .map(crate::http::HostResponse::status)
            .unwrap_or(0)
    }

    async fn header(
        &mut self,
        response: Resource<crate::http::HostResponse>,
        name: String,
    ) -> Option<String> {
        self.table
            .get(&response)
            .ok()
            .and_then(|response| response.header(&name))
    }

    /// Dropping the response is the only close it has: the body stream is
    /// what the guest holds, and the host's sender is released when the
    /// guest drops that stream.
    async fn drop(
        &mut self,
        response: Resource<crate::http::HostResponse>,
    ) -> wasmtime::Result<()> {
        self.table.delete(response)?;
        Ok(())
    }
}

/// `response.body` hands the guest a stream, and a stream handle lives in
/// the store -- hence the `store` flag on this method.
impl<U> bridge_http::HostResponseWithStore<U> for HasSelf<BridgeState> {
    fn body(
        mut host: Access<U, Self>,
        response: Resource<crate::http::HostResponse>,
    ) -> StreamReader<u8> {
        let stream = crate::http::take_body(&mut host.get().table, &response);
        StreamReader::new(&mut host, stream).expect("stream allocation")
    }
}

// ---- ws -------------------------------------------------------------------

/// The contract's frame <-> the host's: identical shapes, generated once
/// per bindgen invocation.
fn ws_frame(frame: bridge_ws::Frame) -> crate::ws::WsFrame {
    match frame {
        bridge_ws::Frame::Text(text) => crate::ws::WsFrame::Text(text),
        bridge_ws::Frame::Binary(bytes) => crate::ws::WsFrame::Binary(bytes),
    }
}

fn bridge_frame(frame: crate::ws::WsFrame) -> bridge_ws::Frame {
    match frame {
        crate::ws::WsFrame::Text(text) => bridge_ws::Frame::Text(text),
        crate::ws::WsFrame::Binary(bytes) => bridge_ws::Frame::Binary(bytes),
    }
}

/// The interface-level marker (ws has no free functions; every call
/// hangs off the connection resource).
impl bridge_ws::Host for BridgeState {}

/// The connection's own methods. Dropping the handle closes the
/// connection (its `Drop` sends the actor the close command) -- 0.6.0's
/// explicit `close` call, and its "close a connection that was never
/// opened" error, both collapse into ownership.
impl bridge_ws::HostConnection for BridgeState {
    async fn drop(
        &mut self,
        connection: Resource<crate::ws::HostConnection>,
    ) -> wasmtime::Result<()> {
        self.table.delete(connection)?;
        Ok(())
    }
}

/// The socket's methods. `connect` and `send` are `async func`s in the
/// contract, so they take an accessor; `receive` is sync + store-flagged
/// (it hands out a stream and a future).
impl<U> bridge_ws::HostConnectionWithStore<U> for HasSelf<BridgeState> {
    async fn connect(
        accessor: &Accessor<U, Self>,
        url: String,
    ) -> Result<Resource<crate::ws::HostConnection>, bridge_types::Error> {
        let registry = accessor.with(|mut access| Arc::clone(&access.get().ws));
        // The TCP/TLS connect and the upgrade handshake are one blocking
        // call under the host's own budget (the contract has no timeout any
        // more), so it goes to the blocking pool; the registry lock is taken
        // inside that body, never across an await.
        let connection =
            tokio::task::spawn_blocking(move || crate::lock_poisoned(&registry).connect(&url))
                .await
                .map_err(|e| {
                    bridge_types::Error::Failed(format!("ws.connect: blocking task failed: {e}"))
                })?
                .map_err(ws_error_to_bridge)?;
        accessor.with(|mut access| {
            access.get().table.push(connection).map_err(|_| {
                bridge_types::Error::Invalid("ws.connect: the resource table is full".into())
            })
        })
    }

    async fn send(
        accessor: &Accessor<U, Self>,
        connection: Resource<crate::ws::HostConnection>,
        frame: bridge_ws::Frame,
    ) -> Result<(), bridge_types::Error> {
        let commands = accessor.with(|mut access| {
            access
                .get()
                .table
                .get(&connection)
                .map(crate::ws::HostConnection::commands)
                .map_err(|_| {
                    bridge_types::Error::Invalid(
                        "ws.send: the connection is not in the resource table".into(),
                    )
                })
        })?;
        // Awaiting this means the frame was WRITTEN to the socket, not
        // merely queued (the dingtalk ack lesson).
        crate::ws::send_frame(commands, ws_frame(frame))
            .await
            .map_err(ws_error_to_bridge)
    }

    /// The sync drain: what the actor has queued right now, in the same
    /// resource the async stream hangs off. A guest that cannot await --
    /// every bridge pump that rides a synchronous probe -- reads the
    /// connection this way (see the contract's `poll` comment for why it
    /// exists at all).
    fn poll(
        mut host: Access<U, Self>,
        connection: Resource<crate::ws::HostConnection>,
    ) -> Result<Vec<bridge_ws::Frame>, bridge_types::Error> {
        let drained = host
            .get()
            .table
            .get_mut(&connection)
            .map(crate::ws::HostConnection::poll_inbound)
            .map_err(|_| {
                bridge_types::Error::Invalid(
                    "ws.poll: the connection is not in the resource table".into(),
                )
            })?;
        match drained {
            Ok(frames) => Ok(frames.into_iter().map(bridge_frame).collect()),
            // One consumer per connection: the second caller's answer is
            // `invalid` -- the contract's word for a contract misuse, and
            // the variant a guest matches on (the string is detail, not
            // the discriminator; 0.7.0 typed these errors so nobody has to
            // parse them).
            Err(crate::ws::InboundError::AlreadyOwned) => Err(bridge_types::Error::Invalid(
                "ws.poll: receive() already owns this connection's frames \
                 (one consumer per connection)"
                    .into(),
            )),
            Err(crate::ws::InboundError::Ended(why)) => Err(bridge_types::Error::Failed(why)),
        }
    }

    /// `receive` hands out two handles over one terminal state: the frame
    /// stream the actor feeds, and the future that reports how the
    /// connection ended. The stream settles that state when it ends, so the
    /// future resolves exactly when the frames stop -- which is what the
    /// contract promises ("that close ends the stream and the future's
    /// error names the reason"). A `receive` that loses the single-consumer
    /// race is settled here instead: the future answers `invalid` and the
    /// stream it hands back is the immediately-ended one, so a guest that
    /// matches the typed variant sees the misuse rather than an empty
    /// stream that looks like a connection ending cleanly.
    fn receive(
        mut host: Access<U, Self>,
        connection: Resource<crate::ws::HostConnection>,
    ) -> (
        StreamReader<bridge_ws::Frame>,
        FutureReader<Result<(), bridge_types::Error>>,
    ) {
        let (frames, refusal) = match host
            .get()
            .table
            .get_mut(&connection)
            .map(crate::ws::HostConnection::take_inbound)
        {
            Ok(Ok(frames)) => (Some(frames), None),
            Ok(Err(crate::ws::InboundError::AlreadyOwned)) => (
                None,
                Some(bridge_types::Error::Invalid(
                    "ws.receive: poll() already owns this connection's frames \
                     (one consumer per connection)"
                        .into(),
                )),
            ),
            // `take_inbound` hands out the queue or refuses it; it has no
            // terminal reason of its own. Kept total anyway, so a variant
            // added later cannot be silently folded into "the stream just
            // ended".
            Ok(Err(crate::ws::InboundError::Ended(why))) => {
                (None, Some(bridge_types::Error::Failed(why)))
            }
            Err(_) => (
                None,
                Some(bridge_types::Error::Invalid(
                    "ws.receive: the connection is not in the resource table".into(),
                )),
            ),
        };
        let verdict: Arc<std::sync::Mutex<Option<Result<(), bridge_types::Error>>>> =
            Arc::new(std::sync::Mutex::new(refusal.map(Err)));
        let verdict_waker = Arc::new(std::sync::Mutex::new(None));
        let stream = StreamReader::new(
            &mut host,
            FrameStream {
                frames,
                verdict: Arc::clone(&verdict),
                verdict_waker: Arc::clone(&verdict_waker),
            },
        )
        .expect("stream allocation");
        let future = FutureReader::new(&mut host, async move {
            std::future::poll_fn(
                |cx| -> Poll<Result<Result<(), bridge_types::Error>, wasmtime::Error>> {
                    let slot = verdict.lock().unwrap_or_else(|e| e.into_inner());
                match &*slot {
                    Some(Ok(())) => Poll::Ready(Ok(Ok(()))),
                    // The one thing that reaches a guest here: the
                    // connection's terminal reason, or the `invalid` a
                    // refused `receive` settled before the stream was even
                    // handed out.
                    Some(Err(err)) => Poll::Ready(Ok(Err(err.clone()))),
                    None => {
                        drop(slot);
                        *verdict_waker.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(cx.waker().clone());
                        Poll::Pending
                    }
                }
                },
            )
            .await
        })
        .expect("future allocation");
        (stream, future)
    }
}

/// The guest's `connection.receive` frame stream: the actor's queue plus
/// the connection's terminal state. A producer must be `'static`, so it
/// holds `Arc`s rather than borrowing the resource table entry.
struct FrameStream {
    /// `None` when there was nothing to hand out: the resource was not in
    /// the table (unreachable through the resource typing), or the sync
    /// drain already owned the queue. Either way the stream ends
    /// immediately rather than panicking inside a host import, and the
    /// reason rides the future as a typed error.
    frames: Option<crate::ws::InboundQueue>,
    /// The future's answer: `Ok(())` when the connection ended, `Err` when
    /// it failed -- or when this call was the refused second consumer
    /// (`invalid`).
    verdict: Arc<std::sync::Mutex<Option<Result<(), bridge_types::Error>>>>,
    verdict_waker: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
}

impl FrameStream {
    /// Record how the connection ended and wake the verdict future. First
    /// answer wins: a connection ends once.
    fn settle(&self, outcome: Result<(), bridge_types::Error>) {
        let mut slot = self.verdict.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(outcome);
            drop(slot);
            if let Some(waker) = self
                .verdict_waker
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                waker.wake();
            }
        }
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        // The guest dropped the stream: cancellation, not a dead
        // connection. The future it took from `receive` still has to
        // resolve, or a guest awaiting it would wait forever on a
        // connection it chose to stop reading.
        self.settle(Ok(()));
    }
}

impl<D> StreamProducer<D> for FrameStream {
    type Item = bridge_ws::Frame;
    type Buffer = VecBuffer<bridge_ws::Frame>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        mut dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // A zero-length read is the guest asking for readiness, not data.
        if dst.remaining(store.as_context_mut()) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let this = self.as_mut().get_mut();
        let Some(queue) = this.frames.as_ref().map(Arc::clone) else {
            this.settle(Ok(()));
            return Poll::Ready(Ok(StreamResult::Dropped));
        };
        let next = {
            let mut frames = queue.lock().unwrap_or_else(|e| e.into_inner());
            frames.poll_recv(cx)
        };
        match next {
            // One frame per round: the guest's buffer may be smaller than
            // the backlog, and a one-item buffer cannot over-deliver.
            Poll::Ready(Some(Ok(frame))) => {
                dst.set_buffer(vec![bridge_frame(frame)].into());
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(Some(Err(why))) => {
                // The actor's terminal report: the peer closed, or the
                // keepalive declared the connection dead. A failed
                // connection, not a contract misuse -- `invalid` is
                // settled only by a refused `receive`.
                this.settle(Err(bridge_types::Error::Failed(why)));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            // Every sender is gone: the actor exited, which is also how a
            // guest-initiated close reports (its Drop sent the command).
            Poll::Ready(None) => {
                this.settle(Ok(()));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Pending if finish => {
                this.settle(Ok(()));
                Poll::Ready(Ok(StreamResult::Cancelled))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---- process --------------------------------------------------------------

/// `spawn` and `kill` are plain functions in the contract (no store, no
/// `async`), so they live on the resource's own trait with `&mut self`.
impl bridge_process::HostChild for BridgeState {
    /// A fork/exec under the host's lock; the child's stdio becomes streams
    /// the guest owns.
    async fn spawn(
        &mut self,
        options: bridge_process::Options,
    ) -> Result<Resource<HostChild>, bridge_types::Error> {
        let argv = options.argv;
        let child = tokio::task::spawn_blocking(move || spawn_child(&argv))
            .await
            .map_err(|e| {
                bridge_types::Error::Failed(format!("process.spawn: blocking task failed: {e}"))
            })?
            .map_err(bridge_types::Error::Failed)?;
        self.table.push(child).map_err(|_| {
            bridge_types::Error::Invalid("process.spawn: the resource table is full".into())
        })
    }

    /// Idempotent, as the contract requires: an already-exited child is a
    /// success, not an error (0.6.0 reported it, wit-review F8).
    async fn kill(&mut self, child: Resource<HostChild>) -> Result<(), bridge_types::Error> {
        let Ok(handle) = self.table.get(&child).map(|child| Arc::clone(&child.child)) else {
            return Err(bridge_types::Error::Invalid(
                "process.kill: the child is not in the resource table".into(),
            ));
        };
        tokio::task::spawn_blocking(move || {
            let mut child = handle.lock().unwrap_or_else(|e| e.into_inner());
            if child
                .try_wait()
                .map_err(|e| format!("process.kill: {e}"))?
                .is_some()
            {
                return Ok(());
            }
            child.kill().map_err(|e| format!("process.kill: {e}"))?;
            child
                .wait()
                .map_err(|e| format!("process.kill: wait: {e}"))?;
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| {
            bridge_types::Error::Failed(format!("process.kill: blocking task failed: {e}"))
        })?
        .map_err(bridge_types::Error::Failed)
    }

    /// Dropping the child releases it, and [`HostChild`]'s own `Drop` kills
    /// it first: 0.6.0's registry killed a bridge's leftover children when
    /// its instance went away, and a trapped-and-rebuilt instance must not
    /// leave its server running behind it.
    async fn drop(&mut self, child: Resource<HostChild>) -> wasmtime::Result<()> {
        self.table.delete(child)?;
        Ok(())
    }
}

/// The three stdio streams and the exit wait: each hands the guest a stream
/// or a future, which is why they are the store-flagged four.
impl<U> bridge_process::HostChildWithStore<U> for HasSelf<BridgeState> {
    fn stdin(
        mut host: Access<U, Self>,
        child: Resource<HostChild>,
        data: StreamReader<u8>,
    ) -> FutureReader<Result<(), bridge_types::Error>> {
        let pipe = host
            .get()
            .table
            .get_mut(&child)
            .ok()
            .and_then(|child| child.stdin.take());
        let Some(pipe) = pipe else {
            return FutureReader::new(
                &mut host,
                std::future::ready(Ok::<Result<(), bridge_types::Error>, wasmtime::Error>(Err(
                    bridge_types::Error::Invalid(
                        "process.stdin: the child's stdin was already taken".into(),
                    ),
                ))),
            )
            .expect("future allocation");
        };
        let done = Arc::clone(&pipe.done);
        let done_waker = Arc::clone(&pipe.done_waker);
        let future = FutureReader::new(&mut host, async move {
            std::future::poll_fn(
                |cx| -> Poll<Result<Result<(), bridge_types::Error>, wasmtime::Error>> {
                    let slot = done.lock().unwrap_or_else(|e| e.into_inner());
                match &*slot {
                    // The pipe closed with everything passed on: EOF at the
                    // child.
                    Some(Ok(())) => Poll::Ready(Ok(Ok(()))),
                    Some(Err(why)) => {
                        Poll::Ready(Ok(Err(bridge_types::Error::Failed(why.clone()))))
                    }
                    None => {
                        drop(slot);
                        *done_waker.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(cx.waker().clone());
                        Poll::Pending
                    }
                }
                },
            )
            .await
        })
        .expect("future allocation");
        // The consumer drains what the guest writes into the writer
        // thread's queue; the future above reports the pipe's end.
        data.pipe(
            &mut host,
            StdinSink {
                tx: pipe.tx,
                room: pipe.room,
                pending: Vec::new(),
                idle: pipe.idle,
                timer: None,
            },
        )
        .expect("stdin pipe");
        future
    }

    fn stdout(mut host: Access<U, Self>, child: Resource<HostChild>) -> StreamReader<u8> {
        let queue = host
            .get()
            .table
            .get_mut(&child)
            .ok()
            .and_then(|child| child.stdout.take());
        if queue.is_none() {
            eprintln!(
                "tau process.stdout: the child's stdout was already taken (or the child is not \
                 in the resource table)"
            );
        }
        StreamReader::new(
            &mut host,
            PipeStream {
                queue,
                pending: Vec::new(),
                name: "process.stdout",
                idle: pipe_idle_timeout(),
                timer: None,
            },
        )
        .expect("stream allocation")
    }

    fn stderr(mut host: Access<U, Self>, child: Resource<HostChild>) -> StreamReader<u8> {
        let queue = host
            .get()
            .table
            .get_mut(&child)
            .ok()
            .and_then(|child| child.stderr.take());
        if queue.is_none() {
            eprintln!(
                "tau process.stderr: the child's stderr was already taken (or the child is not \
                 in the resource table)"
            );
        }
        StreamReader::new(
            &mut host,
            PipeStream {
                queue,
                pending: Vec::new(),
                name: "process.stderr",
                idle: pipe_idle_timeout(),
                timer: None,
            },
        )
        .expect("stream allocation")
    }

    /// The wait is on an OS process, not a reactor event, so it polls on
    /// the host's own cadence -- which costs nothing while the guest awaits
    /// and, unlike a blocking wait, leaves the store driveable.
    fn wait(
        mut host: Access<U, Self>,
        child: Resource<HostChild>,
    ) -> FutureReader<bridge_process::ExitStatus> {
        let handle = host
            .get()
            .table
            .get(&child)
            .map(|child| Arc::clone(&child.child))
            .ok();
        FutureReader::new(&mut host, async move {
            let Some(handle) = handle else {
                // Unreachable through the resource typing (the contract
                // gives `wait` no error channel), so the honest answer is a
                // trap, never a fabricated status for a child that may
                // still be running.
                return Err(wasmtime::Error::msg(
                    "process.wait: the child is not in the resource table",
                ));
            };
            loop {
                let status = {
                    let mut child = handle.lock().unwrap_or_else(|e| e.into_inner());
                    child.try_wait()
                };
                match status {
                    Ok(Some(status)) => return Ok(exit_status(status)),
                    Ok(None) => tokio::time::sleep(WAIT_POLL).await,
                    Err(e) => return Err(e.into()),
                }
            }
        })
        .expect("future allocation")
    }
}

/// The marker the linker asks for (the process interface has resources
/// only).
impl bridge_process::Host for BridgeState {}

// ---- ingress --------------------------------------------------------------

/// Consent is the listen ADDRESS (CLI --ingress), checked by the registry;
/// first use binds the socket, so the call goes to the blocking pool like
/// the other network imports.
impl bridge_ingress::Host for BridgeState {
    async fn listen(
        &mut self,
        route: String,
    ) -> Result<Resource<crate::ingress::HostRegistration>, bridge_types::Error> {
        let registry = Arc::clone(&self.ingress);
        let registration = tokio::task::spawn_blocking(move || registry.listen(&route))
            .await
            .map_err(|e| {
                bridge_types::Error::Failed(format!("ingress.listen: blocking task failed: {e}"))
            })?
            .map_err(ingress_error)?;
        self.table.push(registration).map_err(|_| {
            bridge_types::Error::Invalid("ingress.listen: the resource table is full".into())
        })
    }
}

/// Dropping the registration stops serving the route -- 0.6.0's explicit
/// `close`, minus its "close a route that was never registered" error.
impl bridge_ingress::HostRegistration for BridgeState {
    async fn drop(
        &mut self,
        registration: Resource<crate::ingress::HostRegistration>,
    ) -> wasmtime::Result<()> {
        self.table.delete(registration)?;
        Ok(())
    }
}

// ---- host channel ---------------------------------------------------------

/// The level name the bus carries for a notification. The contract types
/// the enum since 0.7.0; the shared op takes the extension world's enum,
/// so this world's maps onto the same three strings.
fn bridge_level_name(level: bridge_host::Level) -> &'static str {
    match level {
        bridge_host::Level::Info => "info",
        bridge_host::Level::Warn => "warn",
        bridge_host::Level::Error => "error",
    }
}

/// One drained event in this world's generated shape.
fn polled_to_bridge(event: crate::Polled) -> bridge_host::StreamEvent {
    use bridge_host::{AudioSegment, StreamEvent};
    match event {
        crate::Polled::Lagged(n) => StreamEvent::Lagged(n),
        crate::Polled::TextDelta(text) => StreamEvent::TextDelta(text),
        crate::Polled::AudioDelta { bytes, media_type } => {
            StreamEvent::AudioDelta(AudioSegment { bytes, media_type })
        }
    }
}

/// The host channel, same delegations as the extension world's over this
/// world's bindgen types (the shared ops in the crate root carry none).
impl bridge_host::Host for BridgeState {
    async fn notify(
        &mut self,
        level: bridge_host::Level,
        content: Vec<bridge_types::Content>,
    ) -> Result<(), bridge_types::Error> {
        let content = content.into_iter().map(bridge_content_to_host).collect();
        crate::channel_notify(&self.channel, bridge_level_name(level), content)
            .map_err(channel_error)
    }

    async fn emit(&mut self, event_json: String) -> Result<(), bridge_types::Error> {
        crate::channel_emit(&self.channel, event_json).map_err(channel_error)
    }

    async fn steer(
        &mut self,
        message: bridge_types::Message,
    ) -> Result<(), bridge_types::Error> {
        crate::inject_message(
            &self.channel,
            self.inject,
            bridge_message_to_host(message),
            true,
        )
        .map_err(channel_error)
    }

    async fn follow_up(
        &mut self,
        message: bridge_types::Message,
    ) -> Result<(), bridge_types::Error> {
        crate::inject_message(
            &self.channel,
            self.inject,
            bridge_message_to_host(message),
            false,
        )
        .map_err(channel_error)
    }

    async fn subscribe(
        &mut self,
        topics: Vec<bridge_host::Topic>,
    ) -> Result<Resource<crate::StreamSubscription>, bridge_types::Error> {
        let topics: Vec<crate::Topic> = topics
            .into_iter()
            .map(|topic| match topic {
                bridge_host::Topic::TextDelta => crate::Topic::TextDelta,
                bridge_host::Topic::AudioDelta => crate::Topic::AudioDelta,
            })
            .collect();
        let subscription = crate::subscribe(&self.channel, &topics).map_err(channel_error)?;
        self.table.push(subscription).map_err(|_| {
            bridge_types::Error::Invalid("host.subscribe: the resource table is full".into())
        })
    }
}

/// The subscription's own methods; dropping it IS the unsubscribe.
impl bridge_host::HostSubscription for BridgeState {
    async fn poll(
        &mut self,
        subscription: Resource<crate::StreamSubscription>,
    ) -> Vec<bridge_host::StreamEvent> {
        let Ok(sub) = self.table.get_mut(&subscription) else {
            eprintln!("tau host.poll: the subscription resource is not in the table");
            return Vec::new();
        };
        crate::poll(sub).into_iter().map(polled_to_bridge).collect()
    }

    async fn drop(
        &mut self,
        subscription: Resource<crate::StreamSubscription>,
    ) -> wasmtime::Result<()> {
        self.table.delete(subscription)?;
        Ok(())
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
/// children -- they are resources in the dropped store, and `HostChild`'s
/// own drop is what kills them.
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
        let state = BridgeState {
            ctx: ctx.build(),
            table: ResourceTable::new(),
            http: std::sync::Arc::new(std::sync::Mutex::new(HttpRegistry::new(
                self.consent.origins.clone(),
            ))),
            ws: std::sync::Arc::new(std::sync::Mutex::new(crate::ws::WsRegistry::new(
                self.consent.origins.clone(),
            ))),
            channel: self.channel.clone(),
            inject: self.inject,
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
        let definitions = {
            let BridgeInstance { store, bindings } = &mut instance;
            store
                .run_concurrent(async |acc| {
                    bindings.tau_extension_tools().call_definitions(acc).await
                })
                .await
        };
        let definitions = definitions
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("bridge handshake failed: {}", crate::compact_wasm_error(&e)),
            })?
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

        // The contract types the point list since 0.7.0: "unknown point
        // name" is unrepresentable, so there is nothing to filter.
        let points: Vec<ProbePoint> = points.into_iter().map(point_from_bridge).collect();
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

// ---------------------------------------------------------------------------
// Probe payloads, both directions. The bridge world's bindgen generates its
// own copies of the contract's types, so each direction is a field-for-field
// translation into (or out of) the host bindings' shapes; the meaning of a
// payload -- and of a replacement -- is defined once, in `convert`.
// ---------------------------------------------------------------------------

/// Core → bridge bindings for a probe point (the contract's `point` enum is
/// the host's point set, arm for arm).
fn point_to_bridge(point: ProbePoint) -> bridge_probes::Point {
    match point {
        ProbePoint::BeforeRun => bridge_probes::Point::BeforeRun,
        ProbePoint::TransformContext => bridge_probes::Point::TransformContext,
        ProbePoint::BeforeRequest => bridge_probes::Point::BeforeRequest,
        ProbePoint::AfterResponse => bridge_probes::Point::AfterResponse,
        ProbePoint::BeforeTool => bridge_probes::Point::BeforeTool,
        ProbePoint::AfterTool => bridge_probes::Point::AfterTool,
        ProbePoint::BeforeRunEnd => bridge_probes::Point::BeforeRunEnd,
        ProbePoint::BeforeCompaction => bridge_probes::Point::BeforeCompaction,
        ProbePoint::BeforeNavigation => bridge_probes::Point::BeforeNavigation,
        ProbePoint::SessionStart => bridge_probes::Point::SessionStart,
        ProbePoint::Branch => bridge_probes::Point::Branch,
        ProbePoint::SessionEnd => bridge_probes::Point::SessionEnd,
    }
}

/// Bridge bindings -> core for a probe point (the inverse of
/// [`point_to_bridge`]; the contract types the set, so there is no
/// unknown-name case any more).
fn point_from_bridge(point: bridge_probes::Point) -> ProbePoint {
    match point {
        bridge_probes::Point::BeforeRun => ProbePoint::BeforeRun,
        bridge_probes::Point::TransformContext => ProbePoint::TransformContext,
        bridge_probes::Point::BeforeRequest => ProbePoint::BeforeRequest,
        bridge_probes::Point::AfterResponse => ProbePoint::AfterResponse,
        bridge_probes::Point::BeforeTool => ProbePoint::BeforeTool,
        bridge_probes::Point::AfterTool => ProbePoint::AfterTool,
        bridge_probes::Point::BeforeRunEnd => ProbePoint::BeforeRunEnd,
        bridge_probes::Point::BeforeCompaction => ProbePoint::BeforeCompaction,
        bridge_probes::Point::BeforeNavigation => ProbePoint::BeforeNavigation,
        bridge_probes::Point::SessionStart => ProbePoint::SessionStart,
        bridge_probes::Point::Branch => ProbePoint::Branch,
        bridge_probes::Point::SessionEnd => ProbePoint::SessionEnd,
    }
}

/// Core → bridge bindings for a probe firing's payload: one arm per point,
/// the same arm the point carries (the twin of `convert::payload_to_wit`).
fn payload_to_bridge(payload: &ProbePayload) -> bridge_probes::Payload {
    match payload {
        ProbePayload::BeforeRun(p) => {
            bridge_probes::Payload::BeforeRun(message_to_bridge(&p.prompt))
        }
        ProbePayload::TransformContext(p) => {
            bridge_probes::Payload::TransformContext(bridge_probes::AssembledContext {
                system: p.system.clone(),
                messages: p.messages.iter().map(message_to_bridge).collect(),
            })
        }
        ProbePayload::BeforeRequest(p) => {
            bridge_probes::Payload::BeforeRequest(bridge_probes::FinalRequest {
                system: p.system.clone(),
                messages: p.messages.iter().map(message_to_bridge).collect(),
                tools: p.tools.iter().map(definition_to_bridge).collect(),
            })
        }
        ProbePayload::AfterResponse(p) => {
            bridge_probes::Payload::AfterResponse(bridge_probes::AssembledResponse {
                message: message_to_bridge(&p.message),
                stop: stop_to_bridge(p.stop),
            })
        }
        ProbePayload::BeforeTool(call) => {
            bridge_probes::Payload::BeforeTool(tool_call_to_bridge(call))
        }
        ProbePayload::AfterTool(p) => {
            bridge_probes::Payload::AfterTool(bridge_probes::ToolOutcome {
                call: tool_call_to_bridge(&p.call),
                content: p.content.iter().map(result_block_to_bridge).collect(),
                is_error: p.is_error,
            })
        }
        ProbePayload::BeforeRunEnd(p) => bridge_probes::Payload::BeforeRunEnd(bridge_probes::RunEnd {
            messages: p.messages.iter().map(message_to_bridge).collect(),
            stop: stop_to_bridge(p.stop),
        }),
        ProbePayload::BeforeCompaction(p) => {
            bridge_probes::Payload::BeforeCompaction(bridge_probes::Compaction {
                reason: p.reason.clone(),
                messages: p.messages.iter().map(message_to_bridge).collect(),
            })
        }
        ProbePayload::BeforeNavigation(p) => {
            bridge_probes::Payload::BeforeNavigation(bridge_probes::Navigation {
                target: p.target.clone(),
                summary: p.summary.clone(),
            })
        }
        ProbePayload::SessionStart(p) => {
            bridge_probes::Payload::SessionStart(bridge_probes::SessionFacts {
                session: p.session.clone(),
                cwd: p.cwd.clone(),
                model: p.model.clone(),
            })
        }
        ProbePayload::Branch(p) => bridge_probes::Payload::Branch(bridge_probes::Branch {
            previous: p.previous.clone(),
            to: p.to.clone(),
        }),
        ProbePayload::SessionEnd(p) => bridge_probes::Payload::SessionEnd(
            bridge_probes::SessionFacts {
                session: p.session.clone(),
                cwd: p.cwd.clone(),
                model: p.model.clone(),
            },
        ),
    }
}

/// Bridge-world result block -> host-bindings result block. The two bindgen
/// invocations generate distinct types for the same WIT shapes.
fn bridge_block_to_host(
    block: bridge_types::ResultBlock,
) -> crate::bindings::tau::extension::types::ResultBlock {
    use crate::bindings::tau::extension::types as ht;
    match block {
        bridge_types::ResultBlock::Text(text) => ht::ResultBlock::Text(text),
        bridge_types::ResultBlock::Image(media) => {
            ht::ResultBlock::Image(bridge_media_to_host(media))
        }
        bridge_types::ResultBlock::Audio(media) => {
            ht::ResultBlock::Audio(bridge_media_to_host(media))
        }
        bridge_types::ResultBlock::Video(media) => {
            ht::ResultBlock::Video(bridge_media_to_host(media))
        }
        bridge_types::ResultBlock::File(file) => ht::ResultBlock::File(ht::File {
            media: bridge_media_to_host(file.media),
            name: file.name,
        }),
    }
}

/// Bridge-world media -> host-bindings media (no name: the filename lives on
/// the `file` arm, as it does in Rust).
fn bridge_media_to_host(
    media: bridge_types::Media,
) -> crate::bindings::tau::extension::types::Media {
    use crate::bindings::tau::extension::types as ht;
    ht::Media {
        media_type: media.media_type,
        source: match media.source {
            bridge_types::MediaSource::Bytes(bytes) => ht::MediaSource::Bytes(bytes),
            bridge_types::MediaSource::Url(url) => ht::MediaSource::Url(url),
            bridge_types::MediaSource::Blob(hash) => ht::MediaSource::Blob(hash),
        },
    }
}

/// Bridge-world message content block -> host-bindings content block.
fn bridge_content_to_host(
    content: bridge_types::Content,
) -> crate::bindings::tau::extension::types::Content {
    use crate::bindings::tau::extension::types as ht;
    match content {
        bridge_types::Content::Text(text) => ht::Content::Text(text),
        bridge_types::Content::Image(media) => ht::Content::Image(bridge_media_to_host(media)),
        bridge_types::Content::Audio(media) => ht::Content::Audio(bridge_media_to_host(media)),
        bridge_types::Content::Video(media) => ht::Content::Video(bridge_media_to_host(media)),
        bridge_types::Content::File(file) => ht::Content::File(ht::File {
            media: bridge_media_to_host(file.media),
            name: file.name,
        }),
        bridge_types::Content::ToolCall(call) => {
            ht::Content::ToolCall(tool_call_from_bridge(call))
        }
        bridge_types::Content::ToolResult(result) => ht::Content::ToolResult(ht::ToolResult {
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

/// Bridge-world message -> host-bindings message (for steer/follow-up and
/// for the probe payloads).
fn bridge_message_to_host(
    message: bridge_types::Message,
) -> crate::bindings::tau::extension::types::Message {
    use crate::bindings::tau::extension::types as ht;
    ht::Message {
        role: match message.role {
            bridge_types::Role::User => ht::Role::User,
            bridge_types::Role::Assistant => ht::Role::Assistant,
            bridge_types::Role::Tool => ht::Role::Tool,
        },
        content: message
            .content
            .into_iter()
            .map(bridge_content_to_host)
            .collect(),
    }
}

/// Bridge bindings → the host bindings' payload, so a `replace` verdict is
/// folded by the same code the extension world's is (one meaning for a
/// replacement, two generated type sets).
fn payload_from_bridge(
    payload: bridge_probes::Payload,
) -> crate::bindings::exports::tau::extension::probes::Payload {
    use crate::bindings::exports::tau::extension::probes as host_probes;
    match payload {
        bridge_probes::Payload::BeforeRun(message) => {
            host_probes::Payload::BeforeRun(bridge_message_to_host(message))
        }
        bridge_probes::Payload::TransformContext(p) => {
            host_probes::Payload::TransformContext(host_probes::AssembledContext {
                system: p.system,
                messages: p.messages.into_iter().map(bridge_message_to_host).collect(),
            })
        }
        bridge_probes::Payload::BeforeRequest(p) => {
            host_probes::Payload::BeforeRequest(host_probes::FinalRequest {
                system: p.system,
                messages: p.messages.into_iter().map(bridge_message_to_host).collect(),
                tools: p.tools.into_iter().map(definition_from_bridge).collect(),
            })
        }
        bridge_probes::Payload::AfterResponse(p) => {
            host_probes::Payload::AfterResponse(host_probes::AssembledResponse {
                message: bridge_message_to_host(p.message),
                stop: stop_from_bridge(p.stop),
            })
        }
        bridge_probes::Payload::BeforeTool(call) => {
            host_probes::Payload::BeforeTool(tool_call_from_bridge(call))
        }
        bridge_probes::Payload::AfterTool(p) => {
            host_probes::Payload::AfterTool(crate::bindings::exports::tau::extension::probes::ToolOutcome {
                call: tool_call_from_bridge(p.call),
                content: p.content.into_iter().map(bridge_block_to_host).collect(),
                is_error: p.is_error,
            })
        }
        bridge_probes::Payload::BeforeRunEnd(p) => {
            host_probes::Payload::BeforeRunEnd(host_probes::RunEnd {
                messages: p.messages.into_iter().map(bridge_message_to_host).collect(),
                stop: stop_from_bridge(p.stop),
            })
        }
        bridge_probes::Payload::BeforeCompaction(p) => {
            host_probes::Payload::BeforeCompaction(host_probes::Compaction {
                reason: p.reason,
                messages: p.messages.into_iter().map(bridge_message_to_host).collect(),
            })
        }
        bridge_probes::Payload::BeforeNavigation(p) => {
            host_probes::Payload::BeforeNavigation(host_probes::Navigation {
                target: p.target,
                summary: p.summary,
            })
        }
        bridge_probes::Payload::SessionStart(p) => host_probes::Payload::SessionStart(
            host_probes::SessionFacts {
                session: p.session,
                cwd: p.cwd,
                model: p.model,
            },
        ),
        bridge_probes::Payload::Branch(p) => host_probes::Payload::Branch(host_probes::Branch {
            previous: p.previous,
            to: p.to,
        }),
        bridge_probes::Payload::SessionEnd(p) => host_probes::Payload::SessionEnd(
            host_probes::SessionFacts {
                session: p.session,
                cwd: p.cwd,
                model: p.model,
            },
        ),
    }
}

/// Fold a bridge component's `replace` back into the typed payload: the
/// bridge's bindgen types become the host bindings', and the shared folder
/// (`crate::replace_probe_payload`) does the rest -- including the
/// point/payload pairing check.
fn replace_bridge_payload(point: ProbePoint, payload: bridge_probes::Payload) -> Verdict {
    crate::replace_probe_payload(point, payload_from_bridge(payload))
}

// ---- core → bridge bindings ----------------------------------------------

fn message_to_bridge(message: &tau_core::types::Message) -> bridge_types::Message {
    bridge_types::Message {
        role: role_to_bridge(message.role),
        content: message.content.iter().map(content_to_bridge).collect(),
    }
}

fn role_to_bridge(role: tau_core::types::Role) -> bridge_types::Role {
    match role {
        tau_core::types::Role::User => bridge_types::Role::User,
        tau_core::types::Role::Assistant => bridge_types::Role::Assistant,
        tau_core::types::Role::Tool => bridge_types::Role::Tool,
    }
}

fn content_to_bridge(content: &tau_core::types::Content) -> bridge_types::Content {
    use tau_core::types::Content;
    match content {
        Content::Text { text } => bridge_types::Content::Text(text.clone()),
        Content::Image { media } => bridge_types::Content::Image(media_to_bridge(media)),
        Content::Audio { media } => bridge_types::Content::Audio(media_to_bridge(media)),
        Content::Video { media } => bridge_types::Content::Video(media_to_bridge(media)),
        Content::File { media, name } => bridge_types::Content::File(file_to_bridge(media, name)),
        Content::ToolCall {
            id,
            name,
            arguments,
        } => bridge_types::Content::ToolCall(bridge_types::ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments_json: arguments.to_string(),
        }),
        Content::ToolResult {
            call_id,
            content,
            is_error,
        } => bridge_types::Content::ToolResult(bridge_types::ToolResult {
            call_id: call_id.clone(),
            content: content
                .iter()
                .map(content_block_to_bridge)
                .collect(),
            is_error: *is_error,
        }),
    }
}

fn media_to_bridge(media: &tau_core::types::Media) -> bridge_types::Media {
    bridge_types::Media {
        media_type: media.media_type.clone(),
        source: match &media.source {
            tau_core::types::MediaSource::Bytes(bytes) => {
                bridge_types::MediaSource::Bytes(bytes.clone())
            }
            tau_core::types::MediaSource::Url(url) => {
                bridge_types::MediaSource::Url(url.clone())
            }
            tau_core::types::MediaSource::Blob { hash } => {
                bridge_types::MediaSource::Blob(hash.clone())
            }
        },
    }
}

fn file_to_bridge(media: &tau_core::types::Media, name: &Option<String>) -> bridge_types::File {
    bridge_types::File {
        media: media_to_bridge(media),
        name: name.clone(),
    }
}

/// The wire's `result-block` is narrower than `content` (the toolchain
/// refuses a recursive `content`), so a nested call or result degrades to
/// its text projection -- the same narrowing `convert` makes.
fn content_block_to_bridge(block: &tau_core::types::Content) -> bridge_types::ResultBlock {
    match tau_core::types::ResultBlock::try_from(block.clone()) {
        Ok(block) => result_block_to_bridge(&block),
        Err(_) => bridge_types::ResultBlock::Text(tau_core::types::tool_result_text(
            std::slice::from_ref(block),
        )),
    }
}

fn result_block_to_bridge(block: &tau_core::types::ResultBlock) -> bridge_types::ResultBlock {
    use tau_core::types::ResultBlock;
    match block {
        ResultBlock::Text { text } => bridge_types::ResultBlock::Text(text.clone()),
        ResultBlock::Image { media } => bridge_types::ResultBlock::Image(media_to_bridge(media)),
        ResultBlock::Audio { media } => bridge_types::ResultBlock::Audio(media_to_bridge(media)),
        ResultBlock::Video { media } => bridge_types::ResultBlock::Video(media_to_bridge(media)),
        ResultBlock::File { media, name } => {
            bridge_types::ResultBlock::File(file_to_bridge(media, name))
        }
    }
}

fn tool_call_to_bridge(call: &tau_core::types::ToolCall) -> bridge_types::ToolCall {
    bridge_types::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments_json: call.arguments.to_string(),
    }
}

fn definition_to_bridge(def: &ToolDef) -> bridge_tools::Definition {
    bridge_tools::Definition {
        name: def.name.clone(),
        description: def.description.clone(),
        parameters_json: def.parameters.to_string(),
    }
}

fn stop_to_bridge(stop: tau_core::model::StopReason) -> bridge_types::StopReason {
    use tau_core::model::StopReason;
    match stop {
        StopReason::Stop => bridge_types::StopReason::Stop,
        StopReason::ToolUse => bridge_types::StopReason::ToolUse,
        StopReason::Length => bridge_types::StopReason::Length,
        StopReason::Error => bridge_types::StopReason::Error,
        StopReason::Aborted => bridge_types::StopReason::Aborted,
    }
}

// ---- bridge bindings → host bindings -------------------------------------

fn tool_call_from_bridge(
    call: bridge_types::ToolCall,
) -> crate::bindings::tau::extension::types::ToolCall {
    crate::bindings::tau::extension::types::ToolCall {
        id: call.id,
        name: call.name,
        arguments_json: call.arguments_json,
    }
}

fn definition_from_bridge(
    def: bridge_tools::Definition,
) -> crate::bindings::exports::tau::extension::tools::Definition {
    crate::bindings::exports::tau::extension::tools::Definition {
        name: def.name,
        description: def.description,
        parameters_json: def.parameters_json,
    }
}

fn stop_from_bridge(
    stop: bridge_types::StopReason,
) -> crate::bindings::tau::extension::types::StopReason {
    use crate::bindings::tau::extension::types as ht;
    match stop {
        bridge_types::StopReason::Stop => ht::StopReason::Stop,
        bridge_types::StopReason::ToolUse => ht::StopReason::ToolUse,
        bridge_types::StopReason::Length => ht::StopReason::Length,
        bridge_types::StopReason::Error => ht::StopReason::Error,
        bridge_types::StopReason::Aborted => ht::StopReason::Aborted,
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
        // The export is async (0.7.0), so the call goes through the store's
        // concurrent driver: one drive per invocation. The bridge's own
        // protocol traffic (http, process, ws) is made of imports the guest
        // awaits inside this same drive -- which is what lets a `tools/call`
        // wait on its server without parking a host thread.
        let result = {
            let BridgeInstance { store, bindings } = &mut guard.instance;
            store
                .run_concurrent(async |acc| {
                    bindings
                        .tau_extension_tools()
                        .call_execute(acc, name, arguments.to_string())
                        .await
                })
                .await
        };
        if result.is_err() {
            // The trap poisoned the guest; rebuild so the next call
            // reaches a fresh instance (which respawns its server)
            // instead of trapping forever.
            guard.revive().await;
        }
        match result {
            Ok(Ok(r)) => match crate::convert::result_blocks_to_core(
                // The bridge world bindgen has its own copies of the types
                // interface; translate field-by-field into the host-side
                // bindings' shapes (identical by construction).
                r.content.into_iter().map(bridge_block_to_host).collect(),
            ) {
                Ok(content) => ToolOutput {
                    content: content
                        .into_iter()
                        .map(tau_core::types::Content::from)
                        .collect(),
                    is_error: r.is_error,
                },
                Err(e) => ToolOutput::err(format!("invalid tool result: {e}")),
            },
            // Two error layers: the drive's own failure and the guest's
            // trap. The model sees the same thing either way.
            Ok(Err(e)) | Err(e) => {
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
        let wit_point = point_to_bridge(point);
        let wit_payload = payload_to_bridge(&payload);
        let mut guard = self.shared.lock().await;
        let BridgeInstance { store, bindings } = &mut guard.instance;
        // `probe` is the one export still declared sync in the contract (a
        // probe is a decision point, not an I/O opportunity), so it lowers
        // against the store itself; only the ASYNC default makes the result
        // a future.
        let result = bindings
            .tau_extension_probes()
            .call_probe(store, wit_point, &wit_payload)
            .await;
        if result.is_err() {
            // The trap poisoned the guest; rebuild so the next probe
            // still decides instead of degrading forever.
            guard.revive().await;
        } else {
            // The asynchronous half of the same point (contract:
            // `bridge-io`). A bridge's I/O legs land exactly here — the IM
            // adapter's long connection at `session-start`, the platform
            // reply at `after-response` — and they are `async func`s that a
            // synchronously lowered export cannot await. Called after the
            // decision, so a probe that replaces or blocks the payload has
            // already been honored.
            let turn = {
                let BridgeInstance { store, bindings } = &mut guard.instance;
                store
                    .run_concurrent(async |acc| {
                        bindings
                            .tau_extension_bridge_io()
                            .call_turn(acc, wit_point, wit_payload.clone())
                            .await
                    })
                    .await
            };
            if let Err(error) = &turn {
                // Same posture as the probe above: a trap in the guest's
                // I/O half must not leave a poisoned instance behind.
                eprintln!(
                    "tau bridge: bridge-io.turn trapped: {}",
                    crate::compact_wasm_error(error)
                );
                guard.revive().await;
            }
        }
        match result {
            Ok(verdict) => match verdict {
                bridge_probes::Verdict::Continue => Verdict::Continue,
                bridge_probes::Verdict::Replace(payload) => {
                    replace_bridge_payload(point, payload)
                }
                bridge_probes::Verdict::Block(reason) => Verdict::Block { reason },
            },
            // A broken bridge degrades to Continue, never wedges the run.
            Err(_) => Verdict::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pipe deadlines are decided by one function for both directions:
    /// armed on the first silent round, fired only once the budget is spent,
    /// and disarmed by progress (a stream nobody is reading is never timed
    /// out -- the call sites arm it only when something is actually waiting).
    #[test]
    fn a_pipe_deadline_fires_only_after_its_budget() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("scratch runtime");
        rt.block_on(async {
            let idle = std::time::Duration::from_millis(120);
            let waker = std::task::Waker::noop();
            let mut cx = Context::from_waker(waker);
            let mut timer = None;
            assert!(!idle_expired(&mut timer, &mut cx, idle), "fired on the first poll");
            assert!(timer.is_some(), "the deadline was never armed");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert!(idle_expired(&mut timer, &mut cx, idle), "the deadline never fired");
            // Progress clears it; the next silence starts a fresh budget.
            timer = None;
            assert!(!idle_expired(&mut timer, &mut cx, idle), "a disarmed deadline fired");
        });
    }

    /// A child that says one line and exits: the reader thread queues it
    /// and then hits EOF.
    fn echo_child_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/c".into(), "echo hello".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["echo".into(), "hello".into()]
        }
    }

    /// A child that stays alive for several seconds. It is argv[0] itself
    /// (not a shell running a command), so killing it closes the pipe: a
    /// `cmd /c ...` child would leave its own grandchild holding the write
    /// end and the drop test would read "still alive" from a dead child.
    fn long_child_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["ping".into(), "-n".into(), "6".into(), "127.0.0.1".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["sleep".into(), "5".into()]
        }
    }

    /// Wait for one condition on a pipe queue, bounded (a test must fail,
    /// never hang).
    fn wait_for(
        queue: &PipeQueue,
        mut done: impl FnMut(&mut tokio::sync::mpsc::UnboundedReceiver<Result<Vec<u8>, String>>) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
            if done(&mut queue) {
                return true;
            }
            drop(queue);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    /// The guest's stdout stream is fed by a reader thread: what the child
    /// writes is queued for whoever holds the resource (the stream producer
    /// takes it from the same queue).
    #[test]
    fn spawn_child_queues_stdout_for_the_guest() {
        let mut child = spawn_child(&echo_child_argv()).expect("spawn");
        let queue = child.stdout.take().expect("stdout");
        let mut text = Vec::new();
        let saw_eof = wait_for(&queue, |queue| loop {
            match queue.try_recv() {
                Ok(Ok(chunk)) => text.extend_from_slice(&chunk),
                Ok(Err(why)) => panic!("pipe error: {why}"),
                // Every chunk is in `text` once the sender is gone.
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return true,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return false,
            }
        });
        assert!(saw_eof, "stdout never reached EOF");
        let text = String::from_utf8_lossy(&text);
        assert!(text.contains("hello"), "stdout was {text:?}");
    }

    /// Dropping a child kills it: a bridge's leftover server must not
    /// outlive the instance that spawned it (0.6.0's registry killed them
    /// on drop, and ownership moved that duty onto the resource).
    #[test]
    fn dropping_a_child_kills_it() {
        let mut child = spawn_child(&long_child_argv()).expect("spawn");
        let queue = child.stdout.take().expect("stdout");
        drop(child);
        let closed = wait_for(&queue, |queue| {
            matches!(
                queue.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            )
        });
        assert!(
            closed,
            "the child outlived its resource (it still holds the pipe open)"
        );
    }

    /// An already-exited child drops without drama: the kill is
    /// best-effort, and there is nothing left to kill.
    #[test]
    fn dropping_an_exited_child_is_quiet() {
        let child = spawn_child(&echo_child_argv()).expect("spawn");
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(child);
    }
}
