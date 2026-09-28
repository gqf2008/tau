//! ACP over stdio, end to end: the binary is spawned the way an editor
//! spawns it and driven with JSON-RPC over pipes.
//!
//! These are the handshake legs — `initialize`, `session/new`, what that
//! leaves on disk, and how the connection ends. The legs that run a turn
//! live next to the machinery they exercise.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long a leg waits on the agent before calling it a hang. The peer is
/// a pipe on this machine: anything approaching this is a deadlock, not
/// slowness — a handler that waits on the dispatch loop is the failure
/// this bound exists to catch.
const PATIENCE: Duration = Duration::from_secs(30);

/// A scratch directory per test, emptied on entry so a rerun starts clean
/// and a failure leaves its evidence behind.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tau-acp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

fn tau() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tau"))
}

/// One `tau --acp` process, spoken to over its pipes.
struct Connection {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
}

impl Connection {
    fn spawn(dir: &Path, extra: &[&str]) -> Connection {
        let mut child = tau()
            .arg("--acp")
            .args(extra)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tau --acp");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut err = child.stderr.take().expect("piped stderr");

        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        // Drained on its own thread: whoever is reading it is also the one
        // that might be blocked, and an undrained pipe would wedge tau.
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut collected = String::new();
            let _ = err.read_to_string(&mut collected);
            sink.lock().expect("stderr buffer").push_str(&collected);
        });

        Connection {
            child,
            stdin: Some(stdin),
            lines,
            stderr,
        }
    }

    fn diagnostics(&self) -> String {
        self.stderr.lock().expect("stderr buffer").clone()
    }

    fn send(&mut self, message: Value) {
        self.send_raw(&format!("{message}\n"));
    }

    /// Write one line exactly as given — the framing is `\n`-terminated
    /// JSON, and the reader has to tolerate a client that ends lines the
    /// other way.
    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is still open");
        stdin.write_all(line.as_bytes()).expect("write to tau");
        stdin.flush().expect("flush to tau");
    }

    /// The next message, which must be JSON-RPC: stdout carries the
    /// protocol and nothing else.
    fn next(&mut self) -> Value {
        match self.lines.recv_timeout(PATIENCE) {
            Ok(line) => serde_json::from_str(&line).unwrap_or_else(|error| {
                panic!(
                    "stdout carried something that is not JSON-RPC: {line:?} ({error})\n--- stderr ---\n{}",
                    self.diagnostics()
                )
            }),
            Err(RecvTimeoutError::Timeout) => panic!(
                "tau wrote nothing for {PATIENCE:?}\n--- stderr ---\n{}",
                self.diagnostics()
            ),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("tau closed stdout\n--- stderr ---\n{}", self.diagnostics())
            }
        }
    }

    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let answer = self.next();
        assert_eq!(
            answer["id"],
            json!(id),
            "the answer to {method} carries its own id: {answer}"
        );
        answer
    }

    fn initialize(&mut self, id: u64) -> Value {
        self.call(
            id,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}},
                "clientInfo": {"name": "tau-acp-test", "version": "0"},
            }),
        )
    }

    /// Close stdin — what an editor does on quit — and wait for the exit.
    fn shut_down(&mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait on tau") {
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!(
                    "tau did not exit after its stdin closed\n--- stderr ---\n{}",
                    self.diagnostics()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // A leg that panicked leaves the child running otherwise, and the
        // next leg's assertions would race a process nobody is reading.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_reports_what_tau_can_do() {
    let dir = scratch("handshake");
    let mut agent = Connection::spawn(&dir, &["--demo", "--no-builtin-tools"]);

    let answer = agent.initialize(1);
    let result = &answer["result"];
    assert_eq!(result["protocolVersion"], json!(1), "{answer}");
    // The three capability bits an editor reads before it offers anything.
    assert_eq!(
        result["agentCapabilities"]["loadSession"],
        json!(false),
        "session/load is not implemented and must not be advertised: {answer}"
    );
    assert_eq!(
        result["agentCapabilities"]["promptCapabilities"]["image"],
        json!(true),
        "images reach the model as Content::Image, so the bit is honest: {answer}"
    );
    assert_eq!(
        result["authMethods"],
        json!([]),
        "credentials come from the spawn environment, so there is no method to offer: {answer}"
    );
    assert_eq!(result["agentInfo"]["name"], json!("tau"), "{answer}");

    let status = agent.shut_down();
    assert_eq!(status.code(), Some(0), "clean EOF on stdin is a normal end");
}

#[test]
fn session_new_lands_a_file_where_the_flag_points() {
    let dir = scratch("session-new");
    let sessions = dir.join("sessions");
    let mut agent = Connection::spawn(
        &dir,
        &[
            "--demo",
            "--no-builtin-tools",
            "--session",
            sessions.to_str().expect("utf-8 scratch path"),
        ],
    );
    agent.initialize(1);

    let answer = agent.call(
        2,
        "session/new",
        json!({"cwd": dir.to_str().expect("utf-8 scratch path"), "mcpServers": []}),
    );
    let id = answer["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new answered without an id: {answer}"))
        .to_string();
    assert!(!id.is_empty(), "{answer}");

    let file = sessions.join(format!("{id}.jsonl"));
    assert!(
        file.exists(),
        "session/new promised {} and did not write it",
        file.display()
    );

    // A session written here is an ordinary tau session — that is the
    // documented escape hatch, and it doubles as the check that the file
    // parses as a tree.
    let tree = tau()
        .args(["tree", "--session"])
        .arg(&file)
        .output()
        .expect("run tau tree");
    assert!(
        tree.status.success(),
        "tau tree refused the session file: {}",
        String::from_utf8_lossy(&tree.stderr)
    );
    assert!(
        String::from_utf8_lossy(&tree.stdout).contains("empty session"),
        "a fresh session has an empty tree: {}",
        String::from_utf8_lossy(&tree.stdout)
    );

    // Each session gets its own id and its own file.
    let second = agent.call(
        3,
        "session/new",
        json!({"cwd": dir.to_str().expect("utf-8 scratch path"), "mcpServers": []}),
    );
    let second_id = second["result"]["sessionId"].as_str().expect("an id");
    assert_ne!(second_id, id, "{second}");
    assert!(sessions.join(format!("{second_id}.jsonl")).exists());

    agent.shut_down();
}

