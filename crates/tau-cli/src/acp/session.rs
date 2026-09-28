//! The session registry: one ACP session, one JSONL, one agent.
//!
//! Everything a session owns lives here — its tree, its active branch, the
//! agent that runs its turns. Everything it does not is in the
//! [`Harness`]: the tools, the probes, the component host and the model are
//! built once per process and handed to every session, and a clone of the
//! registries shares the tool instances rather than copying them. That is
//! what keeps a wasm component instantiated once however many sessions
//! this process serves.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::v1::{NewSessionRequest, SessionId};
use anyhow::{Context, Result};
use tau_core::probe::ProbePoint;
use tau_core::session::{JsonlStore, new_id};
use tau_core::{Agent, Model, ProbeRegistry, ToolRegistry};

use crate::setup::{Harness, SharedModel};

/// One ACP session.
///
/// What is durable about a session is its file: the tree at
/// `<dir>/<session-id>.jsonl` holds every message, and a turn rebuilds its
/// history from it. What lives here is what the file cannot say — the
/// agent, holding this process's tools and probes, and the payload the
/// session lifecycle was announced with.
pub struct Session {
    /// The agent over the shared tools and probes.
    pub agent: Agent,
    /// The session_start payload, key for key what print mode sends.
    pub payload: serde_json::Value,
}

/// Every session this process is serving, and what they share.
pub struct Sessions {
    /// The component host. Held for the host channel, which is wired to a
    /// session's bus once the first session exists.
    host: tau_ext::ExtensionHost,
    tools: ToolRegistry,
    probes: ProbeRegistry,
    model: Arc<dyn Model>,
    model_label: String,
    /// Captured at startup: one working directory for the process, the
    /// same value print mode reports.
    cwd: PathBuf,
    /// Where the per-session JSONL files go.
    dir: PathBuf,
    system: Option<String>,
    /// The extension host channel takes one bus for the whole process, so
    /// it is wired to the first session and stays there.
    channel_wired: AtomicBool,
    sessions: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub fn new(harness: Harness, dir: PathBuf, system: Option<String>) -> Self {
        let Harness {
            cwd,
            host,
            tools,
            probes,
            model,
            model_label,
            mic_consent: _,
        } = harness;
        Self {
            host,
            tools,
            probes,
            model,
            model_label,
            cwd,
            dir,
            system,
            channel_wired: AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Open a session: a fresh JSONL in the session directory, an agent
    /// over the shared tools, and the session_start observation the loaded
    /// components expect.
    pub async fn create(&self, request: &NewSessionRequest) -> Result<SessionId> {
        // tau has one working directory per process, and the built-in
        // tools resolve against it. A client that asks for a different one
        // still gets a session — it just gets one rooted here, and is told
        // so rather than left to wonder why `ls` shows another tree.
        if !same_dir(&request.cwd, &self.cwd) {
            eprintln!(
                "[tau] acp: session/new asked for cwd {} but this process runs in {}; the tools use the process working directory",
                request.cwd.display(),
                self.cwd.display()
            );
        }
        // MCP servers are not the client's to grant here: tau loads MCP
        // through --mcp-bridge/-e, where the argv and origins the bridge
        // may reach are the explicit consent. Accepting a server list over
        // the wire would route around that gate.
        if !request.mcp_servers.is_empty() {
            eprintln!(
                "[tau] acp: session/new offered {} MCP server(s); ignored — tau takes MCP through --mcp-bridge, where the grant is explicit",
                request.mcp_servers.len()
            );
        }

        let id = new_id();
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating the session directory {}", self.dir.display()))?;
        let path = self.dir.join(format!("{id}.jsonl"));
        // Create it here rather than at the first append: the session is
        // then a file from the moment the client is told its id, so
        // `tau tree --session <file>` sees it, and a session directory tau
        // cannot write to fails at session/new — where the client can be
        // told — instead of mid-turn.
        std::fs::File::create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        // Opened and closed: the file is the session's durable state, and
        // a turn reopens it. Opening here is what surfaces a torn tail
        // left by an earlier crash at the point the client is listening,
        // rather than at the first turn.
        let store =
            JsonlStore::open(&path).with_context(|| format!("opening {}", path.display()))?;
        crate::warn_torn_tail(&store);

        let mut agent = Agent::new(
            Box::new(SharedModel(Arc::clone(&self.model))),
            self.tools.clone(),
        )
        .probes(self.probes.clone())
        .blobs(tau_core::BlobStore::new(tau_core::BlobStore::default_dir()));
        if let Some(system) = &self.system {
            agent = agent.system(system.clone());
        }
        self.wire_host_channel(&agent);

        let payload = serde_json::json!({
            "session": path.display().to_string(),
            "cwd": self.cwd.display().to_string(),
            "model": self.model_label,
        });
        agent
            .observe(ProbePoint::SessionStart, payload.clone())
            .await;

        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let second = sessions.len() == 1;
        sessions.insert(id.clone(), Session { agent, payload });
        drop(sessions);
        if second {
            eprintln!(
                "[tau] acp: the extension host channel stays bound to this process's first session — host.steer and host.notify are process-scoped today"
            );
        }
        Ok(SessionId::new(id))
    }

    /// The first session owns the process-wide host channel.
    fn wire_host_channel(&self, agent: &Agent) {
        if self.channel_wired.swap(true, Ordering::SeqCst) {
            return;
        }
        let (inject_tx, mut inject_rx) = tokio::sync::mpsc::unbounded_channel::<tau_core::Control>();
        self.host.wire_host_channel(agent.bus(), inject_tx);
        let control = agent.control();
        tokio::spawn(async move {
            while let Some(queued) = inject_rx.recv().await {
                let _ = control.send(queued);
            }
        });
    }

    /// The client is gone: fire session_end for every session that is
    /// still open, so the components see the same lifecycle print mode
    /// gives them, then drop them.
    pub async fn end_all(&self) {
        let open: Vec<Session> = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions.drain().map(|(_, session)| session).collect()
        };
        for session in open {
            let payload = session.payload.clone();
            session.agent.observe(ProbePoint::SessionEnd, payload).await;
        }
    }
}

/// Whether two paths name the same directory, tolerating the spelling
/// differences a client and a process can disagree on (a trailing
/// separator, a short name, the case of a drive letter).
fn same_dir(asked: &Path, ours: &Path) -> bool {
    match (std::fs::canonicalize(asked), std::fs::canonicalize(ours)) {
        (Ok(asked), Ok(ours)) => asked == ours,
        _ => asked == ours,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_dir_sees_through_spelling() {
        let dir = std::env::temp_dir();
        assert!(same_dir(&dir, &dir));
        // A path that does not exist cannot be canonicalized; the two are
        // then compared as written, which is the honest fallback.
        let missing = dir.join("tau-acp-no-such-directory");
        assert!(same_dir(&missing, &missing));
        assert!(!same_dir(&missing, &dir));
    }
}
