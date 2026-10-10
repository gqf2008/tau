//! Wasm component extension host.
//!
//! Loads components implementing the `tau:extension` world and exposes their
//! tools to the agent and their probes to the harness. Ambient WASI
//! capabilities (fs/env/stdio/args/network) are granted, always: since 0.8.0
//! there is no `--deny-wasi` and no per-capability gate (docs/wit-0.8-draft.md
//! ruling 1) — a component runs with the permissions of the tau process.
//!
//! Adjacent subsystems behind the same host: [`sign`] (ed25519 signatures
//! and the trust store — the authorization act since 0.8.0),
//! [`oci`] (pull/push components through OCI registries), and
//! [`bridge`] (MCP-over-stdio/HTTP bridges).
//!
//! ## Loading a component
//!
//! ```no_run
//! use tau_ext::ExtensionHost;
//!
//! // Library default loads unsigned components; the tau CLI builds the
//! // host with sign::TrustPolicy::RequireTrusted instead.
//! let host = ExtensionHost::new();
//! let extension = host.load("upper.wasm").expect("load extension");
//! println!("loaded {}", extension.name);
//! let (tools, probes) = extension.into_parts();
//! // register `tools` into a tau_core::ToolRegistry, `probes` into a
//! // tau_core::ProbeRegistry, then build the Agent as usual.
//! ```

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use tau_core::probe::{ProbeHandler, ProbePoint, Verdict};
use tau_core::probe_payload::ProbePayload;
use tau_core::tool::{Tool, ToolDef, ToolOutput};
use thiserror::Error;
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

pub mod ws;

mod bindings {
    wasmtime::component::bindgen!({
        // Vendored copy so the packaged crate builds outside the
        // workspace; drift-checked against the canonical wit/tau.wit
        // by the wit_vendored test below.
        path: "wit/tau.wit",
        world: "extension",
        // Exports are driven as async (`call_*` futures) and WASI is
        // linked in its async form: the host awaits component calls
        // instead of parking a thread on them, and no host function
        // block_on's a runtime thread (docs/wit-redesign.md §6, stage 1).
        exports: { default: async },
        // `host.subscribe` hands the guest a resource, so the generated
        // `subscription` needs a host storage type to stand for; without
        // this it is an empty enum with nothing the host could store
        // (docs/wit-redesign.md, leg 4's binding recipe).
        with: {
            "tau:extension/host.subscription": crate::StreamSubscription,
        },
    });
}

mod bridge_bindings {
    wasmtime::component::bindgen!({
        // Vendored copy so the packaged crate builds outside the
        // workspace; drift-checked against the canonical wit/tau.wit
        // by the wit_vendored test below.
        path: "wit/tau.wit",
        world: "bridge",
        // Last world of stage 2 (docs/wit-redesign.md §6), and the one
        // with the most to await: exports, plus the http, ws, process and
        // ingress imports, whose calls all wait on a socket, a pipe or an
        // actor thread. The store-flagged ones are those that hand the
        // guest a stream or a future -- a handle lives in the store (leg
        // 4's binding recipe).
        imports: {
            default: async,
            "tau:extension/http.[method]response.body": store,
            "tau:extension/process.[method]child.stdin": store,
            "tau:extension/process.[method]child.stdout": store,
            "tau:extension/process.[method]child.stderr": store,
            "tau:extension/process.[method]child.wait": store,
            // `receive` hands out a stream; `poll` is the sync drain (a
            // bridge's inbound pump runs in a probe, which cannot await).
            "tau:extension/ws.[method]connection.receive": store,
            "tau:extension/ws.[method]connection.poll": store,
        },
        // Every resource the guest owns needs a host storage type; without
        // the mapping each is an empty enum with nothing to store.
        with: {
            "tau:extension/http.response": crate::http::HostResponse,
            "tau:extension/ws.connection": crate::ws::HostConnection,
            "tau:extension/process.child": crate::bridge::HostChild,
            "tau:extension/ingress.registration": crate::ingress::HostRegistration,
            "tau:extension/host.subscription": crate::StreamSubscription,
        },
        exports: { default: async },
    });
}

pub mod bridge;
pub mod convert;
mod http;
mod ingress;
pub mod oci;
pub mod sign;

/// Failures loading or running a component.
#[derive(Debug, Error)]
pub enum ExtError {
    /// Wasmtime itself failed (compile, instantiate, trap).
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    /// The component could not be read or validated.
    #[error("extension {path}: {reason}")]
    Load {
        /// Where the component was loaded from.
        path: String,
        /// Why loading failed.
        reason: String,
    },
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

/// The guest→host channel sinks, late-bound by the composition layer.
/// Extensions load before the agent exists; the CLI wires the agent's
/// bus and control channel afterwards ([`ExtensionHost::wire_host_channel`]),
/// and every instance of every extension loaded from that host — including
/// instances rebuilt after a trap — shares the same wiring. Unwired
/// (library use without an agent): notify/emit/steer/follow-up all fail
/// with "host channel not wired" — deliverable-or-error, never a silent
/// drop.
#[derive(Default)]
pub struct HostChannel {
    inner: Mutex<HostChannelInner>,
}

#[derive(Default)]
struct HostChannelInner {
    bus: Option<tau_core::EventBus>,
    control: Option<tau_core::ControlTx>,
}

impl HostChannel {
    /// Point the channel at an agent's bus and control sender.
    pub fn wire(&self, bus: tau_core::EventBus, control: tau_core::ControlTx) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.bus = Some(bus);
        inner.control = Some(control);
    }

    fn bus(&self) -> Result<tau_core::EventBus, HostError> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bus
            .clone()
            .ok_or_else(|| HostError::failed("host channel not wired (no agent bus attached)"))
    }

    fn control(&self) -> Result<tau_core::ControlTx, HostError> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .control
            .clone()
            .ok_or_else(|| {
                HostError::failed("host channel not wired (no agent control channel attached)")
            })
    }
}

