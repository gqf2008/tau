//! Realtime provider components (world `realtime`): persistent
//! bidirectional sessions behind WIT (docs/realtime-av.md, Phase 2b).
//! One component instance per session; the component doubles as an
//! ordinary provider (the world also exports `models`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::http::HttpRegistry;
use crate::{ExtError, WasiPolicy, compact_wasm_error};

mod realtime_bindings {
    wasmtime::component::bindgen!({
        // Vendored copy so the packaged crate builds outside the
        // workspace; drift-checked against the canonical wit/tau.wit
        // by the wit_vendored test in lib.rs.
        path: "wit/tau.wit",
        world: "realtime",
    });
}

use realtime_bindings::tau::extension::{events as rt_events, http as rt_http};

/// Component instance state for the realtime world.
struct RealtimeState {
    ctx: WasiCtx,
    table: ResourceTable,
    /// Session-long event channel (unlike the provider world's
    /// per-call wiring, a session's events flow until close).
    event_tx: Option<UnboundedSender<tau_core::ModelEvent>>,
    /// Origin-allowlisted HTTP egress, granted by per-fingerprint consent.
    http: HttpRegistry,
}

impl WasiView for RealtimeState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl rt_events::Host for RealtimeState {
    /// Same typed posture as the provider world's emit, extended with
    /// the realtime kinds (0.3.0).
    fn emit(&mut self, event: rt_events::ModelEvent) -> Result<(), String> {
        let event = match event {
            rt_events::ModelEvent::TextDelta(text) => tau_core::ModelEvent::TextDelta { text },
            rt_events::ModelEvent::ToolCallDelta(d) => tau_core::ModelEvent::ToolCallDelta {
                index: d.index,
                id: d.id,
                name: d.name,
                arguments_delta: d.arguments_delta,
            },
            rt_events::ModelEvent::AudioDelta(a) => {
                if a.media_type.is_empty() {
                    return Err("emit audio-delta: media-type must not be empty".into());
                }
                tau_core::ModelEvent::AudioDelta {
                    data: a.data,
                    media_type: a.media_type,
                }
            }
            rt_events::ModelEvent::InputAudioChunk(a) => {
                if a.media_type.is_empty() {
                    return Err("emit input-audio-chunk: media-type must not be empty".into());
                }
                tau_core::ModelEvent::InputAudioChunk {
                    data: a.data,
                    media_type: a.media_type,
                }
            }
            rt_events::ModelEvent::SpeechStarted => tau_core::ModelEvent::SpeechStarted,
            rt_events::ModelEvent::SpeechStopped => tau_core::ModelEvent::SpeechStopped,
            rt_events::ModelEvent::Interrupted => tau_core::ModelEvent::Interrupted,
            rt_events::ModelEvent::Done(stop) => tau_core::ModelEvent::Done {
                stop: match stop {
                    rt_events::StopReason::Stop => tau_core::StopReason::Stop,
                    rt_events::StopReason::ToolUse => tau_core::StopReason::ToolUse,
                    rt_events::StopReason::Length => tau_core::StopReason::Length,
                    rt_events::StopReason::Error => tau_core::StopReason::Error,
                    rt_events::StopReason::Aborted => tau_core::StopReason::Aborted,
                },
            },
            rt_events::ModelEvent::Error(message) => tau_core::ModelEvent::Error { message },
        };
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
        Ok(())
    }
}

