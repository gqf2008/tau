//! Wasm component extension host.
//!
//! Loads components implementing the `tau:extension` world and exposes their
//! tools to the agent and their probes to the harness. Ambient WASI
//! capabilities (fs/env/stdio/args/network) are granted by default
//! ([`WasiPolicy::AllowAll`]); [`WasiPolicy::DenyAll`] (CLI `--deny-wasi`)
//! restores the old deny-all sandbox. Custom capabilities — bridge
//! process/http, provider origins — stay consent-gated either way.
//!
//! Adjacent subsystems behind the same host: [`sign`] (ed25519 signatures
//! and the trust store), [`consent`] (per-fingerprint remembered capability
//! grants), [`oci`] (pull/push components through OCI registries), and
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

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use tau_core::probe::{ProbeHandler, ProbePoint, Verdict};
use tau_core::probe_payload::ProbePayload;
use tau_core::tool::{Tool, ToolDef, ToolOutput};
use thiserror::Error;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
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
    });
}

mod provider_bindings {
    wasmtime::component::bindgen!({
        // Vendored copy so the packaged crate builds outside the
        // workspace; drift-checked against the canonical wit/tau.wit
        // by the wit_vendored test below.
        path: "wit/tau.wit",
        world: "provider",
        // Exports awaited (see the `extension` world above) and imports
        // async too: a provider's `run` spends its life in `http.*`, and a
        // host import that blocks would park the very worker thread the
        // run is now awaited on.
        imports: { default: async },
        exports: { default: async },
    });
}

mod bridge_bindings {
    wasmtime::component::bindgen!({
        // Vendored copy so the packaged crate builds outside the
        // workspace; drift-checked against the canonical wit/tau.wit
        // by the wit_vendored test below.
        path: "wit/tau.wit",
        world: "bridge",
    });
}

pub mod bridge;
pub mod consent;
mod realtime;
pub use realtime::WasmRealtimeModel;
mod ingress;
pub mod convert;
mod http;
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

    fn bus(&self) -> Result<tau_core::EventBus, String> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bus
            .clone()
            .ok_or_else(|| "host channel not wired (no agent bus attached)".to_string())
    }

    fn control(&self) -> Result<tau_core::ControlTx, String> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .control
            .clone()
            .ok_or_else(|| "host channel not wired (no agent control channel attached)".to_string())
    }
}

/// Host state handed to every extension component. The WasiCtx follows
/// the host's [`WasiPolicy`]: allow-all inherits the process's
/// capabilities; deny-all links the WASI interfaces but fails every
/// capability call permission-denied. `inject` is this component's
/// session-injection consent (steer/follow-up); notify/emit are facts
/// and never gated.
struct ComponentState {
    ctx: WasiCtx,
    table: ResourceTable,
    channel: Arc<HostChannel>,
    inject: bool,
    /// Stream subscriptions (host.subscribe/poll/unsubscribe,
    /// docs/stream-subscribe.md). Deliberately on the per-instance state,
    /// not the shared channel: a trap rebuild creates a fresh
    /// ComponentState, so old handles stop resolving and the receivers
    /// drop with the old instance — handles never alias across a rebuild.
    subscriptions: HashMap<u64, StreamSubscription>,
    next_subscription: u64,
}

