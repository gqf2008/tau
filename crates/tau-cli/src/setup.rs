//! Assembling a run: the host the components live in, the tools and
//! probes they registered, and the model that drives the loop.
//!
//! Everything here is process-scoped — built once from the command line,
//! before there is a session. A session is what comes after: a store, a
//! history, an [`Agent`](tau_core::Agent) over these tools. Keeping the
//! two apart is what lets one process serve more than one session.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tau_core::faux::FauxModel;
use tau_core::{Model, ProbeRegistry, ToolRegistry};

use crate::Cli;

/// What a run is made of before it has a session.
pub struct Harness {
    /// Captured once, at startup: built-ins resolve relative paths against
    /// it, and the session payload reports the same value.
    pub cwd: PathBuf,
    /// The component host. Kept because the host channel is wired per
    /// agent, once a session exists.
    pub host: tau_ext::ExtensionHost,
    pub tools: ToolRegistry,
    pub probes: ProbeRegistry,
    /// The model, shared: a process serving several sessions builds one
    /// and hands every session a [`SharedModel`] over it.
    pub model: Arc<dyn Model>,
    pub model_label: String,
    /// The system prompt every session on this harness starts with:
    /// `--system` first, then what the working directory itself offers —
    /// `AGENTS.md` project instructions, and the skills manifest when
    /// `load_skill` is in the run (docs/skills.md). `None` when the user
    /// gave neither.
    pub system: Option<String>,
}

