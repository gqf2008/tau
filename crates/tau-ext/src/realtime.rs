//! Realtime provider components (world `realtime`): persistent
//! bidirectional sessions behind WIT (docs/realtime-av.md, Phase 2b).
//! One component instance per session; the component doubles as an
//! ordinary provider (the world also exports `models`).
//!
//! 0.7.0 shape (docs/wit-redesign.md): the session is a guest resource
//! (`session.create`), the uplinks are streams the *host* produces and
//! the downlink is one the guest produces. Events no longer travel
//! through a host import (`events.emit` is gone), so a session's store
//! must be driven for as long as the session lives: one `run_concurrent`
//! block per session owns the instance and both directions flow inside
//! it. `push-audio`/`push-image` feed the uplink producers through
//! channels (no store access, so a slow guest applies backpressure
//! instead of blocking the caller) and `interrupt`/`close` arrive as
//! commands on the same block.

use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use wasmtime::component::{
    Access, Accessor, Component, Destination, HasSelf, Linker, Resource,
    ResourceTable, StreamProducer, StreamReader, StreamResult, VecBuffer,
};
use wasmtime::{AsContextMut, Engine, Store};
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
        // Same posture as the provider world (docs/wit-redesign.md §6,
        // stage 2): exports awaited, imports async so the guest's
        // `http.*` calls leave the runtime worker. `response.body` hands
        // the guest a stream, and a stream handle lives in the store.
        imports: {
            default: async,
            "tau:extension/http.[method]response.body": store,
        },
        exports: { default: async },
        // The storage behind `http.response` -- see the provider world.
        with: { "tau:extension/http.response": crate::http::HostResponse },
    });
}

use realtime_bindings::exports::tau::extension::models as rt_models;
use realtime_bindings::exports::tau::extension::session::{self as rt_session, Session};
use realtime_bindings::tau::extension::{
    http as rt_http, tools as rt_tools, types as rt_types,
};

/// Component instance state for the realtime world.
struct RealtimeState {
    ctx: WasiCtx,
    table: ResourceTable,
    /// Origin-allowlisted HTTP egress, granted by per-fingerprint consent.
    /// Behind a lock the host import may hand to `spawn_blocking` (see
    /// the provider world's state for why).
    http: std::sync::Arc<std::sync::Mutex<HttpRegistry>>,
}

impl WasiView for RealtimeState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Lifts an HTTP failure into this world's `types.error`.
fn http_error_to_wit(error: crate::http::HttpError) -> rt_types::Error {
    use rt_types::Error;
    match error {
        crate::http::HttpError::Refused(detail) => Error::Refused(detail),
        crate::http::HttpError::Failed(detail) => Error::Failed(detail),
        crate::http::HttpError::Invalid(detail) => Error::Invalid(detail),
    }
}

/// The interface's freestanding function: `request` is an `async func`,
/// so it takes the store through an accessor (see the provider world's
/// identical impl for the shape's rationale).
impl<U> rt_http::HostWithStore<U> for HasSelf<RealtimeState> {
    async fn request(
        accessor: &Accessor<U, Self>,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Resource<crate::http::HostResponse>, rt_types::Error> {
        let client = accessor
            .with(|mut access| crate::lock_registry(&access.get().http).start(&url))
            .map_err(http_error_to_wit)?;
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
        .map_err(http_error_to_wit)?;
        accessor.with(|mut access| {
            access.get().table.push(response).map_err(|_| {
                rt_types::Error::Invalid("http.request: the resource table is full".into())
            })
        })
    }
}

/// The marker the linker asks for even when the interface's own methods
/// are all store-flagged.
impl rt_http::Host for RealtimeState {}

/// `types` is type-only for this world; the linker still wants its marker.
impl rt_types::Host for RealtimeState {}

/// Same shape as the provider world's markers: `models` and `session`
/// mention `tools.tool-def`, which imports the whole `tools` interface for
/// its types -- complete instance required, nothing reachable that calls
/// it (the realtime world's imports are `http` alone).
impl rt_tools::Host for RealtimeState {}

impl<U> rt_tools::HostWithStore<U> for HasSelf<RealtimeState> {
    async fn definitions(_accessor: &Accessor<U, Self>) -> Vec<rt_tools::Definition> {
        Vec::new()
    }