/// One open host.subscribe handle: the topic filter plus the bus
/// receiver backing the bounded ring (capacity = the bus's own).
/// pub(crate): bridge state holds the same per-instance subscriptions.
pub(crate) struct StreamSubscription {
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

use bindings::tau::extension::types as wit;

/// steer/follow-up shared path (extensions and bridges alike): consent
/// gate, role validation, conversion (size cap included), then enqueue
/// into the control channel. Enqueue-only — the agent loop applies the
/// message at its own checkpoints (docs/host-channel.md 语义红线 1).
pub(crate) fn inject_message(
    channel: &HostChannel,
    inject: bool,
    message: wit::Message,
    steer: bool,
) -> Result<(), String> {
    if !inject {
        return Err(
            "session injection not consented for this component (host CLI: --allow-inject)"
                .into(),
        );
    }
    if !matches!(message.role, wit::Role::User) {
        return Err("host.steer/follow-up: message role must be user".into());
    }
    let message = convert::message_to_core(message).map_err(|e| e.to_string())?;
    let control = channel.control()?;
    control
        .send(if steer {
            tau_core::Control::Steer(message)
        } else {
            tau_core::Control::FollowUp(message)
        })
        .map_err(|_| "agent control channel closed (run over?)".to_string())
}

/// notify shared path: user-visible notice → the agent's bus as an
/// ExtensionNotice. A fact for the UI, never model history.
pub(crate) fn channel_notify(
    channel: &HostChannel,
    level: String,
    content: Vec<wit::Content>,
) -> Result<(), String> {
    let content = convert::contents_to_core(content).map_err(|e| e.to_string())?;
    let bus = channel.bus()?;
    // No subscribers is fine; a full channel is the subscriber's problem.
    let _ = bus.send(tau_core::AgentEvent::ExtensionNotice { level, content });
    Ok(())
}

/// emit shared path: extension-defined fact → the bus as an
/// ExtensionFact. The schema is external to tau (JSON leaf) but must be
/// well-formed JSON.
pub(crate) fn channel_emit(channel: &HostChannel, event_json: String) -> Result<(), String> {
    let fact: serde_json::Value = serde_json::from_str(&event_json)
        .map_err(|e| format!("host.emit: event-json is not valid JSON: {e}"))?;
    let bus = channel.bus()?;
    let _ = bus.send(tau_core::AgentEvent::ExtensionFact(fact));
    Ok(())
}

/// host.subscribe shared path (docs/stream-subscribe.md): hang a bounded
/// ring on the bus; the guest drains it with poll inside its own
/// invocations. Fail-loud on unknown topics — a silent empty
/// subscription looks identical to "no events".
pub(crate) fn subscribe_topics(
    channel: &HostChannel,
    subscriptions: &mut HashMap<u64, StreamSubscription>,
    next_subscription: &mut u64,
    topics: &[String],
) -> Result<u64, String> {
    let mut sub = StreamSubscription {
        text_delta: false,
        audio_delta: false,
        rx: channel.bus()?.subscribe(),
    };
    if topics.is_empty() {
        return Err("host.subscribe: no topics (catalog: text-delta, audio-delta)".into());
    }
    for topic in topics {
        match topic.as_str() {
            "text-delta" => sub.text_delta = true,
            "audio-delta" => sub.audio_delta = true,
            other => {
                return Err(format!(
                    "host.subscribe: unknown topic {other:?} (catalog: text-delta, audio-delta)"
                ));
            }
        }
    }
    let id = *next_subscription;
    *next_subscription += 1;
    subscriptions.insert(id, sub);
    Ok(id)
}

/// host.poll shared path: non-blocking drain (try_recv — a synchronous
/// host function must never block_on on the runtime thread). Off-topic
/// events are dropped on the floor; an overrun surfaces as one lagged(n)
/// marker at the head of the batch.
pub(crate) fn poll_subscription(
    subscriptions: &mut HashMap<u64, StreamSubscription>,
    subscription: u64,
) -> Result<Vec<bindings::tau::extension::host::StreamEvent>, String> {
    use bindings::tau::extension::host::{AudioSegment, StreamEvent};
    use tokio::sync::broadcast::error::TryRecvError;
    let sub = subscriptions.get_mut(&subscription).ok_or_else(|| {
        format!(
            "host.poll: unknown subscription {subscription}                  (handles do not survive a trap rebuild)"
        )
    })?;
    let mut out = Vec::new();
    loop {
        match sub.rx.try_recv() {
            Ok(tau_core::AgentEvent::TextDelta(text)) if sub.text_delta => {
                out.push(StreamEvent::TextDelta(text));
            }
            // Contract stays count-only (audio-segment): the bytes ride
            // the host bus for the renderer's sink, but no audio hot
            // path crosses to guests.
            Ok(tau_core::AgentEvent::AudioDelta { data, media_type }) if sub.audio_delta => {
                out.push(StreamEvent::AudioDelta(AudioSegment {
                    bytes: data.len() as u64,
                    media_type,
                }));
            }
            Ok(_) => {} // off-topic: advance the ring, drop the event
            Err(TryRecvError::Lagged(n)) => out.push(StreamEvent::Lagged(n)),
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
        }
    }
    Ok(out)
}

/// host.unsubscribe shared path.
pub(crate) fn unsubscribe_subscription(
    subscriptions: &mut HashMap<u64, StreamSubscription>,
    subscription: u64,
) -> Result<(), String> {
    subscriptions
        .remove(&subscription)
        .map(|_| ())
        .ok_or_else(|| format!("host.unsubscribe: unknown subscription {subscription}"))
}

/// The types interface is type-only; bindgen still generates the marker
/// trait for it.
impl bindings::tau::extension::types::Host for ComponentState {}

/// The extension world's host-channel impl: one-line delegations to the
/// shared ops above (the bridge world's impl converts its own bindgen
/// types into these shapes and calls the same functions).
impl bindings::tau::extension::host::Host for ComponentState {
    fn notify(&mut self, level: String, content: Vec<wit::Content>) -> Result<(), String> {
        channel_notify(&self.channel, level, content)
    }

