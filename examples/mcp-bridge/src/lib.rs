//! tau bridge component: exposes an external MCP server as tau tools. tau
//! core knows nothing about MCP — this component translates MCP's JSON-RPC
//! into the `tau:extension/tools` interface, over either transport:
//!
//! - stdio: spawns the server via the `process` capability
//!   (TAU_MCP_COMMAND, a JSON argv array) — no argv approval at call time
//!   since 0.8.0: the world a component is installed as is the declaration
//! - streamable HTTP: POSTs to the server via the `http` capability
//!   (TAU_MCP_URL) — no origin allowlist since 0.8.0: the request goes
//!   where the component says
//!
//! 0.7.0 shape, and the two things that changed are the two things this
//! bridge is made of:
//!
//! * the handshake runs in `definitions()`, which is `async` now — and that
//!   is exactly why the contract made it async. The tool list lives on a
//!   remote server, so enumerating it IS connect + initialize + tools/list,
//!   and a synchronously lowered export cannot wait for anything (measured:
//!   `block_on` inside a sync export traps with "cannot block a synchronous
//!   task before returning" — docs/wit-redesign.md section 5, leg 5).
//! * the pipes and the HTTP body are streams and resources, so the two
//!   transports share one shape: write, then read until the answer arrives.
//!   Every deadline 0.6.0 passed in (`write-stdin(.., timeout-ms)`,
//!   `read-stdout(.., timeout-ms)`, `http.request(.., timeout-ms)`) belongs
//!   to the host now, because a wasm32-wasip2 guest has no clock it can
//!   await: a child that never reads surfaces as the write coming back
//!   unwritten (the host's `TAU_PROCESS_STDIN_IDLE_TIMEOUT_MS` names it on
//!   stderr), and a silent pipe ends the stream (`TAU_PROCESS_STDOUT_IDLE_
//!   TIMEOUT_MS`) — both reported below as named transport failures.
//!
//! Build:
//!   cargo build --manifest-path examples/mcp-bridge/Cargo.toml \
//!       --target wasm32-wasip2 --release

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "bridge",
});

use std::cell::RefCell;