    async fn execute(
        _accessor: &Accessor<U, Self>,
        _name: String,
        _arguments_json: String,
    ) -> rt_tools::ToolResult {
        // No error channel in the contract; an in-band error is the only
        // honest answer for a call this world cannot make.
        rt_tools::ToolResult {
            content: Vec::new(),
            is_error: true,
        }
    }
}

/// The response's own methods: the response lives in this state's
/// resource table, so `&mut self` is enough (`async` only because every
/// import of this world is lowered async).
impl rt_http::HostResponse for RealtimeState {
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

    async fn drop(
        &mut self,
        response: Resource<crate::http::HostResponse>,
    ) -> wasmtime::Result<()> {
        self.table.delete(response)?;
        Ok(())
    }
}

/// `response.body` hands the guest a stream; the `store` flag on it is
/// what makes the accessor available here.
impl<U> rt_http::HostResponseWithStore<U> for HasSelf<RealtimeState> {
    fn body(
        mut host: Access<U, Self>,
        response: Resource<crate::http::HostResponse>,
    ) -> StreamReader<u8> {
        let stream = crate::http::take_body(&mut host.get().table, &response);
        StreamReader::new(&mut host, stream).expect("stream allocation")
    }
}

/// A session's event → `tau_core::ModelEvent`. Same mapping the provider
/// world makes (`event_to_core` in lib.rs); bindgen generates the
/// contract's types once per world, so the two read the same and are
/// written twice (docs/wit-redesign.md section 7).
fn event_to_core(event: rt_models::Event) -> Result<tau_core::ModelEvent, String> {
    use rt_models::Event;
    use rt_types::StopReason;
    Ok(match event {
        Event::TextDelta(text) => tau_core::ModelEvent::TextDelta { text },
        Event::ToolCallDelta(d) => tau_core::ModelEvent::ToolCallDelta {
            index: d.index,
            id: d.id,
            name: d.name,
            arguments_delta: d.arguments_delta,
        },
        Event::AudioDelta(a) => {
            if a.media_type.is_empty() {
                return Err("event: audio-delta with an empty media-type".into());
            }
            tau_core::ModelEvent::AudioDelta {
                data: a.data,
                media_type: a.media_type,
            }
        }
        Event::InputAudioChunk(a) => {
            if a.media_type.is_empty() {
                return Err("event: input-audio-chunk with an empty media-type".into());
            }
            tau_core::ModelEvent::InputAudioChunk {
                data: a.data,
                media_type: a.media_type,
            }
        }
        Event::SpeechStarted => tau_core::ModelEvent::SpeechStarted,
        Event::SpeechStopped => tau_core::ModelEvent::SpeechStopped,
        Event::Interrupted => tau_core::ModelEvent::Interrupted,
        Event::Done(stop) => tau_core::ModelEvent::Done {
            stop: match stop {
                StopReason::Stop => tau_core::StopReason::Stop,
                StopReason::ToolUse => tau_core::StopReason::ToolUse,
                StopReason::Length => tau_core::StopReason::Length,
                StopReason::Error => tau_core::StopReason::Error,
                StopReason::Aborted => tau_core::StopReason::Aborted,
            },
        },
        Event::Error(message) => tau_core::ModelEvent::Error { message },
    })
}

/// Consumes an event stream the guest produced: `models.run` hands one
/// back, and a session's `downlink` is the same shape of the same event
/// type, so one consumer serves both.
struct EventConsumer {
    tx: UnboundedSender<tau_core::ModelEvent>,
    /// Dropped with the consumer: what the drive waits on ([`crate::Done`]).
    _done: crate::DoneHolder,
}

impl<D> wasmtime::component::StreamConsumer<D> for EventConsumer {
    type Item = rt_models::Event;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<D>,
        mut source: wasmtime::component::Source<'_, Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // An empty source means "nothing to take": the ABI forbids
        // `Completed` here (the caller would trap), so wait for the writer.
        if source.remaining(store.as_context_mut()) == 0 {
            return if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                Poll::Pending
            };
        }
        let mut events: Vec<Self::Item> =
            Vec::with_capacity(source.remaining(store.as_context_mut()));
        source.read(store.as_context_mut(), &mut events)?;
        let this = self.get_mut();
        for event in events {
            match event_to_core(event) {
                Ok(event) => {
                    if this.tx.send(event).is_err() {
                        // Nobody is listening any more (the session was
                        // dropped): stop reading, which makes the guest's
                        // next write fail -- the contract's cancellation.
                        return Poll::Ready(Ok(StreamResult::Dropped));
                    }
                }
                Err(why) => eprintln!("tau realtime {why}"),
            }
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// Consumes the verdict future `run`/`downlink` returned. Diagnostic
/// only: the events already carried the terminal state, and holding no
/// sender keeps a guest that never resolves the future from keeping the
/// session alive.
struct VerdictConsumer;

impl<D> wasmtime::component::FutureConsumer<D> for VerdictConsumer {
    type Item = Result<(), rt_types::Error>;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<D>,
        mut source: wasmtime::component::Source<'_, Self::Item>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<()>> {
        if source.remaining(store.as_context_mut()) == 0 {
            return Poll::Pending;
        }
        let mut values: Vec<Self::Item> =
            Vec::with_capacity(source.remaining(store.as_context_mut()));
        source.read(store.as_context_mut(), &mut values)?;
        for value in values {
            if let Err(error) = value {
                let detail = match error {
                    rt_types::Error::Refused(d)
                    | rt_types::Error::Failed(d)
                    | rt_types::Error::Invalid(d) => d,
                };
                eprintln!("tau realtime: the guest ended the stream early: {detail}");
            }
        }
        Poll::Ready(Ok(()))
    }
}

/// How much of an uplink write to hand the guest per poll.
const UPLINK_CAPACITY: usize = 8192;

/// The audio uplink: `push-audio` writes frames into the channel, the
/// session's drive hands them to the guest as `stream<u8>`. Bounded, so a
/// slow provider suspends the host's write instead of overrunning a
/// buffer -- the contract's promise (wit/tau.wit, `session.uplink-audio`).
struct AudioStream {
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    queue: std::collections::VecDeque<u8>,
}

impl AudioStream {
    fn new(rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            queue: std::collections::VecDeque::new(),
        }
    }
}