    fn emit(&mut self, event_json: String) -> Result<(), String> {
        channel_emit(&self.channel, event_json)
    }

    fn steer(&mut self, message: wit::Message) -> Result<(), String> {
        inject_message(&self.channel, self.inject, message, true)
    }

    fn follow_up(&mut self, message: wit::Message) -> Result<(), String> {
        inject_message(&self.channel, self.inject, message, false)
    }

    fn subscribe(&mut self, topics: Vec<String>) -> Result<u64, String> {
        subscribe_topics(
            &self.channel,
            &mut self.subscriptions,
            &mut self.next_subscription,
            &topics,
        )
    }

    fn poll(
        &mut self,
        subscription: u64,
    ) -> Result<Vec<bindings::tau::extension::host::StreamEvent>, String> {
        poll_subscription(&mut self.subscriptions, subscription)
    }

    fn unsubscribe(&mut self, subscription: u64) -> Result<(), String> {
        unsubscribe_subscription(&mut self.subscriptions, subscription)
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
    wasi: WasiPolicy,
    channel: Arc<HostChannel>,
    inject: bool,
}

impl InstanceFactory {
    async fn instantiate(&self) -> Result<ComponentInstance, wasmtime::Error> {
        let state = ComponentState {
            ctx: self.wasi.ctx_builder().build(),
            table: ResourceTable::new(),
            channel: self.channel.clone(),
            inject: self.inject,
            subscriptions: HashMap::new(),
            next_subscription: 0,
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
    /// The old default: WASI interfaces still link, but no capabilities
    /// are granted — fs and network calls fail permission-denied, env
    /// and args come back empty, stdio goes nowhere.
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

/// The WIT contract version this host implements (`wit/tau.wit`). Single
/// source for every load-time hint: hand-writing this string in three
/// places is how a version hint silently goes wrong, so
/// `contract_version_matches_wit` fails the build if it drifts from the
/// vendored WIT.
pub(crate) const CONTRACT_VERSION: &str = "0.6.0";

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
    pub(crate) wasi: WasiPolicy,
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
        std::thread::scope(|scope| {
            scope
                .spawn(|| drive(fut))
                .join()
                .expect("component thread")
        })
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
            wasi: WasiPolicy::default(),
            channel: Arc::new(HostChannel::default()),
        }
    }

    /// Wire the host channel (`host.notify/emit/steer/follow-up`) to an
    /// agent's event bus and control channel. Call once the agent exists;
    /// every extension this host loaded (or later loads) sees it.
    pub fn wire_host_channel(&self, bus: tau_core::EventBus, control: tau_core::ControlTx) {
        self.channel.wire(bus, control);
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
            channel: self.channel.clone(),
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

    /// Run component instantiation and entry-point calls on a plain OS
    /// thread when called from inside a tokio runtime: wasmtime-wasi's sync
    /// host functions block_on internally and panic on a runtime thread
    /// ("Cannot start a runtime from within a runtime"). The extension
    /// world has moved off this path (async linker + [`block_on_component`]);
    /// the provider, bridge and realtime worlds still use it.
    fn off_runtime<T: Send>(f: impl FnOnce() -> Result<T, ExtError> + Send) -> Result<T, ExtError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| scope.spawn(f).join().expect("loader thread"))
        } else {
            f()
        }
    }

    /// Load one component file. Ambient WASI access follows the host's
    /// [`WasiPolicy`] (allow-all by default; `--deny-wasi` to sandbox).
    /// Session injection (`host.steer`/`follow-up`) is NOT consented;
    /// use [`load_with_inject`](Self::load_with_inject) to grant it.
    pub fn load(&self, path: impl AsRef<Path>) -> Result<LoadedExtension, ExtError> {
        self.load_with_inject(path, false)
    }

    /// Load one component file, with `inject` as the session-injection
    /// consent: passing `true` IS the consent (the CLI derives it from
    /// `--allow-inject` or a per-fingerprint remembered grant). Without
    /// it, steer/follow-up fail at call time with a named error;
    /// notify/emit (facts) always work.
    pub fn load_with_inject(
        &self,
        path: impl AsRef<Path>,
        inject: bool,
    ) -> Result<LoadedExtension, ExtError> {
        let path = path.as_ref().to_path_buf();
        block_on_component(self.load_inner(&path, inject))
    }

