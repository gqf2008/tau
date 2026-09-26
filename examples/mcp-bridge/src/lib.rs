//! tau bridge component: exposes an external MCP server (stdio transport) as
//! tau tools. tau core knows nothing about MCP — this component translates
//! MCP's JSON-RPC into the `tau:extension/tools` interface, using the
//! consent-gated `tau:extension/process` host capability for the pipes.
//!
//! The host grants exactly one env var, TAU_MCP_COMMAND: a JSON argv array
//! for the server (e.g. ["python", "server.py"]). Nothing else is granted.
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
use tau::extension::process as proc;

const PROTOCOL_VERSION: &str = "2025-06-18";
const READ_CHUNK: u32 = 65536;

struct Connection {
    handle: u64,
    next_id: u64,
    /// Bytes read from stdout but not yet consumed as lines.
    buffer: Vec<u8>,
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

/// Connect on first use: spawn the consented command, run the MCP
/// initialize handshake. `guard` proves the caller holds the lock.
fn connect<'a>(guard: &'a mut MutexGuard<'_, Option<Connection>>) -> Result<&'a mut Connection, String> {
    if guard.is_none() {
        let argv_json = std::env::var("TAU_MCP_COMMAND")
            .map_err(|_| "TAU_MCP_COMMAND env var not granted by host".to_string())?;
        let argv: Vec<String> = serde_json::from_str(&argv_json)
            .map_err(|e| format!("TAU_MCP_COMMAND is not a JSON argv array: {e}"))?;
        let handle = proc::spawn(&argv).map_err(|e| format!("spawn {argv_json}: {e}"))?;
        let mut conn = Connection {
            handle,
            next_id: 0,
            buffer: Vec::new(),
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
        let mut bytes = message.to_string().into_bytes();
        bytes.push(b'\n');
        proc::write_stdin(self.handle, &bytes)
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
    fn request(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        loop {
            let message = self.read_message()?;
            if message["id"].as_u64() == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(format!("{method}: {}", error["message"].as_str().unwrap_or("rpc error")));
                }
                return Ok(message["result"].clone());
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
        }
    }
}

export!(McpBridge);