impl<D> StreamProducer<D> for AudioStream {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'a, D>,
        dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // A zero-length read is the guest saying "not yet": the ABI
        // forbids `Completed` here (the reader would trap).
        if dst.remaining(store.as_context_mut()) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        while self.queue.is_empty() {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(frame)) => self.queue.extend(frame),
                // The session dropped its sender: the uplink is over.
                Poll::Ready(None) => return Poll::Ready(Ok(StreamResult::Dropped)),
                Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
                Poll::Pending => return Poll::Pending,
            }
        }
        let mut dst = dst.as_direct(store, UPLINK_CAPACITY);
        let buf = dst.remaining();
        let n = buf.len().min(self.queue.len());
        for (slot, byte) in buf[..n].iter_mut().zip(self.queue.drain(..n)) {
            *slot = byte;
        }
        dst.mark_written(n);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// The image uplink: one channel message is one complete JPEG
/// (`stream<list<u8>>`), so a poll delivers whole frames.
struct ImageStream {
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    pending: Option<Vec<u8>>,
}

impl ImageStream {
    fn new(rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self { rx, pending: None }
    }
}

impl<D> StreamProducer<D> for ImageStream {
    type Item = Vec<u8>;
    type Buffer = VecBuffer<Vec<u8>>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'a, D>,
        mut dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if dst.remaining(store.as_context_mut()) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        if self.pending.is_none() {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(jpeg)) => self.pending = Some(jpeg),
                Poll::Ready(None) => return Poll::Ready(Ok(StreamResult::Dropped)),
                Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
                Poll::Pending => return Poll::Pending,
            }
        }
        let jpeg = self.pending.take().expect("the frame was just delivered");
        dst.set_buffer(VecBuffer::from(vec![jpeg]));
        Poll::Ready(Ok(StreamResult::Completed))
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
    async fn instantiate(&self) -> Result<RealtimeInstance, wasmtime::Error> {
        let state = RealtimeState {
            ctx: self.wasi.ctx_builder().build(),
            table: ResourceTable::new(),
            http: std::sync::Arc::new(std::sync::Mutex::new(HttpRegistry::new(
                self.origins.clone(),
            ))),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings = realtime_bindings::Realtime::instantiate_async(
            &mut store,
            &self.component,
            &self.linker,
        )
        .await?;
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
    async fn revive(&mut self) {
        if let Ok(fresh) = self.factory.instantiate().await {
            self.instance = fresh;
        }
    }
}

/// A model served by a realtime component: `stream()` uses the
/// exported `models.run` like any provider; `realtime()` opens a fresh
/// session instance.
pub struct WasmRealtimeModel {
    shared: Arc<tokio::sync::Mutex<SharedRealtimeInstance>>,
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
        crate::block_on_component(self.load_realtime_inner(&path, model, origins, auth))
    }

    async fn load_realtime_inner(
        &self,
        path: &Path,
        model: String,
        origins: std::collections::HashSet<String>,
        auth: Option<String>,
    ) -> Result<WasmRealtimeModel, ExtError> {
        let bytes = std::fs::read(path).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let component = Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let mut linker: Linker<RealtimeState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
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
        let mut probe = factory.instantiate().await.map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: format!("realtime provider instantiation failed: {e}"),
        })?;
        let models = probe
            .bindings
            .tau_extension_models()
            .call_list_models(&mut probe.store)
            .await
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
            shared: Arc::new(tokio::sync::Mutex::new(SharedRealtimeInstance {
                instance: probe,
                factory,
            })),
            model,
            auth,
        })
    }
}

