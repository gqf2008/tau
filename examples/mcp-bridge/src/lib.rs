//! tau bridge component: exposes an external MCP server as tau tools. tau
//! core knows nothing about MCP — this component translates MCP's JSON-RPC
//! into the `tau:extension/tools` interface, over either transport:
//!
//! - stdio: spawns the server via the consent-gated `process` capability
//!   (TAU_MCP_COMMAND, a JSON argv array)
//! - streamable HTTP: POSTs to the server via the origin-allowlisted `http`
//!   capability (TAU_MCP_URL; the host scoped the network to that origin)
//!
//! Build:
//!   cargo build --manifest-path examples/mcp-bridge/Cargo.toml \
//!       --target wasm32-wasip2 --release

wit_bindgen::generate!({
    path: "../../wit/tau.wit",
    world: "bridge",
});

use std::sync::{Mutex, MutexGuard};

use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::{http, process as proc};

const PROTOCOL_VERSION: &str = "2025-06-18";
const READ_CHUNK: u32 = 65536;
/// Cap on one JSON-RPC message (and one HTTP response body): a broken or
/// hostile server flooding bytes without a newline would otherwise grow
/// linear memory until the allocator traps the whole component.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

enum Transport {
    Stdio(StdioConnection),
    Http(HttpConnection),
}

struct StdioConnection {
    handle: u64,
    /// Bytes read from stdout but not yet consumed as lines.
    buffer: Vec<u8>,
}

struct HttpConnection {
    url: String,
    session_id: Option<String>,
}

struct Connection {
    transport: Transport,
    next_id: u64,
}

static CONNECTION: Mutex<Option<Connection>> = Mutex::new(None);

struct McpBridge;

impl Tools for McpBridge {
    /// Connect (if needed) and enumerate the server's tools. Traps on
    /// handshake failure — the host surfaces that as a load error, which is
    /// the visibility a broken server deserves.
    fn definitions() -> Vec<Definition> {
        let mut guard = CONNECTION.lock().unwrap();
        let conn = connect(&mut guard).expect("mcp-bridge: connect/handshake failed");
        let result = conn
            .request("tools/list", serde_json::json!({}))
            .expect("mcp-bridge: tools/list failed");
        result["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|tool| Definition {
                name: tool["name"].as_str().unwrap_or_default().to_string(),
                description: tool["description"].as_str().unwrap_or_default().to_string(),
                parameters_json: tool["inputSchema"].to_string(),
            })
            .collect()
    }

    fn execute(name: String, arguments_json: String) -> ToolResult {
        let arguments = serde_json::from_str::<serde_json::Value>(&arguments_json)
            .unwrap_or_else(|_| serde_json::json!({}));
        let call = || -> Result<ToolResult, String> {
            let mut guard = CONNECTION.lock().map_err(|e| e.to_string())?;
            let conn = connect(&mut guard)?;
            let result = conn.request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": arguments }),
            )?;
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
            Ok(ToolResult {
                content,
                is_error: result["isError"].as_bool().unwrap_or(false),
            })
        };
        call().unwrap_or_else(|e| ToolResult {
            content: format!("mcp-bridge: {e}"),
            is_error: true,
        })
    }
}

/// Connect on first use: pick the transport from the consent env vars and
/// run the MCP initialize handshake. `guard` proves the caller holds the
/// lock.
fn connect<'a>(
    guard: &'a mut MutexGuard<'_, Option<Connection>>,
) -> Result<&'a mut Connection, String> {
    if guard.is_none() {
        let transport = match (
            std::env::var("TAU_MCP_URL").ok(),
            std::env::var("TAU_MCP_COMMAND").ok(),
        ) {
            (Some(url), _) => Transport::Http(HttpConnection {
                url,
                session_id: None,
            }),
            (None, Some(argv_json)) => {
                let argv: Vec<String> = serde_json::from_str(&argv_json)
                    .map_err(|e| format!("TAU_MCP_COMMAND is not a JSON argv array: {e}"))?;
                let handle =
                    proc::spawn(&argv).map_err(|e| format!("spawn {argv_json}: {e}"))?;
                Transport::Stdio(StdioConnection {
                    handle,
                    buffer: Vec::new(),
                })
            }
            (None, None) => {
                return Err("neither TAU_MCP_URL nor TAU_MCP_COMMAND granted by host".into())
            }
        };
        let mut conn = Connection {
            transport,
            next_id: 0,
        };
        conn.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "tau-mcp-bridge", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        let _ = conn.notify("notifications/initialized", serde_json::json!({}));
        **guard = Some(conn);
    }
    guard.as_mut().ok_or_else(|| "connection lost".to_string())
}