use exports::tau::extension::bridge_io::Guest as BridgeIo;
use exports::tau::extension::ingress_handler::{Guest as IngressHandler, Request, Response};
use exports::tau::extension::probes::{Guest as Probes, Payload, Point, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::http::{self, Response as HttpResponse};
use tau::extension::process::{Child, Options};
use tau::extension::types::{Error as HostError, ResultBlock};
use wit_bindgen::rt::async_support::{FutureReader, StreamReader, StreamResult, StreamWriter};

/// The version this bridge asks for (its newest).
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Every version the bridge can actually speak for the
/// initialize/tools-list/tools-call subset it uses. Per spec, a client
/// that does not support the server's chosen version must disconnect
/// rather than muddle through with possibly-divergent semantics.
const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
/// How many bytes one stream read offers room for. The read fills the spare
/// capacity it is given (a `Vec::new()` would read nothing), so this is the
/// one number the guest still picks — and it is a buffer size, not a
/// deadline.
const READ_CHUNK: usize = 65536;
/// Cap on one JSON-RPC message (and one HTTP response body): a broken or
/// hostile server flooding bytes without a newline would otherwise grow
/// linear memory until the allocator traps the whole component.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// Gate knob for the write path's stall leg (scripts/validate.sh): pad every
/// request so the gate can push more bytes than the host's stdin queue holds
/// at a server that never reads. It rides inside the JSON, so a real server
/// sees a valid message with one field it can ignore, and no argv has to
/// carry it.
fn pad_bytes() -> Option<usize> {
    std::env::var("TAU_MCP_PAD").ok()?.parse().ok()
}

// The connection slot. A `thread_local` cell rather than the 0.6.0
// `static Mutex`: the connection owns resources (`process.child`) and stream
// ends, none of which are `Sync`, and a guest is single-threaded anyway.
// Every call *takes* the connection out and puts it back, so no borrow is
// ever held across an await — the executor may drive other tasks while this
// one waits.
thread_local! {
    static CONNECTION: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

struct McpBridge;

enum Transport {
    Stdio(StdioConnection),
    Http(HttpConnection),
}

struct StdioConnection {
    /// Dropping the child kills it (the host's `Drop` for the resource),
    /// which is what makes "drop the dead connection and reconnect" one
    /// statement.
    _child: Child,
    /// The child's stdin: the guest owns the writer, the host pumps what it
    /// holds into the pipe. Writing awaits the host's queue, so the number
    /// that comes back is the UNSENT remainder, not a "taken" count.
    stdin: StreamWriter<u8>,
    /// The child's stdout, read as a stream (0.6.0 asked for one buffer at a
    /// time and named its own timeout).
    stdout: StreamReader<u8>,
    /// Bytes read from stdout but not yet consumed as lines.
    buffer: Vec<u8>,
    /// The host's verdict on the stdin pump. Deliberately never awaited: a
    /// pipe that broke shows up where the bridge can act on it — the next
    /// write returns its remainder — and this future is the diagnostic
    /// detail, not the signal.
    _stdin_done: FutureReader<Result<(), HostError>>,
}

struct HttpConnection {
    url: String,
    session_id: Option<String>,
}

struct Connection {
    transport: Transport,
    next_id: u64,
}

impl Tools for McpBridge {
    /// Connect (if needed) and enumerate the server's tools. Traps on
    /// handshake failure — the host surfaces that as a load error, which is
    /// the visibility a broken server deserves.
    async fn definitions() -> Vec<Definition> {
        let mut conn = connection()
            .await
            .unwrap_or_else(|e| panic!("mcp-bridge: connect/handshake failed: {e}"));
        // The panic carries the reason: this is the only failure path the
        // tools world has (definitions() returns no Result), and the host
        // surfaces the trap as a load error, guest stderr and all.
        let result = conn
            .request("tools/list", serde_json::json!({}))
            .await
            .unwrap_or_else(|e| panic!("mcp-bridge: tools/list failed: {e}"));
        let definitions = result["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|tool| Definition {
                name: tool["name"].as_str().unwrap_or_default().to_string(),
                description: tool["description"].as_str().unwrap_or_default().to_string(),
                parameters_json: tool["inputSchema"].to_string(),
            })
            .collect();
        restore(conn);
        definitions
    }

    async fn execute(name: String, arguments_json: String) -> ToolResult {
        let arguments = serde_json::from_str::<serde_json::Value>(&arguments_json)
            .unwrap_or_else(|_| serde_json::json!({}));
        let mut conn = match connection().await {
            Ok(conn) => conn,
            Err(failure) => return tool_error(&failure),
        };
        let outcome = conn
            .request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": arguments }),
            )
            .await;
        let result = match outcome {
            Ok(result) => {
                restore(conn);
                result
            }
            Err(failure) => {
                if matches!(failure, Failure::Transport(_)) {
                    // The server is gone; dropping the connection (never
                    // restored) kills the child and forgets the HTTP
                    // session, so the next call spawns and re-handshakes
                    // instead of writing to a dead pipe for the rest of the
                    // session. (This call still reports the error — no
                    // silent retry of a possibly non-idempotent tool.)
                    return tool_error(&failure);
                }
                restore(conn);
                return tool_error(&failure);
            }
        };
        // MCP tool results are content blocks; join the text ones.
        // Non-text blocks degrade to a placeholder so nothing is
        // silently dropped.
        let mut content = String::new();
        for block in result["content"].as_array().into_iter().flatten() {
            match block["type"].as_str().unwrap_or_default() {
                "text" => {
                    if let Some(text) = block["text"].as_str() {
                        content.push_str(text);
                    }
                }
                other => {
                    content.push_str(&format!("[mcp content block omitted: {other}]"));
                }
            }
        }
        ToolResult {
            content: vec![ResultBlock::Text(content)],
            is_error: result["isError"].as_bool().unwrap_or(false),
        }
    }
}

