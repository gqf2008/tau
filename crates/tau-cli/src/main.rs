//! tau CLI. v0 is print mode only: run one prompt to completion, stream the
//! answer to stdout, persist the session as JSONL.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tau_core::agent::AgentEvent;
use tau_core::faux::FauxModel;
use tau_core::session::{new_id, EntryKind, JsonlStore, SessionEntry};
use tau_core::{Agent, Message, Model, ProbeRegistry, ToolRegistry};

#[derive(Parser)]
#[command(name = "tau", version, about = "Minimal agent harness with wasm extensions")]
struct Cli {
    #[command(subcommand)]
    command: Option<Sub>,

    /// Prompt to run (print mode).
    #[arg(short, long)]
    print: Option<String>,

    /// Continue the session at --session from its head.
    #[arg(long)]
    r#continue: bool,

    /// Session file (JSONL). Created if missing.
    #[arg(long, default_value = ".tau/session.jsonl")]
    session: PathBuf,

    /// Wasm extension to load; repeatable.
    #[arg(short = 'e', long = "extension")]
    extensions: Vec<PathBuf>,

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

    /// Load unsigned components. By default every extension, provider, and
    /// bridge must carry a valid signature from a key in ~/.tau/trust.
    #[arg(long)]
    allow_unsigned: bool,
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
    /// Trust a base64 ed25519 pubkey (or list trusted keys).
    Trust {
        /// Base64 pubkey to add to ~/.tau/trust.
        pubkey: Option<String>,
        /// List trusted key fingerprints.
        #[arg(long)]
        list: bool,
    },
}

fn run_sub(sub: Sub) -> Result<()> {
    match sub {
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
        Sub::Keygen => {
            let fp = tau_ext::sign::keygen()?;
            println!("key generated and trusted: {fp}");
            println!("  secret: {}", tau_ext::sign::keys_dir().join(format!("{fp}.key")).display());
        }
        Sub::Sign { wasm, key } => {
            let (fp, key) = tau_ext::sign::load_key(key.as_deref())?;
            let signed_fp = tau_ext::sign::sign_file(&wasm, &key)?;
            println!("signed {} with {signed_fp}", wasm.display());
            let _ = fp;
        }
        Sub::Trust { pubkey, list } => {
            if list {
                let dir = tau_ext::sign::trust_dir();
                let mut entries: Vec<_> = std::fs::read_dir(&dir)
                    .map(|rd| rd.filter_map(|e| e.ok()).collect())
                    .unwrap_or_default();
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    println!("{}", entry.file_name().to_string_lossy().trim_end_matches(".pub"));
                }
                return Ok(());
            }
            let Some(pubkey) = pubkey else {
                anyhow::bail!("usage: tau trust <base64-pubkey> | tau trust --list");
            };
            let fp = tau_ext::sign::trust_key(&pubkey)?;
            println!("trusted: {fp}");
        }
    }
    Ok(())
}