impl rt_http::Host for RealtimeState {
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

struct RealtimeInstance {
    store: Store<RealtimeState>,
    bindings: realtime_bindings::Realtime,
}

/// Everything needed to instantiate a realtime component — once for
/// the shared request/response instance, then once PER SESSION (a
/// trapped session poisons only its own instance).
struct RealtimeFactory {
    engine: Engine,
    component: Component,
    linker: Linker<RealtimeState>,
    wasi: WasiPolicy,
    origins: std::collections::HashSet<String>,
}

impl RealtimeFactory {
    fn instantiate(&self) -> Result<RealtimeInstance, wasmtime::Error> {
        let state = RealtimeState {
            ctx: self.wasi.ctx_builder().build(),
            table: ResourceTable::new(),
            event_tx: None,
            http: HttpRegistry::new(self.origins.clone()),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings =
            realtime_bindings::Realtime::instantiate(&mut store, &self.component, &self.linker)?;
        Ok(RealtimeInstance { store, bindings })
    }
}

struct SharedRealtimeInstance {
    instance: RealtimeInstance,
    factory: RealtimeFactory,
}

impl SharedRealtimeInstance {
    /// Drop a poisoned instance and build a fresh one (same revive
    /// doctrine as the request/response provider).
    fn revive(&mut self) {
        if let Ok(fresh) = self.factory.instantiate() {
            self.instance = fresh;
        }
    }
}

/// A model served by a realtime component: `stream()` uses the
/// exported `models.run` like any provider; `realtime()` opens a fresh
/// session instance.
pub struct WasmRealtimeModel {
    shared: Arc<Mutex<SharedRealtimeInstance>>,
    model: String,
    auth: Option<String>,
}

impl crate::ExtensionHost {
    /// Capability probe: does this component export the `session`
    /// interface (world `realtime`)? Reads the component's type — no
    /// error-driven control flow, no double instantiation. A corrupt
    /// binary reports false and the plain provider load names the
    /// defect.
    pub fn is_realtime_component(&self, bytes: &[u8]) -> bool {
        let Ok(component) = Component::from_binary(&self.engine, bytes) else {
            return false;
        };
        let ty = component.component_type();
        ty.exports(&self.engine)
            .any(|(name, _)| name.starts_with("tau:extension/session@"))
    }

    /// Load a realtime provider component (world `realtime`) and select
    /// one of its models by id. Consent posture identical to
    /// `load_provider` (`origins` gate HTTP egress, `auth` is placed in
    /// guest memory only when given); the MICROPHONE grant is checked
    /// by the CLI at capture time, not here — the category guards the
    /// device, not the session.
    pub fn load_realtime(
        &self,
        path: impl AsRef<Path>,
        model: impl Into<String>,
        origins: std::collections::HashSet<String>,
        auth: Option<String>,
    ) -> Result<WasmRealtimeModel, ExtError> {
        let path = path.as_ref().to_path_buf();
        let model = model.into();
        let bytes = std::fs::read(&path).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let component = Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let mut linker: Linker<RealtimeState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        realtime_bindings::Realtime::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let factory = RealtimeFactory {
            engine: self.engine.clone(),
            component,
            linker,
            wasi: self.wasi,
            origins,
        };
        // Discovery contract, same as load_provider: the model id must
        // be one the component actually serves.
        let mut probe = factory.instantiate().map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: format!("realtime provider instantiation failed: {e}"),
        })?;
        let models = probe
            .bindings
            .tau_extension_models()
            .call_list_models(&mut probe.store)
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("list_models trapped: {}", compact_wasm_error(&e)),
            })?;
        if !models.iter().any(|m| m.id == model) {
            let available = models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ExtError::Load {
                path: path.display().to_string(),
                reason: format!(
                    "model '{model}' not provided by this component (available: {available})"
                ),
            });
        }
        Ok(WasmRealtimeModel {
            shared: Arc::new(Mutex::new(SharedRealtimeInstance {
                instance: probe,
                factory,
            })),
            model,
            auth,
        })
    }
}

#[async_trait::async_trait]
impl tau_core::Model for WasmRealtimeModel {
    async fn stream(
        &self,
        req: &tau_core::Request,
    ) -> futures::stream::BoxStream<'static, tau_core::ModelEvent> {
        use tau_core::ModelEvent;

        let request_json = crate::request_json(&self.model, req, self.auth.as_deref());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ModelEvent>();
        let shared = self.shared.clone();
        let call = tokio::task::spawn_blocking(move || {
            let mut guard = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let instance = &mut guard.instance;
            instance.store.data_mut().event_tx = Some(tx);
            let result = instance
                .bindings
                .tau_extension_models()
                .call_run(&mut instance.store, &request_json);
            instance.store.data_mut().event_tx = None;
            if result.is_err() {
                guard.revive();
            }
            result
        });

