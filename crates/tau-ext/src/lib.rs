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

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use tau_core::probe::{ProbeHandler, ProbePoint, Verdict};
use tau_core::probe_payload::ProbePayload;
use tau_core::tool::{Tool, ToolDef, ToolOutput};
use thiserror::Error;
use wasmtime::component::{
    Access, Accessor, Component, HasSelf, Linker, Resource, ResourceTable, StreamReader,
};
use wasmtime::{AsContextMut, Config, Engine, Store};
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
        imports: {
            default: async,
            // `response.body` returns a stream, and a stream handle lives
            // in the store: the `store` flag is wasmtime's answer
            // (docs/wit-redesign.md, leg 4's binding recipe).
            "tau:extension/http.[method]response.body": store,
        },
        // Without this the generated resource type is an empty enum and
        // there is nothing the host could store behind `http.response`.
        with: {
            "tau:extension/http.response": crate::http::HostResponse,
        },
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
/// (three-way since 0.7.0; docs/wit-redesign.md, the error decision):
/// `refused` = policy or consent, `failed` = allowed but broken,
/// `invalid` = never a valid call here. Call sites name the variant;
/// this is the mechanical lift into the generated type.
impl From<HostError> for wit::Error {
    fn from(error: HostError) -> Self {
        match error {
            HostError::Refused(detail) => wit::Error::Refused(detail),
            HostError::Failed(detail) => wit::Error::Failed(detail),
            HostError::Invalid(detail) => wit::Error::Invalid(detail),
        }
    }
}