/// How many uplink frames may be in flight before `push-*` waits: the
/// backpressure bound (`http::BODY_QUEUE`'s sibling).
const UPLINK_QUEUE: usize = 64;

/// How long a close waits for the guest to end its own downlink — its
/// terminal `done`/`error` flush, which rides the very stream the flush
/// is delivered on. Long enough for a guest that answers the close signal
/// at all; short enough that a wedged one cannot hang the caller.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

#[async_trait::async_trait]
impl tau_core::Model for WasmRealtimeModel {
    async fn stream(
        &self,
        req: &tau_core::Request,
    ) -> futures::stream::BoxStream<'static, tau_core::ModelEvent> {
        use tau_core::ModelEvent;

        let request = realtime_request::build(&self.model, req, self.auth.as_deref());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ModelEvent>();
        let shared = self.shared.clone();
        let call = tokio::spawn(async move {
            let mut guard = shared.lock().await;
            // One drive per call, for the same reason the provider world
            // has one: the guest's writer is only polled while the store
            // runs concurrently, so the drive outlives the pipes.
            let (done, done_rx) = crate::Done::new(1);
            let RealtimeInstance { store, bindings } = &mut guard.instance;
            let result = store
                .run_concurrent(async |acc| {
                    let (events, verdict) = bindings
                        .tau_extension_models()
                        .call_run(acc, request)
                        .await?;
                    acc.with(|store| {
                        events.pipe(
                            store,
                            EventConsumer {
                                tx,
                                _done: done.holder(),
                            },
                        )
                    })?;
                    acc.with(|store| verdict.pipe(store, VerdictConsumer))?;
                    let _ = done_rx.await;
                    Ok::<(), wasmtime::Error>(())
                })
                .await;
            if result.is_err() {
                guard.revive().await;
            }
            result
        });