/// Host state handed to every extension component. The WasiCtx is the
/// ambient one ([`ambient_wasi_ctx`]). There is no session-injection flag since
/// 0.8.0: steer/follow-up are part of what installing the component meant
/// (docs/wit-0.8-draft.md ruling 1), while notify/emit are facts.
struct ComponentState {
    ctx: WasiCtx,
    table: ResourceTable,
    channel: Arc<HostChannel>,
}

/// One open host.subscribe handle: the topic filter plus the bus
/// receiver backing the bounded ring (capacity = the bus's own).
///
/// Since 0.7.0 this IS the guest's resource (`with:` maps
/// `host.subscription` onto it), so ownership replaces the 0.6.0 handle
/// table: dropping it drops the receiver, and a trap rebuild cannot
/// alias a stale handle because the table died with the old store.
/// `pub` because the generated bindings re-export it: `with:` names it
/// as the storage type behind `host.subscription`, and the bridge state
/// holds the same per-instance subscriptions.
pub struct StreamSubscription {
    /// Subscribed to AgentEvent::TextDelta.
    text_delta: bool,
    /// Subscribed to AgentEvent::AudioDelta.
    audio_delta: bool,
    rx: tau_core::bus::EventStream,
}

impl WasiView for ComponentState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

use bindings::exports::tau::extension::probes as wit_probes;
use bindings::tau::extension::host as wit_host;
use bindings::tau::extension::types as wit;
use tau_core::error::HostError;

/// A subscribed topic, in a shape both worlds' generated enums map onto.
/// The contract types the topic enum, so "unknown topic" stopped being a
/// runtime refusal; this is only the host's own carrier for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Topic {
    /// Assistant text fragments.
    TextDelta,
    /// Provider audio segments (count + media type).
    AudioDelta,
}

/// One drained event, in a shape both worlds convert into their own
/// generated `stream-event`.
pub(crate) enum Polled {
    /// The ring overran: n events were dropped before the next batch.
    Lagged(u64),
    /// A fragment of assistant text.
    TextDelta(String),
    /// A provider audio segment.
    AudioDelta {
        /// How many bytes arrived.
        bytes: u64,
        /// The segment's media type.
        media_type: String,
    },
}

/// The level name the bus carries for a notification (the bus keeps the
/// 0.6.0 string; the contract types the enum).
pub(crate) fn level_name(level: wit_host::Level) -> &'static str {
    match level {
        wit_host::Level::Info => "info",
        wit_host::Level::Warn => "warn",
        wit_host::Level::Error => "error",
    }
}

/// Classify the host's own failures for the contract's `types.error`
/// (two-way since 0.8.0: the runtime consent gates are gone, so nothing reaches
/// a guest as "nobody granted this" — docs/wit-0.8-draft.md ruling 1 — and the
/// host's own twin of that arm went with them, since nothing could produce it).
impl From<HostError> for wit::Error {
    fn from(error: HostError) -> Self {
        match error {
            HostError::Failed(detail) => wit::Error::Failed(detail),
            HostError::Invalid(detail) => wit::Error::Invalid(detail),
        }
    }
}

/// steer/follow-up shared path (extensions and bridges alike): role
/// validation, conversion (size cap included), then enqueue into the
/// control channel. Enqueue-only: the agent loop applies the message at
/// its own checkpoints (docs/host-channel.md, red line 1). Since 0.8.0
/// there is no runtime consent flag here: injection is part of what
/// installing the component meant (docs/wit-0.8-draft.md ruling 1).
pub(crate) fn inject_message(
    channel: &HostChannel,
    message: wit::Message,
    steer: bool,
) -> Result<(), HostError> {
    if !matches!(message.role, wit::Role::User) {
        return Err(HostError::invalid(
            "host.steer/follow-up: message role must be user",
        ));
    }
    let message =
        convert::message_to_core(message).map_err(|e| HostError::invalid(e.to_string()))?;
    let control = channel.control()?;
    control
        .send(if steer {
            tau_core::Control::Steer(message)
        } else {
            tau_core::Control::FollowUp(message)
        })
        .map_err(|_| HostError::failed("agent control channel closed (run over?)"))
}

/// notify shared path: user-visible notice -> the agent's bus as an
/// ExtensionNotice. A fact for the UI, never model history.
pub(crate) fn channel_notify(
    channel: &HostChannel,
    level: &str,
    content: Vec<wit::Content>,
) -> Result<(), HostError> {
    let content =
        convert::contents_to_core(content).map_err(|e| HostError::invalid(e.to_string()))?;
    let bus = channel.bus()?;
    // No subscribers is fine; a full channel is the subscriber's problem.
    let _ = bus.send(tau_core::AgentEvent::ExtensionNotice {
        level: level.to_string(),
        content,
    });
    Ok(())
}

/// emit shared path: extension-defined fact -> the bus as an
/// ExtensionFact. The schema is external to tau (JSON leaf) but must be
/// well-formed JSON.
pub(crate) fn channel_emit(channel: &HostChannel, event_json: String) -> Result<(), HostError> {
    let fact: serde_json::Value = serde_json::from_str(&event_json)
        .map_err(|e| HostError::invalid(format!("host.emit: event-json is not valid JSON: {e}")))?;
    let bus = channel.bus()?;
    let _ = bus.send(tau_core::AgentEvent::ExtensionFact(fact));
    Ok(())
}

