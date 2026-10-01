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
//!
//! The idle-prompt command surface is aligned with pi's interactive
//! commands (owner ruling 2026-10-01, thread `repl-pi-alignment`); the
//! mapping table and the deliberate non-goals live in `docs/repl.md`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tau_core::probe::ProbePoint;
use tau_core::probe_payload::{Branch, ProbePayload, SessionFacts};
use tau_core::types::{Content, Media, MediaSource, Role};
use tau_core::{Agent, AgentEvent, Control, JsonlStore, Message};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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

// ---- the idle-prompt command surface (pi-aligned, docs/repl.md) ------------

/// pi's grouping for the command list (slash-commands.md): the headers
/// `/help` prints, in pi's order, plus tau's own extension group.
#[derive(Clone, Copy, PartialEq)]
enum Group {
    Session,
    Export,
    Runtime,
    Tau,
}

/// One idle-prompt command: usage as `/help` shows it, one-line description,
/// group. The first whitespace/`[`-delimited token of `usage` is the word
/// Tab-completion offers.
struct SlashCommand {
    usage: &'static str,
    desc: &'static str,
    group: Group,
}

const COMMANDS: &[SlashCommand] = &[
    SlashCommand { usage: "/new", desc: "start a fresh session file", group: Group::Session },
    SlashCommand { usage: "/resume [#n|name]", desc: "list the sessions here; switch to one", group: Group::Session },
    SlashCommand { usage: "/name <name>", desc: "display name for the session (carried by the file name)", group: Group::Session },
    SlashCommand { usage: "/session", desc: "session file, entries, head, model", group: Group::Session },
    SlashCommand { usage: "/tree", desc: "print the session tree, head marked", group: Group::Session },
    SlashCommand { usage: "/fork [#index|id-prefix]", desc: "fork history at an older entry", group: Group::Session },
    SlashCommand { usage: "/clone", desc: "duplicate the session file, continue in the copy", group: Group::Session },
    SlashCommand { usage: "/compact [instructions]", desc: "compact context; instructions steer the summary", group: Group::Session },
    SlashCommand { usage: "/import <path>", desc: "open another session JSONL and continue it here", group: Group::Session },
    SlashCommand { usage: "/export [path]", desc: "write the session out (JSONL; a .html path renders HTML)", group: Group::Export },
    SlashCommand { usage: "/copy", desc: "copy the last assistant message to the clipboard", group: Group::Export },
    SlashCommand { usage: "/hotkeys", desc: "key bindings", group: Group::Runtime },
    SlashCommand { usage: "/changelog", desc: "recent changelog entries (nearest CHANGELOG.md)", group: Group::Runtime },
    SlashCommand { usage: "/reload", desc: "rebuild tools/probes/model from the startup flags", group: Group::Runtime },
    SlashCommand { usage: "/help", desc: "this list", group: Group::Runtime },
    SlashCommand { usage: "/quit", desc: "exit (alias: /exit)", group: Group::Runtime },
    SlashCommand { usage: "/mic <sec> [sine]", desc: "record a voice message (sine synthesizes)", group: Group::Tau },
    SlashCommand { usage: "/live <sec> [sine]", desc: "full-duplex live session", group: Group::Tau },
];

/// The command word of a usage string ("/fork [#index|id-prefix]" → "/fork").
fn command_word(usage: &str) -> &str {
    usage
        .split([' ', '['].as_ref())
        .next()
        .unwrap_or(usage)
}

/// Tab-completion candidates for a `/`-prefixed line start.
/// What `/reload` swaps in: a freshly built agent, its model label, and
/// a keep-alive for whatever the rebuilt harness must not drop (the
/// extension host — its channel wiring is Arc-shared, but main.rs keeps
/// the original alive too, so the REPL does the same for rebuilds).
pub(crate) struct Reloaded {
    pub(crate) agent: Arc<Agent>,
    pub(crate) model_label: String,
    pub(crate) keep_alive: Box<dyn std::any::Any + Send + Sync>,
}

/// `/reload` factory: rebuilds the harness from the startup flags. Lives
/// in main.rs, where the startup assembly (setup::build, host-channel
/// wiring, system prompt) can be replicated; None in tests without one.
pub(crate) type ReloadFactory =
    Box<dyn FnMut() -> futures::future::BoxFuture<'static, Result<Reloaded>> + Send>;

fn slash_candidates(prefix: &str) -> Vec<String> {
    COMMANDS
        .iter()
        .map(|c| command_word(c.usage))
        .filter(|word| word.starts_with(prefix) && *word != prefix)
        .map(str::to_string)
        .collect()
}

/// `/help`: the full command list, grouped the way pi groups it.
fn print_help(print: &impl Fn(&str)) {
    for (group, header) in [
        (Group::Session, "sessions and context:"),
        (Group::Export, "export and share:"),
        (Group::Runtime, "runtime and project:"),
        (Group::Tau, "tau extensions:"),
    ] {
        print(header);
        for c in COMMANDS.iter().filter(|c| c.group == group) {
            print(&format!("  {:<26} {}", c.usage, c.desc));
        }
    }
    print("type / then Tab to complete · mid-run: !text steers, text follows up, Ctrl-C aborts");
}

/// `/changelog`: the first two sections of the nearest CHANGELOG.md
/// (cwd, then beside the executable), or a pointer when none is around —
/// the installed binary does not carry the file.
fn print_changelog(print: &impl Fn(&str)) {
    let mut candidates = vec![PathBuf::from("CHANGELOG.md")];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("CHANGELOG.md"));
        candidates.push(dir.join("../CHANGELOG.md"));
    }
    for candidate in &candidates {
        if let Ok(text) = std::fs::read_to_string(candidate) {
            print(&format!("changelog ({}):", candidate.display()));
            let mut sections = 0;
            for line in text.lines() {
                if line.starts_with("## [") {
                    sections += 1;
                    if sections > 2 {
                        break;
                    }
                }
                print(&format!("  {line}"));
            }
            return;
        }
    }
    print("changelog: no CHANGELOG.md found here — see https://github.com/gqf2008/tau/blob/main/CHANGELOG.md");
}

// ---- T2 helpers (thread repl-pi-alignment-t2) -----------------------------

/// One session file in the `/resume` listing.
struct SessionFile {
    path: PathBuf,
    /// File name as shown and prefix-matched against (`/resume research`).
    name: String,
    current: bool,
    entries: usize,
    /// "12m ago"-style age of the last modification.
    age: String,
    head: String,
}