/// Build the harness from the command line: register the built-ins, load
/// every component (extensions, then the MCP bridge), and pick the model.
pub async fn build(cli: &Cli) -> Result<Harness> {
    // One capture of the working directory for the whole run: every built-in
    // resolves relative paths against it, and the session payload below
    // reports the same value. A directory that cannot be read at startup is
    // not worth failing over — absolute paths still work.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // What the directory offers the model besides tools (issue #4): skill
    // directories under the on-disk conventions, and the `AGENTS.md`
    // files from here up to the repository root. Discovery never fails a
    // run — a broken skill is a stderr note and a skip.
    let skills = tau_core::SkillIndex::discover(&cwd);
    // `--tools` names built-ins *and* component tools (pi's semantics), and
    // the components load below — so a name that is no built-in is not an
    // error yet. Whatever it names that nothing provides is caught after the
    // last component has registered.
    let requested: Option<Vec<String>> = cli.tools.as_deref().map(|list| {
        list.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect()
    });
    let mut available: BTreeSet<String> =
        tau_tools::names().into_iter().map(String::from).collect();

    let mut tools = ToolRegistry::new();
    // Built-ins go in first: registration is last-wins, so a component
    // shipping its own `read` shadows the built-in (pi's behaviour too).
    let selection = if cli.no_builtin_tools {
        tau_tools::BuiltinTools::none(&cwd)
    } else {
        match &requested {
            Some(names) => tau_tools::BuiltinTools::selecting(&cwd, names),
            None => tau_tools::BuiltinTools::all(&cwd),
        }
    };
    // The skills ride along with the built-ins: `load_skill` needs them,
    // and the two switches above decide whether it makes the run like any
    // other built-in. A directory with no skills registers no such tool.
    let builtins = selection.with_skills(skills.clone());
    let registered = tau_tools::register(&mut tools, &builtins);
    // Always printed, "none" included: a run says which built-in tools it
    // has, and validate.sh asserts on this line.
    eprintln!(
        "[tau] built-in tools: {}",
        if registered.is_empty() {
            "none".to_string()
        } else {
            registered.join(", ")
        }
    );
    // Always printed, "none" included, for the same reason the built-in
    // line is: a run says what the directory offered it, and validate.sh
    // asserts on the line. The instructions are named individually —
    // root-first, the order they reach the model in.
    eprintln!(
        "[tau] skills: {}",
        if skills.skills().is_empty() {
            "none".to_string()
        } else {
            skills
                .skills()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    for path in skills.hint_paths() {
        eprintln!("[tau] project instructions: {}", path.display());
    }
    // The manifest is advertised only to a run that can load a body: with
    // `--no-builtin-tools` or a `--tools` list without `load_skill` there
    // is no such tool, and naming skills the model cannot read would
    // invite calls that must fail. The instructions go either way.
    let project_context = if registered.iter().any(|name| name == "load_skill") {
        skills.system_context()
    } else {
        skills.hints_context()
    };

    let mut probes = ProbeRegistry::new();
    let host = if cli.allow_unsigned {
        tau_ext::ExtensionHost::new()
    } else {
        tau_ext::ExtensionHost::with_policy(tau_ext::sign::TrustPolicy::RequireTrusted {
            trust_dir: tau_ext::sign::trust_dir(),
        })
    };
    for path in &cli.extensions {
        let path = &resolve_component(path).await?;
        let extension = host
            .load(path)
            .with_context(|| format!("loading {}", path.display()))?;
        eprintln!("[tau] loaded extension: {}", extension.name);
        let (ext_tools, ext_probes) = extension.into_parts();
        for tool in ext_tools {
            let name = tool.def().name;
            available.insert(name.clone());
            if let Some(names) = &requested
                && !names.contains(&name)
            {
                // `--tools` replaced the selection: a component tool it did
                // not name is loaded but not offered to the model.
                eprintln!("[tau]   tool: {name} (not selected by --tools)");
                continue;
            }
            eprintln!("[tau]   tool: {name}");
            tools.register(tool);
        }
        for probe in ext_probes {
            probes.register(probe);
        }
    }

    if let Some(path) = &cli.mcp_bridge {
        let path = &resolve_component(path).await?;
        let mut config = tau_ext::bridge::BridgeConfig::default();
        if let Some(command_json) = cli.mcp_command.as_deref() {
            let command: Vec<String> = serde_json::from_str(command_json).with_context(|| {
                format!("--mcp-command must be a JSON argv array, got: {command_json}")
            })?;
            eprintln!(
                "[tau] mcp bridge: {} (command: {})",
                path.display(),
                command_json
            );
            config.command = Some(command);
        }
        if let Some(url) = cli.mcp_url.as_deref() {
            // The endpoint must be a URL the host's own client can dial; since
            // 0.8.0 the origin is not a grant (the runtime gates are gone).
            tau_ext::bridge::origin_of(url)
                .or_else(|| tau_ext::ws::origin_of(url))
                .with_context(|| format!("--mcp-url is not a valid http(s)/ws(s) url: {url}"))?;
            eprintln!("[tau] mcp bridge: {} (url: {})", path.display(), url);
            config.mcp_url = Some(url.to_string());
        }
        if !cli.ingress.is_empty() {
            eprintln!(
                "[tau]   ingress: the host serves webhooks on {}",
                cli.ingress.join(", ")
            );
            config.listen = cli.ingress.clone();
        }
        anyhow::ensure!(
            config.command.is_some() || config.mcp_url.is_some() || !config.listen.is_empty(),
            "--mcp-bridge requires --mcp-command, --mcp-url and/or --ingress"
        );
        let bridge = host
            .load_bridge(path, config)
            .with_context(|| format!("loading mcp bridge {}", path.display()))?;
        let (bridge_tools, bridge_probes) = bridge.into_parts();
        for tool in bridge_tools {
            let name = tool.def().name;
            available.insert(name.clone());
            if let Some(names) = &requested
                && !names.contains(&name)
            {
                eprintln!("[tau]   mcp tool: {name} (not selected by --tools)");
                continue;
            }
            eprintln!("[tau]   mcp tool: {name}");
            tools.register(tool);
        }
        // The IM outbound leg: bridge probes (after_response) register
        // alongside extension probes.
        for probe in bridge_probes {
            probes.register(probe);
        }
    }

    // Fail-closed on `--tools`: every name the user asked for has to exist
    // this run, or the run does not start — a typo must not hand the model a
    // smaller toolset than the user believes it has.
    if let Some(names) = &requested {
        for name in names {
            anyhow::ensure!(
                tools.get(name).is_some(),
                "unknown tool: {name} (available: {})",
                available.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }
    }

    let (model, model_label): (Box<dyn Model>, String) = if cli.demo {
        // Host doctrine: the CLI on an explicit user command holds the
        // user's authority (same as /mic).
        (Box::new(FauxModel::demo(tools.demo_pick())), "demo".into())
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
        let name = cli.model.clone().unwrap_or_else(default_model);
        let label = format!("{provider}/{name}");
        let model: Box<dyn Model> = match provider.as_str() {
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
        };
        // Native providers are host code — the user's explicit /live
        // command IS the consent (same doctrine as /mic).
        (model, label)
    };
    Ok(Harness {
        cwd,
        host,
        tools,
        probes,
        model: Arc::from(model),
        model_label,
        system: compose_system(cli.system.clone(), project_context),
    })
}

/// The session's system prompt: the user's own words first, then what the
/// working directory had to say about itself — nothing is dropped, and a
/// session with neither gets no system prompt at all (the request keeps
/// the shape it always had).
fn compose_system(flag: Option<String>, context: Option<String>) -> Option<String> {
    match (flag, context) {
        (Some(flag), Some(context)) => Some(format!("{flag}\n\n{context}")),
        (flag, context) => flag.or(context),
    }
}

/// A [`Model`] that hands every caller the one shared instance.
///
/// [`Agent::new`](tau_core::Agent::new) takes its model by value, and a
/// process serving several sessions creates an agent per session — so
/// without this the model could not be shared at all. Sharing is safe by
/// the trait's own contract: [`Model::stream`] takes `&self`, and a
/// provider that holds state does so behind its own locks.
pub struct SharedModel(pub Arc<dyn Model>);

#[async_trait::async_trait]
impl Model for SharedModel {
    async fn stream(
        &self,
        req: &tau_core::Request,
    ) -> futures::stream::BoxStream<'static, tau_core::ModelEvent> {
        self.0.stream(req).await
    }

    fn realtime(
        &self,
        config: tau_core::RealtimeConfig,
    ) -> Option<Box<dyn tau_core::RealtimeSession>> {
        self.0.realtime(config)
    }
}

fn default_model() -> String {
    std::env::var("TAU_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".into())
}

/// Resolve a component argument: `oci://registry/repo:tag` pulls into the
/// content-addressed cache (signature/trust verification applies to the
/// cached bytes unchanged); anything else is a local path. Pulls run off
/// the runtime — reqwest blocking must not run on a tokio thread.
pub async fn resolve_component(arg: &std::path::Path) -> Result<PathBuf> {
    let text = arg.to_string_lossy();
    if !text.starts_with("oci://") {
        return Ok(arg.to_path_buf());
    }
    let reference = text.into_owned();
    let pulled = {
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || tau_ext::oci::pull(&reference))
            .await
            .context("oci pull task")??
    };
    eprintln!(
        "[tau] oci: {} -> {} ({})",
        reference,
        pulled.digest,
        pulled.path.display()
    );
    if pulled.mutable_tag {
        eprintln!(
            "[tau] note: mutable tag — pin @{} for reproducible loads",
            pulled.digest
        );
    }
    Ok(pulled.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_prompt_is_the_flag_then_the_directory() {
        assert_eq!(compose_system(None, None), None);
        assert_eq!(
            compose_system(Some("be terse".into()), None),
            Some("be terse".to_string())
        );
        assert_eq!(
            compose_system(None, Some("context".into())),
            Some("context".to_string())
        );
        assert_eq!(
            compose_system(Some("be terse".into()), Some("context".into())),
            Some("be terse\n\ncontext".to_string()),
            "the user's words come first, the directory's after"
        );
    }
}
