//! Wasm component extension host.
//!
//! Loads components implementing the `tau:extension` world and exposes their
//! tools to the agent and their probes to the harness. Components run with no
//! WASI capabilities: the world has no imports, so a component that tries to
//! import fs/net/env fails instantiation. Sandboxed by default.

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

/// Host state handed to every component. The WasiCtx grants nothing: no
/// stdio, no env, no args, no preopened directories, no network. Components
/// get the WASI interfaces their runtime links against (io/poll, clocks),
/// but every actual capability call fails with permission-denied.
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

/// Everything one loaded component contributes.
pub struct LoadedExtension {
    pub name: String,
    tools: Vec<Box<dyn Tool>>,
    probes: Vec<Box<dyn ProbeHandler>>,
}

impl LoadedExtension {
    pub fn into_parts(self) -> (Vec<Box<dyn Tool>>, Vec<Box<dyn ProbeHandler>>) {
        (self.tools, self.probes)
    }
}

pub struct ExtensionHost {
    engine: Engine,
    policy: sign::TrustPolicy,
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
        }
    }

    /// Read a component file and enforce the trust policy on its bytes.
    fn read_verified(&self, path: &Path) -> Result<Vec<u8>, ExtError> {
        let bytes = std::fs::read(path).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
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

    /// Load one component file. Fails if the component imports capabilities
    /// the world does not provide (the sandbox).
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
        // WASI interfaces are linked so wasip2-std components instantiate,
        // but the context grants no capabilities: sandbox by context.
        let mut linker: Linker<ComponentState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        let state = ComponentState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings = bindings::Extension::instantiate(&mut store, &component, &linker)
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!(
                    "instantiation failed (does it import capabilities the host does not grant?): {e}"
                ),
            })?;

        let definitions = bindings
            .tau_extension_tools()
            .call_definitions(&mut store)?;
        let points = bindings.tau_extension_hooks().call_points(&mut store)?;

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
            let mut guard = shared.lock().unwrap();
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
            Ok(Err(e)) => ToolOutput::err(format!("wasm trap: {e}")),
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
            let mut guard = shared.lock().unwrap();
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
}

impl ExtensionHost {
    /// Load a provider component and select one of its models by id.
    pub fn load_provider(
        &self,
        path: impl AsRef<Path>,
        model: impl Into<String>,
    ) -> Result<WasmModel, ExtError> {
        let path = path.as_ref().to_path_buf();
        let model = model.into();
        Self::off_runtime(move || self.load_provider_inner(&path, model))
    }

    fn load_provider_inner(&self, path: &Path, model: String) -> Result<WasmModel, ExtError> {
        let bytes = self.read_verified(path)?;
        let component = Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let mut linker: Linker<ProviderState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        provider_bindings::Provider::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let state = ProviderState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            event_tx: None,
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
        })
    }
}

#[async_trait]
impl tau_core::Model for WasmModel {
    async fn stream(
        &self,
        req: &tau_core::Request,
    ) -> futures::stream::BoxStream<'static, tau_core::ModelEvent> {
        use tau_core::ModelEvent;

        let request_json = serde_json::json!({
            "model": self.model,
            "system": req.system,
            "messages": req.messages,
            "tools": req.tools.iter().map(|t| serde_json::json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })).collect::<Vec<_>>(),
        })
        .to_string();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let shared = self.shared.clone();
        let call = tokio::task::spawn_blocking(move || {
            let mut guard = shared.lock().unwrap();
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
                    yield ModelEvent::Error { message: format!("provider trapped: {e}") };
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