fn tool_error(failure: &Failure) -> ToolResult {
    ToolResult {
        content: vec![ResultBlock::Text(format!("mcp-bridge: {failure}"))],
        is_error: true,
    }
}

/// The live connection, connecting on first use. The caller owns it until
/// it calls [`restore`] (or drops it, which is how a dead transport is
/// forgotten).
async fn connection() -> Result<Connection, Failure> {
    if let Some(conn) = CONNECTION.with(|cell| cell.borrow_mut().take()) {
        return Ok(conn);
    }
    connect().await
}

fn restore(conn: Connection) {
    CONNECTION.with(|cell| *cell.borrow_mut() = Some(conn));
}

/// Pick the transport from the host-provided env vars and run the MCP
/// initialize handshake.
async fn connect() -> Result<Connection, Failure> {
    let transport = match (
        std::env::var("TAU_MCP_URL").ok(),
        std::env::var("TAU_MCP_COMMAND").ok(),
    ) {
        (Some(url), _) => Transport::Http(HttpConnection {
            url,
            session_id: None,
        }),
        (None, Some(argv_json)) => {
            let argv: Vec<String> = serde_json::from_str(&argv_json).map_err(|e| {
                Failure::Transport(format!("TAU_MCP_COMMAND is not a JSON argv array: {e}"))
            })?;
            Transport::Stdio(StdioConnection::spawn(&argv).await?)
        }
        (None, None) => {
            return Err(Failure::Transport(
                "neither TAU_MCP_URL nor TAU_MCP_COMMAND granted by host".into(),
            ));
        }
    };
    let mut conn = Connection {
        transport,
        next_id: 0,
    };
    let init = conn
        .request(
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "tau-mcp-bridge", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await?;
    // Version negotiation: the server picks; if it picked one we do
    // not speak, refuse the connection (spec: disconnect). A server
    // that omits the field is tolerated — older implementations do.
    let negotiated = init["protocolVersion"].as_str().unwrap_or_default();
    if !negotiated.is_empty() && !SUPPORTED_VERSIONS.contains(&negotiated) {
        return Err(Failure::Transport(format!(
            "server chose protocol version {negotiated:?}, which this bridge does not speak (supported: {})",
            SUPPORTED_VERSIONS.join(", ")
        )));
    }
    let _ = conn
        .notify("notifications/initialized", serde_json::json!({}))
        .await;
    Ok(conn)
}

/// Why a request failed. `Transport` means the connection is dead and
/// must be rebuilt (the server exited, the pipe broke, HTTP is
/// unreachable); `Rpc` means the server answered an error and the
/// connection is fine.
#[derive(Debug)]
enum Failure {
    Transport(String),
    Rpc(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(message) | Self::Rpc(message) => f.write_str(message),
        }
    }
}

impl Connection {
    async fn send(&mut self, message: &serde_json::Value) -> Result<(), Failure> {
        match &mut self.transport {
            Transport::Stdio(conn) => conn.send(message).await,
            Transport::Http(conn) => {
                // Notifications expect 202 with no body; `send` only exists
                // for them (initialize uses `request`), so anything here is
                // best-effort. Dropping the response closes it.
                let response = conn.post(message).await.map_err(Failure::Transport)?;
                drop(response);
                Ok(())
            }
        }
    }

    async fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), Failure> {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    /// Send a request and read messages until its response arrives.
    /// Notifications and unrelated traffic are skipped.
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Failure> {
        self.next_id += 1;
        let id = self.next_id;
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let response = match &mut self.transport {
            Transport::Stdio(conn) => {
                conn.send(&message).await?;
                conn.read_response(id).await.map_err(Failure::Transport)?
            }
            Transport::Http(conn) => {
                let response = conn.post(&message).await.map_err(Failure::Transport)?;
                conn.read_response(&response, id)
                    .await
                    .map_err(Failure::Transport)?
            }
        };
        if let Some(error) = response.get("error") {
            return Err(Failure::Rpc(format!(
                "{method}: {}",
                error["message"].as_str().unwrap_or("rpc error")
            )));
        }
        Ok(response["result"].clone())
    }
}