        async_stream::stream! {
            while let Some(event) = rx.recv().await {
                yield event;
            }
            match call.await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) | Ok(Err(e)) => {
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
        // The failure paths need a sender of their own: `tx` moves into
        // the driver thread.
        let fail_tx = tx.clone();
        let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(UPLINK_QUEUE);
        let (image_tx, image_rx) = tokio::sync::mpsc::channel(UPLINK_QUEUE);
        let (commands_tx, commands_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let config = rt_session::Config {
            input_media_type: config.input_media_type,
            output_media_type: config.output_media_type,
            instructions: config.instructions,
        };
        // Instantiation happens here (the factory lives behind the shared
        // instance's lock); the instance then moves to the driver thread,
        // which is where the store gets driven for the session's life.
        let instantiated = crate::block_on_component(async {
            let guard = self.shared.lock().await;
            guard.factory.instantiate().await
        });
        let instance = match instantiated {
            Ok(instance) => instance,
            Err(e) => return Some(failed_session(format!("instantiation failed: {e}"), fail_tx, rx)),
        };
        let driver = match std::thread::Builder::new()
            .name("tau-realtime".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("scratch runtime for a realtime session");
                runtime.block_on(drive_session(
                    instance,
                    config,
                    audio_rx,
                    image_rx,
                    tx,
                    commands_rx,
                    ready_tx,
                ));
            }) {
            Ok(driver) => driver,
            Err(e) => return Some(failed_session(format!("driver thread failed: {e}"), fail_tx, rx)),
        };
        // `realtime()` is a synchronous trait method while open is
        // awaited, so the outcome is waited for here -- the guest is
        // driven by the driver thread, not by this one.
        match crate::block_on_component(ready_rx) {
            Ok(Ok(())) => Some(Box::new(WasmRealtimeSession {
                audio: Some(audio_tx),
                image: Some(image_tx),
                commands: Some(commands_tx),
                driver: Some(driver),
                event_rx: Arc::new(Mutex::new(Some(rx))),
                closed: false,
            })),
            Ok(Err(reason)) => {
                let _ = driver.join();
                Some(failed_session(reason, fail_tx, rx))
            }
            Err(_) => {
                let _ = driver.join();
                Some(failed_session(
                    "the driver exited before the session opened".into(),
                    fail_tx,
                    rx,
                ))
            }
        }
    }
}

/// The detail a `types.error` carries, whichever arm it is.
fn detail_of(error: rt_types::Error) -> String {
    match error {
        rt_types::Error::Refused(detail)
        | rt_types::Error::Failed(detail)
        | rt_types::Error::Invalid(detail) => detail,
    }
}

/// What the session handle sends to its driver. Both need the store,
/// which the driver owns: `interrupt` because the guest's own call needs
/// the accessor, `close` because the drive has to end first.
enum SessionCommand {
    /// Barge-in; answered with the guest's result.
    Interrupt(tokio::sync::oneshot::Sender<Result<(), String>>),
    /// End the session; answered once the drive has ended.
    Close(tokio::sync::oneshot::Sender<()>),
}

/// The session's driver, on a thread of its own: one `run_concurrent`
/// block owns the instance for the session's lifetime, so the uplink
/// producers are drained and the downlink is delivered while it lives.
/// The events channel ends when this returns — the consumer holding its
/// sender is dropped with the store.
#[allow(clippy::too_many_arguments)]
async fn drive_session(
    mut instance: RealtimeInstance,
    config: rt_session::Config,
    audio_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    image_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    tx: UnboundedSender<tau_core::ModelEvent>,
    commands: tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
    ready: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    let (done, done_rx) = crate::Done::new(1);
    let RealtimeInstance { store, bindings } = &mut instance;
    let _ = store
        .run_concurrent(async |acc| {
            let opened = open_session(acc, bindings, config, audio_rx, image_rx, tx, done.holder()).await;
            let session = match opened {
                Ok(session) => session,
                Err(reason) => {
                    // The session never opened: the caller turns this into
                    // the terminal pair of events.
                    let _ = ready.send(Err(reason));
                    return;
                }
            };
            let _ = ready.send(Ok(()));
            drive_commands(acc, bindings, session, commands, done_rx).await;
        })
        .await;
}

