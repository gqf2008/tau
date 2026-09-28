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
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tau_core::probe::{ProbeHandler, ProbePoint, Verdict};
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
    processes: ProcessRegistry,
    http: HttpRegistry,
    ws: crate::ws::WsRegistry,
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

/// One spawned child: piped stdin, stdout drained by a reader thread into a
/// channel so `read-stdout` never blocks the wasm engine on a raw pipe.
struct ChildProcess {
    child: Child,
    stdin: ChildStdin,
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    pending: VecDeque<u8>,
    eof: bool,
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
        let stdin = child.stdin.take().ok_or("spawn: no stdin pipe")?;
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
        let handle = ((self.generation as u64) << 32) | (self.next as u64);
        self.next += 1;
        self.children.insert(
            handle,
            ChildProcess {
                child,
                stdin,
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
    fn request(
        &mut self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<u64, String> {
        self.http.request(&method, &url, &headers, &body, timeout_ms)
    }

    fn status(&mut self, handle: u64) -> Result<u16, String> {
        self.http.status(handle)
    }

    fn header(&mut self, handle: u64, name: String) -> Result<Option<String>, String> {
        self.http.header(handle, &name)
    }

    fn read_body(&mut self, handle: u64, max: u32, timeout_ms: u32) -> Result<(Vec<u8>, bool), String> {
        self.http.read_body(handle, max, timeout_ms)
    }

    fn close(&mut self, handle: u64) {
        self.http.close(handle);
    }
}

impl bridge_bindings::tau::extension::ws::Host for BridgeState {
    fn connect(&mut self, url: String, timeout_ms: u32) -> Result<u64, String> {
        self.ws.connect(&url, timeout_ms)
    }

    fn send(
        &mut self,
        handle: u64,
        frame: bridge_bindings::tau::extension::ws::Frame,
    ) -> Result<(), String> {
        use bridge_bindings::tau::extension::ws::Frame;
        let frame = match frame {
            Frame::Text(t) => crate::ws::WsFrame::Text(t),
            Frame::Binary(b) => crate::ws::WsFrame::Binary(b),
        };
        self.ws.send(handle, frame)
    }

    fn recv(
        &mut self,
        handle: u64,
        timeout_ms: u32,
    ) -> Result<bridge_bindings::tau::extension::ws::Frame, String> {
        use bridge_bindings::tau::extension::ws::Frame;
        match self.ws.recv(handle, timeout_ms)? {
            crate::ws::WsFrame::Text(t) => Ok(Frame::Text(t)),
            crate::ws::WsFrame::Binary(b) => Ok(Frame::Binary(b)),
        }
    }

    fn close(&mut self, handle: u64) -> Result<(), String> {
        self.ws.close(handle)
    }
}

/// The bridge world's host channel (docs/im-channels.md contract
/// amendment): the same ops as the extension world's, with the bridge
/// bindgen's own copies of the types converted field-by-field into the
/// host bindings' shapes (identical by construction, like
/// bridge_block_to_host below).
impl bridge_bindings::tau::extension::host::Host for BridgeState {
    fn notify(
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

    fn emit(&mut self, event_json: String) -> Result<(), String> {
        crate::channel_emit(&self.channel, event_json)
    }

    fn steer(
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

    fn follow_up(
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

    fn subscribe(&mut self, topics: Vec<String>) -> Result<u64, String> {
        crate::subscribe_topics(
            &self.channel,
            &mut self.subscriptions,
            &mut self.next_subscription,
            &topics,
        )
    }

    fn poll(
        &mut self,
        subscription: u64,
    ) -> Result<Vec<bridge_bindings::tau::extension::host::StreamEvent>, String> {
        crate::poll_subscription(&mut self.subscriptions, subscription)
            .map(|events| events.into_iter().map(host_event_to_bridge).collect())
    }

    fn unsubscribe(&mut self, subscription: u64) -> Result<(), String> {
        crate::unsubscribe_subscription(&mut self.subscriptions, subscription)
    }
}

/// Webhook ingress: consent is the listen address (CLI --ingress);
/// the registry owns routes/servers and the push dispatch
/// (docs/im-channels.md). Host stays a pipe.
impl bridge_bindings::tau::extension::ingress::Host for BridgeState {
    fn listen(&mut self, route: String) -> Result<(), String> {
        // Arc<S: listen takes &Arc<Self> for server spawning; clone the
        // Arc out of the state (the registry outlives any one instance).
        let registry = self.ingress.clone();
        registry.listen(&route)
    }

    fn close(&mut self, route: String) -> Result<(), String> {
        self.ingress.close(&route)
    }
}

/// Push one inbound webhook request into the component's
/// ingress-handler export, synchronously, under the instance lock. A
/// trap poisons the guest: revive so the NEXT request lands on a fresh
/// instance, and answer this one 502 (the platform retries — a retried
/// webhook is a platform fact, not a loss).
pub(crate) fn ingress_dispatch(
    shared: &SharedBridge,
    request: bridge_bindings::exports::tau::extension::ingress_handler::Request,
) -> Result<bridge_bindings::exports::tau::extension::ingress_handler::Response, String> {
    let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
    let BridgeInstance { store, bindings } = &mut guard.instance;
    let result = bindings
        .tau_extension_ingress_handler()
        .call_handle_request(store, &request);
    match result {
        Ok(response) => Ok(response),
        Err(e) => {
            guard.revive();
            Err(format!(
                "component trapped handling the webhook: {}",
                crate::compact_wasm_error(&e)
            ))
        }
    }
}

impl bridge_bindings::tau::extension::process::Host for BridgeState {
    fn spawn(&mut self, argv: Vec<String>) -> Result<u64, String> {
        self.processes.spawn(&argv)
    }

    fn write_stdin(&mut self, handle: u64, data: Vec<u8>) -> Result<(), String> {
        let child = self.processes.get(handle)?;
        child
            .stdin
            .write_all(&data)
            .and_then(|()| child.stdin.flush())
            .map_err(|e| format!("write-stdin: {e}"))
    }

    fn read_stdout(
        &mut self,
        handle: u64,
        max: u32,
        timeout_ms: u32,
    ) -> Result<(Vec<u8>, bool), String> {
        self.processes.read_stdout(handle, max, timeout_ms)
    }

    fn kill(&mut self, handle: u64) -> Result<(), String> {
        // Validates generation and existence (0.2.0: kill failures are
        // reported, not swallowed — wit-review F8).
        self.processes.get(handle)?;
        let mut child = self
            .processes
            .children
            .remove(&handle)
            .expect("checked above");
        child.child.kill().map_err(|e| format!("kill: {e}"))?;
        child.child.wait().map_err(|e| format!("kill: wait: {e}"))?;
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
    /// [`ProcessRegistry`]). Guarded by the SharedBridgeInstance mutex.
    generation: std::cell::Cell<u32>,
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
    fn instantiate(&self) -> Result<BridgeInstance, wasmtime::Error> {
        let mut ctx = self.wasi.ctx_builder();
        if let Some(command) = &self.consent.command {
            let command_json = serde_json::to_string(command).unwrap_or_else(|_| "[]".into());
            ctx.env("TAU_MCP_COMMAND", &command_json);
        }
        if let Some(url) = &self.consent.mcp_url {
            ctx.env("TAU_MCP_URL", url);
        }
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let state = BridgeState {
            ctx: ctx.build(),
            table: ResourceTable::new(),
            processes: ProcessRegistry::new(generation),
            http: HttpRegistry::new(self.consent.origins.clone()),
            ws: crate::ws::WsRegistry::new(generation, self.consent.origins.clone()),
            channel: self.channel.clone(),
            inject: self.inject,
            subscriptions: HashMap::new(),
            next_subscription: 0,
            ingress: self.ingress.clone(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings =
            bridge_bindings::Bridge::instantiate(&mut store, &self.component, &self.linker)?;
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
    fn revive(&mut self) {
        if let Ok(fresh) = self.factory.instantiate() {
            self.instance = fresh;
        }
    }
}

pub(crate) type SharedBridge = Arc<Mutex<SharedBridgeInstance>>;

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
        Self::off_runtime(move || self.load_bridge_inner(&path, consent))
    }

    fn load_bridge_inner(
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
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
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
            generation: std::cell::Cell::new(0),
            ingress,
        };
        let mut instance = factory.instantiate().map_err(|e| ExtError::Load {
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
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("bridge points() trapped: {}", crate::compact_wasm_error(&e)),
            })?;

        let shared: SharedBridge = Arc::new(Mutex::new(SharedBridgeInstance { instance, factory }));
        // Late-bind the dispatch target: requests arriving between the
        // listener's first accept and this line answer 503, never
        // dispatch into a half-built bridge.
        shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .factory
            .ingress
            .bind(&shared);
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
        let shared = self.shared.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let BridgeInstance { store, bindings } = &mut guard.instance;
            let result =
                bindings
                    .tau_extension_tools()
                    .call_execute(store, &name, &arguments.to_string());
            if result.is_err() {
                // The trap poisoned the guest; rebuild so the next call
                // reaches a fresh instance (which respawns its server)
                // instead of trapping forever.
                guard.revive();
            }
            result
        })
        .await;
        match result {
            Ok(Ok(r)) => match crate::convert::tool_result_blocks_to_core(
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
            Ok(Err(e)) => {
                ToolOutput::err(format!("bridge trap: {}", crate::compact_wasm_error(&e)))
            }
            Err(e) => ToolOutput::err(format!("bridge task failed: {e}")),
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

    async fn probe(&self, point: ProbePoint, payload: serde_json::Value) -> Verdict {
        let shared = self.shared.clone();
        let point_name = point.name().to_string();
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let BridgeInstance { store, bindings } = &mut guard.instance;
            let result =
                bindings
                    .tau_extension_probes()
                    .call_probe(store, &point_name, &payload.to_string());
            if result.is_err() {
                // The trap poisoned the guest; rebuild so the next probe
                // still decides instead of degrading forever.
                guard.revive();
            }
            result
        })
        .await;
        use bridge_bindings::exports::tau::extension::probes::Action;
        match result {
            Ok(Ok(verdict)) => match verdict.action {
                Action::Continue => Verdict::Continue,
                Action::Replace => verdict
                    .payload_json
                    .and_then(|p| serde_json::from_str(&p).ok())
                    .map(Verdict::Replace)
                    .unwrap_or(Verdict::Continue),
                Action::Block => Verdict::Block {
                    reason: verdict.reason.unwrap_or_else(|| "blocked".into()),
                },
            },
            // A broken bridge degrades to Continue, never wedges the run.
            Ok(Err(_)) | Err(_) => Verdict::Continue,
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
}