impl StdioConnection {
    async fn spawn(argv: &[String]) -> Result<Self, Failure> {
        let child = Child::spawn(&Options {
            argv: argv.to_vec(),
        })
        .map_err(|e| Failure::Transport(format!("spawn {argv:?}: {}", describe(&e))))?;
        // The stdin stream: the guest holds the writer for the life of the
        // connection, and dropping it (with the connection) closes the pipe
        // — that is the EOF the child sees, the same signal 0.6.0's explicit
        // close was.
        let (stdin, reader) = wit_stream::new::<u8>();
        let done = child.stdin(reader);
        let stdout = child.stdout();
        Ok(Self {
            _child: child,
            stdin,
            stdout,
            buffer: Vec::new(),
            _stdin_done: done,
        })
    }

    /// Send one newline-delimited message.
    ///
    /// The write is backpressured by the host's queue, and what comes back is
    /// what did NOT go out: an empty vector is a delivered message, and a
    /// non-empty one means the child stopped taking bytes (its pipe was
    /// closed, or the host's `TAU_PROCESS_STDIN_IDLE_TIMEOUT_MS` expired on a
    /// child that never reads). Either way the connection is no longer usable
    /// for this request — nothing is ever sent twice.
    async fn send(&mut self, message: &serde_json::Value) -> Result<(), Failure> {
        let mut message = message.clone();
        if let Some(pad) = pad_bytes() {
            message["_pad"] = serde_json::Value::String(" ".repeat(pad));
        }
        let mut bytes = message.to_string().into_bytes();
        bytes.push(b'\n');
        let total = bytes.len();
        let unwritten = self.stdin.write_all(bytes).await;
        if !unwritten.is_empty() {
            return Err(Failure::Transport(format!(
                "the server stopped taking stdin ({} of {total} bytes undelivered) -- is it reading?",
                unwritten.len()
            )));
        }
        Ok(())
    }

    /// Read newline-delimited messages until the response for `id` arrives.
    async fn read_response(&mut self, id: u64) -> Result<serde_json::Value, String> {
        loop {
            let message = self.read_message().await?;
            if message["id"].as_u64() == Some(id) {
                return Ok(message);
            }
        }
    }

    /// Read one newline-delimited JSON message from the server's stdout. The
    /// stream ends at EOF, when the child dies, or when the host's stdout
    /// idle budget expires — all three are "no more answers", which is all
    /// this loop can act on.
    async fn read_message(&mut self) -> Result<serde_json::Value, String> {
        loop {
            if let Some(pos) = self.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                return serde_json::from_str(line)
                    .map_err(|e| format!("bad json from server: {e}: {line}"));
            }
            match read_chunk(&mut self.stdout).await {
                Ok(chunk) => self.buffer.extend_from_slice(&chunk),
                Err(why) => return Err(why),
            }
            if self.buffer.len() > MAX_MESSAGE {
                return Err("server message exceeds 16 MiB without a newline".to_string());
            }
        }
    }
}

/// One read from a byte stream: the spare capacity is the request, the
/// returned bytes are the answer, and `Dropped` is the stream's only
/// terminal state.
async fn read_chunk(stream: &mut StreamReader<u8>) -> Result<Vec<u8>, String> {
    let buf = Vec::with_capacity(READ_CHUNK);
    let (status, filled) = stream.read(buf).await;
    match status {
        StreamResult::Complete(_) => Ok(filled),
        StreamResult::Dropped => Err("the server stopped talking (its pipe ended)".to_string()),
        StreamResult::Cancelled => Err("the read was cancelled".to_string()),
    }
}