/// host.subscribe shared path (docs/stream-subscribe.md): hang a bounded
/// ring on the bus; the guest drains it with poll inside its own
/// invocations. Both worlds map their generated topic enum onto
/// [`Topic`] first, so this carries no bindgen types.
pub(crate) fn subscribe(
    channel: &HostChannel,
    topics: &[Topic],
) -> Result<StreamSubscription, HostError> {
    if topics.is_empty() {
        return Err(HostError::invalid(
            "host.subscribe: no topics (catalog: text-delta, audio-delta)",
        ));
    }
    let mut sub = StreamSubscription {
        text_delta: false,
        audio_delta: false,
        rx: channel.bus()?.subscribe(),
    };
    for topic in topics {
        match topic {
            Topic::TextDelta => sub.text_delta = true,
            Topic::AudioDelta => sub.audio_delta = true,
        }
    }
    Ok(sub)
}

/// host.poll shared path: non-blocking drain (try_recv -- a synchronous
/// host function must never block_on on the runtime thread). Off-topic
/// events are dropped on the floor; an overrun surfaces as one lagged(n)
/// marker at the head of the batch.
pub(crate) fn poll(sub: &mut StreamSubscription) -> Vec<Polled> {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut out = Vec::new();
    loop {
        match sub.rx.try_recv() {
            Ok(tau_core::AgentEvent::TextDelta(text)) if sub.text_delta => {
                out.push(Polled::TextDelta(text));
            }
            // Contract stays count-only (audio-segment): the bytes ride
            // the host bus for the renderer's sink, but no audio hot
            // path crosses to guests.
            Ok(tau_core::AgentEvent::AudioDelta { data, media_type }) if sub.audio_delta => {
                out.push(Polled::AudioDelta {
                    bytes: data.len() as u64,
                    media_type,
                });
            }
            Ok(_) => {} // off-topic: advance the ring, drop the event
            Err(TryRecvError::Lagged(n)) => out.push(Polled::Lagged(n)),
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
        }
    }
    out
}

/// The types interface is type-only; bindgen still generates the marker
/// trait for it.
impl bindings::tau::extension::types::Host for ComponentState {}

/// The extension world's host-channel impl: one-line delegations to the
/// shared ops above (the bridge world's impl converts its own bindgen
/// types into these shapes and calls the same functions).
impl bindings::tau::extension::host::Host for ComponentState {
    fn notify(
        &mut self,
        level: wit_host::Level,
        content: Vec<wit::Content>,
    ) -> Result<(), wit::Error> {
        channel_notify(&self.channel, level_name(level), content).map_err(Into::into)
    }

    fn emit(&mut self, event_json: String) -> Result<(), wit::Error> {
        channel_emit(&self.channel, event_json).map_err(Into::into)
    }

    fn steer(&mut self, message: wit::Message) -> Result<(), wit::Error> {
        inject_message(&self.channel, message, true).map_err(Into::into)
    }

    fn follow_up(&mut self, message: wit::Message) -> Result<(), wit::Error> {
        inject_message(&self.channel, message, false).map_err(Into::into)
    }

    fn subscribe(
        &mut self,
        topics: Vec<wit_host::Topic>,
    ) -> Result<Resource<StreamSubscription>, wit::Error> {
        let topics: Vec<Topic> = topics
            .into_iter()
            .map(|topic| match topic {
                wit_host::Topic::TextDelta => Topic::TextDelta,
                wit_host::Topic::AudioDelta => Topic::AudioDelta,
            })
            .collect();
        let subscription = subscribe(&self.channel, &topics)?;
        self.table
            .push(subscription)
            .map_err(|_| wit::Error::Failed("host.subscribe: resource table full".into()))
    }
}

/// The subscription resource's own methods. Dropping it IS the
/// unsubscribe -- 0.6.0 had an explicit call plus a "close a
/// subscription that was never opened" error, and ownership makes both
/// unrepresentable.
impl bindings::tau::extension::host::HostSubscription for ComponentState {
    fn poll(
        &mut self,
        subscription: Resource<StreamSubscription>,
    ) -> Vec<bindings::tau::extension::host::StreamEvent> {
        use bindings::tau::extension::host::{AudioSegment, StreamEvent};
        let Ok(sub) = self.table.get_mut(&subscription) else {
            eprintln!("tau host.poll: the subscription resource is not in the table");
            return Vec::new();
        };
        poll(sub)
            .into_iter()
            .map(|event| match event {
                Polled::Lagged(n) => StreamEvent::Lagged(n),
                Polled::TextDelta(text) => StreamEvent::TextDelta(text),
                Polled::AudioDelta { bytes, media_type } => {
                    StreamEvent::AudioDelta(AudioSegment { bytes, media_type })
                }
            })
            .collect()
    }

    fn drop(&mut self, subscription: Resource<StreamSubscription>) -> wasmtime::Result<()> {
        self.table.delete(subscription)?;
        Ok(())
    }
}

struct ComponentInstance {
    store: Store<ComponentState>,
    bindings: bindings::Extension,
}

/// Everything needed to (re)create an instance. A trapped guest poisons
/// its instance — wasmtime lets the store live on, but the component's
/// own state aborted mid-call and later calls trap too — so the host
/// re-instantiates after a trap to keep later calls deciding.
struct InstanceFactory {
    engine: Engine,
    component: Component,
    linker: Linker<ComponentState>,
    channel: Arc<HostChannel>,
}

