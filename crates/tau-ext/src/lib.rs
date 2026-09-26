//! Wasm component extension host.
//!
//! Loads components implementing the `tau:extension` world and exposes their
//! tools to the agent and their probes to the harness. Ambient WASI
//! capabilities (fs/env/stdio/args/network) are granted by default
//! ([`WasiPolicy::AllowAll`]); [`WasiPolicy::DenyAll`] (CLI `--deny-wasi`)
//! restores the old deny-all sandbox. Custom capabilities — bridge
//! process/http, provider origins — stay consent-gated either way.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tau_core::probe::{ProbeHandler, ProbePoint, Verdict};
use tau_core::tool::{Tool, ToolDef, ToolOutput};
use futures::StreamExt;
use thiserror::Error;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/tau.wit",
        world: "extension",
    });
}

mod provider_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/tau.wit",
        world: "provider",
    });
}

mod bridge_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/tau.wit",
        world: "bridge",
    });
}

pub mod bridge;
mod http;
pub mod consent;
pub mod oci;
pub mod sign;

#[derive(Debug, Error)]
pub enum ExtError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("extension {path}: {reason}")]
    Load { path: String, reason: String },
}

/// Compact a wasmtime execution error for user-facing messages. A trap's
/// Display carries a multi-frame wasm backtrace that drowns the actual
/// failure — and the guest's panic message already reached its inherited
/// stderr — so keep the one-line summary plus the root cause.
pub(crate) fn compact_wasm_error(error: &wasmtime::Error) -> String {
    compact_error_display(&error.to_string(), &error.root_cause().to_string())
}

fn compact_error_display(display: &str, root: &str) -> String {
    let summary = display
        .lines()
        .next()
        .unwrap_or(display)
        .trim_end_matches(" at wasm backtrace:")
        .to_string();
    // No context chain: root == the whole multiline Display. Only append
    // a root that adds one line of information.
    if root.is_empty() || root.contains('\n') || root == summary {
        summary
    } else {
        format!("{summary} ({root})")
    }
}

/// Host state handed to every component. The WasiCtx follows the host's
/// [`WasiPolicy`]: allow-all inherits the process's capabilities;
/// deny-all links the WASI interfaces but fails every capability call
/// permission-denied.
struct ComponentState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for ComponentState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

struct ComponentInstance {
    store: Store<ComponentState>,
    bindings: bindings::Extension,
}

type Shared = Arc<Mutex<ComponentInstance>>;

/// What a loaded component contributes, unpacked.
pub type LoadedParts = (Vec<Box<dyn Tool>>, Vec<Box<dyn ProbeHandler>>);

/// Everything one loaded component contributes.
pub struct LoadedExtension {
    pub name: String,
    tools: Vec<Box<dyn Tool>>,
    probes: Vec<Box<dyn ProbeHandler>>,
}

impl LoadedExtension {
    pub fn into_parts(self) -> LoadedParts {
        (self.tools, self.probes)
    }
}

/// Ambient WASI capabilities granted to loaded components (fs, env,
/// stdio, args, network). Independent of the consent-gated custom
/// capabilities (bridge process/http, provider origins), which stay
/// explicit. Default: [`WasiPolicy::AllowAll`] — pass
/// [`WasiPolicy::DenyAll`] (CLI `--deny-wasi`) to restore the old
/// deny-all sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WasiPolicy {
    /// Inherit stdio/env/args, preopen the host filesystem (each drive
    /// on Windows, `/` elsewhere), inherit network + DNS.
    #[default]
    AllowAll,
    /// The old default: WASI interfaces link but every capability call
    /// fails permission-denied.
    DenyAll,
}

impl WasiPolicy {
    /// Base context for the policy; callers may add env vars before
    /// build (the bridge's TAU_MCP_* consent handoff).
    pub(crate) fn ctx_builder(self) -> WasiCtxBuilder {
        let mut ctx = WasiCtxBuilder::new();
        if self == WasiPolicy::AllowAll {
            ctx.inherit_stdio()
                .inherit_env()
                .inherit_args()
                .inherit_network()
                .allow_ip_name_lookup(true);
            preopen_host_fs(&mut ctx);
        }
        ctx
    }
}

