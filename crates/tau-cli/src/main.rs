//! tau CLI. Print mode (`-p`): run one prompt to completion, stream the
//! answer to stdout, persist the session as JSONL. Interactive mode (no
//! `-p` on a terminal): scrollback REPL — see `repl` module.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tau_core::agent::AgentEvent;
use tau_core::probe::ProbePoint;
use tau_core::session::{EntryKind, JsonlStore, SessionEntry, new_id};
use tau_core::{Agent, Message};

use setup::{Harness, SharedModel, resolve_component};

mod acp;
mod audio;
mod live;
mod repl;
mod setup;

#[derive(Parser)]
#[command(
    name = "tau",
    version,
    about = "Minimal agent harness with wasm extensions"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Sub>,

    /// Prompt to run (print mode).
    #[arg(short, long)]
    print: Option<String>,

    /// Compact the session at --session (summarize the active branch into
    /// one entry; originals stay in the tree). With -p, compact first,
    /// then run the prompt on the compacted history.
    #[arg(long)]
    compact: bool,

    /// Continue the session at --session from its head.
    #[arg(long)]
    r#continue: bool,

    /// Fork the session at an older entry (full id or unique prefix):
    /// history walks from there, the next entry appends under it.
    #[arg(long, conflicts_with = "continue")]
    continue_from: Option<String>,

    /// Speak the Agent Client Protocol on stdin/stdout instead of running
    /// a prompt: the editor-attached mode (docs/acp.md). stdout then
    /// carries nothing but JSON-RPC; every diagnostic goes to stderr. One
    /// process serves any number of sessions.
    #[arg(long, conflicts_with_all = ["print", "compact", "continue", "continue_from"])]
    acp: bool,

    /// Session file (JSONL); with --acp, the directory the per-session
    /// files go in, one per ACP session. Created if missing.
    #[arg(long)]
    session: Option<PathBuf>,

    /// Wasm extension to load; repeatable.
    #[arg(short = 'e', long = "extension")]
    extensions: Vec<PathBuf>,

    /// Built-in tools to register, comma-separated — replaces the default
    /// set, which is every built-in this platform has. A name may also be a
    /// component tool (pi's flag picks from both), and a name nothing
    /// answers to stops the run instead of quietly shrinking the toolset.
    /// Unselected tools are never registered, so the model does not see
    /// them.
    #[arg(long, value_name = "LIST", conflicts_with = "no_builtin_tools")]
    tools: Option<String>,

    /// Register no built-in tools: the toolset is exactly what the loaded
    /// components provide, and nothing reads, writes, or runs on its own.
    #[arg(long)]
    no_builtin_tools: bool,

    /// System prompt.
    #[arg(long)]
    system: Option<String>,

    /// Model name (or set TAU_MODEL / OPENAI_MODEL).
    #[arg(long)]
    model: Option<String>,

    /// Built-in provider: openai (chat completions), responses (OpenAI
    /// Responses API), anthropic (Messages API). Default: anthropic if
    /// ANTHROPIC_API_KEY is set, else openai.
    #[arg(long, value_parser = ["openai", "responses", "anthropic"])]
    provider: Option<String>,

    /// Wasm provider component; sets --model to select one of its models.
    #[arg(long)]
    provider_wasm: Option<PathBuf>,

    /// Consent: this wasm realtime provider may drive HOST-side
    /// microphone capture (docs/realtime-av.md — the category guards
    /// the device; `/live N sine` synthesizes and needs no grant).
    /// Remembered per signing fingerprint with --remember.
    #[arg(long, requires = "provider_wasm")]
    microphone: bool,

    /// Run against the scripted faux model; no API key needed.
    #[arg(long)]
    demo: bool,

    /// MCP bridge component; requires --mcp-command and/or --mcp-url.
    /// Granting them IS the consent: the bridge may spawn exactly this
    /// argv and/or reach exactly this origin.
    #[arg(long)]
    mcp_bridge: Option<PathBuf>,

    /// External MCP server argv as a JSON array, e.g.
    /// --mcp-command '["python", "server.py"]'
    #[arg(long)]
    mcp_command: Option<String>,

    /// Remote MCP server URL (streamable HTTP). Its origin becomes the
    /// bridge's HTTP consent allowlist — the bridge can reach exactly this
    /// origin and nothing else.
    #[arg(long)]
    mcp_url: Option<String>,

    /// Bearer token handed to the wasm provider inside every request
    /// payload ({"auth": {"bearer": ...}}). Giving it IS the consent to
    /// place the token in guest memory. The token is never persisted;
    /// the delivery grant can be, per signing fingerprint, with
    /// --remember — a remembered grant lets TAU_PROVIDER_AUTH flow
    /// without the flag. Without flag or remembered grant the env var
    /// alone does NOT reach the component.
    #[arg(long, requires = "provider_wasm")]
    provider_auth: Option<String>,

    /// HTTP origin a wasm provider may reach (repeatable), e.g.
    /// --provider-origin https://api.openai.com — remembered per signing
    /// fingerprint with --remember.
    #[arg(long = "provider-origin")]
    provider_origin: Vec<String>,

    /// Deny ambient WASI capabilities (fs/env/stdio/args/network) to all
    /// components — the pre-allow-all sandbox. Consent-gated custom
    /// capabilities (bridge process/http, provider origins) are
    /// unaffected. With --remember the deny is persisted per signing
    /// fingerprint and applies to that component on later runs without
    /// the flag (sticky — lift it with `tau consent --revoke`).
    #[arg(long)]
    deny_wasi: bool,

    /// Load unsigned components. By default every extension, provider, and
    /// bridge must carry a valid signature from a key in ~/.tau/trust.
    #[arg(long)]
    allow_unsigned: bool,

    /// Consent to session injection for every extension and bridge loaded
    /// this run: the component may push user messages into the session
    /// from inside tool/probe calls (host.steer / host.follow-up) — the
    /// IM inbound leg (docs/im-channels.md). With --remember the grant
    /// persists per signing fingerprint (lift it with
    /// `tau consent --revoke`). Notifications (host.notify/emit) are facts
    /// and never need this grant.
    #[arg(long)]
    allow_inject: bool,

    /// Consent to webhook ingress for the bridge (docs/im-channels.md):
    /// it may listen on the given addr:port (repeatable) and receive
    /// inbound HTTP requests pushed into its ingress-handler export —
    /// the WhatsApp/企微-class webhook leg. The consent names the
    /// ADDRESS (orthogonal to the origin allowlist); TLS is terminated
    /// by the tunnel in front, this listener speaks plain HTTP.
    #[arg(long)]
    ingress: Vec<String>,

    /// Persist this run's capability grants — bridge command/url/origins,
    /// provider credential delivery, WASI deny — under each loaded
    /// component's signing fingerprint; later runs recall them without
    /// the flags. Secrets are never persisted, only grants.
    #[arg(long)]
    remember: bool,
}

