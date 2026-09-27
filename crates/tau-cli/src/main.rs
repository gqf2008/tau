//! tau CLI. Print mode (`-p`): run one prompt to completion, stream the
//! answer to stdout, persist the session as JSONL. Interactive mode (no
//! `-p` on a terminal): scrollback REPL — see `repl` module.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tau_core::agent::AgentEvent;
use tau_core::faux::FauxModel;
use tau_core::session::{EntryKind, JsonlStore, SessionEntry, new_id};
use tau_core::{Agent, Message, Model, ProbeRegistry, ToolRegistry};

mod repl;

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

    /// Persist this run's capability grants — bridge command/url/origins,
    /// provider credential delivery, WASI deny — under each loaded
    /// component's signing fingerprint; later runs recall them without
    /// the flags. Secrets are never persisted, only grants.
    #[arg(long)]
    remember: bool,
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

fn default_model() -> String {
    std::env::var("TAU_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".into())
}

/// Resolve a component argument: `oci://registry/repo:tag` pulls into the
/// content-addressed cache (signature/trust verification applies to the
/// cached bytes unchanged); anything else is a local path. Pulls run off
/// the runtime — reqwest blocking must not run on a tokio thread.
async fn resolve_component(arg: &std::path::Path) -> Result<PathBuf> {
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

/// Recalled-consent flow shared by extension, bridge and provider loads:
/// fingerprints from the component bytes, recall per fingerprint,
/// explicit flags win per field and origins union. Returns the merged
/// transport consent, the fingerprint (None for unsigned components — no
/// recall, no remembering) and the raw remembered record (capability
/// grants live there, not in the transport shape).
fn recall_consent(
    bytes: &[u8],
    explicit: tau_ext::bridge::BridgeConsent,
) -> (
    tau_ext::bridge::BridgeConsent,
    Option<String>,
    tau_ext::consent::RememberedConsent,
) {
    let fingerprints = tau_ext::consent::component_fingerprints(bytes).unwrap_or_default();
    let fingerprint = fingerprints.first().cloned();
    let store = tau_ext::consent::ConsentStore::default();
    let remembered: tau_ext::consent::RememberedConsent = fingerprint
        .as_deref()
        .and_then(|fp| store.load(fp))
        .unwrap_or_default();
    if !remembered.is_empty() {
        eprintln!(
            "[tau] recalled consent for {}",
            fingerprint.as_deref().unwrap_or("?")
        );
    }
    if remembered.wasi_deny {
        eprintln!("[tau] wasi deny recalled for this fingerprint");
    }
    (
        tau_ext::consent::merge(explicit, remembered.clone()),
        fingerprint,
        remembered,
    )
}

/// WASI posture for one load: the host-wide flag or a remembered
/// per-fingerprint deny. Deny is sticky — once remembered it applies to
/// every later load of that fingerprint until `tau consent --revoke`.
fn effective_wasi(
    deny_flag: bool,
    remembered: &tau_ext::consent::RememberedConsent,
) -> tau_ext::WasiPolicy {
    if deny_flag || remembered.wasi_deny {
        tau_ext::WasiPolicy::DenyAll
    } else {
        tau_ext::WasiPolicy::AllowAll
    }
}

/// Provider credential delivery: the explicit flag grants AND carries
/// the token. Without it, TAU_PROVIDER_AUTH flows only when the
/// component's remembered consent carries the auth-delivery grant — the
/// secret is re-given via the environment each run, never persisted.
fn resolve_provider_auth(flag: Option<String>, remembered_grant: bool) -> Option<String> {
    if flag.is_some() {
        return flag;
    }
    if remembered_grant {
        let token = std::env::var("TAU_PROVIDER_AUTH").ok();
        if token.is_some() {
            eprintln!("[tau] provider auth: recalled delivery grant, token from TAU_PROVIDER_AUTH");
        }
        token
    } else {
        if std::env::var("TAU_PROVIDER_AUTH").is_ok() {
            eprintln!(
                "[tau] note: TAU_PROVIDER_AUTH is set but this component has no \
                 auth-delivery grant — pass --provider-auth once with --remember"
            );
        }
        None
    }
}

/// Save this run's grants on --remember, merged into whatever the
/// fingerprint already carries (origins union, boolean grants sticky-on;
/// `--remember` never revokes). Unsigned components cannot carry it.
fn maybe_remember(
    remember: bool,
    fingerprint: &Option<String>,
    grant: tau_ext::consent::RememberedConsent,
) -> Result<()> {
    if !remember {
        return Ok(());
    }
    let Some(fp) = fingerprint else {
        anyhow::bail!(
            "--remember requires a signed component: unsigned components cannot carry remembered consent"
        );
    };
    let store = tau_ext::consent::ConsentStore::default();
    let merged = match store.load(fp) {
        Some(existing) => tau_ext::consent::remember_into(existing, grant),
        None => grant,
    };
    if merged.is_empty() {
        return Ok(());
    }
    store.save(fp, &merged)?;
    eprintln!("[tau] remembered consent for {fp}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(sub) = cli.command {
        return run_sub(sub).await;
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
        let path = &resolve_component(path).await?;
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let (_, fingerprint, remembered) =
            recall_consent(&bytes, tau_ext::bridge::BridgeConsent::default());
        let wasi = effective_wasi(cli.deny_wasi, &remembered);
        let extension = host
            .with_wasi_policy(wasi)
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
        maybe_remember(
            cli.remember,
            &fingerprint,
            tau_ext::consent::RememberedConsent {
                wasi_deny: wasi == tau_ext::WasiPolicy::DenyAll,
                ..Default::default()
            },
        )?;
    }

    if let Some(path) = &cli.mcp_bridge {
        let path = &resolve_component(path).await?;
        let mut explicit = tau_ext::bridge::BridgeConsent::default();
        if let Some(command_json) = cli.mcp_command.as_deref() {
            let command: Vec<String> = serde_json::from_str(command_json).with_context(|| {
                format!("--mcp-command must be a JSON argv array, got: {command_json}")
            })?;
            eprintln!(
                "[tau] mcp bridge: {} (command: {})",
                path.display(),
                command_json
            );
            explicit.command = Some(command);
        }
        if let Some(url) = cli.mcp_url.as_deref() {
            let origin = tau_ext::bridge::origin_of(url)
                .with_context(|| format!("--mcp-url is not a valid http(s) url: {url}"))?;
            eprintln!(
                "[tau] mcp bridge: {} (url: {}, origin: {})",
                path.display(),
                url,
                origin
            );
            explicit.origins.insert(origin);
            explicit.mcp_url = Some(url.to_string());
        }

        // Remembered consent: recalled per signing fingerprint; explicit
        // flags win per field, origins union. Unsigned components have no
        // fingerprint — no recall, no remembering.
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let (consent, fingerprint, remembered) = recall_consent(&bytes, explicit);
        anyhow::ensure!(
            consent.command.is_some() || consent.mcp_url.is_some(),
            concat!(
                "--mcp-bridge requires --mcp-command and/or --mcp-url, ",
                "or remembered consent (sign the component and pass --remember once)"
            )
        );
        let wasi = effective_wasi(cli.deny_wasi, &remembered);
        let bridge_tools = host
            .with_wasi_policy(wasi)
            .load_bridge(path, consent.clone())
            .with_context(|| format!("loading mcp bridge {}", path.display()))?;
        for tool in bridge_tools {
            eprintln!("[tau]   mcp tool: {}", tool.def().name);
            tools.register(tool);
        }

        maybe_remember(
            cli.remember,
            &fingerprint,
            tau_ext::consent::RememberedConsent {
                wasi_deny: wasi == tau_ext::WasiPolicy::DenyAll,
                ..tau_ext::consent::RememberedConsent::from(consent)
            },
        )?;
    }

    let model: Box<dyn Model> = if cli.demo {
        Box::new(FauxModel::demo())
    } else if let Some(raw) = &cli.provider_wasm {
        let path = &resolve_component(raw).await?;
        let name = cli
            .model
            .clone()
            .context("--provider-wasm needs --model to select a model id")?;
        let mut explicit = tau_ext::bridge::BridgeConsent::default();
        for url in &cli.provider_origin {
            let origin = tau_ext::bridge::origin_of(url)
                .with_context(|| format!("--provider-origin is not a valid http(s) url: {url}"))?;
            explicit.origins.insert(origin);
        }
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let (consent, fingerprint, remembered) = recall_consent(&bytes, explicit);
        if !consent.origins.is_empty() {
            eprintln!("[tau] provider http origins: {:?}", consent.origins);
        }
        let auth = resolve_provider_auth(cli.provider_auth.clone(), remembered.auth_delivery);
        let wasi = effective_wasi(cli.deny_wasi, &remembered);
        let model = Box::new(
            host.with_wasi_policy(wasi)
                .load_provider(path, name, consent.origins.clone(), auth)
                .with_context(|| format!("loading provider {}", path.display()))?,
        );
        maybe_remember(
            cli.remember,
            &fingerprint,
            tau_ext::consent::RememberedConsent {
                auth_delivery: cli.provider_auth.is_some(),
                wasi_deny: wasi == tau_ext::WasiPolicy::DenyAll,
                ..tau_ext::consent::RememberedConsent::from(consent)
            },
        )?;
        model
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
        .with_context(|| format!("opening {}", cli.session.display()))?
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

    let mut agent = Agent::new(model, tools)
        .probes(probes)
        .blobs(tau_core::BlobStore::new(tau_core::BlobStore::default_dir()));
    if let Some(system) = cli.system {
        agent = agent.system(system);
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
                eprintln!("[tau] compacted session: {}", cli.session.display());
                // Whatever follows — a -p prompt or the REPL — runs on
                // the compacted branch, whether or not --continue was
                // given: compacting and then ignoring the summary would
                // defeat the point (and the help text promises it).
                let head = store.head().unwrap().id.clone();
                history = store.active_branch(&head)?;
            }
        }
        if cli.print.is_none() && !interactive {
            return Ok(());
        }
    }

    // --continue-from: fork at an older entry. The probe can veto or
    // redirect; the fork materializes when the next entry appends under
    // the target.
    let mut base: Option<String> = None;
    if let Some(target) = &cli.continue_from {
        let (id, branch) = agent.navigate(&store, target).await?;
        let summary = store
            .get(&id)
            .map(tau_core::session::entry_summary)
            .unwrap_or_default();
        eprintln!("[tau] forked at {}: {summary}", &id[..12.min(id.len())]);
        history = branch;
        base = Some(id);
    }

    if interactive {
        return repl::interactive(agent, store, history, base).await;
    }
    let prompt_text = cli.print.expect("print mode checked above");

    let parent = base.or_else(|| store.head().map(|h| h.id.clone()));

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
                Ok(AgentEvent::AudioDelta { bytes, media_type }) => {
                    eprintln!("[tau] audio Δ {bytes} bytes ({media_type})")
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
                Ok(AgentEvent::FollowUp(message)) => {
                    eprintln!("[tau] follow-up: {}", message.text())
                }
                Ok(AgentEvent::Abort) => eprintln!("[tau] aborted"),
                Ok(AgentEvent::RunEnd { .. } | AgentEvent::RunError { .. }) => break,
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
    eprintln!("[tau] session: {}", cli.session.display());
    Ok(())
}

/// Depth of every entry (parent-chain length) in one pass. The store
/// is append-only and rejects unknown parents, so a parent always sits
/// earlier in the vec than its children: depth(entry) = depth(parent)+1
/// is O(n). Walking the parent chain per entry is O(n^2) and hung
/// `tau tree` for ~21s on a 20k chain.
/// Surface a discarded crash-torn tail once, where the user can see it.
fn warn_torn_tail(store: &JsonlStore) {
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

    #[test]
    fn wasi_deny_from_flag_or_recall() {
        let remembered = tau_ext::consent::RememberedConsent::default();
        assert_eq!(
            effective_wasi(false, &remembered),
            tau_ext::WasiPolicy::AllowAll
        );
        assert_eq!(
            effective_wasi(true, &remembered),
            tau_ext::WasiPolicy::DenyAll
        );
        let remembered = tau_ext::consent::RememberedConsent {
            wasi_deny: true,
            ..Default::default()
        };
        // Sticky: recalled deny applies without the flag.
        assert_eq!(
            effective_wasi(false, &remembered),
            tau_ext::WasiPolicy::DenyAll
        );
    }

    #[test]
    fn provider_auth_requires_flag_or_remembered_grant() {
        unsafe {
            std::env::set_var("TAU_PROVIDER_AUTH", "env-token");
        }
        // Env alone is not consent: no flag, no grant → no delivery.
        assert_eq!(resolve_provider_auth(None, false), None);
        // A remembered grant lets the env token flow.
        assert_eq!(
            resolve_provider_auth(None, true),
            Some("env-token".to_string())
        );
        // The explicit flag always wins, grant or not.
        assert_eq!(
            resolve_provider_auth(Some("flag-token".into()), false),
            Some("flag-token".to_string())
        );
        unsafe {
            std::env::remove_var("TAU_PROVIDER_AUTH");
        }
        // Grant without a token source delivers nothing.
        assert_eq!(resolve_provider_auth(None, true), None);
    }
}
