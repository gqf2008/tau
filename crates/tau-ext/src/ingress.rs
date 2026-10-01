//! Webhook ingress (docs/im-channels.md): WASI p2 has no listen, so the
//! host runs the HTTP server for bridge components. The listen
//! listen ADDRESS (CLI `--ingress`), orthogonal to the origin allowlist.
//! The host is a pipe — method/path/headers/body pass through untouched;
//! signature verification against platform secrets is the component's
//! job. Each inbound request is pushed into the component's
//! `ingress-handler` export under the instance lock (the server thread
//! drives the awaited call to completion, so a component mid-tool-call
//! queues the webhook — platforms retry, that is a fact of the platform,
//! not a loss) and the export's return value is written back verbatim as
//! the HTTP response.
//!
//! TLS is terminated by the tunnel/reverse proxy in front (the docs say
//! so honestly); this listener speaks plain HTTP on the configured
//! loopback/internal address only.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::bridge::{SharedBridge, SharedBridgeInner, ingress_dispatch};
use crate::bridge_bindings::exports::tau::extension::ingress_handler as wit_ingress;

/// How long the accept loop sleeps between shutdown polls.
const TICK: Duration = Duration::from_millis(250);

/// Why a listen failed, in the contract's own three-way split
/// (`types.error`): the variant is what a guest branches on, the detail is
/// for the human reading the log. Same shape as `http::HttpError`.
#[derive(Debug)]
pub(crate) enum IngressError {
    /// The bind failed (taken, no permission), or the host serves no
    /// listen address at all.
    Failed(String),
    /// The call is not valid here: a route that is not an absolute path, or
    /// one that is already registered.
    Invalid(String),
}

impl std::fmt::Display for IngressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(detail) => write!(f, "failed: {detail}"),
            Self::Invalid(detail) => write!(f, "invalid: {detail}"),
        }
    }
}

/// One registered webhook route, owned by the guest as
/// `ingress.registration`. Dropping it unregisters the route (the servers
/// keep running -- a listener with zero routes answers 404, which is the
/// honest state: the address stays configured).
pub struct HostRegistration {
    route: String,
    registry: Arc<IngressRegistry>,
}

impl Drop for HostRegistration {
    fn drop(&mut self) {
        let _ = self.registry.close(&self.route);
    }
}

/// One bridge's ingress world: the configured listen addresses, the
/// routes it registered, the servers it spawned, and (late-bound, after
/// instantiation) the instance the server threads push requests into.
pub(crate) struct IngressRegistry {
    /// Consented `addr:port` list (BridgeConsent.ingress). Empty = every
    /// listen() fails naming the missing consent.
    addrs: Vec<String>,
    routes: Mutex<HashSet<String>>,
    servers: Mutex<HashMap<String, JoinHandle<()>>>,
    /// Weak on purpose: the registry is reachable from the factory
    /// (BridgeFactory.ingress), so a strong ref here would cycle and
    /// nothing would ever shut the listener down. Upgrade per request;
    /// a dead instance answers 503.
    target: Mutex<Weak<SharedBridgeInner>>,
    shutdown: AtomicBool,
}

