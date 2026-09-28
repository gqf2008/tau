//! ACP over stdio, end to end: the binary is spawned the way an editor
//! spawns it and driven with JSON-RPC over pipes.
//!
//! Two halves: the handshake (`initialize`, `session/new`, what that
//! leaves on disk, how the connection ends) and the turn (`session/prompt`
//! and its `session/update` stream, `session/cancel`). Both are driven the
//! way a client drives them — write a line, read a line, assert on what
//! came back — because the things worth pinning here are exactly the ones
//! a unit test cannot see: what reaches the wire, and in what order.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
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
        Connection::spawn_with(dir, extra, &[])
    }

    /// [`Connection::spawn`], with environment: the legs here need
    /// `TAU_ACP_STALL_MS`, and a client that configures its agent through
    /// the spawn environment is the documented way to run this mode.
    fn spawn_with(dir: &Path, extra: &[&str], env: &[(&str, &str)]) -> Connection {
        let mut command = tau();
        command.arg("--acp").args(extra).current_dir(dir);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
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

    /// `session/new` in `dir`, returning the id the client will use.
    fn new_session(&mut self, id: u64, dir: &Path) -> String {
        let answer = self.call(
            id,
            "session/new",
            json!({"cwd": dir.to_str().expect("utf-8 scratch path"), "mcpServers": []}),
        );
        answer["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new answered without an id: {answer}"))
            .to_string()
    }

    fn prompt(&mut self, id: u64, session: &str, text: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {"sessionId": session, "prompt": [{"type": "text", "text": text}]},
        }));
    }

    /// Read until the message carrying this id, collecting the
    /// notifications that arrive first: they interleave with the answer,
    /// and which ones arrived is what most of these legs are about.
    fn answer(&mut self, id: u64, notes: &mut Vec<Value>) -> Value {
        loop {
            let line = self.next();
            if line.get("id").is_some() {
                assert_eq!(line["id"], json!(id), "the answer to request {id}: {line}");
                return line;
            }
            notes.push(line);
        }
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
    let out = tau().args(["--acp", "-p", "hi"]).output().expect("run tau");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a prompt and ACP are two ways to run; clap refuses the pair: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The `update` object of every `session/update` notification, in arrival
/// order — and the assertion that nothing else arrived, and that it all
/// belonged to this session. A leg that says which updates came also says
/// nothing else did.
fn updates(notes: &[Value], session: &str) -> Vec<Value> {
    notes
        .iter()
        .map(|note| {
            assert_eq!(note["method"], json!("session/update"), "{note}");
            assert_eq!(note["params"]["sessionId"], json!(session), "{note}");
            note["params"]["update"].clone()
        })
        .collect()
}

/// The `sessionUpdate` tag of each update, in arrival order.
fn kinds(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .map(|update| {
            update["sessionUpdate"]
                .as_str()
                .unwrap_or_else(|| panic!("an update without a tag: {update}"))
                .to_string()
        })
        .collect()
}

/// The text an agent message chunk carries.
fn chunk_text(update: &Value) -> &str {
    update["content"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a text chunk: {update}"))
}

/// `tau tree` over one session file: the file is the session, so this is
/// how a leg checks what a turn left behind.
fn tree_of(file: &Path) -> String {
    let tree = tau()
        .args(["tree", "--session"])
        .arg(file)
        .output()
        .expect("run tau tree");
    assert!(
        tree.status.success(),
        "tau tree refused {}: {}",
        file.display(),
        String::from_utf8_lossy(&tree.stderr)
    );
    String::from_utf8_lossy(&tree.stdout).to_string()
}

#[test]
fn a_prompt_streams_the_answer_and_writes_the_session() {
    let dir = scratch("prompt");
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
    let session = agent.new_session(2, &dir);

    agent.prompt(3, &session, "hello");
    let mut notes = Vec::new();
    let answer = agent.answer(3, &mut notes);
    assert_eq!(
        answer["result"]["stopReason"],
        json!("end_turn"),
        "{answer}"
    );

    // The answer arrives as chunks, and the demo model streams three of
    // them (one of which is empty — a delta is passed through as it
    // comes, not quietly dropped).
    let streamed = updates(&notes, &session);
    assert_eq!(
        kinds(&streamed),
        vec!["agent_message_chunk"; 3],
        "{streamed:?}"
    );
    let text: String = streamed.iter().map(chunk_text).collect();
    assert_eq!(
        text, "tau is alive. (faux model — set ANTHROPIC_API_KEY or OPENAI_API_KEY for a real one)",
        "{streamed:?}"
    );

    // The file is the record: the prompt and the answer, nothing else.
    let file = sessions.join(format!("{session}.jsonl"));
    let tree = tree_of(&file);
    assert!(tree.contains("hello"), "{tree}");
    assert!(tree.contains("tau is alive."), "{tree}");
    assert_eq!(tree.lines().count(), 2, "one prompt, one answer: {tree}");

    // A second turn runs on what the first one wrote — the history is the
    // file, not anything this process kept in memory.
    agent.prompt(4, &session, "again");
    let mut notes = Vec::new();
    let answer = agent.answer(4, &mut notes);
    assert_eq!(
        answer["result"]["stopReason"],
        json!("end_turn"),
        "{answer}"
    );
    let tree = tree_of(&file);
    assert_eq!(tree.lines().count(), 4, "the second turn appended: {tree}");

    let status = agent.shut_down();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn a_built_in_tool_runs_and_reports_over_the_wire() {
    let dir = scratch("tool");
    let sessions = dir.join("sessions");
    // `--tools ls` names one read-only built-in, and the demo script only
    // calls a tool the user put in the run: the model asks for `ls`, the
    // loop runs it for real, and the answer quotes what it printed. A
    // whole tool round trip with no wasm component anywhere in it.
    let mut agent = Connection::spawn(
        &dir,
        &[
            "--demo",
            "--tools",
            "ls",
            "--session",
            sessions.to_str().expect("utf-8 scratch path"),
        ],
    );
    agent.initialize(1);
    let session = agent.new_session(2, &dir);

    agent.prompt(3, &session, "list it");
    let mut notes = Vec::new();
    let answer = agent.answer(3, &mut notes);
    assert_eq!(
        answer["result"]["stopReason"],
        json!("end_turn"),
        "{answer}"
    );

    let streamed = updates(&notes, &session);
    assert_eq!(
        kinds(&streamed),
        vec![
            "tool_call",
            "tool_call_update",
            "agent_message_chunk",
            "agent_message_chunk",
            "agent_message_chunk",
        ],
        "{streamed:?}"
    );

    // Announced before its result, so an editor has something to draw the
    // update on: the call carries the tool's name as its title, the kind
    // it is drawn by, and an id namespaced to this turn — the loop's own
    // ids come from the provider and repeat across turns.
    let call = &streamed[0];
    assert_eq!(call["toolCallId"], json!("1:demo-call-1"), "{call}");
    assert_eq!(call["title"], json!("ls"), "{call}");
    assert_eq!(call["kind"], json!("read"), "{call}");
    assert_eq!(call["status"], json!("in_progress"), "{call}");

    // Closed by the update, carrying a preview of what the tool printed:
    // the scratch directory holds the session directory and nothing else.
    let done = &streamed[1];
    assert_eq!(done["status"], json!("completed"), "{done}");
    let preview = done["content"][0]["content"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("the update carries the output: {done}"));
    assert!(preview.contains("sessions/"), "{done}");

    // And the model saw it: the second round answers with what the tool
    // returned, which is the loop closing rather than the call firing.
    let text: String = streamed[2..].iter().map(chunk_text).collect();
    assert!(text.contains("The tool answered: sessions/"), "{text}");

    // The session file keeps the provider's own id and the tool result.
    let file = sessions.join(format!("{session}.jsonl"));
    let tree = tree_of(&file);
    assert!(tree.contains("[tool call: ls]"), "{tree}");
    assert!(tree.contains("[tool result]"), "{tree}");

    // The next turn runs on what the first one wrote. The demo script
    // calls the picked tool only while the request has no tool result in
    // it, so a second call here — or an answer that does not quote the
    // first one's output — would mean the turn had started from an empty
    // history instead of from the file.
    agent.prompt(4, &session, "list it again");
    let mut notes = Vec::new();
    let answer = agent.answer(4, &mut notes);
    assert_eq!(
        answer["result"]["stopReason"],
        json!("end_turn"),
        "{answer}"
    );
    let streamed = updates(&notes, &session);
    assert_eq!(
        kinds(&streamed),
        vec!["agent_message_chunk"; 3],
        "the second turn already has the tool result: {streamed:?}"
    );
    assert!(
        streamed
            .iter()
            .map(chunk_text)
            .collect::<String>()
            .contains("The tool answered: sessions/"),
        "the second turn remembered the first one's result: {streamed:?}"
    );
    // The file is not rewritten: it still holds the provider's own id.
    assert!(tree_of(&file).contains("[tool call: ls]"), "{file:?}");

    agent.shut_down();
}

#[test]
fn a_cancel_stops_the_turn_and_leaves_the_session_usable() {
    let dir = scratch("cancel");
    let sessions = dir.join("sessions");
    // The stall knob holds a turn between its claim and its run: the only
    // window a scripted model — which answers in microseconds — leaves
    // open for a cancel to land inside.
    let mut agent = Connection::spawn_with(
        &dir,
        &[
            "--demo",
            "--no-builtin-tools",
            "--session",
            sessions.to_str().expect("utf-8 scratch path"),
        ],
        &[("TAU_ACP_STALL_MS", "2000")],
    );
    agent.initialize(1);
    let session = agent.new_session(2, &dir);

    agent.prompt(3, &session, "hello");
    std::thread::sleep(Duration::from_millis(300));
    agent.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": session},
    }));

    let mut notes = Vec::new();
    let answer = agent.answer(3, &mut notes);
    assert_eq!(
        answer["result"]["stopReason"],
        json!("cancelled"),
        "the cancel reached the turn: {answer}"
    );
    // Nothing was streamed: the run stopped before its first token, so
    // the client gets a stop reason and no half-answer.
    assert!(updates(&notes, &session).is_empty(), "{notes:?}");

    // The cancel is not poison: the session runs the next turn normally.
    agent.prompt(4, &session, "again");
    let mut notes = Vec::new();
    let answer = agent.answer(4, &mut notes);
    assert_eq!(
        answer["result"]["stopReason"],
        json!("end_turn"),
        "{answer}"
    );
    assert!(!updates(&notes, &session).is_empty(), "{notes:?}");

    // The prompt of the cancelled turn is still in the file — a turn that
    // ran and stopped is part of the record, not a transaction to unwind.
    let tree = tree_of(&sessions.join(format!("{session}.jsonl")));
    assert!(tree.contains("hello"), "{tree}");

    agent.shut_down();
}