impl Connection {
    fn send(&mut self, message: &serde_json::Value) -> Result<(), String> {
        match &mut self.transport {
            Transport::Stdio(conn) => conn.send(message),
            Transport::Http(conn) => {
                let response = conn.post(message)?;
                // Notifications expect 202 with no body; a request sent via
                // send() only exists for initialize's handshake, which uses
                // request() — so anything here is best-effort.
                http::close(response);
                Ok(())
            }
        }
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), String> {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    /// Send a request and read messages until its response arrives.
    /// Notifications and unrelated traffic are skipped.
    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
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
                conn.send(&message)?;
                conn.read_response(id)?
            }
            Transport::Http(conn) => conn.post(&message).and_then(|h| conn.read_response(h, id))?,
        };
        if let Some(error) = response.get("error") {
            return Err(format!(
                "{method}: {}",
                error["message"].as_str().unwrap_or("rpc error")
            ));
        }
        Ok(response["result"].clone())
    }
}

impl StdioConnection {
    fn send(&mut self, message: &serde_json::Value) -> Result<(), String> {
        let mut bytes = message.to_string().into_bytes();
        bytes.push(b'\n');
        proc::write_stdin(self.handle, &bytes)
    }

    /// Read newline-delimited messages until the response for `id` arrives.
    fn read_response(&mut self, id: u64) -> Result<serde_json::Value, String> {
        loop {
            let message = self.read_message()?;
            if message["id"].as_u64() == Some(id) {
                return Ok(message);
            }
        }
    }

    /// Read one newline-delimited JSON message from the server's stdout.
    fn read_message(&mut self) -> Result<serde_json::Value, String> {
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
            let (chunk, eof) = proc::read_stdout(self.handle, READ_CHUNK)?;
            if chunk.is_empty() && eof {
                return Err("server closed stdout".to_string());
            }
            self.buffer.extend_from_slice(&chunk);
            if self.buffer.len() > MAX_MESSAGE {
                return Err("server message exceeds 16 MiB without a newline".to_string());
            }
        }
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

    fn post(&mut self, message: &serde_json::Value) -> Result<u64, String> {
        http::request(
            "POST",
            &self.url,
            &self.headers(),
            message.to_string().as_bytes(),
        )
    }

    /// Read the response body for `handle` until the JSON-RPC response for
    /// `id` arrives, then close. Handles both JSON and SSE responses.
    fn read_response(&mut self, handle: u64, id: u64) -> Result<serde_json::Value, String> {
        let result = self.read_response_inner(handle, id);
        http::close(handle);
        result
    }

    fn read_response_inner(&mut self, handle: u64, id: u64) -> Result<serde_json::Value, String> {
        let status = http::status(handle)?;
        if status == 202 {
            return Ok(serde_json::json!({})); // notification accepted
        }
        if status != 200 {
            let body = read_all(handle).unwrap_or_default();
            return Err(format!(
                "http {status}: {}",
                String::from_utf8_lossy(&body)
            ));
        }
        if let Some(session) = http::header(handle, "mcp-session-id")? {
            self.session_id = Some(session);
        }
        let content_type = http::header(handle, "content-type")?
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            // SSE: scan data: lines incrementally and close as soon as our
            // response arrives — the server may legally hold the stream
            // open for later server-initiated messages.
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
                let (chunk, eof) = http::read_body(handle, READ_CHUNK)?;
                if chunk.is_empty() && eof {
                    return Err("sse stream ended without our response".into());
                }
                buffer.extend_from_slice(&chunk);
                if buffer.len() > MAX_MESSAGE {
                    return Err("sse message exceeds 16 MiB without a newline".into());
                }
            }
        } else {
            let body = read_all(handle)?;
            let text = String::from_utf8_lossy(&body);
            let message: serde_json::Value = serde_json::from_str(text.trim())
                .map_err(|e| format!("bad json from server: {e}"))?;
            if message["id"].as_u64() == Some(id) || message.get("id").is_none() {
                return Ok(message);
            }
            Err(format!("response id mismatch: {text}"))
        }
    }
}

fn read_all(handle: u64) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let (chunk, eof) = http::read_body(handle, READ_CHUNK)?;
        body.extend_from_slice(&chunk);
        if body.len() > MAX_MESSAGE {
            return Err("response body exceeds 16 MiB".into());
        }
        if eof {
            return Ok(body);
        }
    }
}

export!(McpBridge);