/// Preopen the whole host filesystem read-write: `/` on unix, every
/// existing drive letter as `/<letter>` on Windows.
fn preopen_host_fs(ctx: &mut WasiCtxBuilder) {
    #[cfg(windows)]
    for letter in b'a'..=b'z' {
        let drive = format!("{}:\\", (letter as char).to_ascii_uppercase());
        if std::path::Path::new(&drive).is_dir() {
            let _ = ctx.preopened_dir(
                &drive,
                format!("/{letter}"),
                wasmtime_wasi::FsPerms::ReadWrite,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = ctx.preopened_dir("/", "/", wasmtime_wasi::FsPerms::ReadWrite);
}

pub struct ExtensionHost {
    engine: Engine,
    policy: sign::TrustPolicy,
    wasi: WasiPolicy,
}

impl Default for ExtensionHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionHost {
    /// Library default: unsigned components load. The CLI product uses
    /// `with_policy(RequireTrusted)` and gates this behind --allow-unsigned.
    pub fn new() -> Self {
        Self::with_policy(sign::TrustPolicy::AllowUnsigned)
    }

    pub fn with_policy(policy: sign::TrustPolicy) -> Self {
        Self {
            engine: Engine::new(&Config::new()).expect("wasmtime engine"),
            policy,
            wasi: WasiPolicy::default(),
        }
    }

    /// Set the ambient WASI policy (default allow-all).
    pub fn with_wasi(mut self, policy: WasiPolicy) -> Self {
        self.wasi = policy;
        self
    }

    /// Per-load WASI override: a host sharing this one's engine and
    /// trust policy but instantiating components under `policy`. Engine
    /// clones are cheap (Arc internals); use this for per-fingerprint
    /// remembered posture (a recalled deny tightens one component's load
    /// without touching the host default).
    pub fn with_wasi_policy(&self, policy: WasiPolicy) -> Self {
        Self {
            engine: self.engine.clone(),
            policy: self.policy.clone(),
            wasi: policy,
        }
    }

    /// Read a component file and enforce the trust policy on its bytes.
    fn read_verified(&self, path: &Path) -> Result<Vec<u8>, ExtError> {
        // Windows virus scanners briefly lock freshly-written files; a
        // component read right after an OCI pull can hit ERROR_ACCESS_DENIED
        // for a few hundred ms. Bounded retry on PermissionDenied only.
        let mut attempt = 0;
        let bytes = loop {
            match std::fs::read(path) {
                Ok(bytes) => break bytes,
                Err(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 10 =>
                {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => {
                    return Err(ExtError::Load {
                        path: path.display().to_string(),
                        reason: e.to_string(),
                    })
                }
            }
        };
        sign::check_policy(&bytes, &self.policy).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Ok(bytes)
    }

    /// Run component instantiation and entry-point calls on a plain OS
    /// thread when called from inside a tokio runtime: wasmtime-wasi's sync
    /// host functions block_on internally and panic on a runtime thread
    /// ("Cannot start a runtime from within a runtime").
    fn off_runtime<T: Send>(f: impl FnOnce() -> Result<T, ExtError> + Send) -> Result<T, ExtError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| scope.spawn(f).join().expect("loader thread"))
        } else {
            f()
        }
    }

    /// Load one component file. Ambient WASI access follows the host's
    /// [`WasiPolicy`] (allow-all by default; `--deny-wasi` to sandbox).
    pub fn load(&self, path: impl AsRef<Path>) -> Result<LoadedExtension, ExtError> {
        let path = path.as_ref().to_path_buf();
        Self::off_runtime(move || self.load_inner(&path))
    }

    fn load_inner(&self, path: &Path) -> Result<LoadedExtension, ExtError> {
        let bytes = self.read_verified(path)?;
        let component = Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        // WASI interfaces are linked so wasip2-std components instantiate;
        // what they may actually do follows the host's WasiPolicy.
        let mut linker: Linker<ComponentState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        let state = ComponentState {
            ctx: self.wasi.ctx_builder().build(),
            table: ResourceTable::new(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings = bindings::Extension::instantiate(&mut store, &component, &linker)
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!(
                    "instantiation failed (does it import capabilities the host does not grant?): {}",
                    compact_wasm_error(&e)
                ),
            })?;

        let definitions = bindings
            .tau_extension_tools()
            .call_definitions(&mut store)
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("definitions() trapped: {}", compact_wasm_error(&e)),
            })?;
        let points = bindings
            .tau_extension_hooks()
            .call_points(&mut store)
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("points() trapped: {}", compact_wasm_error(&e)),
            })?;

        let shared: Shared = Arc::new(Mutex::new(ComponentInstance { store, bindings }));

        let tools: Vec<Box<dyn Tool>> = definitions
            .into_iter()
            .map(|def| {
                Box::new(WasmTool {
                    def: ToolDef {
                        name: def.name,
                        description: def.description,
                        parameters: serde_json::from_str(&def.parameters_json)
                            .unwrap_or_else(|_| serde_json::json!({ "type": "object" })),
                    },
                    shared: shared.clone(),
                }) as Box<dyn Tool>
            })
            .collect();

        let points: Vec<ProbePoint> = points
            .iter()
            .filter_map(|name| ProbePoint::from_name(name))
            .collect();
        let probes: Vec<Box<dyn ProbeHandler>> = if points.is_empty() {
            Vec::new()
        } else {
            vec![Box::new(WasmProbes {
                points,
                shared: shared.clone(),
            })]
        };

        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "extension".into());
        Ok(LoadedExtension {
            name,
            tools,
            probes,
        })
    }
}