impl InstanceFactory {
    async fn instantiate(&self) -> Result<ComponentInstance, wasmtime::Error> {
        let state = ComponentState {
            ctx: ambient_wasi_ctx().build(),
            table: ResourceTable::new(),
            channel: self.channel.clone(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings =
            bindings::Extension::instantiate_async(&mut store, &self.component, &self.linker)
                .await?;
        Ok(ComponentInstance { store, bindings })
    }
}

struct SharedInstance {
    instance: ComponentInstance,
    factory: InstanceFactory,
}

impl SharedInstance {
    /// Drop a poisoned instance and build a fresh one. Best-effort: if
    /// re-instantiation somehow fails, the poisoned instance stays and
    /// calls keep degrading the way they did before this fix.
    async fn revive(&mut self) {
        if let Ok(fresh) = self.factory.instantiate().await {
            self.instance = fresh;
        }
    }
}

/// One instance behind an async lock: calls await the component while
/// holding it, so a call in flight (not a thread) is what serializes two
/// callers (the `spawn_blocking` + std-mutex pair this replaced parked a
/// blocking-pool thread per call).
type Shared = Arc<tokio::sync::Mutex<SharedInstance>>;

/// What a loaded component contributes, unpacked.
pub type LoadedParts = (Vec<Box<dyn Tool>>, Vec<Box<dyn ProbeHandler>>);

/// Everything one loaded component contributes.
pub struct LoadedExtension {
    /// The extension's self-declared name.
    pub name: String,
    tools: Vec<Box<dyn Tool>>,
    probes: Vec<Box<dyn ProbeHandler>>,
}

impl LoadedExtension {
    /// Bridge loading builds the same contribution shape (docs/
    /// im-channels.md: bridges export probes too since 0.3.0).
    pub(crate) fn new(
        name: String,
        tools: Vec<Box<dyn Tool>>,
        probes: Vec<Box<dyn ProbeHandler>>,
    ) -> Self {
        Self {
            name,
            tools,
            probes,
        }
    }

    /// Consume into the tools and probes the component contributed.
    pub fn into_parts(self) -> LoadedParts {
        (self.tools, self.probes)
    }
}

/// The ambient WASI context every component gets: stdio, the host's
/// environment and argv, the host filesystem preopened read-write (each
/// drive on Windows, `/` elsewhere), network and DNS.
///
/// This is the whole posture since 0.8.0 (docs/wit-0.8-draft.md ruling 1):
/// the runtime gates are gone and `--deny-wasi` with them, so a component
/// runs with the permissions of the tau process. Callers may add env vars
/// before build (the bridge's TAU_MCP_* handoff).
pub(crate) fn ambient_wasi_ctx() -> WasiCtxBuilder {
    let mut ctx = WasiCtxBuilder::new();
    ctx.inherit_stdio()
        .inherit_env()
        .inherit_args()
        .inherit_network()
        .allow_ip_name_lookup(true);
    preopen_host_fs(&mut ctx);
    ctx
}

/// The WIT contract version this host implements (`wit/tau.wit`). Single
/// source for every load-time hint: hand-writing this string in three
/// places is how a version hint silently goes wrong, so
/// `contract_version_matches_wit` fails the build if it drifts from the
/// vendored WIT.
pub(crate) const CONTRACT_VERSION: &str = "0.8.0";

/// If the component exports `tau:extension` interfaces of another
/// contract version, say so — "missing export tau:extension/tools@0.6.0"
/// alone leaves the user guessing what the component was built against
/// (docs/host-channel.md 兼容性: load errors name the version mismatch).
fn version_hint(component: &Component) -> String {
    let ty = component.component_type();
    let mut found: Vec<String> = Vec::new();
    for (name, _) in ty.exports(component.engine()) {
        if let Some(rest) = name.strip_prefix("tau:extension/")
            && let Some((_, version)) = rest.split_once('@')
        {
            found.push(version.to_string());
        }
    }
    found.sort();
    found.dedup();
    if found.is_empty() || found.iter().any(|v| v == CONTRACT_VERSION) {
        String::new()
    } else {
        format!(
            " [component targets tau:extension@{}; this host requires @{CONTRACT_VERSION}; \
             rebuild with the {CONTRACT_VERSION} bindings (wit/tau.wit), see CHANGELOG.md]",
            found.join(", ")
        )
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
                format!("/{}", letter as char),
                wasmtime_wasi::FsPerms::ReadWrite,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = ctx.preopened_dir("/", "/", wasmtime_wasi::FsPerms::ReadWrite);
}

/// Loads wasm components and exposes their contributions as tau
/// tools and probe handlers. One host = one wasmtime engine.
pub struct ExtensionHost {
    pub(crate) engine: Engine,
    policy: sign::TrustPolicy,
    /// pub(crate): bridge loading wires the same channel into bridge
    /// instances (docs/im-channels.md contract amendment).
    pub(crate) channel: Arc<HostChannel>,
}

/// Engine with the module compile cache enabled when it initializes:
/// components load from precompiled machine code on every run after the
/// first instead of recompiling. Best-effort — a cache that fails to
/// initialize (read-only config dir, exotic filesystem) just disables
/// itself; loads still work, only slower.
fn engine() -> Engine {
    let mut config = Config::new();
    let dir = compile_cache_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        let mut cache_config = wasmtime::CacheConfig::new();
        cache_config.with_directory(dir);
        if let Ok(cache) = wasmtime::Cache::new(cache_config) {
            config.cache(Some(cache));
        }
    }
    Engine::new(&config).expect("wasmtime engine")
}

/// Where the module compile cache lives (`~/.tau/cache/wasmtime`).
/// Public so tests and operators can assert the cache is actually
/// populating — a silently-disabled cache is a silent perf loss.
pub fn compile_cache_dir() -> std::path::PathBuf {
    sign::config_dir().join("cache").join("wasmtime")
}

impl Default for ExtensionHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Drive a component call to completion from a synchronous caller.
///
/// Component calls are async: the store's WASI imports are the async ones
/// (`add_to_linker_async`) precisely so that no host function block_on's,
/// and their blocking pool wants a Tokio runtime context. Inside a runtime
/// the future is driven on a scratch thread with its own current-thread
/// runtime (blocking a worker would deadlock the executor that may be
/// driving the guest); outside one it runs here.
pub(crate) fn block_on_component<T: Send>(fut: impl std::future::Future<Output = T> + Send) -> T {
    fn drive<T>(fut: impl std::future::Future<Output = T>) -> T {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("scratch runtime for component calls");
        rt.block_on(fut)
    }
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| scope.spawn(|| drive(fut)).join().expect("component thread"))
    } else {
        drive(fut)
    }
}