#[test]
fn a_cancel_with_no_turn_running_is_ignored() {
    let dir = scratch("cancel-at-rest");
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
    let session = agent.new_session(2, &dir);

    // Nothing is running. The abort would sit in the control channel and
    // be taken by the *next* run, which would then stop before its first
    // token on behalf of a client that asked to cancel a finished turn.
    // So it has to be dropped here, and the turn below is the proof.
    agent.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": session},
    }));
    agent.prompt(3, &session, "hello");
    let mut notes = Vec::new();
    let answer = agent.answer(3, &mut notes);
    assert_eq!(
        answer["result"]["stopReason"],
        json!("end_turn"),
        "a cancel sent at rest must not reach the next turn: {answer}"
    );
    assert!(!updates(&notes, &session).is_empty(), "{notes:?}");

    agent.shut_down();
}

#[test]
fn a_second_prompt_while_one_runs_is_an_error() {
    let dir = scratch("concurrent");
    let sessions = dir.join("sessions");
    let mut agent = Connection::spawn_with(
        &dir,
        &[
            "--demo",
            "--no-builtin-tools",
            "--session",
            sessions.to_str().expect("utf-8 scratch path"),
        ],
        &[("TAU_ACP_STALL_MS", "1500")],
    );
    agent.initialize(1);
    let session = agent.new_session(2, &dir);

    agent.prompt(3, &session, "one");
    std::thread::sleep(Duration::from_millis(300));
    agent.prompt(4, &session, "two");

    // The second is refused — and refused now, rather than queued behind
    // the first, which would look to a client like the agent sitting on a
    // message it had accepted.
    let mut notes = Vec::new();
    let refused = agent.answer(4, &mut notes);
    assert_eq!(refused["error"]["code"], json!(-32602), "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already running a turn"),
        "the refusal says what is wrong: {refused}"
    );
    assert_eq!(
        refused["error"]["data"], refused["error"]["message"],
        "the detail rides `data` too, for a client that logs only that: {refused}"
    );

    // And the first turn was never disturbed by it.
    let mut notes = Vec::new();
    let answer = agent.answer(3, &mut notes);
    assert_eq!(
        answer["result"]["stopReason"],
        json!("end_turn"),
        "{answer}"
    );
    let streamed = updates(&notes, &session);
    assert!(
        streamed
            .iter()
            .map(chunk_text)
            .collect::<String>()
            .contains("tau is alive."),
        "{streamed:?}"
    );

    agent.shut_down();
}

