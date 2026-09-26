//! Interactive mode: a scrollback REPL. Streamed text prints inline (as in
//! print mode) and a rustyline prompt sits underneath — output is routed
//! through rustyline's external printer so the line being typed is never
//! corrupted. Works over ssh/tmux/conhost; no alternate screen.
//!
//! Mid-run input is the control plane (see `tau_core::control`): plain
//! text queues as a follow-up (continues the same run at its natural
//! end), `!`-prefixed text steers (lands after the current turn's tool
//! results), Ctrl-C aborts the run. At an idle prompt: `/help`, `/quit`,
//! Ctrl-D.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tau_core::{Agent, AgentEvent, Control, JsonlStore, Message};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// One-line, length-capped preview of a tool output for `[tau] tool ←`
/// lines: whitespace collapsed, char-safe truncation.
pub(crate) fn compact_preview(output: &str) -> String {
    const LIMIT: usize = 100;
    let flat: String = output.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flat.chars();
    let head: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// What the input thread observed.
pub(crate) enum LineEvent {
    Line(String),
    /// Ctrl-C at the line editor (abort when running, hint when idle).
    Interrupt,
    /// Ctrl-D / end of input.
    Eof,
}

/// Entry point from main: build the rustyline input thread and run the
/// loop on the real terminal.
/// `base` is the fork base from --continue-from: the parent the next
/// append grows under. None seeds from the store head.
pub(crate) async fn interactive(
    agent: Agent,
    store: JsonlStore,
    history: Vec<Message>,
    base: Option<String>,
) -> Result<()> {
    let agent = Arc::new(agent);
    let (line_tx, line_rx) = unbounded_channel();
    let (printer_tx, mut printer_rx) = unbounded_channel::<Box<dyn Fn(&str) + Send + Sync>>();

    // rustyline owns stdin on a dedicated thread; readline blocks.
    std::thread::spawn(move || {
        let mut editor = rustyline::DefaultEditor::new().expect("line editor");
        let history_path = tau_core::BlobStore::default_dir()
            .parent()
            .map(|p| p.join("repl_history.txt"));
        if let Some(path) = &history_path {
            let _ = editor.load_history(path);
        }
        // Route output through the external printer so streamed text and
        // status lines land above the line being edited. Fall back to
        // plain println if the terminal can't do it.
        let printer: Box<dyn Fn(&str) + Send + Sync> = match editor.create_external_printer() {
            Ok(p) => {
                let p = Mutex::new(p);
                Box::new(move |line: &str| {
                    use rustyline::ExternalPrinter as _;
                    if let Ok(mut p) = p.lock() {
                        let _ = p.print(line.to_string());
                    }
                })
            }
            Err(_) => Box::new(|line: &str| println!("{line}")),
        };
        if printer_tx.send(printer).is_err() {
            return;
        }
        loop {
            match editor.readline("you> ") {
                Ok(line) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let _ = editor.add_history_entry(trimmed);
                    }
                    if line_tx.send(LineEvent::Line(line)).is_err() {
                        break;
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    if line_tx.send(LineEvent::Interrupt).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = line_tx.send(LineEvent::Eof);
                    break;
                }
            }
        }
        if let Some(path) = &history_path {
            let _ = editor.save_history(path);
        }
    });

    let print = printer_rx
        .recv()
        .await
        .context("input thread failed to start")?;
    drive(agent, store, history, base, line_rx, print.as_ref()).await
}