impl Cli {
    /// The session file for the modes that run one session: print, the
    /// REPL, and the subcommands that take a file.
    fn session_file(&self) -> PathBuf {
        self.session
            .clone()
            .unwrap_or_else(|| PathBuf::from(".tau/session.jsonl"))
    }

    /// With `--acp` the same flag names a directory instead: one JSONL per
    /// ACP session, named after the session id.
    fn session_dir(&self) -> PathBuf {
        self.session
            .clone()
            .unwrap_or_else(|| PathBuf::from(".tau/sessions"))
    }
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Generate a signing keypair into ~/.tau/keys and trust it.
    Keygen,
    /// Sign a wasm component in place (embedded signature section).
    Sign {
        /// Component file to sign.
        wasm: PathBuf,
        /// Key fingerprint in ~/.tau/keys; required if more than one key.
        #[arg(long)]
        key: Option<String>,
    },
    /// List probe points with payload shapes and verdict semantics.
    Probes {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Manage remembered capability consent (per signing fingerprint).
    Consent {
        /// List fingerprints with remembered grants.
        #[arg(long)]
        list: bool,
        /// Revoke remembered grants for a fingerprint.
        #[arg(long)]
        revoke: Option<String>,
    },
    /// Garbage-collect the blob store: keep blobs referenced by the
    /// given sessions, report (or with --yes, delete) the rest.
    Gc {
        /// Session file to mark live blobs from; repeatable. Defaults
        /// to .tau/session.jsonl. List EVERY session you still use —
        /// blobs referenced only by an unlisted session are collected.
        #[arg(long)]
        session: Vec<PathBuf>,
        /// Actually delete. Without it, only report what would go.
        #[arg(long)]
        yes: bool,
    },
    /// Push a wasm component to an OCI registry (oci://registry/repo:tag).
    Push {
        /// Component file to push.
        wasm: PathBuf,
        /// Target reference; must name a tag, not a digest.
        reference: String,
    },
    /// Print the session tree: every entry, indented by depth, head marked.
    Tree {
        /// Session file [default: .tau/session.jsonl].
        #[arg(long)]
        session: Option<PathBuf>,
    },
    /// Trust a base64 ed25519 pubkey (or list trusted keys).
    Trust {
        /// Base64 pubkey to add to ~/.tau/trust.
        #[arg(conflicts_with = "from_component")]
        pubkey: Option<String>,
        /// Trust the key(s) embedded in a signed component (local file or
        /// oci:// reference): the signature section carries the pubkeys,
        /// and only keys whose signature verifies on the exact bytes are
        /// trusted. Prints fingerprints — verify them out-of-band.
        #[arg(long, value_name = "COMPONENT")]
        from_component: Option<PathBuf>,
        /// List trusted key fingerprints.
        #[arg(long)]
        list: bool,
    },
}

async fn run_sub(sub: Sub) -> Result<()> {
    match sub {
        Sub::Consent { list, revoke } => {
            let store = tau_ext::consent::ConsentStore::default();
            if let Some(fp) = revoke {
                anyhow::ensure!(store.revoke(&fp)?, "no remembered consent for {fp}");
                println!("revoked: {fp}");
                return Ok(());
            }
            if list {
                for fp in store.list() {
                    println!("{fp}");
                    if let Some(consent) = store.load(&fp) {
                        if let Some(command) = &consent.command {
                            println!("  command: {}", serde_json::to_string(command)?);
                        }
                        if let Some(url) = &consent.mcp_url {
                            println!("  mcp_url: {url}");
                        }
                        for origin in &consent.origins {
                            println!("  origin: {origin}");
                        }
                        if consent.auth_delivery {
                            println!("  auth_delivery: true");
                        }
                        for addr in &consent.ingress {
                            println!("  ingress: listen on {addr}");
                        }
                        if consent.inject {
                            println!("  inject: session injection (steer/follow-up)");
                        }
                        if consent.wasi_deny {
                            println!("  wasi_deny: true");
                        }
                    }
                }
                return Ok(());
            }
            anyhow::bail!("usage: tau consent --list | tau consent --revoke <fingerprint>");
        }
        Sub::Probes { json } => {
            if json {
                let entries: Vec<serde_json::Value> = tau_core::probe::CATALOG
                    .iter()
                    .map(|info| {
                        serde_json::json!({
                            "name": info.name,
                            "wired": info.wired,
                            "payload": info.payload,
                            "verdicts": info.verdicts,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                for info in tau_core::probe::CATALOG {
                    let status = if info.wired { "wired" } else { "reserved" };
                    println!("{} [{}]", info.name, status);
                    println!("  payload:  {}", info.payload);
                    println!("  verdicts: {}", info.verdicts);
                }
            }
        }
        Sub::Gc { session, yes } => {
            let sessions = if session.is_empty() {
                vec![PathBuf::from(".tau/session.jsonl")]
            } else {
                session
            };
            let mut live = std::collections::HashSet::new();
            for path in &sessions {
                // Fail closed: a missing session must not silently mark
                // nothing — under --yes a mistyped path would delete
                // every blob the real sessions still reference.
                anyhow::ensure!(
                    path.exists(),
                    "session file not found: {} — gc marks live blobs from the sessions you list; a wrong path would orphan live blobs",
                    path.display()
                );
                let store = tau_core::session::JsonlStore::open(path)
                    .with_context(|| format!("opening {}", path.display()))?;
                live.extend(store.live_blob_hashes());
            }
            let blobs = tau_core::BlobStore::new(tau_core::BlobStore::default_dir());
            let report = blobs.sweep(&live, !yes)?;
            let verb = if yes { "freed" } else { "would free" };
            println!(
                "{verb} {} blob(s), {} byte(s); {} kept (live across {} session(s))",
                report.removed,
                report.bytes_freed,
                report.kept,
                sessions.len()
            );
            for hash in &report.removed_hashes {
                println!("  {hash}");
            }
            if !yes && report.removed > 0 {
                println!("dry run — re-run with --yes to delete");
            }
        }
        Sub::Push { wasm, reference } => {
            let bytes =
                std::fs::read(&wasm).with_context(|| format!("reading {}", wasm.display()))?;
            if tau_ext::sign::verify(&bytes)
                .map(|keys| keys.is_empty())
                .unwrap_or(true)
            {
                eprintln!(
                    "[tau] warning: {} is unsigned — consumers will need --allow-unsigned",
                    wasm.display()
                );
            }
            // reqwest blocking must not run on a runtime thread.
            let pushed = tokio::task::spawn_blocking(move || tau_ext::oci::push(&reference, &wasm))
                .await
                .expect("push task")?;
            println!("pushed {} ({})", pushed.reference, pushed.digest);
        }
        Sub::Tree { session } => {
            let path = session.unwrap_or_else(|| PathBuf::from(".tau/session.jsonl"));
            let store = tau_core::session::JsonlStore::open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            warn_torn_tail(&store);
            let head = store.head().map(|h| h.id.clone());
            let depths = entry_depths(store.entries());
            for (index, entry) in store.entries().iter().enumerate() {
                let depth = depths[index];
                let mark = if Some(&entry.id) == head.as_ref() {
                    " ← head"
                } else {
                    ""
                };
                println!(
                    "{}#{index} {} {}{mark}",
                    "  ".repeat(depth),
                    &entry.id[..12.min(entry.id.len())],
                    tau_core::session::entry_summary(entry)
                );
            }
            if store.entries().is_empty() {
                println!("(empty session: {})", path.display());
            }
        }
        Sub::Keygen => {
            let fp = tau_ext::sign::keygen()?;
            println!("key generated and trusted: {fp}");
            println!(
                "  secret: {}",
                tau_ext::sign::keys_dir()
                    .join(format!("{fp}.key"))
                    .display()
            );
        }
        Sub::Sign { wasm, key } => {
            let (fp, key) = tau_ext::sign::load_key(key.as_deref())?;
            let signed_fp = tau_ext::sign::sign_file(&wasm, &key)?;
            println!("signed {} with {signed_fp}", wasm.display());
            let _ = fp;
        }
        Sub::Trust {
            pubkey,
            list,
            from_component,
        } => {
            if let Some(source) = from_component {
                let path = resolve_component(&source).await?;
                let bytes =
                    std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                let fps = tau_ext::sign::trust_component_keys(&bytes)?;
                for fp in &fps {
                    println!("trusted: {fp} (from {})", source.display());
                }
                println!(
                    "verify {} out-of-band before relying on it",
                    if fps.len() == 1 {
                        "this fingerprint"
                    } else {
                        "these fingerprints"
                    }
                );
                return Ok(());
            }
            if list {
                let dir = tau_ext::sign::trust_dir();
                let mut entries: Vec<_> = std::fs::read_dir(&dir)
                    .map(|rd| rd.filter_map(|e| e.ok()).collect())
                    .unwrap_or_default();
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    println!(
                        "{}",
                        entry.file_name().to_string_lossy().trim_end_matches(".pub")
                    );
                }
                return Ok(());
            }
            let Some(pubkey) = pubkey else {
                anyhow::bail!(
                    "usage: tau trust <base64-pubkey> | tau trust --from-component <component> | tau trust --list"
                );
            };
            let fp = tau_ext::sign::trust_key(&pubkey)?;
            println!("trusted: {fp}");
        }
    }
    Ok(())
}


#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(sub) = cli.command {
        return run_sub(sub).await;
    }
    // Before the prompt/stdin check below, not after: under an editor
    // stdin IS a pipe, and that check exits 2 on exactly that.
    if cli.acp {
        return acp::serve(&cli).await;
    }
    let interactive = match &cli.print {
        Some(_) => false,
        // --compact without -p is itself the action: never error out for a
        // missing prompt; in a terminal it still drops into the REPL after.
        None if cli.compact => std::io::IsTerminal::is_terminal(&std::io::stdin()),
        None if std::io::IsTerminal::is_terminal(&std::io::stdin()) => true,
        None => {
            eprintln!(
                "tau: no prompt given and stdin is not a terminal. Try: tau --demo -p \"hello\""
            );
            std::process::exit(2);
        }
    };

    // Everything process-scoped: the component host, the tools and
    // probes the components registered, and the model. The session — its
    // store, history, and agent — is built from it below.
    let Harness {
        cwd,
        host,
        tools,
        probes,
        model,
        model_label,
        mic_consent,
    } = setup::build(&cli).await?;

    let session_path = cli.session_file();
    let mut store = JsonlStore::open(&session_path)
        .with_context(|| format!("opening {}", session_path.display()))?
        .with_blobs(tau_core::BlobStore::new(tau_core::BlobStore::default_dir()));
    warn_torn_tail(&store);
    let mut history = if cli.r#continue {
        match store.head() {
            Some(head) => store.active_branch(&head.id)?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let mut agent = Agent::new(Box::new(SharedModel(model)), tools)
        .probes(probes)
        .blobs(tau_core::BlobStore::new(tau_core::BlobStore::default_dir()));

    // The host channel exists before the agent (extensions load first):
    // wire its sinks now so host.notify/emit reach this bus and
    // host.steer/follow-up reach this control channel. Every loaded
    // extension shares the wiring, including trap-rebuilt instances.
    // The control channel is interposed: injections land in inject_rx
    // first. Interactive mode hands the receiver to the REPL, which
    // forwards mid-run and — crucially — turns an idle injection into
    // the next turn's prompt (an inbound IM wakes the agent;
    // docs/im-channels.md). Print mode forwards straight through.
    let (inject_tx, inject_rx) = tokio::sync::mpsc::unbounded_channel::<tau_core::Control>();
    host.wire_host_channel(agent.bus(), inject_tx);
    if let Some(system) = cli.system {
        agent = agent.system(system);
    }

    // Observe leg (probes.md): session lifecycle observations fire once
    // the agent exists so every loaded extension sees them. Verdicts are
    // ignored by contract (observe-only).
    let session_payload = serde_json::json!({
        "session": session_path.display().to_string(),
        "cwd": cwd.display().to_string(),
        "model": model_label,
    });
    // Print mode: the renderer's receiver subscribes before the session
    // lifecycle fires — broadcast buffers for existing receivers, so the
    // renderer (spawned below) still sees session_start and every probe
    // verdict from compact/navigate. Interactive mode renders inside
    // repl::drive instead, which fires session_start itself.
    let print_events = (!interactive).then(|| agent.events());
    if !interactive {
        agent
            .observe(ProbePoint::SessionStart, session_payload.clone())
            .await;
    }

    if cli.compact {
        let head = store.head().map(|h| h.id.clone());
        match head {
            None => eprintln!("[tau] nothing to compact"),
            Some(head) => {
                let branch = store.active_branch(&head)?;
                let summary = agent.compact(&branch).await?;
                let entry = SessionEntry {
                    id: new_id(),
                    parent: Some(head),
                    kind: EntryKind::Compaction { summary },
                };
                store.append(entry)?;
                eprintln!("[tau] compacted session: {}", session_path.display());
                // Whatever follows — a -p prompt or the REPL — runs on
                // the compacted branch, whether or not --continue was
                // given: compacting and then ignoring the summary would
                // defeat the point (and the help text promises it).
                let head = store.head().unwrap().id.clone();
                history = store.active_branch(&head)?;
            }
        }
        if cli.print.is_none() && !interactive {
            agent.observe(ProbePoint::SessionEnd, session_payload).await;
            return Ok(());
        }
    }

    // --continue-from: fork at an older entry. The probe can veto or
    // redirect; the fork materializes when the next entry appends under
    // the target.
    let mut base: Option<String> = None;
    if let Some(target) = &cli.continue_from {
        let from = store.head().map(|h| h.id.clone());
        let (id, branch) = agent.navigate(&store, target).await?;
        let summary = store
            .get(&id)
            .map(tau_core::session::entry_summary)
            .unwrap_or_default();
        eprintln!("[tau] forked at {}: {summary}", &id[..12.min(id.len())]);
        history = branch;
        base = Some(id.clone());
        agent
            .observe(ProbePoint::Branch, serde_json::json!({ "from": from, "to": id }))
            .await;
    }

    if interactive {
        return repl::interactive(agent, store, history, base, session_payload, inject_rx, mic_consent)
            .await;
    }
    let prompt_text = cli.print.expect("print mode checked above");

    // Print mode: no idle REPL to wake, so injections keep their
    // original semantics — straight into the agent's control channel
    // (a mid-run steer lands after the current turn).
    let mut inject_rx = inject_rx;
    let inject_control = agent.control();
    tokio::spawn(async move {
        while let Some(control) = inject_rx.recv().await {
            let _ = inject_control.send(control);
        }
    });

    let parent = base.or_else(|| store.head().map(|h| h.id.clone()));

    // Renderer: just another event-bus subscriber. Its receiver was
    // created before the session lifecycle fired, so it drains from
    // session_start on.
    let mut events = print_events.expect("print mode always subscribes");
    let renderer = tokio::spawn(async move {
        use std::io::Write;
        // Same live sink as the interactive renderer (Phase 1).
        let mut sink = audio::PlaybackSink::new();
        loop {
            match events.recv().await {
                Ok(AgentEvent::TextDelta(delta)) => {
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                }
                Ok(AgentEvent::RunStart) => sink.begin_run(),
                Ok(AgentEvent::AudioDelta { data, media_type }) => {
                    sink.push(&data, &media_type);
                    if sink.take_announce() {
                        eprintln!("[tau] ▶ streaming ({})", sink.desc());
                    }
                }
                Ok(AgentEvent::ToolCallStart { name, .. }) => eprintln!("\n[tau] tool → {name}"),
                Ok(AgentEvent::ToolCallEnd {
                    name,
                    is_error,
                    output,
                    ..
                }) => {
                    eprintln!(
                        "[tau] tool ← {name}{}: {}",
                        if is_error { " (error)" } else { "" },
                        repl::compact_preview(&output)
                    )
                }
                Ok(AgentEvent::Probe { point, action }) => {
                    eprintln!("[tau] probe {point}: {action}")
                }
                Ok(AgentEvent::Steer(message)) => {
                    eprintln!("[tau] steer: {}", message.text())
                }
                Ok(AgentEvent::ExtensionNotice { level, content }) => {
                    // Text blocks render; everything else is a placeholder
                    // (host-channel semantics: notices are user-visible
                    // facts, never model history).
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
                    eprintln!("[tau] ext {level}: {line}");
                }
                Ok(AgentEvent::ExtensionFact(fact)) => {
                    eprintln!("[tau] ext fact: {}", repl::compact_preview(&fact.to_string()))
                }
                Ok(AgentEvent::FollowUp(message)) => {
                    eprintln!("[tau] follow-up: {}", message.text())
                }
                Ok(AgentEvent::Abort) => {
                    sink.clear();
                    eprintln!("[tau] aborted");
                }
                Ok(AgentEvent::RunEnd { .. } | AgentEvent::RunError { .. }) => {
                    if let Some(summary) = sink.end_run() {
                        eprintln!("{summary}");
                    }
                    break;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[tau] renderer lagged, skipped {n} events")
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Ctrl-C aborts through the control channel — the same path any
    // embedder (TUI, RPC) uses, not a signal hack.
    let control = agent.control();
    let ctrl_c = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = control.send(tau_core::Control::Abort);
    });

    let produced = agent.run(&history, Message::user(prompt_text)).await;
    ctrl_c.abort();
    let produced = produced?;
    let _ = renderer.await;
    println!();

    let mut parent = parent;
    for message in produced {
        let entry = SessionEntry {
            id: new_id(),
            parent,
            kind: EntryKind::Message { message },
        };
        parent = Some(entry.id.clone());
        store.append(entry)?;
    }
    eprintln!("[tau] session: {}", session_path.display());
    agent.observe(ProbePoint::SessionEnd, session_payload).await;
    Ok(())
}

/// Depth of every entry (parent-chain length) in one pass. The store
/// is append-only and rejects unknown parents, so a parent always sits
/// earlier in the vec than its children: depth(entry) = depth(parent)+1
/// is O(n). Walking the parent chain per entry is O(n^2) and hung
/// `tau tree` for ~21s on a 20k chain.
/// Surface a discarded crash-torn tail once, where the user can see it.
pub(crate) fn warn_torn_tail(store: &JsonlStore) {
    if let Some(torn) = store.torn_tail() {
        eprintln!(
            "[tau] discarded a torn tail at line {} ({} bytes) — likely a crash mid-append; continuing from the last intact entry",
            torn.line, torn.discarded_bytes
        );
    }
}

fn entry_depths(entries: &[SessionEntry]) -> Vec<usize> {
    let mut by_id: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    entries
        .iter()
        .map(|entry| {
            let depth = entry
                .parent
                .as_deref()
                .and_then(|parent| by_id.get(parent).copied())
                .map(|depth| depth + 1)
                .unwrap_or(0);
            by_id.insert(&entry.id, depth);
            depth
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_depths_tracks_chains_and_forks() {
        let entry = |id: &str, parent: Option<&str>| SessionEntry {
            id: id.to_string(),
            parent: parent.map(str::to_string),
            kind: EntryKind::Message {
                message: Message::user("x"),
            },
        };
        let entries = vec![
            entry("a", None),
            entry("b", Some("a")),
            entry("c", Some("b")),
            entry("d", Some("a")), // fork off a
        ];
        assert_eq!(entry_depths(&entries), vec![0, 1, 2, 1]);
        // A parentless entry after a chain starts a new root at depth 0.
        let entries = vec![entry("a", None), entry("b", Some("a")), entry("z", None)];
        assert_eq!(entry_depths(&entries), vec![0, 1, 0]);
    }

}