struct WasmTool {
    def: ToolDef,
    shared: Shared,
}

#[async_trait]
impl Tool for WasmTool {
    fn def(&self) -> ToolDef {
        self.def.clone()
    }

    async fn execute(&self, arguments: serde_json::Value) -> ToolOutput {
        let name = self.def.name.clone();
        let shared = self.shared.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let ComponentInstance { store, bindings } = &mut *guard;
            bindings
                .tau_extension_tools()
                .call_execute(store, &name, &arguments.to_string())
        })
        .await;
        match result {
            Ok(Ok(r)) => ToolOutput {
                content: r.content,
                is_error: r.is_error,
            },
            Ok(Err(e)) => ToolOutput::err(format!("wasm trap: {}", compact_wasm_error(&e))),
            Err(e) => ToolOutput::err(format!("extension task failed: {e}")),
        }
    }
}

struct WasmProbes {
    points: Vec<ProbePoint>,
    shared: Shared,
}

#[async_trait]
impl ProbeHandler for WasmProbes {
    fn points(&self) -> &[ProbePoint] {
        &self.points
    }

    async fn probe(&self, point: ProbePoint, payload: serde_json::Value) -> Verdict {
        let shared = self.shared.clone();
        let point_name = point.name().to_string();
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let ComponentInstance { store, bindings } = &mut *guard;
            bindings
                .tau_extension_hooks()
                .call_probe(store, &point_name, &payload.to_string())
        })
        .await;
        match result {
            Ok(Ok(verdict)) => match verdict.action {
                bindings::exports::tau::extension::hooks::Action::Continue => Verdict::Continue,
                bindings::exports::tau::extension::hooks::Action::Replace => verdict
                    .payload_json
                    .and_then(|p| serde_json::from_str(&p).ok())
                    .map(Verdict::Replace)
                    .unwrap_or(Verdict::Continue),
                bindings::exports::tau::extension::hooks::Action::Block => Verdict::Block {
                    reason: verdict.reason.unwrap_or_else(|| "blocked".into()),
                },
            },
            // A broken extension degrades to Continue, never wedges the run.
            Ok(Err(_)) | Err(_) => Verdict::Continue,
        }
    }
}

// ---------------------------------------------------------------------------
// Provider components (world "provider"): models behind WIT, push streaming.
// ---------------------------------------------------------------------------

/// Provider component instance: separate state type so the store's `T`
/// matches the provider bindgen's Host requirements.
struct ProviderState {
    ctx: WasiCtx,
    table: ResourceTable,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Origin-allowlisted HTTP egress, granted by per-fingerprint consent.
    http: http::HttpRegistry,
}

impl WasiView for ProviderState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl provider_bindings::tau::extension::events::Host for ProviderState {
    fn emit(&mut self, event_json: String) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event_json);
        }
    }
}

struct ProviderInstance {
    store: Store<ProviderState>,
    bindings: provider_bindings::Provider,
}

type SharedProvider = Arc<Mutex<ProviderInstance>>;

/// A model served by a wasm provider component. Events arrive push-mode:
/// the component calls the imported `events.emit` per chunk; `stream`
/// forwards them into the returned event stream.
pub struct WasmModel {
    shared: SharedProvider,
    model: String,
    /// Bearer token injected into every request payload as
    /// `{"auth": {"bearer": ...}}`; None omits the field entirely.
    auth: Option<String>,
}

impl ExtensionHost {
    /// Load a provider component and select one of its models by id.
    /// Load a provider component. `origins` is the HTTP egress allowlist
    /// ("scheme://host[:port]") this component may reach — passing it IS
    /// the consent; empty means every http call fails permission-denied.
    /// `auth`, when given, is handed to the component inside every
    /// request payload (`{"auth": {"bearer": ...}}`) — passing it IS the
    /// consent to place the token in guest memory. It is never persisted
    /// by the host.
    pub fn load_provider(
        &self,
        path: impl AsRef<Path>,
        model: impl Into<String>,
        origins: std::collections::HashSet<String>,
        auth: Option<String>,
    ) -> Result<WasmModel, ExtError> {
        let path = path.as_ref().to_path_buf();
        let model = model.into();
        Self::off_runtime(move || self.load_provider_inner(&path, model, origins, auth))
    }