    async fn load_inner(&self, path: &Path, inject: bool) -> Result<LoadedExtension, ExtError> {
        let bytes = self.read_verified(path)?;
        let component =
            Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        // WASI interfaces are linked so wasip2-std components instantiate;
        // what they may actually do follows the host's WasiPolicy. The
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
            wasi: self.wasi,
            channel: self.channel.clone(),
            inject,
        };
        let mut instance = factory.instantiate().await.map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: format!(
                "instantiation failed (does it import capabilities the host does not grant?): {}{}",
                compact_wasm_error(&e),
                version_hint(&component),
            ),
        })?;

        let definitions = instance
            .bindings
            .tau_extension_tools()
            .call_definitions(&mut instance.store)
            .await
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
    let parameters = serde_json::from_str(parameters_json).map_err(|e| {
        format!("tool '{name}' has an invalid parameters-json schema: {e}")
    })?;
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
        let result = {
            let ComponentInstance { store, bindings } = &mut guard.instance;
            bindings
                .tau_extension_tools()
                .call_execute(store, &name, &arguments.to_string())
                .await
        };
        if result.is_err() {
            // The trap poisoned the guest; rebuild so the next call
            // reaches a working tool instead of trapping forever.
            guard.revive().await;
        }
        match result {
            Ok(r) => match convert::tool_result_blocks_to_core(r.content) {
                // 校验即错误: invalid/oversize blocks become a tool error
                // the model sees — never silently truncated or dropped.
                Ok(content) => ToolOutput {
                    content,
                    is_error: r.is_error,
                },
                Err(e) => ToolOutput::err(format!("invalid tool result: {e}")),
            },
            Err(e) => ToolOutput::err(format!("wasm trap: {}", compact_wasm_error(&e))),
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
pub(crate) fn replace_probe_payload(
    point: ProbePoint,
    payload: ProbePayload,
    raw: Option<String>,
) -> Verdict {
    let Some(raw) = raw else {
        return Verdict::Continue;
    };
    let Ok(replacement) = serde_json::from_str::<serde_json::Value>(&raw) else {
        eprintln!("tau probe: {}: replacement is not JSON", point.name());
        return Verdict::Continue;
    };
    match payload.merge_json(replacement) {
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
        let point_name = point.name().to_string();
        let payload_json = payload.to_json().to_string();
        let mut guard = self.shared.lock().await;
        let result = {
            let ComponentInstance { store, bindings } = &mut guard.instance;
            bindings
                .tau_extension_probes()
                .call_probe(store, &point_name, &payload_json)
                .await
        };
        if result.is_err() {
            // The trap poisoned the guest; rebuild so the next probe
            // still decides instead of degrading forever.
            guard.revive().await;
        }
        match result {
            Ok(verdict) => match verdict.action {
                bindings::exports::tau::extension::probes::Action::Continue => Verdict::Continue,
                bindings::exports::tau::extension::probes::Action::Replace => {
                    replace_probe_payload(point, payload, verdict.payload_json)
                }
                bindings::exports::tau::extension::probes::Action::Block => Verdict::Block {
                    reason: verdict.reason.unwrap_or_else(|| "blocked".into()),
                },
            },
            // A broken extension degrades to Continue, never wedges the run.
            Err(_) => Verdict::Continue,
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
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<tau_core::ModelEvent>>,
    /// Origin-allowlisted HTTP egress, granted by per-fingerprint consent.
    /// Behind a lock the host import may hand to `spawn_blocking`: the
    /// registry is the blocking reqwest client, and its methods must not
    /// run on a runtime worker (`Arc` so the body can move off-thread).
    http: std::sync::Arc<std::sync::Mutex<http::HttpRegistry>>,
}

impl WasiView for ProviderState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

use provider_bindings::tau::extension::events as provider_events;

impl provider_events::Host for ProviderState {
    /// Typed in 0.2.0: malformed frames are impossible by construction
    /// (the 0.1.0 JSON envelope's silent-skip path is gone); the result
    /// reports the remaining semantic violations to the guest.
    async fn emit(&mut self, event: provider_events::ModelEvent) -> Result<(), String> {
        let event = match event {
            provider_events::ModelEvent::TextDelta(text) => tau_core::ModelEvent::TextDelta { text },
            provider_events::ModelEvent::ToolCallDelta(d) => {
                tau_core::ModelEvent::ToolCallDelta {
                    index: d.index,
                    id: d.id,
                    name: d.name,
                    arguments_delta: d.arguments_delta,
                }
            }
            provider_events::ModelEvent::AudioDelta(a) => {
                if a.media_type.is_empty() {
                    return Err("emit audio-delta: media-type must not be empty".into());
                }
                tau_core::ModelEvent::AudioDelta {
                    data: a.data,
                    media_type: a.media_type,
                }
            }
            provider_events::ModelEvent::InputAudioChunk(a) => {
                if a.media_type.is_empty() {
                    return Err("emit input-audio-chunk: media-type must not be empty".into());
                }
                tau_core::ModelEvent::InputAudioChunk {
                    data: a.data,
                    media_type: a.media_type,
                }
            }
            provider_events::ModelEvent::SpeechStarted => tau_core::ModelEvent::SpeechStarted,
            provider_events::ModelEvent::SpeechStopped => tau_core::ModelEvent::SpeechStopped,
            provider_events::ModelEvent::Interrupted => tau_core::ModelEvent::Interrupted,
            provider_events::ModelEvent::Done(stop) => tau_core::ModelEvent::Done {
                stop: match stop {
                    provider_events::StopReason::Stop => tau_core::StopReason::Stop,
                    provider_events::StopReason::ToolUse => tau_core::StopReason::ToolUse,
                    provider_events::StopReason::Length => tau_core::StopReason::Length,
                    provider_events::StopReason::Error => tau_core::StopReason::Error,
                    provider_events::StopReason::Aborted => tau_core::StopReason::Aborted,
                },
            },
            provider_events::ModelEvent::Error(message) => {
                tau_core::ModelEvent::Error { message }
            }
        };
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
        Ok(())
    }
}

struct ProviderInstance {
    store: Store<ProviderState>,
    bindings: provider_bindings::Provider,
}

/// Everything needed to (re)create a provider instance. A trapped guest
/// poisons its instance; the CLI reuses one provider across every run of
/// a REPL session, so without a rebuild one crash would fail every later
/// prompt until restart.
struct ProviderFactory {
    engine: Engine,
    component: Component,
    linker: Linker<ProviderState>,
    wasi: WasiPolicy,
    origins: std::collections::HashSet<String>,
}

impl ProviderFactory {
    async fn instantiate(&self) -> Result<ProviderInstance, wasmtime::Error> {
        let state = ProviderState {
            ctx: self.wasi.ctx_builder().build(),
            table: ResourceTable::new(),
            event_tx: None,
            http: std::sync::Arc::new(std::sync::Mutex::new(http::HttpRegistry::new(
                self.origins.clone(),
            ))),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings =
            provider_bindings::Provider::instantiate_async(&mut store, &self.component, &self.linker)
                .await?;
        Ok(ProviderInstance { store, bindings })
    }
}

struct SharedProviderInstance {
    instance: ProviderInstance,
    factory: ProviderFactory,
}

impl SharedProviderInstance {
    /// Drop a poisoned instance and build a fresh one. Best-effort: if
    /// re-instantiation somehow fails, the poisoned instance stays and
    /// runs keep surfacing the trap.
    async fn revive(&mut self) {
        if let Ok(fresh) = self.factory.instantiate().await {
            self.instance = fresh;
        }
    }
}

type SharedProvider = Arc<tokio::sync::Mutex<SharedProviderInstance>>;

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
    /// (`scheme://host[:port]`) this component may reach — passing it IS
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
        block_on_component(self.load_provider_inner(&path, model, origins, auth))
    }

    async fn load_provider_inner(
        &self,
        path: &Path,
        model: String,
        origins: std::collections::HashSet<String>,
        auth: Option<String>,
    ) -> Result<WasmModel, ExtError> {
        let bytes = self.read_verified(path)?;
        let component =
            Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        let mut linker: Linker<ProviderState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        provider_bindings::Provider::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let factory = ProviderFactory {
            engine: self.engine.clone(),
            component,
            linker,
            wasi: self.wasi,
            origins,
        };
        let mut instance = factory.instantiate().await.map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: format!("provider instantiation failed: {e}"),
        })?;
        // The load contract is "select one of the component's models by
        // id" — enforce it. Without this check a typo'd --model silently
        // runs whatever the guest's run() does with an unknown model
        // name, and the user never learns the real ids.
        let models = instance
            .bindings
            .tau_extension_models()
            .call_list_models(&mut instance.store)
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
        Ok(WasmModel {
            shared: Arc::new(tokio::sync::Mutex::new(SharedProviderInstance {
                instance,
                factory,
            })),
            model,
            auth,
        })
    }
}