/// Open the session and register every pipe. The uplinks are host-produced
/// streams (the guest drains them at its own pace), the downlink is the
/// guest's; `done` counts the downlink consumer out when it ends.
async fn open_session<T>(
    acc: &Accessor<T>,
    bindings: &realtime_bindings::Realtime,
    config: rt_session::Config,
    audio_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    image_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    tx: UnboundedSender<tau_core::ModelEvent>,
    done: crate::DoneHolder,
) -> Result<Session, String>
where
    T: Send + 'static,
{
    let api = bindings.tau_extension_session();
    let session = match api.session().call_create(acc, config).await {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => return Err(format!("open refused: {}", detail_of(error))),
        Err(e) => return Err(format!("open trapped: {}", compact_wasm_error(&e))),
    };
    let audio = acc
        .with(|mut store| StreamReader::new(&mut store, AudioStream::new(audio_rx)))
        .map_err(|e| format!("uplink-audio stream: {}", compact_wasm_error(&e)))?;
    let image = acc
        .with(|mut store| StreamReader::new(&mut store, ImageStream::new(image_rx)))
        .map_err(|e| format!("uplink-image stream: {}", compact_wasm_error(&e)))?;
    let audio_future = api
        .session()
        .call_uplink_audio(acc, session, audio)
        .await
        .map_err(|e| format!("uplink-audio trapped: {}", compact_wasm_error(&e)))?;
    let image_future = api
        .session()
        .call_uplink_image(acc, session, image)
        .await
        .map_err(|e| format!("uplink-image trapped: {}", compact_wasm_error(&e)))?;
    let (events, downlink) = api
        .session()
        .call_downlink(acc, session)
        .await
        .map_err(|e| format!("downlink trapped: {}", compact_wasm_error(&e)))?;
    let piped = acc
        .with(|store| {
            events.pipe(
                store,
                EventConsumer {
                    tx,
                    _done: done,
                },
            )
        })
        .map_err(|e| format!("downlink pipe: {}", compact_wasm_error(&e)));
    // The three verdicts are diagnostic: each resolves when its stream
    // ends, and none of them may hold the session open.
    acc.with(|store| audio_future.pipe(store, VerdictConsumer))
        .map_err(|e| format!("uplink-audio verdict: {}", compact_wasm_error(&e)))?;
    acc.with(|store| image_future.pipe(store, VerdictConsumer))
        .map_err(|e| format!("uplink-image verdict: {}", compact_wasm_error(&e)))?;
    acc.with(|store| downlink.pipe(store, VerdictConsumer))
        .map_err(|e| format!("downlink verdict: {}", compact_wasm_error(&e)))?;
    piped?;
    Ok(session)
}

/// The command loop: it is what keeps the drive alive, and therefore what
/// keeps both directions flowing. It ends on `close`, when the handle is
/// dropped, or when the guest ends its downlink.
async fn drive_commands<T>(
    acc: &Accessor<T>,
    bindings: &realtime_bindings::Realtime,
    session: Session,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
    mut ended: tokio::sync::oneshot::Receiver<()>,
) where
    T: Send + 'static,
{
    let api = bindings.tau_extension_session();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(SessionCommand::Interrupt(ack)) => {
                    let result = match api.session().call_interrupt(acc, session).await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(detail_of(error)),
                        Err(e) => Err(compact_wasm_error(&e)),
                    };
                    let _ = ack.send(result);
                }
                Some(SessionCommand::Close(ack)) => {
                    let _ = ack.send(());
                    // The uplink writable ends are already gone by now (the
                    // client drops them before sending this), which is the
                    // guest's signal to flush its terminal events and end
                    // its downlink. Wait for that end instead of racing it:
                    // the flush is what the close is FOR. Bounded, because
                    // a guest that never ends its downlink must not hold
                    // the door shut.
                    let _ = tokio::time::timeout(CLOSE_GRACE, &mut ended).await;
                    break;
                }
                None => break,
            },
            _ = &mut ended => break,
        }
    }
}