/// The `*.jsonl` session files in `dir`, most recently modified first.
/// Unreadable files list as `(unreadable)` rather than vanishing.
fn list_sessions(dir: &Path, current: &Path) -> Vec<SessionFile> {
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .map(|path| {
                    let modified = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    (path, modified)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    files
        .into_iter()
        .map(|(path, modified)| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let age = fmt_age(modified.elapsed().unwrap_or_default());
            let is_current = path == current;
            match JsonlStore::open(&path) {
                Ok(store) => {
                    let head = store
                        .head()
                        .map(tau_core::session::entry_summary)
                        .unwrap_or_else(|| "(empty)".to_string());
                    SessionFile {
                        path,
                        name,
                        current: is_current,
                        entries: store.entries().len(),
                        age,
                        head,
                    }
                }
                Err(_) => SessionFile {
                    path,
                    name,
                    current: is_current,
                    entries: 0,
                    age,
                    head: "(unreadable)".to_string(),
                },
            }
        })
        .collect()
}

fn fmt_age(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// `/name`: the display name travels in the file name, not the JSONL —
/// the session format stays untouched while the 0.7.x contract is
/// frozen, and a named file still opens in any older tau. Forbidden
/// and whitespace characters collapse to dashes; the Windows-reserved
/// basenames get a prefix; the result is capped at 40 chars.
fn sanitize_session_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        let usable = !matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            && !ch.is_control()
            && !ch.is_whitespace();
        if usable {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let capped: String = out.trim_matches(['-', '.']).chars().take(40).collect();
    let capped = capped.trim_end_matches(['-', '.']);
    const RESERVED: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
        "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if RESERVED.contains(&capped.to_ascii_lowercase().as_str()) {
        format!("session-{capped}")
    } else {
        capped.to_string()
    }
}

/// `/copy`: the text of the most recent assistant message, if any.
fn last_assistant_text(history: &[Message]) -> Option<String> {
    history
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| message.text())
        .filter(|text| !text.trim().is_empty())
}

/// Write text to the system clipboard via the platform tool — no new
/// dependency. The error names what was tried.
fn copy_to_clipboard(text: &str) -> std::result::Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: &[&[&str]] = if cfg!(windows) {
        &[&["clip"]]
    } else if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else {
        &[
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    };
    let mut tried = Vec::new();
    for command in candidates {
        tried.push(command[0]);
        let spawned = Command::new(command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(_) => continue,
        };
        let fed = child
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(text.as_bytes()).is_ok())
            .unwrap_or(false);
        if !fed {
            continue;
        }
        if matches!(child.wait(), Ok(status) if status.success()) {
            return Ok(());
        }
    }
    Err(format!("tried {}", tried.join(", ")))
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `/export <path>.html`: the active branch as a self-contained page —
/// inline CSS, one section per message, everything user-controlled
/// escaped.
fn render_session_html(store: &JsonlStore) -> String {
    let branch = store
        .head()
        .map(|head| store.active_branch(&head.id).unwrap_or_default())
        .unwrap_or_default();
    let title = store
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut html = String::from("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str(&format!("<title>tau session — {}</title>\n", html_escape(&title)));
    html.push_str("<style>body{font:15px/1.55 system-ui,sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem;background:#141414;color:#ddd}h1{font-size:1.05rem;color:#aaa}section{margin:.9rem 0;padding:.6rem .9rem;border-radius:.5rem;background:#1d1d1d}h2{margin:0 0 .4rem;font-size:.75rem;text-transform:uppercase;letter-spacing:.06em;color:#888}p{margin:.35rem 0;white-space:pre-wrap}pre{margin:.35rem 0;padding:.5rem;background:#111;border-radius:.35rem;overflow:auto;font-size:.85rem}.user h2{color:#7ab8ff}.assistant h2{color:#8fd18f}.tool h2{color:#c9a227}</style>\n</head>\n<body>\n");
    html.push_str(&format!("<h1>tau session — {}</h1>\n", html_escape(&title)));
    for message in &branch {
        let class = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        html.push_str(&format!("<section class=\"{class}\"><h2>{class}</h2>\n"));
        for block in &message.content {
            match block {
                Content::Text { text } => {
                    html.push_str(&format!("<p>{}</p>\n", html_escape(text)));
                }
                Content::ToolCall {
                    name, arguments, ..
                } => {
                    html.push_str(&format!(
                        "<pre>→ {} {}</pre>\n",
                        html_escape(name),
                        html_escape(&arguments.to_string())
                    ));
                }
                Content::ToolResult {
                    content, is_error, ..
                } => {
                    html.push_str(&format!(
                        "<pre>←{}{}</pre>\n",
                        if *is_error { " (error) " } else { " " },
                        html_escape(&tau_core::types::tool_result_text(content))
                    ));
                }
                Content::Image { media } => {
                    html.push_str(&format!("<p>[image: {}]</p>\n", html_escape(&media.media_type)));
                }
                Content::Audio { media } => {
                    html.push_str(&format!("<p>[audio: {}]</p>\n", html_escape(&media.media_type)));
                }
                Content::Video { media } => {
                    html.push_str(&format!("<p>[video: {}]</p>\n", html_escape(&media.media_type)));
                }
                Content::File { media, name } => {
                    html.push_str(&format!(
                        "<p>[file: {}]</p>\n",
                        html_escape(name.as_deref().unwrap_or(&media.media_type))
                    ));
                }
            }
        }
        html.push_str("</section>\n");
    }
    html.push_str("</body>\n</html>\n");
    html
}

/// Tab-completion for slash commands (rustyline calls it on the line so
/// far); everything else about the editor stays stock.
struct SlashHelper;

impl rustyline::completion::Completer for SlashHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let prefix = &line[..pos];
        if !prefix.starts_with('/') {
            return Ok((pos, Vec::new()));
        }
        Ok((0, slash_candidates(prefix)))
    }
}

impl rustyline::hint::Hinter for SlashHelper {
    type Hint = String;
}

impl rustyline::highlight::Highlighter for SlashHelper {}
impl rustyline::validate::Validator for SlashHelper {}
impl rustyline::Helper for SlashHelper {}

