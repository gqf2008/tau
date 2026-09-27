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
use tau_core::probe::ProbePoint;
use tau_core::types::{Content, Media, MediaSource};
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
    session_payload: serde_json::Value,
    inject: UnboundedReceiver<Control>,
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
                        // Persist per line, not only at EOF: the save at
                        // loop exit is unreachable when the user quits
                        // via /quit (readline still blocks), so exiting
                        // the normal way used to lose the recall history.
                        if let Some(path) = &history_path {
                            let _ = editor.append_history(path);
                        }
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
    let result = drive(
        agent.clone(),
        store,
        history,
        base,
        line_rx,
        print.as_ref(),
        Some(session_payload.clone()),
        inject,
    )
    .await;
    if result.is_ok() {
        // Best-effort clean-exit observation (observe-only; verdicts
        // ignored). Error exits skip it — a crash is not a session end.
        agent
            .observe(ProbePoint::SessionEnd, session_payload)
            .await;
    }
    result
}

/// The REPL loop, factored for tests: lines arrive on a channel, rendered
/// output goes to `print`. `base` seeds the parent of the next append (a
/// fork base); None = store head.
// The REPL loop's full wiring (channels in, printer out, session
// payload, injection receiver) is the parameter list — bundling it into
// a struct would only rename the same eight slots at nine call sites.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive(
    agent: Arc<Agent>,
    mut store: JsonlStore,
    mut history: Vec<Message>,
    base: Option<String>,
    mut lines: UnboundedReceiver<LineEvent>,
    print: impl Fn(&str) + Send + Sync,
    session_start: Option<serde_json::Value>,
    mut inject: UnboundedReceiver<Control>,
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
                Ok(AgentEvent::ExtensionNotice { level, content }) => {
                    // Text blocks render; other media are placeholders —
                    // notices are user-visible facts, never history.
                    let mut line = String::new();
                    for block in &content {
                        match block {
                            tau_core::Content::Text { text } => line.push_str(text),
                            tau_core::Content::Image { media } => {
                                line.push_str(&format!("[image: {}]", media.media_type))
                            }
                            tau_core::Content::Audio { media } => {
                                line.push_str(&format!("[audio: {}]", media.media_type))
                            }
                            tau_core::Content::Video { media } => {
                                line.push_str(&format!("[video: {}]", media.media_type))
                            }
                            tau_core::Content::File { media, name } => line.push_str(&format!(
                                "[file: {}]",
                                name.as_deref().unwrap_or(&media.media_type)
                            )),
                            tau_core::Content::ToolCall { name, .. } => {
                                line.push_str(&format!("[tool-call: {name}]"))
                            }
                            tau_core::Content::ToolResult { call_id, .. } => {
                                line.push_str(&format!("[tool-result: {call_id}]"))
                            }
                        }
                    }
                    format!("[tau] ext {level}: {line}")
                }
                Ok(AgentEvent::ExtensionFact(fact)) => {
                    format!("[tau] ext fact: {}", compact_preview(&fact.to_string()))
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

    // The renderer is attached; session_start observes now so its
    // notices render (observe leg, probes.md — verdicts ignored).
    if let Some(payload) = session_start {
        agent.observe(ProbePoint::SessionStart, payload).await;
    }

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
                            let steer = steer.trim();
                            if steer.is_empty() {
                                print("!text steers — `!` alone sends nothing");
                            } else {
                                let _ = control.send(Control::Steer(Message::user(steer)));
                            }
                        } else {
                            let _ = control.send(Control::FollowUp(Message::user(text)));
                        }
                        continue;
                    }
                    match text {
                        "/quit" | "/exit" => break,
                        "/help" => {
                            print("commands: /help /compact /fork [#index|id-prefix] /mic <sec> [sine] /quit /exit");
                            print("  !<text> while running: steer; plain text while running: follow-up");
                            continue;
                        }
                        // Phase 0 push-to-talk (docs/realtime-av.md):
                        // record (or synthesize) a clip, send it as a
                        // voice message. The recording IS the consent —
                        // the host CLI on an explicit user command.
                        line if line.starts_with("/mic") => {
                            if running {
                                print("[tau] mid-run — /mic waits for the run to finish");
                                continue;
                            }
                            let mut parts = line.split_whitespace();
                            let _ = parts.next();
                            let seconds: u32 = parts
                                .next()
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(3)
                                .clamp(1, 30);
                            let sine = parts.next() == Some("sine");
                            print(&format!(
                                "[tau] 🎙 recording {seconds}s{}…",
                                if sine { " (sine)" } else { "" }
                            ));
                            match crate::audio::record_wav(seconds, sine) {
                                Ok(wav) => {
                                    print(&format!(
                                        "[tau] 🎙 captured {} bytes (audio/wav)",
                                        wav.len()
                                    ));
                                    let message = Message {
                                        role: tau_core::types::Role::User,
                                        content: vec![
                                            Content::Audio {
                                                media: Media {
                                                    media_type: "audio/wav".into(),
                                                    source: MediaSource::Bytes(wav),
                                                },
                                            },
                                            Content::Text {
                                                text: "(voice message — audio/wav clip)".into(),
                                            },
                                        ],
                                    };
                                    let agent = agent.clone();
                                    let turn_history = history.clone();
                                    let done_tx = done_tx.clone();
                                    tokio::spawn(async move {
                                        let result = agent.run(&turn_history, message).await;
                                        let _ = done_tx.send(result);
                                    });
                                    running = true;
                                }
                                Err(e) => print(&format!("[tau] mic: {e:#}")),
                            }
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
                                let entries = store.entries();
                                if entries.is_empty() {
                                    print("(empty session — nothing to fork yet)");
                                    continue;
                                }
                                print("recent entries (fork target = #index or id prefix):");
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
                            // The listing prints #index targets; bare
                            // indexes keep working too.
                            let target = arg.strip_prefix('#').unwrap_or(arg);
                            match agent.navigate(&store, target).await {
                                Ok((id, branch)) => {
                                    let summary = store
                                        .get(&id)
                                        .map(tau_core::session::entry_summary)
                                        .unwrap_or_default();
                                    let from = parent.clone();
                                    parent = Some(id.clone());
                                    history = branch;
                                    agent
                                        .observe(
                                            ProbePoint::Branch,
                                            serde_json::json!({ "from": from, "to": id.clone() }),
                                        )
                                        .await;
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
            injected = inject.recv() => match injected {
                // The host channel's injection leg (host.steer/follow-up
                // from an extension or IM bridge). Mid-run it joins the
                // agent's control channel unchanged (steer lands after
                // the current turn, follow-up after the run). IDLE, the
                // message IS the next turn — an inbound IM wakes the
                // agent instead of sitting in a queue nobody drains
                // (docs/im-channels.md: the push model's reason to exist).
                Some(control) => match control {
                    Control::Steer(message) | Control::FollowUp(message) if !running => {
                        print(&format!("[tau] steer: {}", message.text()));
                        let agent = agent.clone();
                        let turn_history = history.clone();
                        let done_tx = done_tx.clone();
                        tokio::spawn(async move {
                            let result = agent.run(&turn_history, message).await;
                            let _ = done_tx.send(result);
                        });
                        running = true;
                    }
                    other => {
                        let _ = agent.control().send(other);
                    }
                },
                // Every sender gone (the host channel is dropped): the
                // REPL's own input still works, so just disarm this arm.
                None => inject.close(),
            },
            rendered = render_rx.recv() => {
                if let Some(line) = rendered {
                    print(&line);
                }
            }
            result = done_rx.recv(), if running => {
                running = false;
                let produced = match result {
                    Some(Ok(produced)) => produced,
                    // A failed run (model error, vetoed run) produced
                    // nothing — history and parent stand; the session
                    // must survive a flaky provider.
                    Some(Err(e)) => {
                        print(&format!(
                            "[tau] run failed: {e} — session intact, keep going"
                        ));
                        continue;
                    }
                    None => return Err(anyhow::anyhow!("run channel closed")),
                };
                for message in &produced {
                    for content in &message.content {
                        if let Content::Audio { media } = content {
                            let MediaSource::Bytes(bytes) = &media.source else {
                                continue;
                            };
                            // Phase 0 playback: after the run, blocking
                            // (docs/realtime-av.md — the live sink is
                            // Phase 1). No output device is a notice,
                            // never a failure (headless machines exist).
                            match crate::audio::play_wav(bytes) {
                                Ok(n) => print(&format!(
                                    "[tau] ▶ played {n} samples ({})",
                                    media.media_type
                                )),
                                Err(e) => print(&format!("[tau] playback: {e:#}")),
                            }
                        }
                    }
                }
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1));
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

    /// An injected steer while the REPL idles IS the next turn: the IM
    /// push path (docs/im-channels.md) must wake the agent, not sit in
    /// a queue nobody drains while the select waits on user input.
    #[tokio::test]
    async fn idle_injection_starts_a_turn() {
        let (_dir, store) = store();
        let agent = Arc::new(Agent::new(Box::new(StaticModel), ToolRegistry::new()));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let (inject_tx, inject_rx) = unbounded_channel();
        let task = tokio::spawn(drive(
            agent,
            store,
            Vec::new(),
            None,
            rx,
            capture.printer(),
            None,
            inject_rx,
        ));
        // No typed line at all: the injection alone must run the turn.
        inject_tx
            .send(Control::Steer(Message::user("from the IM platform")))
            .unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(
            text.contains("[tau] steer: from the IM platform"),
            "output: {text}"
        );
        assert!(text.contains("tau is alive"), "output: {text}");

        let store = JsonlStore::open(_dir.path().join("session.jsonl")).unwrap();
        let head = store.head().unwrap().id.clone();
        let branch = store.active_branch(&head).unwrap();
        assert_eq!(branch.len(), 2);
        assert_eq!(branch[0].text(), "from the IM platform");
    }

    /// The session_start payload interactive() hands over fires once the
    /// renderer is attached — the guest observes before the first turn.
    #[tokio::test]
    async fn drive_fires_session_start_before_the_first_turn() {
        struct Recorder {
            seen: Arc<Mutex<Vec<serde_json::Value>>>,
        }
        #[async_trait::async_trait]
        impl tau_core::probe::ProbeHandler for Recorder {
            fn points(&self) -> &[ProbePoint] {
                &[ProbePoint::SessionStart]
            }
            async fn probe(&self, _point: ProbePoint, payload: serde_json::Value) -> tau_core::probe::Verdict {
                self.seen.lock().unwrap().push(payload);
                tau_core::probe::Verdict::Continue
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut probes = tau_core::ProbeRegistry::new();
        probes.register(Box::new(Recorder { seen: seen.clone() }));
        let (_dir, store) = store();
        let agent = Arc::new(
            Agent::new(Box::new(StaticModel), ToolRegistry::new()).probes(probes),
        );
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(drive(
            agent,
            store,
            Vec::new(),
            None,
            rx,
            capture.printer(),
            Some(serde_json::json!({ "session": "s.jsonl", "model": "demo" })),
            unbounded_channel().1,
        ));
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[serde_json::json!({ "session": "s.jsonl", "model": "demo" })]
        );
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1));

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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1));
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1));
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1));
        tx.send(LineEvent::Line("/help".into())).unwrap();
        tx.send(LineEvent::Line("/bogus".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("commands:"), "output: {text}");
        assert!(text.contains("unknown command /bogus"), "output: {text}");
    }

    /// Poll until `needle` shows up in the captured output.
    async fn wait_for(capture: &Capture, needle: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if capture.text().contains(needle) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("output gains {needle:?}: {}", capture.text()));
    }

    #[tokio::test]
    async fn run_failure_keeps_the_repl_alive() {
        let (_dir, store) = store();
        // First turn errors (flaky provider), second answers.
        let model = FauxModel::scripted(vec![
            vec![
                ModelEvent::Error {
                    message: "boom".into(),
                },
                ModelEvent::Done {
                    stop: StopReason::Error,
                },
            ],
            vec![
                ModelEvent::TextDelta {
                    text: "recovered".into(),
                },
                ModelEvent::Done {
                    stop: StopReason::Stop,
                },
            ],
        ]);
        let agent = Arc::new(Agent::new(Box::new(model), ToolRegistry::new()));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1));
        tx.send(LineEvent::Line("hi".into())).unwrap();
        wait_for(&capture, "run failed: model error: boom").await;
        // The loop survived: the next prompt runs and completes.
        tx.send(LineEvent::Line("again".into())).unwrap();
        wait_for(&capture, "[tau] ready").await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        // The failed turn produced nothing; the session holds only the
        // successful exchange.
        let store = JsonlStore::open(_dir.path().join("session.jsonl")).unwrap();
        let branch: Vec<String> = store
            .active_branch(&store.head().unwrap().id)
            .unwrap()
            .iter()
            .map(|m| m.text())
            .collect();
        assert_eq!(branch, vec!["again", "recovered"]);
    }

    #[tokio::test]
    async fn fork_accepts_the_hash_index_form_it_lists() {
        let (_dir, store) = store();
        let agent = Arc::new(Agent::new(Box::new(StaticModel), ToolRegistry::new()));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1));
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("two".into())).unwrap();
        capture.ready.notified().await;

        // The listing prints #index targets; the command must accept them.
        tx.send(LineEvent::Line("/fork #0".into())).unwrap();
        wait_for(&capture, "forked at").await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let store = JsonlStore::open(_dir.path().join("session.jsonl")).unwrap();
        let first = store.entries()[0].id.clone();
        let text = capture.text();
        assert!(
            text.contains(&format!("forked at {}", &first[..12.min(first.len())])),
            "output: {text}"
        );
    }
}