/// The REPL loop, factored for tests: lines arrive on a channel, rendered
/// output goes to `print`. `base` seeds the parent of the next append (a
/// fork base); None = store head.
pub(crate) async fn drive(
    agent: Arc<Agent>,
    mut store: JsonlStore,
    mut history: Vec<Message>,
    base: Option<String>,
    mut lines: UnboundedReceiver<LineEvent>,
    print: impl Fn(&str) + Send + Sync,
) -> Result<()> {
    // Renderer: another event-bus subscriber, formatting events into
    // complete lines for the printer.
    let (render_tx, mut render_rx) = unbounded_channel::<String>();
    let mut events = agent.events();
    let renderer = tokio::spawn(async move {
        let mut partial = String::new();
        loop {
            let line = match events.recv().await {
                Ok(AgentEvent::TextDelta(delta)) => {
                    partial.push_str(&delta);
                    match partial.rfind('\n') {
                        Some(at) => {
                            let done: String = partial.drain(..=at).collect();
                            done.trim_end_matches('\n').to_string()
                        }
                        None => continue,
                    }
                }
                Ok(AgentEvent::AudioDelta { bytes, media_type }) => {
                    format!("[tau] audio Δ {bytes} bytes ({media_type})")
                }
                Ok(AgentEvent::ToolCallStart { name, .. }) => format!("[tau] tool → {name}"),
                Ok(AgentEvent::ToolCallEnd {
                    name,
                    is_error,
                    output,
                    ..
                }) => {
                    format!(
                        "[tau] tool ← {name}{}: {}",
                        if is_error { " (error)" } else { "" },
                        compact_preview(&output)
                    )
                }
                Ok(AgentEvent::Probe { point, action }) => format!("[tau] probe {point}: {action}"),
                Ok(AgentEvent::Steer(message)) => format!("[tau] steer: {}", message.text()),
                Ok(AgentEvent::FollowUp(message)) => {
                    format!("[tau] follow-up: {}", message.text())
                }
                Ok(AgentEvent::Abort) => "[tau] aborted".to_string(),
                Ok(AgentEvent::RunEnd { .. } | AgentEvent::RunError { .. }) => {
                    if partial.is_empty() {
                        continue;
                    }
                    std::mem::take(&mut partial)
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    format!("[tau] renderer lagged, skipped {n} events")
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            for line in line.lines() {
                if render_tx.send(line.to_string()).is_err() {
                    return;
                }
            }
        }
    });

    // Run completion is signalled over a channel so the select stays
    // borrow-free.
    let (done_tx, mut done_rx) =
        unbounded_channel::<Result<Vec<Message>, tau_core::agent::AgentError>>();
    let mut running = false;
    let mut parent = base.or_else(|| store.head().map(|h| h.id.clone()));

    print("tau interactive — /help for commands, /quit to exit");
    print("  mid-run: text queues as follow-up, !text steers, Ctrl-C aborts");
    loop {
        tokio::select! {
            event = lines.recv() => match event {
                Some(LineEvent::Line(text)) => {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if running {
                        if text.starts_with('/') {
                            print("commands only when idle — mid-run: !text steers, text follows up");
                            continue;
                        }
                        let control = agent.control();
                        if let Some(steer) = text.strip_prefix('!') {
                            let _ = control.send(Control::Steer(Message::user(steer.trim())));
                        } else {
                            let _ = control.send(Control::FollowUp(Message::user(text)));
                        }
                        continue;
                    }
                    match text {
                        "/quit" | "/exit" => break,
                        "/help" => {
                            print("commands: /help /compact /fork [id-prefix] /quit /exit");
                            print("  !<text> while running: steer; plain text while running: follow-up");
                            continue;
                        }
                        "/compact" => {
                            if history.is_empty() {
                                print("nothing to compact");
                                continue;
                            }
                            print("[tau] compacting…");
                            match agent.compact(&history).await {
                                Ok(summary) => {
                                    let entry = tau_core::SessionEntry {
                                        id: tau_core::session::new_id(),
                                        parent,
                                        kind: tau_core::session::EntryKind::Compaction {
                                            summary: summary.clone(),
                                        },
                                    };
                                    parent = Some(entry.id.clone());
                                    store.append(entry)?;
                                    history = vec![summary];
                                    print("[tau] compacted — the summary now stands in for earlier turns");
                                }
                                Err(e) => print(&format!("[tau] compaction failed: {e}")),
                            }
                            continue;
                        }
                        _ if text == "/fork" || text.starts_with("/fork ") => {
                            let arg = text.strip_prefix("/fork").unwrap().trim();
                            if arg.is_empty() {
                                print("recent entries (fork target = #index or id prefix):");
                                let entries = store.entries();
                                let start = entries.len().saturating_sub(8);
                                for (index, entry) in entries.iter().enumerate().skip(start) {
                                    let here = if Some(&entry.id) == parent.as_ref() {
                                        " ← here"
                                    } else {
                                        ""
                                    };
                                    print(&format!(
                                        "  #{index} {} {}{here}",
                                        &entry.id[..12.min(entry.id.len())],
                                        tau_core::session::entry_summary(entry)
                                    ));
                                }
                                continue;
                            }
                            match agent.navigate(&store, arg).await {
                                Ok((id, branch)) => {
                                    let summary = store
                                        .get(&id)
                                        .map(tau_core::session::entry_summary)
                                        .unwrap_or_default();
                                    parent = Some(id.clone());
                                    history = branch;
                                    print(&format!(
                                        "[tau] forked at {}: {summary} ({} messages in context)",
                                        &id[..12.min(id.len())],
                                        history.len()
                                    ));
                                }
                                Err(e) => print(&format!("[tau] fork failed: {e}")),
                            }
                            continue;
                        }
                        _ if text.starts_with('/') => {
                            print(&format!("unknown command {text} — /help"));
                            continue;
                        }
                        _ => {}
                    }
                    let agent = agent.clone();
                    let prompt = Message::user(text);
                    let turn_history = history.clone();
                    let done_tx = done_tx.clone();
                    tokio::spawn(async move {
                        let result = agent.run(&turn_history, prompt).await;
                        let _ = done_tx.send(result);
                    });
                    running = true;
                }
                Some(LineEvent::Interrupt) => {
                    if running {
                        let _ = agent.control().send(Control::Abort);
                    } else {
                        print("(Ctrl-C aborts a running turn; /quit exits)");
                    }
                }
                Some(LineEvent::Eof) | None => break,
            },
            rendered = render_rx.recv() => {
                if let Some(line) = rendered {
                    print(&line);
                }
            }
            result = done_rx.recv(), if running => {
                running = false;
                let produced = result.context("run channel closed")??;
                for message in produced {
                    let entry = tau_core::SessionEntry {
                        id: tau_core::session::new_id(),
                        parent,
                        kind: tau_core::session::EntryKind::Message {
                            message: message.clone(),
                        },
                    };
                    parent = Some(entry.id.clone());
                    store.append(entry)?;
                    history.push(message);
                }
                print(&format!("[tau] ready (session: {} messages)", history.len()));
            }
        }
    }

    if running {
        let _ = agent.control().send(Control::Abort);
    }
    renderer.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tau_core::ToolRegistry;
    use tau_core::faux::FauxModel;
    use tau_core::model::{Model, ModelEvent, Request, StopReason};
    use tokio::sync::Notify;

    struct Capture {
        lines: Mutex<Vec<String>>,
        ready: Notify,
    }

    impl Capture {
        fn printer(self: &Arc<Self>) -> impl Fn(&str) + Send + Sync + 'static {
            let capture = self.clone();
            move |line: &str| {
                capture.lines.lock().unwrap().push(line.to_string());
                if line.contains("[tau] ready") {
                    capture.ready.notify_one();
                }
            }
        }

        fn text(&self) -> String {
            self.lines.lock().unwrap().join("\n")
        }
    }

    fn store() -> (tempfile::TempDir, JsonlStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::open(dir.path().join("session.jsonl")).unwrap();
        (dir, store)
    }

    /// First call waits on `gate` after signalling `started`; later calls
    /// answer immediately.
    struct GatedModel {
        calls: std::sync::atomic::AtomicUsize,
        started: Arc<Notify>,
        gate: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl Model for GatedModel {
        async fn stream(&self, _req: &Request) -> futures::stream::BoxStream<'static, ModelEvent> {
            use futures::StreamExt;
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let started = self.started.clone();
            let gate = self.gate.clone();
            async_stream::stream! {
                if call == 0 {
                    started.notify_one();
                    gate.notified().await;
                    yield ModelEvent::TextDelta { text: "first".into() };
                } else {
                    yield ModelEvent::TextDelta { text: "again".into() };
                }
                yield ModelEvent::Done { stop: StopReason::Stop };
            }
            .boxed()
        }
    }

    /// Answers every turn (FauxModel::echo fires once).
    struct StaticModel;

    #[async_trait::async_trait]
    impl Model for StaticModel {
        async fn stream(&self, _req: &Request) -> futures::stream::BoxStream<'static, ModelEvent> {
            use futures::StreamExt;
            futures::stream::iter([
                ModelEvent::TextDelta {
                    text: "tau is alive.".into(),
                },
                ModelEvent::Done {
                    stop: StopReason::Stop,
                },
            ])
            .boxed()
        }
    }

    #[tokio::test]
    async fn two_turns_persist_to_session() {
        let (_dir, store) = store();
        let agent = Arc::new(Agent::new(Box::new(StaticModel), ToolRegistry::new()));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer()));
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("two".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("tau is alive"), "output: {text}");
        assert_eq!(text.matches("[tau] ready").count(), 2, "output: {text}");

        // Two prompts + two answers chained into the session.
        let store = JsonlStore::open(_dir.path().join("session.jsonl")).unwrap();
        let head = store.head().unwrap().id.clone();
        let branch = store.active_branch(&head).unwrap();
        assert_eq!(branch.len(), 4);
        assert_eq!(branch[0].text(), "one");
        assert_eq!(branch[2].text(), "two");
    }

    #[tokio::test]
    async fn mid_run_followup_and_steer_join_the_same_run() {
        let (_dir, store) = store();
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let agent = Arc::new(Agent::new(
            Box::new(GatedModel {
                calls: std::sync::atomic::AtomicUsize::new(0),
                started: started.clone(),
                gate: gate.clone(),
            }),
            ToolRegistry::new(),
        ));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer()));

        tx.send(LineEvent::Line("start".into())).unwrap();
        started.notified().await; // model is mid-stream now
        tx.send(LineEvent::Line("queued".into())).unwrap();
        tx.send(LineEvent::Line("!now".into())).unwrap();
        gate.notify_one(); // let the first turn finish
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("steer: now"), "output: {text}");
        assert!(text.contains("follow-up: queued"), "output: {text}");

        let store = JsonlStore::open(_dir.path().join("session.jsonl")).unwrap();
        let head = store.head().unwrap().id.clone();
        let branch: Vec<String> = store
            .active_branch(&head)
            .unwrap()
            .iter()
            .map(|m| m.text())
            .collect();
        // Steers land before follow-ups at the natural end; one run, one
        // trail of five messages.
        assert_eq!(branch, vec!["start", "first", "now", "queued", "again"]);
    }

    #[tokio::test]
    async fn fork_rewinds_history_and_the_next_turn_appends_there() {
        let (_dir, store) = store();
        let agent = Arc::new(Agent::new(Box::new(StaticModel), ToolRegistry::new()));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer()));
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("two".into())).unwrap();
        capture.ready.notified().await;

        // Fork back to the first user entry, then run a new turn: it
        // must grow under the fork point, not under the old head.
        let store = JsonlStore::open(_dir.path().join("session.jsonl")).unwrap();
        let first = store.entries()[0].id.clone();
        tx.send(LineEvent::Line(format!("/fork {first}"))).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if capture.text().contains("forked at") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fork completes");
        tx.send(LineEvent::Line("three".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        // The entry for "three" has the fork point as parent.
        let store = JsonlStore::open(_dir.path().join("session.jsonl")).unwrap();
        let three = store
            .entries()
            .iter()
            .find(|e| tau_core::session::entry_summary(e) == "three")
            .expect("three appended");
        assert_eq!(three.parent.as_deref(), Some(first.as_str()));

        let text = capture.text();
        assert!(text.contains("forked at"), "output: {text}");
    }

    #[tokio::test]
    async fn compact_replaces_context_with_summary_entry() {
        let (_dir, store) = store();
        let agent = Arc::new(Agent::new(Box::new(StaticModel), ToolRegistry::new()));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer()));
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/compact".into())).unwrap();
        // Wait for the compaction to complete, then run another turn.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if capture.text().contains("compacted") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("compaction completes");
        tx.send(LineEvent::Line("two".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        // Session: one, answer, compaction summary, two, answer — and the
        // current branch starts at the summary.
        let store = JsonlStore::open(_dir.path().join("session.jsonl")).unwrap();
        let head = store.head().unwrap().id.clone();
        let branch: Vec<String> = store
            .active_branch(&head)
            .unwrap()
            .iter()
            .map(|m| m.text())
            .collect();
        assert_eq!(branch.len(), 3, "branch: {branch:?}");
        assert!(
            branch[0].contains("[summary of the earlier conversation]"),
            "branch: {branch:?}"
        );
        assert_eq!(branch[1], "two");

        let text = capture.text();
        assert!(text.contains("compacted"), "output: {text}");
    }

    #[tokio::test]
    async fn commands_and_unknown_slash() {
        let (_dir, store) = store();
        let agent = Arc::new(Agent::new(Box::new(FauxModel::echo()), ToolRegistry::new()));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer()));
        tx.send(LineEvent::Line("/help".into())).unwrap();
        tx.send(LineEvent::Line("/bogus".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("commands:"), "output: {text}");
        assert!(text.contains("unknown command /bogus"), "output: {text}");
    }
}