impl IngressRegistry {
    pub(crate) fn new(addrs: Vec<String>) -> Self {
        Self {
            addrs,
            routes: Mutex::new(HashSet::new()),
            servers: Mutex::new(HashMap::new()),
            target: Mutex::new(Weak::new()),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Called once the bridge instance exists; server threads upgrade
    /// the weak ref per request (load-in-progress or a dropped bridge
    /// answers 503).
    pub(crate) fn bind(&self, target: &SharedBridge) {
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(target);
    }

    /// Stop the accept loops (threads exit within a tick and drop their
    /// Arcs; the registry itself drops with the last one). Called from
    /// BridgeFactory::drop — the factory is the owner whose lifetime
    /// tracks the bridge's.
    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// `ingress.listen`: register `route` on every consented address,
    /// spawning the address's server on first use. The returned handle IS
    /// the registration since 0.7.0: dropping it stops serving the route
    /// (0.6.0's explicit `close`, and its "close a route that was never
    /// registered" error, are both unrepresentable now).
    pub(crate) fn listen(self: &Arc<Self>, route: &str) -> Result<HostRegistration, IngressError> {
        if self.addrs.is_empty() {
            // Not a gate (0.8.0): the listen address is host configuration, and
            // a host with none configured has no server to register a route on.
            return Err(IngressError::Failed(
                "ingress: the host serves no listen address (configure one with --ingress)".into(),
            ));
        }
        if !route.starts_with('/') || route.contains(['?', '#']) {
            return Err(IngressError::Invalid(format!(
                "invalid route {route:?}: an absolute path without query/fragment"
            )));
        }
        {
            let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
            if !routes.insert(route.to_string()) {
                return Err(IngressError::Invalid(format!(
                    "route {route} already registered"
                )));
            }
        }
        for addr in &self.addrs {
            // Past the consent check above, so a failure here is the bind
            // itself (address in use, no permission to bind): `failed`.
            self.ensure_server(addr).map_err(IngressError::Failed)?;
        }
        Ok(HostRegistration {
            route: route.to_string(),
            registry: Arc::clone(self),
        })
    }

    /// `ingress.close`: stop serving `route`. Servers keep running until
    /// drop — a listener with zero routes answers 404, which is the
    /// honest state (the address stays consented).
    pub(crate) fn close(&self, route: &str) -> Result<(), String> {
        let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        if routes.remove(route) {
            Ok(())
        } else {
            Err(format!("route {route} was never registered"))
        }
    }

    fn ensure_server(self: &Arc<Self>, addr: &str) -> Result<(), String> {
        let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        if servers.contains_key(addr) {
            return Ok(());
        }
        let server =
            tiny_http::Server::http(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        let registry = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name(format!("ingress-{addr}"))
            .spawn(move || serve_loop(registry, server))
            .map_err(|e| format!("cannot spawn ingress server for {addr}: {e}"))?;
        servers.insert(addr.to_string(), handle);
        Ok(())
    }

    /// Is `route` currently registered? (The server thread's gate.)
    fn serves(&self, route: &str) -> bool {
        self.routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(route)
    }
}

// No Drop impl: server threads hold Arcs of the registry, so Drop would
// never fire while they run — shutdown flows the other way
// (BridgeFactory::drop → shutdown() → threads exit → Arcs drop).

/// One server's accept loop: poll with a timeout so shutdown lands,
/// dispatch each request to the component or answer it locally.
fn serve_loop(registry: Arc<IngressRegistry>, server: tiny_http::Server) {
    while !registry.shutdown.load(Ordering::Relaxed) {
        let request = match server.recv_timeout(TICK) {
            Ok(Some(request)) => request,
            // Timeout (poll again) or a recv error (client vanished) —
            // neither ends the listener.
            Ok(None) | Err(_) => continue,
        };
        dispatch(&registry, request);
    }
}

/// Answer one inbound request: 404 for unregistered paths, 503 while
/// the component is not bound, otherwise the component's export result
/// (a trap answers 502 after the revive). A respond error means the
/// client went away; the loop serves on.
fn dispatch(registry: &Arc<IngressRegistry>, mut request: tiny_http::Request) {
    let url = request.url().to_string();
    let path = url.split(['?', '#']).next().unwrap_or("/").to_string();
    // The query passes through RAW (pipe doctrine): signature schemes
    // live in it (wecom msg_signature), and parsing is the component's
    // semantics, not the host's.
    let query = url
        .split('#')
        .next()
        .and_then(|no_frag| no_frag.split_once('?'))
        .map(|(_, q)| q.to_string())
        .unwrap_or_default();
    let respond = |request: tiny_http::Request,
                   status: u16,
                   headers: Vec<(String, String)>,
                   body: Vec<u8>| {
        let mut response = tiny_http::Response::new(
            tiny_http::StatusCode(status),
            Vec::new(),
            std::io::Cursor::new(body),
            None,
            None,
        );
        for (name, value) in headers {
            // Header::from_bytes rejects invalid bytes — a component
            // cannot smuggle CRLF into the response this way.
            if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
                response.add_header(header);
            }
        }
        if let Err(e) = request.respond(response) {
            eprintln!("tau ingress: respond failed: {e}");
        }
    };
    if !registry.serves(&path) {
        return respond(request, 404, Vec::new(), b"no such route".to_vec());
    }
    let method = request.method().to_string();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string()))
        .collect();
    let mut body = Vec::new();
    // tiny_http hands us exactly the body bytes (chunked is decoded by
    // the parser before we see it).
    if request.as_reader().read_to_end(&mut body).is_err() {
        return respond(request, 400, Vec::new(), b"unreadable body".to_vec());
    }
    let target = registry
        .target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upgrade();
    let Some(target) = target else {
        return respond(
            request,
            503,
            Vec::new(),
            b"component not bound yet".to_vec(),
        );
    };
    let wit_request = wit_ingress::Request {
        route: path.clone(),
        method,
        path,
        query,
        headers,
        body,
    };
    match ingress_dispatch(&target, wit_request) {
        Ok(response) => respond(request, response.status, response.headers, response.body),
        Err(trap_note) => respond(request, 502, Vec::new(), trap_note.into_bytes()),
    }
}