/// One live session's handle. The instance is not here: it lives in the
/// driver thread, inside the drive that owns it. `close` ends that drive,
/// which drops the store, which drops the downlink consumer, which is
/// what ends the event stream.
struct WasmRealtimeSession {
    /// Audio frames: one message is one `push-audio`.
    audio: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// Image frames: one message is one complete JPEG.
    image: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    commands: Option<tokio::sync::mpsc::UnboundedSender<SessionCommand>>,
    driver: Option<std::thread::JoinHandle<()>>,
    event_rx: Arc<Mutex<Option<UnboundedReceiver<tau_core::ModelEvent>>>>,
    closed: bool,
}

/// A session that only delivers the failure that kept it from opening:
/// 0.6.0's shape (the capability exists, the session reports the error
/// through the event channel). The local sender drops here, so the event
/// stream carries the pair and ends.
fn failed_session(
    reason: String,
    tx: UnboundedSender<tau_core::ModelEvent>,
    rx: UnboundedReceiver<tau_core::ModelEvent>,
) -> Box<dyn tau_core::RealtimeSession> {
    let _ = tx.send(tau_core::ModelEvent::Error {
        message: format!("realtime open failed: {reason}"),
    });
    let _ = tx.send(tau_core::ModelEvent::Done {
        stop: tau_core::StopReason::Error,
    });
    drop(tx);
    Box::new(WasmRealtimeSession {
        audio: None,
        image: None,
        commands: None,
        driver: None,
        event_rx: Arc::new(Mutex::new(Some(rx))),
        closed: true,
    })
}

#[async_trait::async_trait]
impl tau_core::RealtimeSession for WasmRealtimeSession {
    /// Uplink audio. The write is a channel send: the guest drains it
    /// inside the session's drive, so a slow provider suspends this call
    /// instead of overrunning a buffer. Nothing here touches the store,
    /// which is why a microphone loop can push while the session runs.
    async fn push_audio(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        let audio = self
            .audio
            .as_ref()
            .ok_or_else(|| "realtime session is closed".to_string())?;
        audio
            .send(bytes)
            .await
            .map_err(|_| "realtime session is closed".to_string())
    }

    async fn push_image(&mut self, jpeg: Vec<u8>) -> Result<(), String> {
        let image = self
            .image
            .as_ref()
            .ok_or_else(|| "realtime session is closed".to_string())?;
        image
            .send(jpeg)
            .await
            .map_err(|_| "realtime session is closed".to_string())
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        let (ack, result) = tokio::sync::oneshot::channel();
        self.commands
            .as_ref()
            .ok_or_else(|| "realtime session is closed".to_string())?
            .send(SessionCommand::Interrupt(ack))
            .map_err(|_| "realtime session is closed".to_string())?;
        result
            .await
            .map_err(|_| "realtime session is closed".to_string())?
    }

    async fn close(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(()); // closing twice is a no-op
        }
        self.closed = true;
        // Dropping the uplink senders ends the guest's reads (and is the
        // contract's "dropping the writable end ends the uplink").
        self.audio = None;
        self.image = None;
        if let Some(commands) = self.commands.take() {
            let (ack, ended) = tokio::sync::oneshot::channel();
            if commands.send(SessionCommand::Close(ack)).is_ok() {
                let _ = ended.await;
            }
        }
        if let Some(driver) = self.driver.take() {
            // The drive ends as soon as it takes the command; the join is
            // off the runtime because it waits for that thread, not for
            // anything this one has to do.
            let _ = tokio::task::spawn_blocking(move || driver.join()).await;
        }
        Ok(())
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

/// Core to this world's generated types: the provider world's
/// `provider_request`, one world over (same mapping, this world's paths).
///
/// bindgen generates the contract's types once per world, so this mirrors
/// `convert::` one world over (the same mapping, different Rust types).
/// Aliasing the shared `types` interface into one module with `with:` is the
/// way to collapse that duplication; it is a change to the binding layer, not
/// to the contract, and is deliberately not part of 0.7.0's migration
/// (docs/wit-redesign.md section 7).
mod realtime_request {
    use super::realtime_bindings::exports::tau::extension::models;
    use super::realtime_bindings::tau::extension::types as wit;
    use tau_core::tool::ToolDef;
    use tau_core::types::{Content, Media, MediaSource, Message, ResultBlock, Role};