fn default_model() -> String {
    std::env::var("TAU_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(sub) = cli.command {
        return run_sub(sub);
    }
    let Some(prompt_text) = cli.print else {
        eprintln!("tau v0 is print-mode only. Try: tau --demo -p \"hello\"");
        std::process::exit(2);
    };

    let mut tools = ToolRegistry::new();
    let mut probes = ProbeRegistry::new();
    let host = if cli.allow_unsigned {
        tau_ext::ExtensionHost::new()
    } else {
        tau_ext::ExtensionHost::with_policy(tau_ext::sign::TrustPolicy::RequireTrusted {
            trust_dir: tau_ext::sign::trust_dir(),
        })
    };
    for path in &cli.extensions {
        let extension = host
            .load(path)
            .with_context(|| format!("loading {}", path.display()))?;
        eprintln!("[tau] loaded extension: {}", extension.name);
        let (ext_tools, ext_probes) = extension.into_parts();
        for tool in ext_tools {
            eprintln!("[tau]   tool: {}", tool.def().name);
            tools.register(tool);
        }
        for probe in ext_probes {
            probes.register(probe);
        }
    }

    if let Some(path) = &cli.mcp_bridge {
        anyhow::ensure!(
            cli.mcp_command.is_some() || cli.mcp_url.is_some(),
            "--mcp-bridge requires --mcp-command and/or --mcp-url (each granted capability is a consent)"
        );
        let mut consent = tau_ext::bridge::BridgeConsent::default();
        if let Some(command_json) = cli.mcp_command.as_deref() {
            let command: Vec<String> = serde_json::from_str(command_json).with_context(|| {
                format!("--mcp-command must be a JSON argv array, got: {command_json}")
            })?;
            eprintln!("[tau] mcp bridge: {} (command: {})", path.display(), command_json);
            consent.command = Some(command);
        }
        if let Some(url) = cli.mcp_url.as_deref() {
            let origin = tau_ext::bridge::origin_of(url)
                .with_context(|| format!("--mcp-url is not a valid http(s) url: {url}"))?;
            eprintln!("[tau] mcp bridge: {} (url: {}, origin: {})", path.display(), url, origin);
            consent.origins.insert(origin);
            consent.mcp_url = Some(url.to_string());
        }
        let bridge_tools = host
            .load_bridge(path, consent)
            .with_context(|| format!("loading mcp bridge {}", path.display()))?;
        for tool in bridge_tools {
            eprintln!("[tau]   mcp tool: {}", tool.def().name);
            tools.register(tool);
        }
    }

    let model: Box<dyn Model> = if cli.demo {
        Box::new(FauxModel::echo())
    } else if let Some(path) = &cli.provider_wasm {
        let name = cli
            .model
            .clone()
            .context("--provider-wasm needs --model to select a model id")?;
        Box::new(
            host.load_provider(path, name)
                .with_context(|| format!("loading provider {}", path.display()))?,
        )
    } else {
        let provider = cli.provider.clone().unwrap_or_else(|| {
            if std::env::var("ANTHROPIC_API_KEY").is_ok()
                && std::env::var("OPENAI_API_KEY").is_err()
            {
                "anthropic".into()
            } else {
                "openai".into()
            }
        });
        let name = cli.model.unwrap_or_else(default_model);
        match provider.as_str() {
            "openai" => Box::new(
                tau_openai::OpenAiModel::from_env(name)
                    .context("OPENAI_API_KEY not set (or use --demo)")?,
            ),
            "responses" => Box::new(
                tau_openai::OpenAiModel::from_env_with(name, tau_openai::Api::Responses)
                    .context("OPENAI_API_KEY not set (or use --demo)")?,
            ),
            "anthropic" => Box::new(
                tau_anthropic::AnthropicModel::from_env(name)
                    .context("ANTHROPIC_API_KEY not set (or use --demo)")?,
            ),
            other => anyhow::bail!("unknown provider: {other}"),
        }
    };

    let mut store = JsonlStore::open(&cli.session)
        .with_context(|| format!("opening {}", cli.session.display()))?;
    let history = if cli.r#continue {
        match store.head() {
            Some(head) => store.active_branch(&head.id)?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let mut agent = Agent::new(model, tools).probes(probes);
    if let Some(system) = cli.system {
        agent = agent.system(system);
    }

    let parent = store.head().map(|h| h.id.clone());

    // Renderer: just another event-bus subscriber.
    let mut events = agent.events();
    let renderer = tokio::spawn(async move {
        use std::io::Write;
        loop {
            match events.recv().await {
                Ok(AgentEvent::TextDelta(delta)) => {
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                }
                Ok(AgentEvent::ToolCallStart { name, .. }) => eprintln!("\n[tau] tool → {name}"),
                Ok(AgentEvent::ToolCallEnd { name, is_error, .. }) => {
                    eprintln!("[tau] tool ← {name}{}", if is_error { " (error)" } else { "" })
                }
                Ok(AgentEvent::Probe { point, action }) => {
                    eprintln!("[tau] probe {point}: {action}")
                }
                Ok(AgentEvent::RunEnd { .. } | AgentEvent::RunError { .. }) => break,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[tau] renderer lagged, skipped {n} events")
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let produced = agent.run(&history, Message::user(prompt_text)).await;
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
    eprintln!("[tau] session: {}", cli.session.display());
    Ok(())
}