/// Lock a registry, surviving a poisoned mutex: a panic inside one
/// blocking http call must not poison every later call in the session.
pub(crate) fn lock_registry(
    registry: &std::sync::Mutex<http::HttpRegistry>,
) -> std::sync::MutexGuard<'_, http::HttpRegistry> {
    registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl provider_bindings::tau::extension::http::Host for ProviderState {
    /// The registry is the *blocking* reqwest client (its per-call
    /// timeouts are enforced by reader threads, docs/extensions.md), so
    /// the calls that touch the network are handed to the blocking pool
    /// and awaited — awaiting in place would park a runtime worker for
    /// as long as the peer takes, which is exactly the thread the
    /// provider's `run` is being awaited on.
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
            lock_registry(&registry).request(&method, &url, &headers, &body, timeout_ms)
        })
        .await
        .map_err(|e| format!("http.request: blocking task failed: {e}"))?
    }

    async fn status(&mut self, handle: u64) -> Result<u16, String> {
        lock_registry(&self.http).status(handle)
    }

    async fn header(&mut self, handle: u64, name: String) -> Result<Option<String>, String> {
        lock_registry(&self.http).header(handle, &name)
    }

    async fn read_body(
        &mut self,
        handle: u64,
        max: u32,
        timeout_ms: u32,
    ) -> Result<(Vec<u8>, bool), String> {
        let registry = self.http.clone();
        tokio::task::spawn_blocking(move || lock_registry(&registry).read_body(handle, max, timeout_ms))
            .await
            .map_err(|e| format!("http.read-body: blocking task failed: {e}"))?
    }

    async fn close(&mut self, handle: u64) {
        lock_registry(&self.http).close(handle);
    }
}

