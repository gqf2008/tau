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

    /// MCP bridge component; requires --mcp-command. Granting the command
    /// IS the consent: the bridge may spawn exactly this argv.
    #[arg(long, requires = "mcp_command")]
    mcp_bridge: Option<PathBuf>,

    /// External MCP server argv as a JSON array, e.g.
    /// --mcp-command '["python", "server.py"]'
    #[arg(long)]
    mcp_command: Option<String>,
}

fn default_model() -> String {
    std::env::var("TAU_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(prompt_text) = cli.print else {
        eprintln!("tau v0 is print-mode only. Try: tau --demo -p \"hello\"");
        std::process::exit(2);
    };

    let mut tools = ToolRegistry::new();
    let mut probes = ProbeRegistry::new();
    let host = tau_ext::ExtensionHost::new();
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
        let command_json = cli.mcp_command.as_deref().unwrap_or("[]");
        let command: Vec<String> = serde_json::from_str(command_json)
            .with_context(|| format!("--mcp-command must be a JSON argv array, got: {command_json}"))?;
        eprintln!("[tau] mcp bridge: {} (command: {})", path.display(), command_json);
        let bridge_tools = host
            .load_bridge(path, &command)
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