/// steer/follow-up shared path (extensions and bridges alike): consent
/// gate, role validation, conversion (size cap included), then enqueue
/// into the control channel. Enqueue-only: the agent loop applies the
/// message at its own checkpoints (docs/host-channel.md, red line 1).
pub(crate) fn inject_message(
    channel: &HostChannel,
    inject: bool,
    message: wit::Message,
    steer: bool,
) -> Result<(), HostError> {
    if !inject {
        return Err(HostError::refused(
            "session injection not consented for this component (host CLI: --allow-inject)",
        ));
    }
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
        inject_message(&self.channel, self.inject, message, true).map_err(Into::into)
    }

    fn follow_up(&mut self, message: wit::Message) -> Result<(), wit::Error> {
        inject_message(&self.channel, self.inject, message, false).map_err(Into::into)
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
pub(crate) const CONTRACT_VERSION: &str = "0.7.0";

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
                    content: content.into_iter().map(tau_core::types::Content::from).collect(),
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

// ---------------------------------------------------------------------------
// Provider components (world "provider"): models behind WIT, push streaming.
// ---------------------------------------------------------------------------

/// Provider component instance: separate state type so the store's `T`
/// matches the provider bindgen's Host requirements.
struct ProviderState {
    ctx: WasiCtx,
    table: ResourceTable,
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

/// The provider's event type in core terms. Malformed frames are
/// impossible by construction (the type is generated from the contract);
/// what survives is the semantic check the host has always made -- an
/// audio delta with no media type cannot be assembled into anything.
fn event_to_core(
    event: provider_bindings::exports::tau::extension::models::Event,
) -> Result<tau_core::ModelEvent, String> {
    use provider_bindings::exports::tau::extension::models::Event;
    use provider_bindings::tau::extension::types::StopReason;
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
                return Err("run: audio-delta with an empty media-type".into());
            }
            tau_core::ModelEvent::AudioDelta {
                data: a.data,
                media_type: a.media_type,
            }
        }
        Event::InputAudioChunk(a) => {
            if a.media_type.is_empty() {
                return Err("run: input-audio-chunk with an empty media-type".into());
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

/// Consumes the event stream a provider's `run` returned (0.7.0: the
/// guest writes it, the host reads it -- `events.emit` is gone) and
/// forwards each event into the model's channel.
///
/// The end needs no detection: when the guest drops its writer the
/// machinery drops this consumer, which drops `tx`, which is what ends
/// the model stream's drain loop.
struct EventConsumer {
    tx: tokio::sync::mpsc::UnboundedSender<tau_core::ModelEvent>,
    /// Dropped with the consumer, which is what ends the drive: see
    /// [`crate::Done`].
    _done: crate::DoneHolder,
}

impl<D> wasmtime::component::StreamConsumer<D> for EventConsumer {
    type Item = provider_bindings::exports::tau::extension::models::Event;

    fn poll_consume(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        mut store: wasmtime::StoreContextMut<D>,
        mut source: wasmtime::component::Source<'_, Self::Item>,
        finish: bool,
    ) -> std::task::Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
        use wasmtime::component::StreamResult;
        // An empty source means "nothing to take": the ABI forbids
        // `Completed` here (the caller would trap), so wait for the
        // writer.
        if source.remaining(store.as_context_mut()) == 0 {
            return if finish {
                std::task::Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                std::task::Poll::Pending
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
                        // Nobody is listening any more (the run was
                        // aborted): stop reading, which makes the guest's
                        // next write fail -- the contract's cancellation.
                        return std::task::Poll::Ready(Ok(StreamResult::Dropped));
                    }
                }
                Err(why) => eprintln!("tau provider {why}"),
            }
        }
        std::task::Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// Consumes the verdict future `run` returned: `ok` when the guest saw the
/// host read its stream to the end, `err` when the host closed it early.
/// Diagnostic only -- the events already carried the terminal state, and
/// holding no sender here keeps a guest that never resolves the future
/// from keeping the model stream open.
struct VerdictConsumer;

impl<D> wasmtime::component::FutureConsumer<D> for VerdictConsumer {
    type Item = Result<(), provider_bindings::tau::extension::types::Error>;

    fn poll_consume(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        mut store: wasmtime::StoreContextMut<D>,
        mut source: wasmtime::component::Source<'_, Self::Item>,
        _finish: bool,
    ) -> std::task::Poll<wasmtime::Result<()>> {
        if source.remaining(store.as_context_mut()) == 0 {
            return std::task::Poll::Pending;
        }
        let mut values: Vec<Self::Item> =
            Vec::with_capacity(source.remaining(store.as_context_mut()));
        source.read(store.as_context_mut(), &mut values)?;
        for value in values {
            if let Err(error) = value {
                let detail = match error {
                    provider_bindings::tau::extension::types::Error::Refused(d)
                    | provider_bindings::tau::extension::types::Error::Failed(d)
                    | provider_bindings::tau::extension::types::Error::Invalid(d) => d,
                };
                eprintln!("tau provider run: the host ended the stream early: {detail}");
            }
        }
        std::task::Poll::Ready(Ok(()))
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

/// A model served by a wasm provider component. Since 0.7.0 the component
/// returns its events as a stream from `run`; `stream` consumes that
/// stream and forwards each event into the returned event stream.
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

/// Lifts an HTTP failure into the contract's `types.error` (three-way since
/// 0.7.0, docs/wit-redesign.md): the classification is made where the
/// failure happens (`http::HttpError`), never by matching on a message.
fn http_error_to_wit(
    error: http::HttpError,
) -> provider_bindings::tau::extension::types::Error {
    use provider_bindings::tau::extension::types::Error;
    match error {
        http::HttpError::Refused(detail) => Error::Refused(detail),
        http::HttpError::Failed(detail) => Error::Failed(detail),
        http::HttpError::Invalid(detail) => Error::Invalid(detail),
    }
}

/// The interface's freestanding functions. Since 0.7.0 `request` is an
/// `async func` in the contract, and an async import needs the store (the
/// async lift is driven on it), so it lands here rather than on the plain
/// trait: the receiver is an `Accessor`, and store access is taken in
/// short synchronous blocks -- an `Accessor`'s borrow cannot cross an
/// await (wasmtime::component::Accessor::with).
impl<U> provider_bindings::tau::extension::http::HostWithStore<U> for HasSelf<ProviderState> {
    /// Consent is checked synchronously (the registry lock is scoped to
    /// the gate, so no std guard is held across the await); the request
    /// itself is awaited on the runtime -- 0.6.0 handed it to the blocking
    /// pool because the client was the blocking one.
    async fn request(
        accessor: &Accessor<U, Self>,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Resource<http::HostResponse>, provider_bindings::tau::extension::types::Error> {
        let client = accessor
            .with(|mut access| crate::lock_registry(&access.get().http).start(&url))
            .map_err(http_error_to_wit)?;
        let response = http::send(
            &client,
            &method,
            &url,
            &headers,
            &body,
            http::request_timeout(),
            http::idle_timeout(),
        )
        .await
        .map_err(http_error_to_wit)?;
        accessor.with(|mut access| {
            access.get().table.push(response).map_err(|_| {
                provider_bindings::tau::extension::types::Error::Invalid(
                    "http.request: the resource table is full".into(),
                )
            })
        })
    }
}

/// The resource's own methods that need no store access: the response
/// lives in this state's resource table, so `&mut self` is enough.
impl provider_bindings::tau::extension::http::HostResponse for ProviderState {
    async fn status(&mut self, response: Resource<http::HostResponse>) -> u16 {
        self.table
            .get(&response)
            .map(http::HostResponse::status)
            .unwrap_or(0)
    }

    async fn header(&mut self, response: Resource<http::HostResponse>, name: String) -> Option<String> {
        self.table
            .get(&response)
            .ok()
            .and_then(|response| response.header(&name))
    }

    /// Dropping the response is the only close it has: the body stream is
    /// what the guest holds, and the host's sender is released when the
    /// guest drops that stream.
    async fn drop(&mut self, response: Resource<http::HostResponse>) -> wasmtime::Result<()> {
        self.table.delete(response)?;
        Ok(())
    }
}

/// `models`'s request and result mention `tools.definition` /
/// `tools.tool-result`, which makes the component type import the whole
/// `tools` interface for those types -- and the component model makes the
/// host supply the complete instance, functions included, whether or not
/// anything can call them. Nothing in this world can (the provider world's
/// import list is `http` alone), so the honest answer is "not provided
/// here": an empty table, and a refusal for a call that cannot arrive.
impl provider_bindings::tau::extension::types::Host for ProviderState {}

impl provider_bindings::tau::extension::tools::Host for ProviderState {}

impl<U> provider_bindings::tau::extension::tools::HostWithStore<U> for HasSelf<ProviderState> {
    async fn definitions(
        _accessor: &Accessor<U, Self>,
    ) -> Vec<provider_bindings::tau::extension::tools::Definition> {
        Vec::new()
    }

    async fn execute(
        _accessor: &Accessor<U, Self>,
        _name: String,
        _arguments_json: String,
    ) -> provider_bindings::tau::extension::tools::ToolResult {
        // The contract gives this function no error channel (`-> tool-result`),
        // so the only honest unreachable answer is an in-band error.
        provider_bindings::tau::extension::tools::ToolResult {
            content: Vec::new(),
            is_error: true,
        }
    }
}

/// The marker the linker asks for even when every method is store-flagged.
impl provider_bindings::tau::extension::http::Host for ProviderState {}

/// `response.body` is the one call that has to hand the guest a stream, and
/// a stream handle lives in the store -- hence the `store` flag on this
/// method (docs/wit-redesign.md, leg 4's binding recipe).
impl<U> provider_bindings::tau::extension::http::HostResponseWithStore<U>
    for HasSelf<ProviderState>
{
    fn body(
        mut host: Access<U, Self>,
        response: Resource<http::HostResponse>,
    ) -> StreamReader<u8> {
        let stream = http::take_body(&mut host.get().table, &response);
        StreamReader::new(&mut host, stream).expect("stream allocation")
    }
}

#[async_trait]
impl tau_core::Model for WasmModel {
    async fn stream(
        &self,
        req: &tau_core::Request,
    ) -> futures::stream::BoxStream<'static, tau_core::ModelEvent> {
        use tau_core::ModelEvent;

        let request = provider_request::build(&self.model, req, self.auth.as_deref());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ModelEvent>();
        let shared = self.shared.clone();
        let call = tokio::spawn(async move {
            let mut guard = shared.lock().await;
            // 0.7.0: the provider hands back the event stream and its
            // verdict future, and the host reads both. Reading is a
            // *drive*: the guest's writer is only polled while the store
            // is running concurrently, so the call stays inside one
            // `run_concurrent` until the guest's writer ends (the
            // consumer's [`crate::DoneHolder`] says when) -- a drive that
            // returned earlier would deliver nothing and the model stream
            // would sit empty.
            let (done, done_rx) = crate::Done::new(1);
            let ProviderInstance { store, bindings } = &mut guard.instance;
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
                // The trap poisoned the guest; rebuild so the next run
                // reaches a fresh provider instead of trapping for the
                // rest of the REPL session.
                guard.revive().await;
            }
            result
        });

        async_stream::stream! {
            // Drain events until the component hangs up (the consumer
            // holding the sender is dropped when the guest's writer is),
            // then surface any trap as an error
            // event — the Model contract forbids propagating failures.
            // Events arrive already typed (0.2.0 contract): there is no
            // malformed-frame path to skip.
            while let Some(event) = rx.recv().await {
                yield event;
            }
            // Three layers deep: the spawned task's JoinError, the
            // drive's own error, then the closure's.
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
    //! The provider request is a typed value since 0.7.0 (`models.request`),
    //! so 0.6.0's JSON-shape assertions become field assertions.
    use crate::provider_bindings::exports::tau::extension::models;

    #[test]
    fn the_provider_request_carries_auth_only_when_consented() {
        let req = tau_core::Request {
            system: None,
            messages: vec![tau_core::Message::user("hi")],
            tools: vec![],
        };
        let without = super::provider_request::build("m", &req, None);
        assert!(without.auth.is_none());
        assert_eq!(without.model, "m");
        assert_eq!(without.messages.len(), 1);

        let with = super::provider_request::build("m", &req, Some("tok-1"));
        assert!(
            matches!(with.auth, Some(models::Auth::Bearer(token)) if token == "tok-1"),
            "the grant is the bearer arm and carries the token"
        );
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

    /// The request's media must arrive at the guest byte-for-byte. 8 MiB of
    /// noise is far beyond the few KiB every other test sends, and the guest
    /// reports back the length and checksum of the bytes IT received. Both
    /// are recomputed here over the very bytes the host sends, so truncation
    /// or corruption anywhere on the host→component copy mismatches loudly.
    /// (0.6.0 measured this over the request JSON, whose base64 inflated the
    /// fixture past 10 MiB; the payload is typed now, so the bytes cross the
    /// boundary as bytes.)
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
            media: Media::bytes("image/png", bytes.clone()),
        });
        let req = Request {
            system: None,
            messages: vec![message],
            tools: vec![],
        };
        assert_eq!(bytes.len(), 8 * 1024 * 1024, "fixture is 8 MiB of media");

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
        let want = format!("bytes={} fnv1a={:016x}", bytes.len(), fnv1a(&bytes));
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
            inject: false,
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
        state.channel.wire(bus.clone(), tau_core::control::channel().0);
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
            replace_probe_payload(ProbePoint::BeforeRun, wit_probes::Payload::SessionEnd(facts)),
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
        assert_eq!(parse_budget(Some(" 300 ")), Some(Duration::from_millis(300)));
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
            budget("TAU_VALIDATE_NOBODY_EXPORTS_THIS_MS", Duration::from_secs(7)),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn the_host_budgets_are_bounded_by_default() {
        // The knobs must not have replaced the production values: with no
        // environment in play, each host budget is the long one.
        assert_eq!(crate::http::request_timeout(), crate::http::REQUEST_TIMEOUT);
        assert_eq!(crate::http::idle_timeout(), crate::http::IDLE_TIMEOUT);
        assert_eq!(crate::ws::connect_timeout_ms(), crate::ws::CONNECT_TIMEOUT_MS);
        // The process pipes (bridge.rs): 0.7.0's guest cannot keep a clock
        // (no awaiting `wasi:clocks`), so the two deadlines 0.6.0's
        // `write-stdin(timeout-ms)` / `read-stdout(timeout-ms)` carried
        // live host-side now.
        assert_eq!(crate::bridge::stdin_idle_timeout(), crate::bridge::STDIN_IDLE_DEFAULT);
        assert_eq!(crate::bridge::pipe_idle_timeout(), crate::bridge::PIPE_IDLE_DEFAULT);
    }
}

/// Core to the provider world's generated types.
///
/// bindgen generates the contract's types once per world, so this mirrors
/// `convert::` one world over (the same mapping, different Rust types).
/// Aliasing the shared `types` interface into one module with `with:` is the
/// way to collapse that duplication; it is a change to the binding layer, not
/// to the contract, and is deliberately not part of 0.7.0's migration
/// (docs/wit-redesign.md section 7).
mod provider_request {
    use crate::provider_bindings::exports::tau::extension::models;
    use crate::provider_bindings::tau::extension::types as wit;
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

/// The "everyone is done" signal a drive loop waits on.
///
/// A host-side drive (`Store::run_concurrent`) has to stay active for as
/// long as the consumers it registered are alive: the machinery drops a
/// consumer when the guest's writer ends, and a driver that returns
/// earlier stops running the executor, so nothing would ever be
/// delivered. Each consumer holds one [`DoneHolder`] and drops it with
/// itself; the last one out hands the receiver its `()`.
pub(crate) struct Done {
    live: std::sync::atomic::AtomicUsize,
    signal: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl Done {
    /// `live` is how many holders will exist.
    pub(crate) fn new(live: usize) -> (std::sync::Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            std::sync::Arc::new(Self {
                live: std::sync::atomic::AtomicUsize::new(live),
                signal: std::sync::Mutex::new(Some(tx)),
            }),
            rx,
        )
    }

    /// One holder, for one consumer.
    pub(crate) fn holder(self: &std::sync::Arc<Self>) -> DoneHolder {
        DoneHolder(std::sync::Arc::clone(self))
    }
}

/// A consumer's share of a [`Done`]; dropping it counts it out.
pub(crate) struct DoneHolder(std::sync::Arc<Done>);

impl Drop for DoneHolder {
    fn drop(&mut self) {
        if self.0.live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1
            && let Some(tx) = self.0.signal.lock().unwrap().take()
        {
            let _ = tx.send(());
        }
    }
}
