//! Bridge components (world "bridge"): external tool protocols (MCP) behind
//! WIT. Beyond the deny-all sandbox the host grants exactly two scoped
//! capabilities, and only on explicit consent: spawn-with-pipes (the caller
//! passes the allowed command argv) and origin-allowlisted HTTP (the caller
//! passes the allowed origins). The host knows nothing about MCP; the
//! bridge component speaks whatever protocol it likes over the pipes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tau_core::tool::{Tool, ToolDef, ToolOutput};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::Store;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::{bridge_bindings, ExtError, ExtensionHost};

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

#[derive(Default)]
struct ProcessRegistry {
    next: u64,
    children: HashMap<u64, ChildProcess>,
}

impl ProcessRegistry {
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
        let handle = self.next;
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
#[derive(Default)]
pub struct BridgeConsent {
    pub command: Option<Vec<String>>,
    pub mcp_url: Option<String>,
    pub origins: HashSet<String>,
}

/// Extract the consent origin ("scheme://host[:port]") from an http(s) URL.
/// Public so the CLI can build a consent allowlist from --mcp-url.
pub fn origin_of(url: &str) -> Option<String> {
    HttpRegistry::origin_of(url)
}

/// One in-flight HTTP response: headers already received, body drained by a
/// reader thread into a channel — same shape as ChildProcess, so SSE
/// streams can be consumed incrementally and closed early.
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    rx: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    pending: VecDeque<u8>,
    eof: bool,
}

#[derive(Default)]
struct HttpRegistry {
    next: u64,
    responses: HashMap<u64, HttpResponse>,
    /// Consented origins: "scheme://host[:port]". Empty = deny all.
    origins: HashSet<String>,
}

impl HttpRegistry {
    fn origin_of(url: &str) -> Option<String> {
        let (scheme, rest) = url.split_once("://")?;
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let authority = rest.split('/').next()?;
        // Strip any userinfo; host[:port] is what consent covers.
        let host_port = authority.rsplit('@').next()?;
        if host_port.is_empty() {
            return None;
        }
        Some(format!("{scheme}://{}", host_port.to_ascii_lowercase()))
    }

    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<u64, String> {
        let origin = Self::origin_of(url).ok_or_else(|| format!("bad url: {url}"))?;
        if !self.origins.contains(&origin) {
            return Err(format!(
                "http: origin {origin} not in consent allowlist ({} granted)",
                self.origins.len()
            ));
        }
        // Redirects are never followed: a redirect would silently move the
        // request to an origin the user did not consent to.
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| format!("bad method {method}: {e}"))?;
        let mut request = client.request(method, url).body(body.to_vec());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let mut response = request.send().map_err(|e| format!("http {url}: {e}"))?;
        let status = response.status().as_u16();
        let response_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match response.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("read body: {e}")));
                        break;
                    }
                }
            }
        });
        let handle = self.next;
        self.next += 1;
        self.responses.insert(
            handle,
            HttpResponse {
                status,
                headers: response_headers,
                rx,
                pending: VecDeque::new(),
                eof: false,
            },
        );
        Ok(handle)
    }

    fn get(&mut self, handle: u64) -> Result<&mut HttpResponse, String> {
        self.responses
            .get_mut(&handle)
            .ok_or_else(|| format!("unknown http handle {handle}"))
    }
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
        Ok(self.http.get(handle)?.status)
    }

    fn header(&mut self, handle: u64, name: String) -> Result<Option<String>, String> {
        let response = self.http.get(handle)?;
        let name = name.to_ascii_lowercase();
        Ok(response
            .headers
            .iter()
            .find(|(n, _)| n.to_ascii_lowercase() == name)
            .map(|(_, v)| v.clone()))
    }

    fn read_body(&mut self, handle: u64, max: u32) -> Result<(Vec<u8>, bool), String> {
        let response = self.http.get(handle)?;
        let max = max.max(1) as usize;
        // Same contract as read_stdout: block only until SOMETHING arrives.
        if response.pending.is_empty() && !response.eof {
            match response.rx.recv() {
                Ok(Ok(chunk)) => response.pending.extend(chunk),
                Ok(Err(e)) => return Err(e),
                Err(_) => response.eof = true,
            }
        }
        let take = response.pending.len().min(max);
        let bytes: Vec<u8> = response.pending.drain(..take).collect();
        Ok((bytes, response.eof && response.pending.is_empty()))
    }

    fn close(&mut self, handle: u64) {
        self.http.responses.remove(&handle);
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

    fn kill(&mut self, handle: u64) {
        if let Some(mut child) = self.processes.children.remove(&handle) {
            let _ = child.child.kill();
            let _ = child.child.wait();
        }
    }
}

struct BridgeInstance {
    store: Store<BridgeState>,
    bindings: bridge_bindings::Bridge,
}

type SharedBridge = Arc<Mutex<BridgeInstance>>;

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
        let component = Component::from_binary(&self.engine, &bytes).map_err(|e| ExtError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let mut linker: Linker<BridgeState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        bridge_bindings::Bridge::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let mut ctx = WasiCtxBuilder::new();
        if let Some(command) = &consent.command {
            let command_json = serde_json::to_string(command).unwrap_or_else(|_| "[]".into());
            ctx.env("TAU_MCP_COMMAND", &command_json);
        }
        if let Some(url) = &consent.mcp_url {
            ctx.env("TAU_MCP_URL", url);
        }
        let state = BridgeState {
            ctx: ctx.build(),
            table: ResourceTable::new(),
            processes: ProcessRegistry::default(),
            http: HttpRegistry {
                origins: consent.origins,
                ..HttpRegistry::default()
            },
        };
        let mut store = Store::new(&self.engine, state);
        let bindings = bridge_bindings::Bridge::instantiate(&mut store, &component, &linker)
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("bridge instantiation failed: {e}"),
            })?;

        // definitions() performs the protocol handshake (MCP initialize +
        // tools/list); failure here means the server is unusable.
        let definitions = bindings
            .tau_extension_tools()
            .call_definitions(&mut store)
            .map_err(|e| ExtError::Load {
                path: path.display().to_string(),
                reason: format!("bridge handshake failed: {e}"),
            })?;

        let shared: SharedBridge = Arc::new(Mutex::new(BridgeInstance { store, bindings }));
        Ok(definitions
            .into_iter()
            .map(|def| {
                Box::new(BridgeTool {
                    def: ToolDef {
                        name: def.name,
                        description: def.description,
                        parameters: serde_json::from_str(&def.parameters_json)
                            .unwrap_or_else(|_| serde_json::json!({ "type": "object" })),
                    },
                    shared: shared.clone(),
                }) as Box<dyn Tool>
            })
            .collect())
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
            let mut guard = shared.lock().unwrap();
            let BridgeInstance { store, bindings } = &mut *guard;
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
            Ok(Err(e)) => ToolOutput::err(format!("bridge trap: {e}")),
            Err(e) => ToolOutput::err(format!("bridge task failed: {e}")),
        }
    }
}