        async_stream::stream! {
            while let Some(event) = rx.recv().await {
                yield event;
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

    fn realtime(
        &self,
        config: tau_core::RealtimeConfig,
    ) -> Option<Box<dyn tau_core::RealtimeSession>> {
        // Session-per-instance: build one, wire its session-long event
        // channel, open. An open failure is NOT None (the capability
        // exists) — the session reports it through the event channel
        // per the model-boundary contract.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let config_json = serde_json::json!({
            "input-media-type": config.input_media_type,
            "output-media-type": config.output_media_type,
            "instructions": config.instructions,
        })
        .to_string();
        let opened = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .factory
            .instantiate()
            .map_err(|e| e.to_string())
            .and_then(|mut instance| {
                instance.store.data_mut().event_tx = Some(tx.clone());
                instance
                    .bindings
                    .tau_extension_session()
                    .call_open(&mut instance.store, &config_json)
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                    .map(|()| instance)
            });
        match opened {
            Ok(instance) => Some(Box::new(WasmRealtimeSession {
                instance: Some(Arc::new(Mutex::new(instance))),
                event_rx: Arc::new(Mutex::new(Some(rx))),
                closed: false,
            })),
            Err(reason) => {
                let _ = tx.send(tau_core::ModelEvent::Error {
                    message: format!("realtime open failed: {reason}"),
                });
                let _ = tx.send(tau_core::ModelEvent::Done {
                    stop: tau_core::StopReason::Error,
                });
                Some(Box::new(WasmRealtimeSession {
                    // No instance survived open; the session exists
                    // only to deliver the failure events — and the
                    // local tx drops at the end of this block, ending
                    // the stream right after them.
                    instance: None,
                    event_rx: Arc::new(Mutex::new(Some(rx))),
                    closed: true,
                }))
            }
        }
    }
}

/// One live session on a realtime component instance. The instance is
/// an Option because close() CONSUMES it — dropping the instance drops
/// the event sender, which ends the event stream after the terminal
/// events flush (without this the stream would never end and the
/// driver would wait forever).
struct WasmRealtimeSession {
    instance: Option<Arc<Mutex<RealtimeInstance>>>,
    event_rx: Arc<Mutex<Option<UnboundedReceiver<tau_core::ModelEvent>>>>,
    closed: bool,
}

impl WasmRealtimeSession {
    /// Call a session export on the blocking pool; a trap or a guest
    /// error both surface as the door-level Err the trait promises
    /// (sessions do not revive — a poisoned session is a dead session).
    async fn call(
        &mut self,
        op: impl FnOnce(&mut RealtimeInstance) -> Result<Result<(), String>, wasmtime::Error>
        + Send
        + 'static,
    ) -> Result<(), String> {
        let Some(instance) = self.instance.as_ref().cloned() else {
            return Err("realtime session is closed".into());
        };
        tokio::task::spawn_blocking(move || {
            let mut guard = instance.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            op(&mut guard)
        })
        .await
        .map_err(|e| format!("realtime call task failed: {e}"))?
        .map_err(|e| format!("realtime call trapped: {}", compact_wasm_error(&e)))?
    }
}

#[async_trait::async_trait]
impl tau_core::RealtimeSession for WasmRealtimeSession {
    async fn push_audio(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.call(move |instance| {
            instance
                .bindings
                .tau_extension_session()
                .call_push_audio(&mut instance.store, &bytes)
        })
        .await
    }

    async fn push_image(&mut self, jpeg: Vec<u8>) -> Result<(), String> {
        self.call(move |instance| {
            instance
                .bindings
                .tau_extension_session()
                .call_push_image(&mut instance.store, &jpeg)
        })
        .await
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        self.call(|instance| {
            instance
                .bindings
                .tau_extension_session()
                .call_interrupt(&mut instance.store)
        })
        .await
    }

    async fn close(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(()); // closing twice is a no-op
        }
        self.closed = true;
        let Some(instance) = self.instance.take() else {
            return Ok(());
        };
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = instance.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Split the field borrows (bindings vs store) explicitly.
            let RealtimeInstance { bindings, store } = &mut *guard;
            let result = bindings.tau_extension_session().call_close(store);
            // Dropping the instance HERE (after close flushed the
            // terminal events) drops event_tx — the event stream ends.
            drop(guard);
            result
        })
        .await
        .map_err(|e| format!("realtime close task failed: {e}"))?;
        result.map_err(|e| format!("realtime close trapped: {}", compact_wasm_error(&e)))?
    }

    fn events(&self) -> futures::stream::BoxStream<'static, tau_core::ModelEvent> {
        // Taken once, at open (the trait documents this).
        match self.event_rx.lock().unwrap().take() {
            Some(mut rx) => async_stream::stream! {
                while let Some(event) = rx.recv().await {
                    yield event;
                }
            }
            .boxed(),
            None => futures::stream::empty().boxed(),
        }
    }
}