impl ExtensionHost {
    /// Library default: unsigned components load. The CLI product uses
    /// `with_policy(RequireTrusted)` and gates this behind --allow-unsigned.
    pub fn new() -> Self {
        Self::with_policy(sign::TrustPolicy::AllowUnsigned)
    }

    /// A host enforcing `policy` on every loaded component.
    pub fn with_policy(policy: sign::TrustPolicy) -> Self {
        Self {
            engine: engine(),
            policy,
            channel: Arc::new(HostChannel::default()),
        }
    }

    /// Wire the host channel (`host.notify/emit/steer/follow-up`) to an
    /// agent's event bus and control channel. Call once the agent exists;
    /// every extension this host loaded (or later loads) sees it.
    pub fn wire_host_channel(&self, bus: tau_core::EventBus, control: tau_core::ControlTx) {
        self.channel.wire(bus, control);
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
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 10 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => {
                    return Err(ExtError::Load {
                        path: path.display().to_string(),
                        reason: e.to_string(),
                    });
                }
            }
        };
        sign::check_policy(&bytes, &self.policy).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Ok(bytes)
    }

    /// Load one component file. Ambient WASI access is granted, always.
    ///
    /// One load path since 0.8.0: the session-injection consent flag is gone
    /// with the other runtime gates (docs/wit-0.8-draft.md ruling 1) — a
    /// loaded component may steer, and that is part of what installing it
    /// meant.
    pub fn load(&self, path: impl AsRef<Path>) -> Result<LoadedExtension, ExtError> {
        let path = path.as_ref().to_path_buf();
        block_on_component(self.load_inner(&path))
    }

    async fn load_inner(&self, path: &Path) -> Result<LoadedExtension, ExtError> {
        let bytes = self.read_verified(path)?;
        let component =
            Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        // WASI interfaces are linked so wasip2-std components instantiate;
        // what they may actually do is ambient (see `ambient_wasi_ctx`). The
        // async linker is what lets a call await its own I/O: the sync one
        // block_on's inside a host function, which is why this loader used
        // to need a runtime-free thread under it.
        let mut linker: Linker<ComponentState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        // The extension world's own import: the host channel. Its sinks
        // are late-bound (wire_host_channel); the functions are always
        // linked so components instantiate before the agent exists.
        bindings::Extension::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let factory = InstanceFactory {
            engine: self.engine.clone(),
            component: component.clone(),
            linker,
            channel: self.channel.clone(),
        };
        let mut instance = factory.instantiate().await.map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: format!(
                "instantiation failed (does it import capabilities the host does not grant?): {}{}",
                compact_wasm_error(&e),
                version_hint(&component),
            ),
        })?;

        // `definitions` is async since 0.7.0 (a bridge performs its
        // handshake there — docs/wit-redesign.md section 5, leg 5), so the
        // call goes through the store's concurrent driver like `execute`.
        let definitions = {
            let ComponentInstance { store, bindings } = &mut instance;
            store
                .run_concurrent(async |acc| {
                    bindings.tau_extension_tools().call_definitions(acc).await
                })
                .await
        };
        let definitions = definitions
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("definitions() trapped: {}", compact_wasm_error(&e)),
            })?
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("definitions() trapped: {}", compact_wasm_error(&e)),
            })?;
        let points = instance
            .bindings
            .tau_extension_probes()
            .call_points(&mut instance.store)
            .await
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("points() trapped: {}", compact_wasm_error(&e)),
            })?;

        let shared: Shared = Arc::new(tokio::sync::Mutex::new(SharedInstance {
            instance,
            factory,
        }));

        let mut tools: Vec<Box<dyn Tool>> = Vec::with_capacity(definitions.len());
        for def in definitions {
            let tool_def = tool_def_strict(def.name, def.description, &def.parameters_json)
                .map_err(|reason| ExtError::Load {
                    path: path.display().to_string(),
                    reason,
                })?;
            tools.push(Box::new(WasmTool {
                def: tool_def,
                shared: shared.clone(),
            }) as Box<dyn Tool>);
        }

        let points: Vec<ProbePoint> = points.into_iter().map(convert::point_to_core).collect();
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

/// Build a ToolDef from a component's declaration, strictly: a
/// parameters-json that does not parse fails the whole load, naming the
/// tool (wit-review F5 — the trust spine is fail-closed everywhere; a
/// silent fallback to an open schema would let the model free-wheel
/// arguments past a broken contract).
pub(crate) fn tool_def_strict(
    name: String,
    description: String,
    parameters_json: &str,
) -> Result<ToolDef, String> {
    let parameters = serde_json::from_str(parameters_json)
        .map_err(|e| format!("tool '{name}' has an invalid parameters-json schema: {e}"))?;
    Ok(ToolDef {
        name,
        description,
        parameters,
    })
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
        let mut guard = self.shared.lock().await;
        // The export is async (0.7.0), so the call goes through the
        // store's concurrent driver: one drive per invocation, with the
        // call itself as the only thing running inside it (a tool call
        // has no streams to pump).
        let result = {
            let ComponentInstance { store, bindings } = &mut guard.instance;
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
            // reaches a working tool instead of trapping forever.
            guard.revive().await;
        }
        match result {
            Ok(Ok(r)) => match convert::result_blocks_to_core(r.content) {
                // 校验即错误: invalid/oversize blocks become a tool error
                // the model sees — never silently truncated or dropped.
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
                ToolOutput::err(format!("wasm trap: {}", compact_wasm_error(&e)))
            }
        }
    }
}