/// The payload handed to a provider component's `run`: the documented
/// wire shape, plus `auth` when the caller consented a token.
pub(crate) fn request_json(model: &str, req: &tau_core::Request, auth: Option<&str>) -> String {
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

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ModelEvent>();
        let shared = self.shared.clone();
        let call = tokio::spawn(async move {
            let mut guard = shared.lock().await;
            let instance = &mut guard.instance;
            instance.store.data_mut().event_tx = Some(tx);
            let result = instance
                .bindings
                .tau_extension_models()
                .call_run(&mut instance.store, &request_json)
                .await;
            instance.store.data_mut().event_tx = None;
            if result.is_err() {
                // The trap poisoned the guest; rebuild so the next run
                // reaches a fresh provider instead of trapping for the
                // rest of the REPL session.
                guard.revive().await;
            }
            result
        });

        async_stream::stream! {
            // Drain events until the component hangs up (event_tx dropped
            // when stream() returns), then surface any trap as an error
            // event — the Model contract forbids propagating failures.
            // Events arrive already typed (0.2.0 contract): there is no
            // malformed-frame path to skip.
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

#[cfg(test)]
mod large_payload_tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use futures::StreamExt;
    use tau_core::{Content, Media, Model, ModelEvent, Request};

    fn artifact() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/echo-provider/target/wasm32-wasip2/release/echo_provider.wasm");
        path.exists().then_some(path)
    }

    /// Same FNV-1a the echo provider's "probe" reports.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }

    /// The request JSON handed to the guest must arrive byte-for-byte.
    /// 8 MiB of media inflates it past 10 MiB of base64 — far beyond the
    /// few KiB every other test sends — and the guest reports back the
    /// length and checksum of what IT received. Both are recomputed here
    /// over the very string the host sends (`super::request_json`, not a
    /// re-implementation), so truncation or corruption anywhere on the
    /// host→component copy mismatches loudly.
    #[tokio::test]
    async fn a_multi_mib_request_arrives_at_the_guest_byte_for_byte() {
        let Some(path) = artifact() else {
            eprintln!("skipping: echo_provider.wasm not built");
            return;
        };
        let mut message = tau_core::Message::user("probe");
        // xorshift noise, not a repeating pattern: a checksum over
        // periodic bytes could miss a swapped or duplicated chunk.
        let mut bytes = vec![0u8; 8 * 1024 * 1024];
        let mut state: u64 = 0x9e3779b97f4a7c15;
        for b in bytes.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = state as u8;
        }
        message.content.push(Content::Image {
            media: Media::bytes("image/png", bytes),
        });
        let req = Request {
            system: None,
            messages: vec![message],
            tools: vec![],
        };
        let expected = super::request_json("echo", &req, None);
        assert!(
            expected.len() > 10 * 1024 * 1024,
            "fixture must inflate past 10 MiB of base64, got {}",
            expected.len()
        );

        let host = super::ExtensionHost::new();
        let model = host
            .load_provider(&path, "echo", HashSet::new(), None)
            .expect("load echo provider");
        let mut stream = model.stream(&req).await;
        let mut text = String::new();
        let mut done = false;
        while let Some(event) = stream.next().await {
            match event {
                ModelEvent::TextDelta { text: t } => text.push_str(&t),
                ModelEvent::Done { .. } => done = true,
                ModelEvent::Error { message } => panic!("provider error: {message}"),
                _ => {}
            }
        }
        assert!(done, "stream never completed");
        let want = format!(
            "bytes={} fnv1a={:016x}",
            expected.len(),
            fnv1a(expected.as_bytes())
        );
        assert!(
            text.contains(&want),
            "guest received a different payload than the host sent: got {text:?}, want {want:?}"
        );

        // The instance is reused across runs: a normal-sized turn right
        // after the big one must still work (no poisoned allocator, no
        // leaked linear memory breaking the next call).
        let followup = Request {
            system: None,
            messages: vec![tau_core::Message::user("still alive ")],
            tools: vec![],
        };
        let mut stream = model.stream(&followup).await;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let ModelEvent::TextDelta { text: t } = event {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "still alive ");
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
    //! host.subscribe/poll/unsubscribe (docs/stream-subscribe.md): the
    //! high-frequency observation leg of F2. ComponentState is built
    //! directly — no component needed, the host functions under test
    //! never touch wasm.
    use super::bindings::tau::extension::host::{Host, StreamEvent};
    use super::*;
    use tau_core::AgentEvent;

    fn state() -> ComponentState {
        ComponentState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            channel: Arc::new(HostChannel::default()),
            inject: false,
            subscriptions: HashMap::new(),
            next_subscription: 0,
        }
    }

    #[test]
    fn poll_drains_matching_events_in_order_then_empty() {
        let mut state = state();
        let bus = tau_core::bus::new_bus();
        state.channel.wire(bus.clone(), tau_core::control::channel().0);
        let id = Host::subscribe(&mut state, vec!["text-delta".into()]).unwrap();

        bus.send(AgentEvent::TextDelta("he".into())).unwrap();
        bus.send(AgentEvent::AudioDelta {
            data: vec![0; 7],
            media_type: "audio/pcm".into(),
        })
        .unwrap(); // off-topic: dropped
        bus.send(AgentEvent::TextDelta("llo".into())).unwrap();

        let batch = Host::poll(&mut state, id).unwrap();
        let texts: Vec<&str> = batch
            .iter()
            .map(|e| match e {
                StreamEvent::TextDelta(t) => t.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(texts, ["he", "llo"]);
        assert!(Host::poll(&mut state, id).unwrap().is_empty());
    }

    #[test]
    fn audio_topic_receives_segments_not_bytes() {
        let mut state = state();
        let bus = tau_core::bus::new_bus();
        state.channel.wire(bus.clone(), tau_core::control::channel().0);
        let id = Host::subscribe(&mut state, vec!["audio-delta".into()]).unwrap();
        bus.send(AgentEvent::AudioDelta {
            data: vec![0; 2048],
            media_type: "audio/pcm;rate=24000".into(),
        })
        .unwrap();
        bus.send(AgentEvent::TextDelta("ignored".into())).unwrap();
        match &Host::poll(&mut state, id).unwrap()[..] {
            [StreamEvent::AudioDelta(seg)] => {
                assert_eq!(seg.bytes, 2048);
                assert_eq!(seg.media_type, "audio/pcm;rate=24000");
            }
            other => panic!("unexpected batch: {other:?}"),
        }
    }

    #[test]
    fn unknown_topic_and_handle_fail_loud() {
        let mut state = state();
        let bus = tau_core::bus::new_bus();
        state.channel.wire(bus, tau_core::control::channel().0);
        let err = Host::subscribe(&mut state, vec!["tool-progress".into()]).unwrap_err();
        assert!(err.contains("unknown topic"), "{err}");
        assert!(err.contains("tool-progress"), "{err}");
        assert!(Host::subscribe(&mut state, vec![]).unwrap_err().contains("no topics"));
        assert!(Host::poll(&mut state, 99).unwrap_err().contains("unknown subscription"));
        assert!(Host::unsubscribe(&mut state, 99)
            .unwrap_err()
            .contains("unknown subscription"));
    }

    #[test]
    fn subscribe_before_wiring_is_an_error() {
        let mut state = state();
        let err = Host::subscribe(&mut state, vec!["text-delta".into()]).unwrap_err();
        assert!(err.contains("not wired"), "{err}");
    }

    #[test]
    fn ring_overrun_marks_the_gap() {
        let mut state = state();
        let bus = tau_core::bus::new_bus();
        state.channel.wire(bus.clone(), tau_core::control::channel().0);
        let id = Host::subscribe(&mut state, vec!["text-delta".into()]).unwrap();
        let total = tau_core::bus::BUS_CAPACITY + 76;
        for i in 0..total {
            bus.send(AgentEvent::TextDelta(format!("d{i}"))).unwrap();
        }
        let batch = Host::poll(&mut state, id).unwrap();
        match batch[0] {
            StreamEvent::Lagged(n) => assert_eq!(n, 76),
            ref other => panic!("expected lagged marker, got {other:?}"),
        }
        assert_eq!(batch.len(), tau_core::bus::BUS_CAPACITY + 1);
        match &batch[1] {
            StreamEvent::TextDelta(t) => {
                assert_eq!(t, "d76", "first retained event is the oldest survivor")
            }
            other => panic!("expected text delta, got {other:?}"),
        }
    }

    #[test]
    fn unsubscribe_stops_the_flow() {
        let mut state = state();
        let bus = tau_core::bus::new_bus();
        state.channel.wire(bus.clone(), tau_core::control::channel().0);
        let id = Host::subscribe(&mut state, vec!["text-delta".into()]).unwrap();
        Host::unsubscribe(&mut state, id).unwrap();
        // No receivers left: broadcast send reports SendError — fine.
        let _ = bus.send(AgentEvent::TextDelta("gone".into()));
        assert!(Host::poll(&mut state, id).is_err());
        // Re-subscribing gets a fresh handle that sees only new events.
        let id2 = Host::subscribe(&mut state, vec!["text-delta".into()]).unwrap();
        assert!(Host::poll(&mut state, id2).unwrap().is_empty());
    }
}

#[cfg(test)]
mod replace_tests {
    use super::*;
    use tau_core::probe_payload::BeforeRun;

    fn payload() -> ProbePayload {
        ProbePayload::BeforeRun(BeforeRun {
            prompt: tau_core::Message::user("original"),
        })
    }

    #[test]
    fn a_well_aimed_replacement_replaces() {
        let verdict = replace_probe_payload(
            ProbePoint::BeforeRun,
            payload(),
            Some(r#"{"prompt": {"role": "user", "content": [{"type": "text", "text": "rewritten"}]}}"#.into()),
        );
        match verdict {
            Verdict::Replace(ProbePayload::BeforeRun(replaced)) => {
                assert_eq!(replaced.prompt.text(), "rewritten");
            }
            other => panic!("expected a replacement, got {other:?}"),
        }
    }

    /// The 0.6.0 host failed the run on these two; a component bug must
    /// not wedge the harness, so they read as "no opinion".
    #[test]
    fn junk_and_misaimed_replacements_degrade_to_continue() {
        assert!(matches!(
            replace_probe_payload(ProbePoint::BeforeRun, payload(), Some("not json".into())),
            Verdict::Continue
        ));
        assert!(matches!(
            replace_probe_payload(
                ProbePoint::BeforeRun,
                payload(),
                Some(r#"{"prompt": 5}"#.into())
            ),
            Verdict::Continue
        ));
        assert!(matches!(
            replace_probe_payload(ProbePoint::BeforeRun, payload(), None),
            Verdict::Continue
        ));
    }

    /// A field the point does not know is not an error: 0.6.0 read the
    /// fields it knew and left the rest alone, and so does `merge_json`.
    #[test]
    fn an_unknown_field_is_ignored() {
        let verdict = replace_probe_payload(
            ProbePoint::BeforeRun,
            payload(),
            Some(r#"{"nonsense": true}"#.into()),
        );
        match verdict {
            Verdict::Replace(ProbePayload::BeforeRun(replaced)) => {
                assert_eq!(replaced.prompt.text(), "original");
            }
            other => panic!("expected a replacement, got {other:?}"),
        }
    }
}