    /// The payload handed to `models.run`: the request is a typed
    /// value since 0.7.0, plus `auth` when the caller consented a token.
    pub(super) fn build(
        model: &str,
        req: &tau_core::Request,
        auth: Option<&str>,
    ) -> models::Request {
        models::Request {
            model: model.to_string(),
            system: req.system.clone(),
            messages: req.messages.iter().map(message).collect(),
            tools: req.tools.iter().map(definition).collect(),
            auth: auth.map(|token| models::Auth::Bearer(token.to_string())),
        }
    }

    fn message(message: &Message) -> wit::Message {
        wit::Message {
            role: match message.role {
                Role::User => wit::Role::User,
                Role::Assistant => wit::Role::Assistant,
                Role::Tool => wit::Role::Tool,
            },
            content: message.content.iter().map(content).collect(),
        }
    }

    fn content(content: &Content) -> wit::Content {
        match content {
            Content::Text { text } => wit::Content::Text(text.clone()),
            Content::Image { media } => wit::Content::Image(media_to_wit(media)),
            Content::Audio { media } => wit::Content::Audio(media_to_wit(media)),
            Content::Video { media } => wit::Content::Video(media_to_wit(media)),
            Content::File { media, name } => wit::Content::File(wit::File {
                media: media_to_wit(media),
                name: name.clone(),
            }),
            Content::ToolCall {
                id,
                name,
                arguments,
            } => wit::Content::ToolCall(wit::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments_json: arguments.to_string(),
            }),
            Content::ToolResult {
                call_id,
                content,
                is_error,
            } => wit::Content::ToolResult(wit::ToolResult {
                call_id: call_id.clone(),
                content: content.iter().map(result_block).collect(),
                is_error: *is_error,
            }),
        }
    }

    /// The wire's `result-block` is narrower than `content` (the toolchain
    /// refuses a recursive `content`), so a nested call or result degrades
    /// to its text projection -- the same posture `convert::` takes.
    fn result_block(block: &Content) -> wit::ResultBlock {
        match ResultBlock::try_from(block.clone()) {
            Ok(block) => match block {
                ResultBlock::Text { text } => wit::ResultBlock::Text(text),
                ResultBlock::Image { media } => wit::ResultBlock::Image(media_to_wit(&media)),
                ResultBlock::Audio { media } => wit::ResultBlock::Audio(media_to_wit(&media)),
                ResultBlock::Video { media } => wit::ResultBlock::Video(media_to_wit(&media)),
                ResultBlock::File { media, name } => wit::ResultBlock::File(wit::File {
                    media: media_to_wit(&media),
                    name,
                }),
            },
            Err(_) => wit::ResultBlock::Text(tau_core::types::tool_result_text(
                std::slice::from_ref(block),
            )),
        }
    }

    fn media_to_wit(media: &Media) -> wit::Media {
        wit::Media {
            media_type: media.media_type.clone(),
            source: match &media.source {
                MediaSource::Bytes(bytes) => wit::MediaSource::Bytes(bytes.clone()),
                MediaSource::Url(url) => wit::MediaSource::Url(url.clone()),
                MediaSource::Blob { hash } => wit::MediaSource::Blob(hash.clone()),
            },
        }
    }

    fn definition(def: &ToolDef) -> models::Definition {
        models::Definition {
            name: def.name.clone(),
            description: def.description.clone(),
            parameters_json: def.parameters.to_string(),
        }
    }
}