struct WasmProbes {
    points: Vec<ProbePoint>,
    shared: Shared,
}

/// Fold a component's `replace-json` back into the typed payload. Shared
/// by both probe adapters (the `extension` world's and the bridge's): the
/// two differ in their bindings, not in what a replacement means.
///
/// A replacement that does not fit the point it answers is a component
/// bug: like a trap, it degrades to Continue — and says so on stderr —
/// rather than wedging the run (0.6.0 treated the same input as a hard
/// run error; a broken extension must not wedge the harness).
pub(crate) fn replace_probe_payload(point: ProbePoint, payload: wit_probes::Payload) -> Verdict {
    match convert::payload_from_point(point, payload) {
        Ok(replaced) => Verdict::Replace(replaced),
        Err(error) => {
            eprintln!("tau probe: {error}");
            Verdict::Continue
        }
    }
}

#[async_trait]
impl ProbeHandler for WasmProbes {
    fn points(&self) -> &[ProbePoint] {
        &self.points
    }

    async fn probe(&self, point: ProbePoint, payload: ProbePayload) -> Verdict {
        let wit_point = convert::point_to_wit(point);
        let wit_payload = convert::payload_to_wit(&payload);
        let mut guard = self.shared.lock().await;
        // `probe` is the one extension export still declared sync in the
        // contract, so it lowers against the store itself (a sync lowering
        // needs the store exclusively, which is why it cannot go through
        // an accessor the way `tools.execute` does). Nothing is driving
        // this instance between model calls, so the store call is right
        // here; only the ASYNC default makes the result a future.
        let result = {
            let ComponentInstance { store, bindings } = &mut guard.instance;
            bindings
                .tau_extension_probes()
                .call_probe(store, wit_point, &wit_payload)
                .await
        };
        if result.is_err() {
            // The trap poisoned the guest; rebuild so the next probe
            // still decides instead of degrading forever.
            guard.revive().await;
        }
        match result {
            Ok(verdict) => match verdict {
                wit_probes::Verdict::Continue => Verdict::Continue,
                wit_probes::Verdict::Replace(payload) => replace_probe_payload(point, payload),
                wit_probes::Verdict::Block(reason) => Verdict::Block { reason },
            },
            // A broken extension degrades to Continue, never wedges the run.
            Err(_) => Verdict::Continue,
        }
    }
}

/// Lock a registry, surviving a poisoned mutex: a panic inside one
/// blocking call must not poison every later call in the session.
/// A host-side budget knob: `name=<milliseconds>` overrides the
/// production default. Since 0.7.0 no contract call takes a timeout any
/// more ("waiting is host policy"), so a validation leg that needs to see
/// a silent peer cut off cannot ask the guest for a short one -- it sets
/// the host's budget instead, the same species of test knob as
/// `TAU_MCP_PAD` and `TAU_ACP_STALL_MS`.
///
/// The parse rule is theirs too: anything that is not a number means
/// "keep the default", never a panic -- an environment that exported the
/// variable for something else is not a reason to refuse to serve a call.
pub(crate) fn budget(name: &str, default: std::time::Duration) -> std::time::Duration {
    parse_budget(std::env::var(name).ok().as_deref()).unwrap_or(default)
}

/// The knob's rule, kept apart from the environment it reads so it can be
/// tested. Zero is not a budget (it would cut off every wait), so it keeps
/// the default too.
fn parse_budget(value: Option<&str>) -> Option<std::time::Duration> {
    let ms: u64 = value?.trim().parse().ok()?;
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

pub(crate) fn lock_poisoned<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// [`lock_poisoned`] for the http table the provider and realtime worlds
/// share.
pub(crate) fn lock_registry(
    registry: &std::sync::Mutex<http::HttpRegistry>,
) -> std::sync::MutexGuard<'_, http::HttpRegistry> {
    lock_poisoned(registry)
}

#[cfg(test)]
mod wit_vendored {
    // The packaged crate vendors wit/tau.wit (bindgen paths are relative
    // to the manifest; the canonical copy lives at the workspace root
    // for the examples). They must never drift.
    #[test]
    fn vendored_wit_matches_canonical() {
        assert_eq!(
            include_str!("../wit/tau.wit"),
            include_str!("../../../wit/tau.wit"),
            "crates/tau-ext/wit/tau.wit drifted from wit/tau.wit — sync the vendored copy"
        );
    }

    /// The host's advertised contract version must be the one the WIT
    /// declares, or every load error names the wrong upgrade target.
    #[test]
    fn contract_version_matches_wit() {
        let declared = include_str!("../wit/tau.wit")
            .lines()
            .find_map(|line| line.strip_prefix("package tau:extension@"))
            .and_then(|rest| rest.strip_suffix(';'))
            .expect("wit/tau.wit declares `package tau:extension@X.Y.Z;`");
        assert_eq!(
            declared,
            crate::CONTRACT_VERSION,
            "the host advertises a contract version the WIT does not declare"
        );
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
mod schema_strictness_tests {
    use std::path::PathBuf;

    fn artifact() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/bad-schema/target/wasm32-wasip2/release/bad_schema.wasm");
        path.exists().then_some(path)
    }

    #[test]
    fn valid_parameters_json_builds_a_tool_def() {
        let def = super::tool_def_strict(
            "upper".into(),
            "shout".into(),
            r#"{"type": "object", "properties": {"text": {"type": "string"}}}"#,
        )
        .expect("valid schema");
        assert_eq!(def.name, "upper");
        assert_eq!(def.parameters["type"], "object");
    }

    #[test]
    fn invalid_parameters_json_names_the_tool() {
        let err = super::tool_def_strict("bad_schema".into(), "d".into(), "this is not json {")
            .expect_err("invalid schema must be refused");
        assert!(err.contains("bad_schema"), "tool not named: {err}");
        assert!(err.contains("parameters-json"), "field not named: {err}");
    }

    #[test]
    fn empty_parameters_json_is_invalid() {
        assert!(super::tool_def_strict("t".into(), "d".into(), "").is_err());
    }

    /// End-to-end: a component declaring an invalid parameters-json is
    /// refused at load (not silently widened to an open schema), and the
    /// load error names the broken tool.
    #[test]
    fn a_component_with_an_invalid_schema_is_refused_at_load() {
        let Some(path) = artifact() else {
            eprintln!("skipping: bad_schema.wasm not built");
            return;
        };
        let host = super::ExtensionHost::new();
        let shown = match host.load(&path) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("load must fail closed"),
        };
        assert!(shown.contains("bad_schema"), "tool not named: {shown}");
        assert!(
            shown.contains("invalid parameters-json"),
            "reason not named: {shown}"
        );
    }
}