/// Entry point from main: build the rustyline input thread and run the
/// loop on the real terminal.
/// `base` is the fork base from --continue-from: the parent the next
/// append grows under. None seeds from the store head.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn interactive(
    agent: Agent,
    store: JsonlStore,
    history: Vec<Message>,
    base: Option<String>,
    session_facts: SessionFacts,
    inject: UnboundedReceiver<Control>,
    mic_consent: bool,
    reload: Option<ReloadFactory>,
) -> Result<()> {
    let agent = Arc::new(agent);
    let (line_tx, line_rx) = unbounded_channel();
    let (printer_tx, mut printer_rx) = unbounded_channel::<Box<dyn Fn(&str) + Send + Sync>>();

    // rustyline owns stdin on a dedicated thread; readline blocks.
    std::thread::spawn(move || {
        let mut editor =
            rustyline::Editor::<SlashHelper, _>::new().expect("line editor");
        editor.set_helper(Some(SlashHelper));
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
        Some(session_facts.clone()),
        inject,
        mic_consent,
        reload,
    )
    .await;
    if result.is_ok() {
        // Best-effort clean-exit observation (observe-only; verdicts
        // ignored). Error exits skip it — a crash is not a session end.
        agent
            .observe(ProbePoint::SessionEnd, ProbePayload::SessionEnd(session_facts))
            .await;
    }
    result
}

/// The renderer: another event-bus subscriber, formatting events into
/// complete lines for the printer. It also owns the live playback
/// sink (realtime-av Phase 1): AudioDelta bytes play as they
/// arrive, an abort silences the buffer mid-run. Factored out of
/// `drive` so `/reload` can re-attach a renderer to the swapped agent.
fn spawn_renderer(
    agent: &Arc<Agent>,
) -> (
    UnboundedReceiver<String>,
    Arc<std::sync::atomic::AtomicU64>,
    tokio::task::JoinHandle<()>,
) {
    // Renderer: another event-bus subscriber, formatting events into
    // complete lines for the printer. It also owns the live playback
    // sink (realtime-av Phase 1): AudioDelta bytes play as they
    // arrive, an abort silences the buffer mid-run.
    let (render_tx, render_rx) = unbounded_channel::<String>();
    let mut events = agent.events();
    let mut sink = crate::audio::PlaybackSink::new();
    let sink_streamed = sink.streamed_handle();
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
                Ok(AgentEvent::RunStart) => {
                    sink.begin_run();
                    continue;
                }
                Ok(AgentEvent::AudioDelta { data, media_type }) => {
                    // Live playback IS the rendering of an audio delta;
                    // one announce line per segment, not per chunk.
                    sink.push(&data, &media_type);
                    if sink.take_announce() {
                        format!("[tau] ▶ streaming ({})", sink.desc())
                    } else {
                        continue;
                    }
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
                // Realtime kinds (live sessions; docs/realtime-av.md
                // Phase 2a). Uplink facts are not rendered per chunk —
                // 20/s would bury the conversation; VAD and barge-in
                // are the user-visible beats.
                Ok(AgentEvent::InputAudioChunk { .. }) => continue,
                Ok(AgentEvent::SpeechStarted) => "[tau] 🎤 speech".to_string(),
                Ok(AgentEvent::SpeechStopped) => "[tau] 🎤 speech stopped".to_string(),
                Ok(AgentEvent::Interrupted) => {
                    sink.clear();
                    "[tau] ⚡ interrupted — buffer cleared".to_string()
                }
                Ok(AgentEvent::Abort) => {
                    sink.clear();
                    "[tau] aborted".to_string()
                }
                Ok(AgentEvent::RunEnd { .. } | AgentEvent::RunError { .. }) => {
                    if let Some(summary) = sink.end_run()
                        && render_tx.send(summary).is_err()
                    {
                        return;
                    }
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

    (render_rx, sink_streamed, renderer)
}

/// The REPL loop, factored for tests: lines arrive on a channel, rendered
/// output goes to `print`. `base` seeds the parent of the next append (a
/// fork base); None = store head.
// The REPL loop's full wiring (channels in, printer out, session
// payload, injection receiver, reload factory) is the parameter list —
// bundling it into a struct would only rename the same slots at the
// call sites.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive(
    mut agent: Arc<Agent>,
    mut store: JsonlStore,
    mut history: Vec<Message>,
    base: Option<String>,
    mut lines: UnboundedReceiver<LineEvent>,
    print: impl Fn(&str) + Send + Sync,
    session_start: Option<SessionFacts>,
    mut inject: UnboundedReceiver<Control>,
    // May the host capture the real microphone for THIS model?
    // (Consent category per fingerprint for wasm providers; host
    // doctrine grants it to native/demo — docs/realtime-av.md.)
    mic_consent: bool,
    mut reload: Option<ReloadFactory>,
) -> Result<()> {
    let (mut render_rx, mut sink_streamed, mut renderer) = spawn_renderer(&agent);
    // Whatever the last rebuilt harness must keep alive (Reloaded::
    // keep_alive); the startup harness lives in main.rs's scope.
    let mut _reload_guard: Option<Box<dyn std::any::Any + Send + Sync>> = None;
    // The renderer is attached; session_start observes now so its
    // notices render (observe leg, probes.md — verdicts ignored).
    let mut model_label = session_start
        .as_ref()
        .map(|facts| facts.model.clone())
        .unwrap_or_else(|| "(unknown)".to_string());
    if let Some(facts) = session_start {
        agent
            .observe(ProbePoint::SessionStart, ProbePayload::SessionStart(facts))
            .await;
    }

    // Run completion is signalled over a channel so the select stays
    // borrow-free.
    let (done_tx, mut done_rx) =
        unbounded_channel::<Result<Vec<Message>, tau_core::agent::AgentError>>();
    let mut running = false;
    // A live (full-duplex) session: command sender while active,
    // outcome receiver for its terminal recording (Phase 2a).
    let (live_done_tx, mut live_done_rx) =
        unbounded_channel::<crate::live::LiveOutcome>();
    let mut live: Option<UnboundedSender<crate::live::LiveCmd>> = None;
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
                        "/quit" | "/exit" => {
                            close_live(&mut live, &mut live_done_rx, &mut store, &mut history, &mut parent, &print).await;
                            break;
                        }
                        "/help" => {
                            print_help(&print);
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
                        line if line == "/compact" || line.starts_with("/compact ") => {
                            let notes = text.strip_prefix("/compact").unwrap().trim();
                            let notes = if notes.is_empty() { None } else { Some(notes) };
                            if history.is_empty() {
                                print("nothing to compact");
                                continue;
                            }
                            print("[tau] compacting…");
                            match agent.compact_guided(&history, notes).await {
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
                                            ProbePayload::Branch(Branch {
                                                previous: from,
                                                to: id.clone(),
                                            }),
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
                        // Phase 2a full duplex (docs/realtime-av.md):
                        // open a RealtimeSession, stream mic/sine
                        // uplink, play downlink live via the bus.
                        line if line.starts_with("/live") => {
                            if running || live.is_some() {
                                print("[tau] busy — /live waits for the run/session to finish");
                                continue;
                            }
                            let mut parts = line.split_whitespace();
                            let _ = parts.next();
                            let seconds: u32 = parts
                                .next()
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(10)
                                .clamp(1, 120);
                            let sine = parts.next() == Some("sine");
                            if !sine && !mic_consent {
                                // The category guards the DEVICE, not
                                // the session: synthetic uplink never
                                // touches the mic and needs no grant.
                                print(
                                    "[tau] microphone capture for this provider needs consent — \
                                     rerun with --microphone (remember with --remember); \
                                     `/live N sine` synthesizes and needs no grant",
                                );
                                continue;
                            }
                            let config = tau_core::RealtimeConfig {
                                input_media_type: "audio/pcm;rate=16000".into(),
                                ..Default::default()
                            };
                            match agent.realtime(config) {
                                Some(session) => {
                                    print(&format!(
                                        "[tau] 🎤 live {seconds}s{} — Ctrl-C = barge-in",
                                        if sine { " (sine)" } else { "" }
                                    ));
                                    // The live span is run-shaped on
                                    // the bus: RunStart/RunEnd reuse
                                    // every Phase 1 sink arm.
                                    let _ = agent.bus().send(AgentEvent::RunStart);
                                    let (cmd_tx, cmd_rx) = unbounded_channel();
                                    let bus = agent.bus();
                                    let done = live_done_tx.clone();
                                    let media_type = "audio/pcm;rate=16000".to_string();
                                    tokio::spawn(crate::live::run(
                                        session, seconds, sine, media_type, bus, cmd_rx, done,
                                    ));
                                    live = Some(cmd_tx);
                                }
                                None => print(
                                    "[tau] this model has no realtime session                                      (Model::realtime → None — try --demo)",
                                ),
                            }
                            continue;
                        }
                        // ---- pi-aligned session commands (docs/repl.md) ----
                        "/session" => {
                            let entries = store.entries();
                            print(&format!("session: {}", store.path().display()));
                            print(&format!(
                                "  entries: {} · context: {} messages · model: {model_label}",
                                entries.len(),
                                history.len()
                            ));
                            match store.head() {
                                Some(head) => print(&format!(
                                    "  head: {} {}",
                                    &head.id[..12.min(head.id.len())],
                                    tau_core::session::entry_summary(head)
                                )),
                                None => print("  head: (empty session)"),
                            }
                            continue;
                        }
                        "/tree" => {
                            let entries = store.entries();
                            if entries.is_empty() {
                                print("(empty session)");
                                continue;
                            }
                            let head = store.head().map(|h| h.id.clone());
                            let depths = crate::entry_depths(entries);
                            for (index, entry) in entries.iter().enumerate() {
                                let mark = if Some(&entry.id) == head.as_ref() {
                                    " ← head"
                                } else {
                                    ""
                                };
                                print(&format!(
                                    "{}#{index} {} {}{mark}",
                                    "  ".repeat(depths[index]),
                                    &entry.id[..12.min(entry.id.len())],
                                    tau_core::session::entry_summary(entry)
                                ));
                            }
                            continue;
                        }
                        "/new" => {
                            if live.is_some() {
                                print("[tau] live session active — end it first (Ctrl-C or /quit)");
                                continue;
                            }
                            let old = store.path().to_path_buf();
                            let dir = old.parent().map(PathBuf::from).unwrap_or_default();
                            let path = dir.join(format!(
                                "session-{}.jsonl",
                                &tau_core::session::new_id()[..8]
                            ));
                            match JsonlStore::open(&path) {
                                Ok(fresh) => {
                                    store = fresh;
                                    history = Vec::new();
                                    parent = None;
                                    print(&format!("[tau] new session: {}", path.display()));
                                    print(&format!(
                                        "  (previous: {} — /import it or restart with --session to return)",
                                        old.display()
                                    ));
                                }
                                Err(e) => print(&format!("[tau] /new failed: {e}")),
                            }
                            continue;
                        }
                        "/clone" => {
                            if live.is_some() {
                                print("[tau] live session active — end it first (Ctrl-C or /quit)");
                                continue;
                            }
                            let source = store.path().to_path_buf();
                            let dir = source.parent().map(PathBuf::from).unwrap_or_default();
                            let path = dir.join(format!(
                                "session-{}.jsonl",
                                &tau_core::session::new_id()[..8]
                            ));
                            let cloned = std::fs::copy(&source, &path)
                                .map_err(|e| e.to_string())
                                .and_then(|_| {
                                    JsonlStore::open(&path).map_err(|e| e.to_string())
                                });
                            match cloned {
                                // history/parent stay: the copy holds the
                                // same entries, so the parent id resolves.
                                Ok(copy) => {
                                    store = copy;
                                    print(&format!(
                                        "[tau] cloned into {} — continuing there",
                                        path.display()
                                    ));
                                }
                                Err(e) => print(&format!("[tau] /clone failed: {e}")),
                            }
                            continue;
                        }
                        line if line == "/import" || line.starts_with("/import ") => {
                            if live.is_some() {
                                print("[tau] live session active — end it first (Ctrl-C or /quit)");
                                continue;
                            }
                            let arg = text.strip_prefix("/import").unwrap().trim();
                            if arg.is_empty() {
                                print("usage: /import <path-to-session.jsonl>");
                                continue;
                            }
                            let path = PathBuf::from(arg);
                            if !path.exists() {
                                print(&format!("[tau] no such session file: {}", path.display()));
                                continue;
                            }
                            match JsonlStore::open(&path) {
                                Ok(opened) => {
                                    if let Some(torn) = opened.torn_tail() {
                                        print(&format!(
                                            "[tau] discarded a torn tail at line {} ({} bytes)",
                                            torn.line, torn.discarded_bytes
                                        ));
                                    }
                                    let count = opened.entries().len();
                                    let head = opened.head().map(|h| h.id.clone());
                                    let branch = match &head {
                                        Some(id) => {
                                            opened.active_branch(id).unwrap_or_default()
                                        }
                                        None => Vec::new(),
                                    };
                                    store = opened;
                                    parent = head;
                                    history = branch;
                                    print(&format!(
                                        "[tau] imported {} — {} entries, {} messages in context",
                                        path.display(),
                                        count,
                                        history.len()
                                    ));
                                }
                                Err(e) => print(&format!("[tau] import failed: {e}")),
                            }
                            continue;
                        }
                        line if line == "/export" || line.starts_with("/export ") => {
                            let arg = text.strip_prefix("/export").unwrap().trim();
                            let target = if arg.is_empty() {
                                let dir =
                                    store.path().parent().map(PathBuf::from).unwrap_or_default();
                                dir.join(format!(
                                    "export-{}.jsonl",
                                    &tau_core::session::new_id()[..8]
                                ))
                            } else {
                                PathBuf::from(arg)
                            };
                            if target.exists() {
                                print(&format!(
                                    "[tau] not overwriting existing file: {}",
                                    target.display()
                                ));
                                continue;
                            }
                            let as_html = target.extension().is_some_and(|ext| {
                                ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm")
                            });
                            if as_html {
                                match std::fs::write(&target, render_session_html(&store)) {
                                    Ok(()) => print(&format!(
                                        "[tau] exported {} (HTML)",
                                        target.display()
                                    )),
                                    Err(e) => print(&format!("[tau] export failed: {e}")),
                                }
                                continue;
                            }
                            match std::fs::copy(store.path(), &target) {
                                Ok(bytes) => print(&format!(
                                    "[tau] exported {} ({} bytes, JSONL)",
                                    target.display(),
                                    bytes
                                )),
                                Err(e) => print(&format!("[tau] export failed: {e}")),
                            }
                            continue;
                        }
                        line if line == "/resume" || line.starts_with("/resume ") => {
                            if live.is_some() {
                                print("[tau] live session active — end it first (Ctrl-C or /quit)");
                                continue;
                            }
                            let arg = text.strip_prefix("/resume").unwrap().trim();
                            let dir = store
                                .path()
                                .parent()
                                .map(PathBuf::from)
                                .unwrap_or_else(|| PathBuf::from("."));
                            let mut listing = list_sessions(&dir, store.path());
                            if !listing.iter().any(|file| file.current) {
                                // The current session may not be on disk
                                // yet — a file is created on its first
                                // append. List it from memory.
                                listing.insert(
                                    0,
                                    SessionFile {
                                        path: store.path().to_path_buf(),
                                        name: store
                                            .path()
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default(),
                                        current: true,
                                        entries: store.entries().len(),
                                        age: "current".to_string(),
                                        head: store
                                            .head()
                                            .map(tau_core::session::entry_summary)
                                            .unwrap_or_else(|| "(empty)".to_string()),
                                    },
                                );
                            }
                            if listing.is_empty() {
                                print("[tau] no session files here yet");
                                continue;
                            }
                            if arg.is_empty() {
                                print("sessions here — /resume <n|name> to switch:");
                                for (index, file) in listing.iter().enumerate() {
                                    print(&format!(
                                        "  {}. {}{} — {} entries · {} · {}",
                                        index + 1,
                                        file.name,
                                        if file.current { " (current)" } else { "" },
                                        file.entries,
                                        file.age,
                                        file.head
                                    ));
                                }
                                continue;
                            }
                            let pick = if let Ok(n) = arg.parse::<usize>() {
                                n.checked_sub(1).and_then(|index| listing.get(index))
                            } else {
                                let matches: Vec<&SessionFile> = listing
                                    .iter()
                                    .filter(|file| file.name.starts_with(arg))
                                    .collect();
                                (matches.len() == 1).then_some(matches[0])
                            };
                            let Some(file) = pick else {
                                print(&format!(
                                    "[tau] no unique session matching {arg:?} — /resume lists them"
                                ));
                                continue;
                            };
                            match JsonlStore::open(&file.path) {
                                Ok(opened) => {
                                    let count = opened.entries().len();
                                    let head = opened.head().map(|h| h.id.clone());
                                    let branch = match &head {
                                        Some(id) => {
                                            opened.active_branch(id).unwrap_or_default()
                                        }
                                        None => Vec::new(),
                                    };
                                    store = opened;
                                    parent = head;
                                    history = branch;
                                    print(&format!(
                                        "[tau] resumed {} — {} entries, {} messages in context",
                                        file.name,
                                        count,
                                        history.len()
                                    ));
                                }
                                Err(e) => print(&format!("[tau] resume failed: {e}")),
                            }
                            continue;
                        }
                        line if line == "/name" || line.starts_with("/name ") => {
                            if live.is_some() {
                                print("[tau] live session active — end it first (Ctrl-C or /quit)");
                                continue;
                            }
                            let arg = text.strip_prefix("/name").unwrap().trim();
                            if arg.is_empty() {
                                print("usage: /name <display-name> — carried by the session file name");
                                continue;
                            }
                            let clean = sanitize_session_name(arg);
                            if clean.is_empty() {
                                print("[tau] that name has no usable characters");
                                continue;
                            }
                            let dir = store.path().parent().map(PathBuf::from).unwrap_or_default();
                            let target = dir.join(format!("{clean}.jsonl"));
                            if target == *store.path() {
                                print(&format!("[tau] already named {clean}"));
                                continue;
                            }
                            if target.exists() {
                                print(&format!(
                                    "[tau] not overwriting existing file: {}",
                                    target.display()
                                ));
                                continue;
                            }
                            match std::fs::rename(store.path(), &target) {
                                Ok(()) => match JsonlStore::open(&target) {
                                    Ok(opened) => {
                                        store = opened;
                                        print(&format!(
                                            "[tau] named {arg:?} — session file is now {}",
                                            target.display()
                                        ));
                                    }
                                    Err(e) => {
                                        print(&format!("[tau] renamed but reopen failed: {e}"))
                                    }
                                },
                                Err(e) => print(&format!("[tau] rename failed: {e}")),
                            }
                            continue;
                        }
                        "/copy" => {
                            match last_assistant_text(&history) {
                                None => print("[tau] nothing to copy yet"),
                                Some(text) => match copy_to_clipboard(&text) {
                                    Ok(()) => print(&format!(
                                        "[tau] copied {} chars to the clipboard",
                                        text.chars().count()
                                    )),
                                    Err(e) => print(&format!("[tau] clipboard unavailable ({e})")),
                                },
                            }
                            continue;
                        }
                        "/hotkeys" => {
                            print("keys: Enter send · ↑/↓ prompt history · Tab complete /commands");
                            print("  Ctrl-C idle: hint · Ctrl-C mid-run: abort the turn (barge-in when live) · Ctrl-D: exit");
                            continue;
                        }
                        "/changelog" => {
                            print_changelog(&print);
                            continue;
                        }
                        "/reload" => {
                            if live.is_some() {
                                print("[tau] live session active — end it first (Ctrl-C or /quit)");
                                continue;
                            }
                            match &mut reload {
                                None => print("[tau] reload is unavailable here"),
                                Some(factory) => match factory().await {
                                    Ok(reloaded) => {
                                        renderer.abort();
                                        agent = reloaded.agent;
                                        model_label = reloaded.model_label;
                                        let (rx, streamed, task) = spawn_renderer(&agent);
                                        render_rx = rx;
                                        sink_streamed = streamed;
                                        renderer = task;
                                        _reload_guard = Some(reloaded.keep_alive);
                                        print(&format!(
                                            "[tau] reloaded from the startup flags — model: {model_label}"
                                        ));
                                    }
                                    Err(e) => print(&format!(
                                        "[tau] reload failed: {e:#} — keeping the current harness"
                                    )),
                                },
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
                    if let Some(cmd) = &live {
                        // Barge-in: same user intent as Ctrl-C on a
                        // running turn, realtime-shaped.
                        let _ = cmd.send(crate::live::LiveCmd::Interrupt);
                    } else if running {
                        let _ = agent.control().send(Control::Abort);
                    } else {
                        print("(Ctrl-C aborts a running turn; /quit exits)");
                    }
                }
                Some(LineEvent::Eof) | None => {
                    close_live(&mut live, &mut live_done_rx, &mut store, &mut history, &mut parent, &print).await;
                    break;
                }
            },
            outcome = live_done_rx.recv() => {
                live = None;
                if let Some(outcome) = outcome {
                    record_live(&outcome, &mut store, &mut history, &mut parent, &print);
                }
            }
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
                // (live outcomes arrive on their own arm below)
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
                // Post-run replay is only for audio whose deltas never
                // streamed (a non-streaming provider — no current path
                // produces it, kept for the semantics): what the live
                // sink already played must not play twice.
                if sink_streamed.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    for message in &produced {
                        for content in &message.content {
                            if let Content::Audio { media } = content {
                                let MediaSource::Bytes(bytes) = &media.source else {
                                    continue;
                                };
                                // No output device is a notice, never a
                                // failure (headless machines exist).
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

/// Orderly live shutdown (Eof and /quit share it): close the session,
/// let the driver flush terminal events, record the outcome.
async fn close_live(
    live: &mut Option<UnboundedSender<crate::live::LiveCmd>>,
    live_done_rx: &mut UnboundedReceiver<crate::live::LiveOutcome>,
    store: &mut JsonlStore,
    history: &mut Vec<Message>,
    parent: &mut Option<String>,
    print: &impl Fn(&str),
) {
    if let Some(cmd) = live.take() {
        let _ = cmd.send(crate::live::LiveCmd::Close);
        if let Some(outcome) = live_done_rx.recv().await {
            record_live(&outcome, store, history, parent, print);
        }
    }
}

/// Write one live session's outcome into the tree: the accumulated
/// uplink as one user message, the assembled downlink as one assistant
/// message — same entry shape as post-run recording.
fn record_live(
    outcome: &crate::live::LiveOutcome,
    store: &mut JsonlStore,
    history: &mut Vec<Message>,
    parent: &mut Option<String>,
    print: &impl Fn(&str),
) {
    let mut messages = Vec::new();
    if !outcome.uplink.is_empty() {
        messages.push(Message {
            role: tau_core::types::Role::User,
            content: vec![
                Content::Audio {
                    media: Media {
                        media_type: outcome.uplink_media_type.clone(),
                        source: MediaSource::Bytes(outcome.uplink.clone()),
                    },
                },
                Content::Text { text: "(live voice — uplink stream)".into() },
            ],
        });
    }
    if !outcome.assistant.is_empty() {
        messages.push(Message {
            role: tau_core::types::Role::Assistant,
            content: outcome.assistant.clone(),
        });
    }
    for message in messages {
        let entry = tau_core::SessionEntry {
            id: tau_core::session::new_id(),
            parent: parent.clone(),
            kind: tau_core::session::EntryKind::Message {
                message: message.clone(),
            },
        };
        *parent = Some(entry.id.clone());
        if let Err(e) = store.append(entry) {
            print(&format!("[tau] live recording failed: {e:#}"));
            return;
        }
        history.push(message);
    }
    print(&format!(
        "[tau] 🎤 live ended — uplink {} bytes, {} assistant blocks, {} interruption{}",
        outcome.uplink.len(),
        outcome.assistant.len(),
        outcome.interruptions,
        if outcome.interruptions == 1 { "" } else { "s" },
    ));
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1, true, None));
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
            true,
            None,
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
            seen: Arc<Mutex<Vec<tau_core::probe_payload::SessionFacts>>>,
        }
        #[async_trait::async_trait]
        impl tau_core::probe::ProbeHandler for Recorder {
            fn points(&self) -> &[ProbePoint] {
                &[ProbePoint::SessionStart]
            }
            async fn probe(
                &self,
                _point: ProbePoint,
                payload: tau_core::probe_payload::ProbePayload,
            ) -> tau_core::probe::Verdict {
                let tau_core::probe_payload::ProbePayload::SessionStart(facts) = payload else {
                    panic!("session_start fired with another point's payload");
                };
                self.seen.lock().unwrap().push(facts);
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
            Some(SessionFacts {
                session: "s.jsonl".into(),
                cwd: "/tmp".into(),
                model: "demo".into(),
            }),
            unbounded_channel().1,
            true,
            None,
        ));
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].session, "s.jsonl");
        assert_eq!(seen[0].model, "demo");
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1, true, None));

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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1, true, None));
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1, true, None));
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1, true, None));
        tx.send(LineEvent::Line("/help".into())).unwrap();
        tx.send(LineEvent::Line("/bogus".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("sessions and context:"), "output: {text}");
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

    // ---- pi-aligned command surface (thread repl-pi-alignment) -------------

    /// Spawn a drive loop on a temp session; returns (dir, input, capture,
    /// join). StaticModel answers every turn with "tau is alive.".
    fn rig() -> (
        tempfile::TempDir,
        UnboundedSender<LineEvent>,
        Arc<Capture>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let (dir, store) = store();
        let agent = Arc::new(Agent::new(Box::new(StaticModel), ToolRegistry::new()));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let printer = capture.printer();
        let task = tokio::spawn(drive(
            agent,
            store,
            Vec::new(),
            None,
            rx,
            printer,
            None,
            unbounded_channel().1,
            true,
            None,
        ));
        (dir, tx, capture, task)
    }

    /// The .jsonl session files in `dir` other than session.jsonl itself.
    fn extra_session_files(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "jsonl")
                    && path.file_name().unwrap() != "session.jsonl"
            })
            .collect();
        files.sort();
        files
    }

    #[tokio::test]
    async fn help_lists_pi_groups_and_all_commands() {
        let (_dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("/help".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        for header in [
            "sessions and context:",
            "export and share:",
            "runtime and project:",
            "tau extensions:",
        ] {
            assert!(text.contains(header), "help lacks {header}: {text}");
        }
        for command in [
            "/new", "/resume", "/name", "/session", "/tree", "/fork", "/clone", "/compact",
            "/import", "/export", "/copy", "/hotkeys", "/changelog", "/reload", "/help",
            "/quit", "/mic", "/live",
        ] {
            assert!(text.contains(command), "help lacks {command}: {text}");
        }
    }

    #[tokio::test]
    async fn session_reports_file_entries_and_head() {
        let (_dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/session".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("session:"), "output: {text}");
        assert!(text.contains("entries: 2"), "output: {text}");
        assert!(text.contains("context: 2 messages"), "output: {text}");
        assert!(text.contains("head:"), "output: {text}");
    }

    #[tokio::test]
    async fn tree_indents_entries_and_marks_head() {
        let (_dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/tree".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("#0"), "output: {text}");
        assert!(text.contains("#1"), "output: {text}");
        assert!(text.contains("← head"), "output: {text}");
    }

    #[tokio::test]
    async fn new_starts_a_fresh_session_file() {
        let (dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/new".into())).unwrap();
        wait_for(&capture, "[tau] new session:").await;
        tx.send(LineEvent::Line("two".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        // The old file holds only the first turn; the second turn landed
        // in the fresh file.
        let old = JsonlStore::open(dir.path().join("session.jsonl")).unwrap();
        assert_eq!(old.entries().len(), 2, "old session grew after /new");
        let fresh = extra_session_files(dir.path());
        assert_eq!(fresh.len(), 1, "expected one new session file: {fresh:?}");
        let fresh = JsonlStore::open(&fresh[0]).unwrap();
        assert_eq!(fresh.entries().len(), 2, "fresh file: {:?}", fresh.entries());
        let head = fresh.head().unwrap().id.clone();
        let branch: Vec<String> = fresh
            .active_branch(&head)
            .unwrap()
            .iter()
            .map(|m| m.text())
            .collect();
        assert_eq!(branch, vec!["two".to_string(), "tau is alive.".to_string()]);
    }

    #[tokio::test]
    async fn clone_duplicates_and_continues_in_the_copy() {
        let (dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/clone".into())).unwrap();
        wait_for(&capture, "[tau] cloned into").await;
        tx.send(LineEvent::Line("two".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        // The original is frozen at the first turn; the copy has both.
        let old = JsonlStore::open(dir.path().join("session.jsonl")).unwrap();
        assert_eq!(old.entries().len(), 2, "original changed after /clone");
        let copies = extra_session_files(dir.path());
        assert_eq!(copies.len(), 1, "expected one clone file: {copies:?}");
        let copy = JsonlStore::open(&copies[0]).unwrap();
        assert_eq!(copy.entries().len(), 4, "copy: {:?}", copy.entries());
    }

    #[tokio::test]
    async fn import_switches_to_the_given_session_file() {
        let (dir, tx, capture, task) = rig();
        // A foreign session file with one old turn.
        let foreign_path = dir.path().join("foreign.jsonl");
        let mut foreign = JsonlStore::open(&foreign_path).unwrap();
        foreign
            .append(tau_core::SessionEntry {
                id: tau_core::session::new_id(),
                parent: None,
                kind: tau_core::EntryKind::Message {
                    message: Message::user("an old turn"),
                },
            })
            .unwrap();
        drop(foreign);

        tx.send(LineEvent::Line(format!("/import {}", foreign_path.display())))
            .unwrap();
        wait_for(&capture, "[tau] imported").await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(
            text.contains("1 entries, 1 messages in context"),
            "output: {text}"
        );
    }

    #[tokio::test]
    async fn export_writes_a_copy_and_refuses_to_overwrite() {
        let (dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        let target = dir.path().join("out.jsonl");
        tx.send(LineEvent::Line(format!("/export {}", target.display())))
            .unwrap();
        wait_for(&capture, "[tau] exported").await;
        tx.send(LineEvent::Line(format!("/export {}", target.display())))
            .unwrap();
        wait_for(&capture, "not overwriting").await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let exported = JsonlStore::open(&target).unwrap();
        assert_eq!(exported.entries().len(), 2, "export: {:?}", exported.entries());
    }

    #[tokio::test]
    async fn hotkeys_prints_the_key_bindings() {
        let (_dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("/hotkeys".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("keys: Enter send"), "output: {text}");
        assert!(
            text.contains("Ctrl-C mid-run: abort the turn"),
            "output: {text}"
        );
    }

    #[tokio::test]
    async fn changelog_falls_back_to_a_pointer_without_a_file() {
        // cargo test runs with cwd = crates/tau-cli and the test binary in
        // target/debug/deps: none of the three CHANGELOG.md candidates
        // exist, so the fallback line is deterministic in this environment.
        let (_dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("/changelog".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        let fallback = "no CHANGELOG.md found here — see https://github.com/gqf2008/tau/blob/main/CHANGELOG.md";
        assert!(text.contains(fallback), "output: {text}");
    }

    #[tokio::test]
    async fn resume_lists_sessions_and_marks_current() {
        let (_dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/new".into())).unwrap();
        wait_for(&capture, "[tau] new session:").await;
        tx.send(LineEvent::Line("/resume".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("sessions here"), "output: {text}");
        assert!(text.contains("session.jsonl"), "old file listed: {text}");
        assert!(text.contains("(current)"), "current marked: {text}");
        assert!(text.contains("1 entries") || text.contains("2 entries"), "counts: {text}");
    }

    #[tokio::test]
    async fn resume_switches_by_index() {
        let (_dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/new".into())).unwrap();
        wait_for(&capture, "[tau] new session:").await;
        // Most recently modified first: 1 is the fresh file (current),
        // 2 is session.jsonl with the first turn.
        tx.send(LineEvent::Line("/resume 2".into())).unwrap();
        wait_for(&capture, "[tau] resumed session.jsonl").await;
        tx.send(LineEvent::Line("/session".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(
            text.contains("2 entries, 2 messages in context"),
            "switch report: {text}"
        );
    }

    #[tokio::test]
    async fn name_renames_the_session_file_and_keeps_appending() {
        let (dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/name research log".into())).unwrap();
        wait_for(&capture, "named \"research log\"").await;
        tx.send(LineEvent::Line("two".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let renamed = dir.path().join("research-log.jsonl");
        assert!(renamed.exists(), "files: {:?}", extra_session_files(dir.path()));
        assert!(
            !dir.path().join("session.jsonl").exists(),
            "old name gone: {:?}",
            extra_session_files(dir.path())
        );
        let store = JsonlStore::open(&renamed).unwrap();
        assert_eq!(store.entries().len(), 4, "both turns survived the rename");
    }

    #[tokio::test]
    async fn copy_reports_nothing_without_an_assistant_message() {
        let (_dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("/copy".into())).unwrap();
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let text = capture.text();
        assert!(text.contains("nothing to copy"), "output: {text}");
    }

    #[test]
    fn last_assistant_text_picks_the_latest_assistant_text() {
        let assistant = |text: &str| Message {
            role: Role::Assistant,
            content: vec![Content::Text { text: text.into() }],
        };
        let history = vec![
            Message::user("hi"),
            assistant("first"),
            Message::user("again"),
            assistant("second"),
        ];
        assert_eq!(last_assistant_text(&history).as_deref(), Some("second"));
        assert_eq!(last_assistant_text(&[]), None);
        assert_eq!(last_assistant_text(&[Message::user("hi")]), None);
    }

    #[test]
    fn sanitize_session_name_cleans_and_caps() {
        assert_eq!(sanitize_session_name("research log"), "research-log");
        assert_eq!(
            sanitize_session_name("a/b\\c:d*e?f\"g<h>i|j"),
            "a-b-c-d-e-f-g-h-i-j"
        );
        assert_eq!(sanitize_session_name("CON"), "session-CON");
        assert_eq!(sanitize_session_name("  ...  "), "");
        assert_eq!(sanitize_session_name(&"x".repeat(100)).len(), 40);
    }

    #[tokio::test]
    async fn export_html_writes_an_escaped_styled_copy() {
        let (dir, tx, capture, task) = rig();
        tx.send(LineEvent::Line("<b>bold</b> & co".into())).unwrap();
        capture.ready.notified().await;
        let target = dir.path().join("out.html");
        tx.send(LineEvent::Line(format!("/export {}", target.display())))
            .unwrap();
        wait_for(&capture, "(HTML)").await;
        tx.send(LineEvent::Line(format!("/export {}", target.display())))
            .unwrap();
        wait_for(&capture, "not overwriting").await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let html = std::fs::read_to_string(&target).unwrap();
        assert!(html.starts_with("<!doctype html>"), "html: {html}");
        assert!(
            html.contains("&lt;b&gt;bold&lt;/b&gt; &amp; co"),
            "user text escaped: {html}"
        );
        assert!(!html.contains("<b>bold</b>"), "no raw markup: {html}");
        assert!(html.contains("<section class=\"user\">"), "sections: {html}");
        assert!(html.contains("tau is alive."), "assistant text: {html}");
    }

    #[tokio::test]
    async fn reload_invokes_the_factory_and_swaps_the_agent() {
        let (_dir, store) = store();
        let agent = Arc::new(Agent::new(Box::new(StaticModel), ToolRegistry::new()));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_in = calls.clone();
        let factory: ReloadFactory = Box::new(move || {
            let calls = calls_in.clone();
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(Reloaded {
                    agent: Arc::new(Agent::new(Box::new(StaticModel), ToolRegistry::new())),
                    model_label: "reloaded-model".to_string(),
                    keep_alive: Box::new(()),
                })
            })
        });
        let printer = capture.printer();
        let task = tokio::spawn(drive(
            agent,
            store,
            Vec::new(),
            None,
            rx,
            printer,
            None,
            unbounded_channel().1,
            true,
            Some(factory),
        ));
        tx.send(LineEvent::Line("/reload".into())).unwrap();
        wait_for(&capture, "reloaded from the startup flags").await;
        // A turn after the swap still works, on the new agent.
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        let text = capture.text();
        assert!(text.contains("model: reloaded-model"), "output: {text}");
        assert!(text.contains("tau is alive"), "turn after swap: {text}");
    }

    /// Records the text of every request; the compaction instructions
    /// must reach the summarizer (pi's `/compact [instructions]`).
    struct RecordingModel {
        seen: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Model for RecordingModel {
        async fn stream(&self, req: &Request) -> futures::stream::BoxStream<'static, ModelEvent> {
            use futures::StreamExt;
            let text: Vec<String> = req.messages.iter().map(|m| m.text()).collect();
            self.seen.lock().unwrap().push(text.join("\n---\n"));
            futures::stream::iter([
                ModelEvent::TextDelta {
                    text: "the brief.".into(),
                },
                ModelEvent::Done {
                    stop: StopReason::Stop,
                },
            ])
            .boxed()
        }
    }

    #[tokio::test]
    async fn compact_passes_instructions_to_the_summarizer() {
        let (_dir, store) = store();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let agent = Arc::new(Agent::new(
            Box::new(RecordingModel { seen: seen.clone() }),
            ToolRegistry::new(),
        ));
        let capture = Arc::new(Capture {
            lines: Mutex::new(Vec::new()),
            ready: Notify::new(),
        });
        let (tx, rx) = unbounded_channel();
        let printer = capture.printer();
        let task = tokio::spawn(drive(
            agent,
            store,
            Vec::new(),
            None,
            rx,
            printer,
            None,
            unbounded_channel().1,
            true,
            None,
        ));
        tx.send(LineEvent::Line("one".into())).unwrap();
        capture.ready.notified().await;
        tx.send(LineEvent::Line("/compact keep the cats".into())).unwrap();
        wait_for(&capture, "compacted").await;
        tx.send(LineEvent::Line("/quit".into())).unwrap();
        task.await.unwrap().unwrap();

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().any(|request| request.contains("keep the cats")),
            "instructions never reached the summarizer: {seen:?}"
        );
    }

    #[test]
    fn slash_candidates_complete_on_slash_prefix() {
        assert_eq!(slash_candidates("/f"), vec!["/fork".to_string()]);
        let all = slash_candidates("/");
        assert_eq!(all.len(), COMMANDS.len(), "every command completes: {all:?}");
        assert!(all.contains(&"/quit".to_string()));
        assert_eq!(slash_candidates("/q"), vec!["/quit".to_string()]);
        assert!(slash_candidates("/zzz").is_empty());
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1, true, None));
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
        let task = tokio::spawn(drive(agent, store, Vec::new(), None, rx, capture.printer(), None, unbounded_channel().1, true, None));
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