impl HttpConnection {
    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("content-type".into(), "application/json".into()),
            (
                "accept".into(),
                "application/json, text/event-stream".into(),
            ),
            ("mcp-protocol-version".into(), PROTOCOL_VERSION.into()),
        ];
        if let Some(session) = &self.session_id {
            headers.push(("mcp-session-id".into(), session.clone()));
        }
        headers
    }

    async fn post(&self, message: &serde_json::Value) -> Result<HttpResponse, String> {
        http::request(
            "POST".to_string(),
            self.url.clone(),
            self.headers(),
            message.to_string().into_bytes(),
        )
        .await
        .map_err(|e| describe(&e))
    }

    /// Read the response body until the JSON-RPC response for `id` arrives.
    /// Handles both JSON and SSE responses; dropping the response resource at
    /// the end of this call closes the connection.
    async fn read_response(
        &mut self,
        response: &HttpResponse,
        id: u64,
    ) -> Result<serde_json::Value, String> {
        let status = response.status();
        if status == 202 {
            return Ok(serde_json::json!({})); // notification accepted
        }
        if status != 200 {
            let body = read_all(response).await.unwrap_or_default();
            return Err(format!("http {status}: {}", String::from_utf8_lossy(&body)));
        }
        if let Some(session) = response.header("mcp-session-id") {
            self.session_id = Some(session);
        }
        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            // SSE: scan data: lines incrementally and stop as soon as our
            // response arrives — the server may legally hold the stream open
            // for later server-initiated messages, and dropping the body
            // stream is how this bridge says it is done reading.
            let mut body = response.body();
            let mut buffer: Vec<u8> = Vec::new();
            loop {
                while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line);
                    let Some(data) = line.trim().strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    if let Ok(message) = serde_json::from_str::<serde_json::Value>(data)
                        && message["id"].as_u64() == Some(id)
                    {
                        return Ok(message);
                    }
                }
                match read_chunk(&mut body).await {
                    Ok(chunk) => buffer.extend_from_slice(&chunk),
                    Err(_) => return Err("sse stream ended without our response".into()),
                }
                if buffer.len() > MAX_MESSAGE {
                    return Err("sse message exceeds 16 MiB without a newline".into());
                }
            }
        }
        let body = read_all(response).await?;
        let text = String::from_utf8_lossy(&body);
        let message: serde_json::Value =
            serde_json::from_str(text.trim()).map_err(|e| format!("bad json from server: {e}"))?;
        if message["id"].as_u64() == Some(id) || message.get("id").is_none() {
            return Ok(message);
        }
        Err(format!("response id mismatch: {text}"))
    }
}

async fn read_all(response: &HttpResponse) -> Result<Vec<u8>, String> {
    let mut body = response.body();
    let mut out = Vec::new();
    loop {
        match read_chunk(&mut body).await {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(_) => return Ok(out), // the end of the body IS the whole body
        }
        if out.len() > MAX_MESSAGE {
            return Err("response body exceeds 16 MiB".into());
        }
    }
}

/// A host error, spelled for the log. The typed variant is what a guest
/// branches on (never the string); the string is the host's detail, meant
/// for the human reading the log.
fn describe(error: &HostError) -> String {
    match error {
        HostError::Failed(why) => format!("failed: {why}"),
        HostError::Invalid(why) => format!("invalid: {why}"),
    }
}

/// The bridge world exports `bridge-io` since 0.7.0: the asynchronous
/// half of a probe point, for a bridge whose I/O legs must wait (connect
/// at session start, post a reply after a response). This adapter does
/// all of its I/O inside tool calls, which are async already — so there
/// is nothing to do around a probe, and the empty turn says so.
impl BridgeIo for McpBridge {
    async fn turn(_point: Point, _payload: Payload) {}
}

/// Nothing to observe — the bridge world exports probes since 0.3.0
/// (docs/im-channels.md); an empty points() list opts out.
impl Probes for McpBridge {
    fn points() -> Vec<Point> {
        Vec::new()
    }

    fn probe(_point: Point, _payload: Payload) -> Verdict {
        Verdict::Continue
    }
}

// The 0.3.0 bridge world makes ingress-handler a mandatory export. This
// adapter has no webhook leg (it never calls ingress.listen), so nothing
// can invoke this — the stub is explicit, not dead weight.
impl IngressHandler for McpBridge {
    async fn handle_request(_request: Request) -> Response {
        Response {
            status: 501,
            headers: Vec::new(),
            body: b"this bridge has no webhook leg".to_vec(),
        }
    }
}

export!(McpBridge);