#[cfg(test)]
mod stream_subscription_tests {
    //! host.subscribe + the subscription resource (docs/stream-subscribe.md):
    //! the high-frequency observation leg of F2, and the smallest surface of
    //! the 0.7.0 resource migration -- an unknown topic is unrepresentable
    //! (typed enum) and dropping the handle IS the unsubscribe. ComponentState
    //! is built directly: no component needed, the host functions under test
    //! never touch wasm.
    use super::bindings::tau::extension::host::{Host, HostSubscription, StreamEvent, Topic};
    use super::wit;
    use super::*;
    use tau_core::AgentEvent;

    fn state() -> ComponentState {
        ComponentState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            channel: Arc::new(HostChannel::default()),
        }
    }

    /// The ABI hands the host a *borrow* of the guest's resource, and the
    /// host-side [`Resource`] is neither `Copy` nor `Clone`, so a handle
    /// polled more than once is re-borrowed from the rep the subscription
    /// was created with. (Deleting, by contrast, wants the owned handle --
    /// the table asserts on it.)
    fn borrow(rep: u32) -> Resource<StreamSubscription> {
        Resource::new_borrow(rep)
    }

    /// A state with the bus wired, plus the bus: the host channel is the
    /// publisher side, which the agent loop plays in production.
    fn wired() -> (ComponentState, tau_core::EventBus) {
        let state = state();
        let bus = tau_core::bus::new_bus();
        state
            .channel
            .wire(bus.clone(), tau_core::control::channel().0);
        (state, bus)
    }

    #[test]
    fn poll_drains_matching_events_in_order_then_empty() {
        let (mut state, bus) = wired();
        let id = Host::subscribe(&mut state, vec![Topic::TextDelta]).unwrap();
        let rep = id.rep();

        bus.send(AgentEvent::TextDelta("he".into())).unwrap();
        bus.send(AgentEvent::AudioDelta {
            data: vec![0; 7],
            media_type: "audio/pcm".into(),
        })
        .unwrap(); // off-topic: dropped
        bus.send(AgentEvent::TextDelta("llo".into())).unwrap();

        let batch = HostSubscription::poll(&mut state, borrow(rep));
        let texts: Vec<&str> = batch
            .iter()
            .map(|e| match e {
                StreamEvent::TextDelta(t) => t.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(texts, ["he", "llo"]);
        assert!(HostSubscription::poll(&mut state, borrow(rep)).is_empty());
    }

    #[test]
    fn audio_topic_receives_segments_not_bytes() {
        let (mut state, bus) = wired();
        let id = Host::subscribe(&mut state, vec![Topic::AudioDelta]).unwrap();
        bus.send(AgentEvent::AudioDelta {
            data: vec![0; 2048],
            media_type: "audio/pcm;rate=24000".into(),
        })
        .unwrap();
        bus.send(AgentEvent::TextDelta("ignored".into())).unwrap();
        match &HostSubscription::poll(&mut state, id)[..] {
            [StreamEvent::AudioDelta(seg)] => {
                assert_eq!(seg.bytes, 2048);
                assert_eq!(seg.media_type, "audio/pcm;rate=24000");
            }
            other => panic!("unexpected batch: {other:?}"),
        }
    }

    #[test]
    fn subscribe_before_wiring_is_an_error() {
        let mut state = state();
        let err = Host::subscribe(&mut state, vec![Topic::TextDelta]).unwrap_err();
        assert!(
            matches!(&err, wit::Error::Failed(detail) if detail.contains("not wired")),
            "{err:?}"
        );
    }

    /// Two one-sided refusals: asking for no topics at all, and a handle the
    /// table can no longer resolve. Only the first is an error -- the
    /// contract's `poll` has no error arm, so a dead handle drains to empty
    /// (and says so on stderr).
    #[test]
    fn no_topics_is_refused_and_a_dead_handle_polls_empty() {
        let (mut state, _bus) = wired();
        let err = Host::subscribe(&mut state, vec![]).unwrap_err();
        assert!(
            matches!(&err, wit::Error::Invalid(detail) if detail.contains("no topics")),
            "{err:?}"
        );

        let id = Host::subscribe(&mut state, vec![Topic::TextDelta]).unwrap();
        let rep = id.rep();
        state
            .table
            .delete(id)
            .expect("the subscription was in the table");
        // The stale rep is not in the table any more: empty, and said so.
        assert!(HostSubscription::poll(&mut state, borrow(rep)).is_empty());
    }

    /// Dropping the handle is the unsubscribe (0.7.0): 0.6.0 had an explicit
    /// call plus an error for "close a subscription that was never opened",
    /// and ownership makes both unrepresentable.
    #[test]
    fn dropping_the_handle_unsubscribes() {
        let (mut state, bus) = wired();
        let id = Host::subscribe(&mut state, vec![Topic::TextDelta]).unwrap();
        let rep = id.rep();
        drop(state.table.delete(id));
        // No receivers left: the publisher's send reports SendError -- fine.
        let _ = bus.send(AgentEvent::TextDelta("gone".into()));
        assert!(HostSubscription::poll(&mut state, borrow(rep)).is_empty());
        // Re-subscribing gets a fresh handle that sees only new events.
        let id2 = Host::subscribe(&mut state, vec![Topic::TextDelta]).unwrap();
        assert!(HostSubscription::poll(&mut state, id2).is_empty());
    }

    #[test]
    fn ring_overrun_marks_the_gap() {
        let (mut state, bus) = wired();
        let id = Host::subscribe(&mut state, vec![Topic::TextDelta]).unwrap();
        let total = tau_core::bus::BUS_CAPACITY + 76;
        for i in 0..total {
            bus.send(AgentEvent::TextDelta(format!("d{i}"))).unwrap();
        }
        let batch = HostSubscription::poll(&mut state, id);
        match &batch[0] {
            StreamEvent::Lagged(n) => assert_eq!(*n, 76),
            other => panic!("expected lagged marker, got {other:?}"),
        }
        assert_eq!(batch.len(), tau_core::bus::BUS_CAPACITY + 1);
        match &batch[1] {
            StreamEvent::TextDelta(t) => {
                assert_eq!(t, "d76", "first retained event is the oldest survivor")
            }
            other => panic!("expected text delta, got {other:?}"),
        }
    }
}
#[cfg(test)]
mod replace_tests {
    //! A guest's `replace` verdict, folded back into the payload it answers
    //! ([`replace_probe_payload`]). The payload is typed since 0.7.0, so
    //! 0.6.0's junk cases ("not json", a field of the wrong type) are
    //! unrepresentable; what a component can still get wrong is answering a
    //! point with another point's payload, or with a value that does not
    //! convert (a tool call whose arguments-json is not JSON).
    use super::*;

    fn a_message(text: &str) -> wit::Message {
        wit::Message {
            role: wit::Role::User,
            content: vec![wit::Content::Text(text.to_string())],
        }
    }

    #[test]
    fn a_well_aimed_replacement_replaces() {
        let verdict = replace_probe_payload(
            ProbePoint::BeforeRun,
            wit_probes::Payload::BeforeRun(a_message("rewritten")),
        );
        match verdict {
            Verdict::Replace(ProbePayload::BeforeRun(replaced)) => {
                assert_eq!(replaced.prompt.text(), "rewritten");
            }
            other => panic!("expected a replacement, got {other:?}"),
        }
    }

    /// A payload belonging to another point is a component bug: like a trap it
    /// degrades to Continue and says so on stderr (0.6.0 made the same shape a
    /// hard run error; a broken extension must not wedge the harness).
    #[test]
    fn a_misaimed_replacement_degrades_to_continue() {
        let facts = wit_probes::SessionFacts {
            session: "s".into(),
            cwd: ".".into(),
            model: "m".into(),
        };
        assert!(matches!(
            replace_probe_payload(
                ProbePoint::BeforeRun,
                wit_probes::Payload::SessionEnd(facts)
            ),
            Verdict::Continue
        ));
    }

    #[test]
    fn an_unconvertible_payload_degrades_to_continue() {
        let bad = wit_probes::Payload::BeforeTool(wit::ToolCall {
            id: "c".into(),
            name: "t".into(),
            arguments_json: "not json".into(),
        });
        assert!(matches!(
            replace_probe_payload(ProbePoint::BeforeTool, bad),
            Verdict::Continue
        ));
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_knob_reads_as_milliseconds_and_junk_keeps_the_default() {
        assert_eq!(parse_budget(Some("300")), Some(Duration::from_millis(300)));
        assert_eq!(
            parse_budget(Some(" 300 ")),
            Some(Duration::from_millis(300))
        );
        assert_eq!(parse_budget(None), None);
        // Anything that is not a positive number means "no opinion": zero
        // is not a budget (it would cut off every wait), and a value that
        // was exported for something else must not panic a host call.
        for junk in ["", "soon", "0", "-5", "300ms", "1.5"] {
            assert_eq!(parse_budget(Some(junk)), None, "{junk:?}");
        }
    }

    #[test]
    fn a_name_nothing_exported_keeps_the_callers_default() {
        assert_eq!(
            budget(
                "TAU_VALIDATE_NOBODY_EXPORTS_THIS_MS",
                Duration::from_secs(7)
            ),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn the_host_budgets_are_bounded_by_default() {
        // The knobs must not have replaced the production values: with no
        // environment in play, each host budget is the long one.
        assert_eq!(crate::http::request_timeout(), crate::http::REQUEST_TIMEOUT);
        assert_eq!(crate::http::idle_timeout(), crate::http::IDLE_TIMEOUT);
        assert_eq!(
            crate::ws::connect_timeout_ms(),
            crate::ws::CONNECT_TIMEOUT_MS
        );
        // The process pipes (bridge.rs): 0.7.0's guest cannot keep a clock
        // (no awaiting `wasi:clocks`), so the two deadlines 0.6.0's
        // `write-stdin(timeout-ms)` / `read-stdout(timeout-ms)` carried
        // live host-side now.
        assert_eq!(
            crate::bridge::stdin_idle_timeout(),
            crate::bridge::STDIN_IDLE_DEFAULT
        );
        assert_eq!(
            crate::bridge::pipe_idle_timeout(),
            crate::bridge::PIPE_IDLE_DEFAULT
        );
    }
}