#[test]
fn a_prompt_for_a_session_this_process_does_not_serve_is_an_error() {
    let dir = scratch("unknown");
    let mut agent = Connection::spawn(&dir, &["--demo", "--no-builtin-tools"]);
    agent.initialize(1);

    agent.prompt(2, "no-such-session", "hello");
    let answer = agent.answer(2, &mut Vec::new());
    assert_eq!(answer["error"]["code"], json!(-32602), "{answer}");
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("no-such-session"), "{answer}");

    // Answering, not hanging or dying: the connection still serves.
    let session = agent.new_session(3, &dir);
    agent.prompt(4, &session, "hello");
    let mut notes = Vec::new();
    let answer = agent.answer(4, &mut notes);
    assert_eq!(
        answer["result"]["stopReason"],
        json!("end_turn"),
        "{answer}"
    );

    agent.shut_down();
}

/// A provider this test controls: a loopback HTTP server that answers the
/// first request of a run with a `bash` tool call and the second with
/// final text.
///
/// The other legs use `--demo`, whose script cannot call a gated tool by
/// design (a scripted model must never be able to write to the machine).
/// So reaching the permission gate with a model in the loop takes a model
/// that is not the faux one — and a provider the test owns is the only
/// kind available offline.
struct MockProvider {
    /// What `OPENAI_BASE_URL` should be pointed at.
    base: String,
    /// Every request body the provider was sent, in order.
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockProvider {
    fn start() -> MockProvider {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().expect("a bound port").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let raw = read_request(&mut stream);
                seen.lock().expect("request log").push(raw.clone());
                // The second request of a run carries the tool result. The
                // answer is chosen from the request itself rather than from
                // a counter, so the mock says which round it thinks it is
                // in and a stray retry cannot silently shift the script.
                let body = if raw.contains(r#""role":"tool""#) {
                    final_body()
                } else {
                    tool_call_body()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        MockProvider {
            base: format!("http://127.0.0.1:{port}/v1"),
            requests,
        }
    }
}

/// One SSE event.
fn chunk(value: serde_json::Value) -> String {
    format!("data: {value}\n\n")
}

/// The name and arguments of the call this mock asks for.
const COMMAND: &str = "touch marker.txt";

/// The first round: a `bash` call, split across two deltas so the
/// provider's partial-JSON assembler is exercised (the id and name arrive
/// before the arguments do).
fn tool_call_body() -> String {
    let mut body = String::new();
    body.push_str(&chunk(serde_json::json!({"choices": [{"delta": {"tool_calls": [
        {"index": 0, "id": "call_bash_1", "function": {"name": "bash", "arguments": ""}}
    ]}}]})));
    body.push_str(&chunk(serde_json::json!({"choices": [{"delta": {"tool_calls": [
        {"index": 0, "function": {"arguments": serde_json::json!({"command": COMMAND}).to_string()}}
    ]}}]})));
    body.push_str(&chunk(
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

/// The second round: the answer, once the tool has reported back.
fn final_body() -> String {
    let mut body = String::new();
    body.push_str(&chunk(
        serde_json::json!({"choices": [{"delta": {"content": "marker "}}]}),
    ));
    body.push_str(&chunk(
        serde_json::json!({"choices": [{"delta": {"content": "handled"}, "finish_reason": "stop"}]}),
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

/// Read one HTTP request: its head, then exactly as many body bytes as the
/// head announces. Byte at a time — a test reads a few hundred of them.
fn read_request(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return String::new(),
        }
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    let mut filled = 0;
    while filled < length {
        match stream.read(&mut body[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
    }
    String::from_utf8_lossy(&body).to_string()
}

impl Connection {
    /// Read until the answer with this id, answering any permission
    /// request along the way with `option` and collecting the questions
    /// asked into `asked`.
    fn answer_asking(
        &mut self,
        id: u64,
        option: &str,
        notes: &mut Vec<Value>,
        asked: &mut Vec<Value>,
    ) -> Value {
        loop {
            let line = self.next();
            if let Some(method) = line["method"].as_str() {
                if line["id"].is_null() {
                    notes.push(line);
                    continue;
                }
                assert_eq!(method, "session/request_permission", "{line}");
                asked.push(line["params"].clone());
                let request_id = line["id"].clone();
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {"outcome": {"outcome": "selected", "optionId": option}},
                }));
                continue;
            }
            assert_eq!(line["id"], json!(id), "the answer to request {id}: {line}");
            return line;
        }
    }
}

#[test]
fn a_gated_call_asks_the_client_and_the_answer_decides() {
    let dir = scratch("permission");
    let provider = MockProvider::start();
    let sessions = dir.join("sessions");
    let mut agent = Connection::spawn_with(
        &dir,
        &[
            "--provider",
            "openai",
            "--tools",
            "bash",
            "--session",
            sessions.to_str().expect("utf-8 scratch path"),
        ],
        &[
            ("OPENAI_API_KEY", "dummy"),
            ("OPENAI_BASE_URL", &provider.base),
            ("TAU_MODEL", "mock-model"),
        ],
    );
    agent.initialize(1);
    let session = agent.new_session(2, &dir);

    // Allowed: the question is asked, the answer is honored, and the
    // command runs. The file it writes is the proof.
    agent.prompt(3, &session, "write the marker");
    let mut notes = Vec::new();
    let mut asked = Vec::new();
    let answer = agent.answer_asking(3, "allow_once", &mut notes, &mut asked);
    assert_eq!(answer["result"]["stopReason"], json!("end_turn"), "{answer}");
    assert_eq!(asked.len(), 1, "one gated call, one question: {asked:?}");

    // The question is about the call the client was already shown: same
    // session, same turn-namespaced id, and the arguments the user is
    // being asked to approve.
    let question = &asked[0];
    assert_eq!(question["sessionId"], json!(session), "{question}");
    assert_eq!(
        question["toolCall"]["toolCallId"],
        json!("1:call_bash_1"),
        "{question}"
    );
    assert_eq!(question["toolCall"]["title"], json!("bash"), "{question}");
    assert_eq!(question["toolCall"]["kind"], json!("execute"), "{question}");
    assert_eq!(
        question["toolCall"]["rawInput"]["command"],
        json!(COMMAND),
        "{question}"
    );
    let offered: Vec<&str> = question["options"]
        .as_array()
        .expect("options")
        .iter()
        .filter_map(|option| option["optionId"].as_str())
        .collect();
    assert_eq!(
        offered,
        vec!["allow_once", "allow_always", "reject_once", "reject_always"]
    );
    assert!(dir.join("marker.txt").exists(), "the allowed command ran");

    let streamed = updates(&notes, &session);
    assert_eq!(
        kinds(&streamed),
        vec![
            "tool_call",
            "tool_call_update",
            "agent_message_chunk",
            "agent_message_chunk",
        ],
        "{streamed:?}"
    );
    assert_eq!(streamed[1]["status"], json!("completed"), "{streamed:?}");
    let text: String = streamed[2..].iter().map(chunk_text).collect();
    assert_eq!(text, "marker handled", "the model saw the tool's output");
    assert!(
        provider.requests.lock().expect("request log").len() >= 2,
        "the run took two rounds: the call, then its result"
    );

    // Refused: a new session, so the gate has nothing remembered, and the
    // same command is asked about again — and this time not run. The
    // marker is removed first: "it was not created" is only evidence if it
    // was not there to begin with.
    std::fs::remove_file(dir.join("marker.txt")).expect("remove the marker");
    let second = agent.new_session(4, &dir);
    agent.prompt(5, &second, "write the marker");
    let mut notes = Vec::new();
    let mut asked = Vec::new();
    let answer = agent.answer_asking(5, "reject_once", &mut notes, &mut asked);
    assert_eq!(answer["result"]["stopReason"], json!("end_turn"), "{answer}");
    assert_eq!(asked.len(), 1, "a new session asks again: {asked:?}");
    assert_eq!(
        asked[0]["toolCall"]["toolCallId"],
        json!("1:call_bash_1"),
        "its own session's turn 1: {}",
        asked[0]
    );
    assert!(
        !dir.join("marker.txt").exists(),
        "a refused command must not run"
    );

    let streamed = updates(&notes, &second);
    assert_eq!(streamed[1]["status"], json!("failed"), "{streamed:?}");
    let preview = streamed[1]["content"][0]["content"]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(preview.contains("rejected"), "{streamed:?}");

    // And the model is told why — the refusal is in the request the
    // second round was built from, where a model would read it.
    let requests = provider.requests.lock().expect("request log");
    let last = requests.last().expect("the last request").clone();
    drop(requests);
    assert!(
        last.contains("blocked: the user rejected bash once"),
        "the refusal never reached the model: {last}"
    );

    agent.shut_down();
}
