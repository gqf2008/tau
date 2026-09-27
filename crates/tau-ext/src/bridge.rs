//! Bridge components (world "bridge"): external tool protocols (MCP) behind
//! WIT. The host grants exactly two scoped capabilities, and only on
//! explicit consent: spawn-with-pipes (the caller passes the allowed
//! command argv) and origin-allowlisted HTTP (the caller passes the
//! allowed origins). The host knows nothing about MCP; the bridge
//! component speaks whatever protocol it likes over the pipes. (Ambient
//! WASI follows the host's WasiPolicy — allow-all by default.)

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tau_core::tool::{Tool, ToolDef, ToolOutput};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::http::HttpRegistry;
use crate::{ExtError, ExtensionHost, WasiPolicy, bridge_bindings};

impl bridge_bindings::tau::extension::types::Host for BridgeState {}

struct BridgeState {
    ctx: WasiCtx,
    table: ResourceTable,
    processes: ProcessRegistry,
    http: HttpRegistry,
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
    ) -> Result<u64, String> {
        self.http.request(&method, &url, &headers, &body)
    }

    fn status(&mut self, handle: u64) -> Result<u16, String> {
        self.http.status(handle)
    }

    fn header(&mut self, handle: u64, name: String) -> Result<Option<String>, String> {
        self.http.header(handle, &name)
    }

    fn read_body(&mut self, handle: u64, max: u32) -> Result<(Vec<u8>, bool), String> {
        self.http.read_body(handle, max)
    }

    fn close(&mut self, handle: u64) {
        self.http.close(handle);
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

    fn read_stdout(&mut self, handle: u64, max: u32) -> Result<(Vec<u8>, bool), String> {
        let child = self.processes.get(handle)?;
        let max = max.max(1) as usize;
        // Block only until SOMETHING is available, then return immediately:
        // waiting to fill `max` would deadlock any peer that sends a short
        // message and then waits for a reply.
        if child.pending.is_empty() && !child.eof {
            match child.rx.recv() {
                Ok(Ok(chunk)) => child.pending.extend(chunk),
                Ok(Err(e)) => return Err(format!("read-stdout: {e}")),
                Err(_) => child.eof = true,
            }
        }
        let take = child.pending.len().min(max);
        let bytes: Vec<u8> = child.pending.drain(..take).collect();
        Ok((bytes, child.eof && child.pending.is_empty()))
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
    /// Bumped per instantiation; baked into process handles (see
    /// [`ProcessRegistry`]). Guarded by the SharedBridgeInstance mutex.
    generation: std::cell::Cell<u32>,
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
        };
        let mut store = Store::new(&self.engine, state);
        let bindings =
            bridge_bindings::Bridge::instantiate(&mut store, &self.component, &self.linker)?;
        Ok(BridgeInstance { store, bindings })
    }
}

struct SharedBridgeInstance {
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

type SharedBridge = Arc<Mutex<SharedBridgeInstance>>;

impl ExtensionHost {
    /// Load a bridge component. `consent` carries everything the bridge is
    /// allowed to touch: the spawn argv (delivered via TAU_MCP_COMMAND) and
    /// the HTTP origins it may reach (its endpoint via TAU_MCP_URL).
    /// Passing the consent IS the consent; with both empty the bridge loads
    /// but every capability call fails permission-denied.
    pub fn load_bridge(
        &self,
        path: impl AsRef<Path>,
        consent: BridgeConsent,
    ) -> Result<Vec<Box<dyn Tool>>, ExtError> {
        let path = path.as_ref().to_path_buf();
        Self::off_runtime(move || self.load_bridge_inner(&path, consent))
    }

    fn load_bridge_inner(
        &self,
        path: &Path,
        consent: BridgeConsent,
    ) -> Result<Vec<Box<dyn Tool>>, ExtError> {
        let bytes = self.read_verified(path)?;
        let component =
            Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        let mut linker: Linker<BridgeState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        bridge_bindings::Bridge::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let factory = BridgeFactory {
            engine: self.engine.clone(),
            component,
            linker,
            wasi: self.wasi,
            consent,
            generation: std::cell::Cell::new(0),
        };
        let mut instance = factory.instantiate().map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: format!(
                "bridge instantiation failed: {}",
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

        let shared: SharedBridge = Arc::new(Mutex::new(SharedBridgeInstance { instance, factory }));
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
        Ok(tools)
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