    fn load_provider_inner(
        &self,
        path: &Path,
        model: String,
        origins: std::collections::HashSet<String>,
        auth: Option<String>,
    ) -> Result<WasmModel, ExtError> {
        let bytes = self.read_verified(path)?;
        let component = Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let mut linker: Linker<ProviderState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        provider_bindings::Provider::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let state = ProviderState {
            ctx: self.wasi.ctx_builder().build(),
            table: ResourceTable::new(),
            event_tx: None,
            http: http::HttpRegistry::new(origins),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings = provider_bindings::Provider::instantiate(&mut store, &component, &linker)
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("provider instantiation failed: {e}"),
            })?;
        Ok(WasmModel {
            shared: Arc::new(Mutex::new(ProviderInstance { store, bindings })),
            model,
            auth,
        })
    }
}

impl provider_bindings::tau::extension::http::Host for ProviderState {
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

/// The payload handed to a provider component's `run`: the documented
/// wire shape, plus `auth` when the caller consented a token.
fn request_json(model: &str, req: &tau_core::Request, auth: Option<&str>) -> String {
    let mut payload = serde_json::json!({
        "model": model,
        "system": req.system,
        "messages": req.messages,
        "tools": req.tools.iter().map(|t| serde_json::json!({
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        })).collect::<Vec<_>>(),
    });
    if let Some(token) = auth {
        payload["auth"] = serde_json::json!({ "bearer": token });
    }
    payload.to_string()
}

#[async_trait]
impl tau_core::Model for WasmModel {
    async fn stream(
        &self,
        req: &tau_core::Request,
    ) -> futures::stream::BoxStream<'static, tau_core::ModelEvent> {
        use tau_core::ModelEvent;

        let request_json = request_json(&self.model, req, self.auth.as_deref());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let shared = self.shared.clone();
        let call = tokio::task::spawn_blocking(move || {
            let mut guard = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let instance = &mut *guard;
            instance.store.data_mut().event_tx = Some(tx);
            let result = instance
                .bindings
                .tau_extension_models()
                .call_run(&mut instance.store, &request_json);
            instance.store.data_mut().event_tx = None;
            result
        });

        async_stream::stream! {
            // Drain events until the component hangs up (event_tx dropped
            // when stream() returns), then surface any trap as an error
            // event — the Model contract forbids propagating failures.
            while let Some(json) = rx.recv().await {
                match serde_json::from_str::<ModelEvent>(&json) {
                    Ok(event) => yield event,
                    Err(_) => continue, // malformed frame: skip, never kill the turn
                }
            }
            match call.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    yield ModelEvent::Error { message: format!("provider trapped: {}", compact_wasm_error(&e)) };
                    yield ModelEvent::Done { stop: tau_core::StopReason::Error };
                }
                Err(e) => {
                    yield ModelEvent::Error { message: format!("provider task failed: {e}") };
                    yield ModelEvent::Done { stop: tau_core::StopReason::Error };
                }
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod compact_error_tests {
    #[test]
    fn strips_backtrace_and_appends_root_cause() {
        let display = "error while executing at wasm backtrace:\n    0:  0x17cff - abort\n    1:  0x15e03 - panic";
        assert_eq!(
            super::compact_error_display(display, "wasm `unreachable` instruction executed"),
            "error while executing (wasm `unreachable` instruction executed)"
        );
    }

    #[test]
    fn without_a_distinct_root_the_summary_stands_alone() {
        assert_eq!(
            super::compact_error_display("plain failure", "plain failure"),
            "plain failure"
        );
        // No context chain: root is the whole multiline Display — never
        // appended.
        let display = "first line\nsecond line";
        assert_eq!(super::compact_error_display(display, display), "first line");
    }
}

#[cfg(test)]
mod auth_payload_tests {
    #[test]
    fn request_json_carries_auth_only_when_consented() {
        let req = tau_core::Request {
            system: None,
            messages: vec![tau_core::Message::user("hi")],
            tools: vec![],
        };
        let without: serde_json::Value =
            serde_json::from_str(&super::request_json("m", &req, None)).unwrap();
        assert!(without.get("auth").is_none());

        let with: serde_json::Value =
            serde_json::from_str(&super::request_json("m", &req, Some("tok-1"))).unwrap();
        assert_eq!(with["auth"]["bearer"], "tok-1");
        assert_eq!(with["model"], "m");
    }
}