#[test]
fn a_client_that_ends_its_lines_with_crlf_is_understood() {
    let dir = scratch("crlf");
    let mut agent = Connection::spawn(&dir, &["--demo", "--no-builtin-tools"]);

    agent.send_raw(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\
         \"params\":{\"protocolVersion\":1}}\r\n",
    );
    let answer = agent.next();
    assert_eq!(answer["id"], json!(1), "{answer}");
    assert_eq!(answer["result"]["protocolVersion"], json!(1), "{answer}");

    agent.shut_down();
}

#[test]
fn what_tau_does_not_implement_it_says_so() {
    let dir = scratch("unimplemented");
    let mut agent = Connection::spawn(&dir, &["--demo", "--no-builtin-tools"]);
    agent.initialize(1);

    // v1 declares loadSession false, so a client that asks anyway gets the
    // protocol's own answer rather than a hang or a half-session.
    let load = agent.call(
        2,
        "session/load",
        json!({"sessionId": "nope", "cwd": dir, "mcpServers": []}),
    );
    assert_eq!(
        load["error"]["code"],
        json!(-32601),
        "session/load is method-not-found: {load}"
    );

    // No auth methods were advertised; authenticate stays unimplemented.
    let authenticate = agent.call(3, "authenticate", json!({"methodId": "anything"}));
    assert_eq!(
        authenticate["error"]["code"],
        json!(-32601),
        "authenticate is method-not-found: {authenticate}"
    );

    agent.shut_down();
}

#[test]
fn acp_refuses_to_share_the_run_with_a_prompt() {
    let out = tau()
        .args(["--acp", "-p", "hi"])
        .output()
        .expect("run tau");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a prompt and ACP are two ways to run; clap refuses the pair: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
