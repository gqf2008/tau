//! The editor-attached mode (`--acp`): tau as an ACP agent on stdio.
//!
//! The client — Zed, or anything else speaking the Agent Client Protocol —
//! spawns `tau --acp` and then talks JSON-RPC over our stdin and stdout.
//! stdout carries that protocol and nothing else; every diagnostic tau
//! prints goes to stderr, which is where the client will show it.
//!
//! # The invariant that keeps this alive
//!
//! A handler registered below runs *inside* the SDK's dispatch loop: while
//! one runs, no other incoming message is processed. Awaiting anything that
//! itself needs that loop — a request of ours, another message — deadlocks
//! the connection, runtime threads and all. So a handler parses and hands
//! the work to a spawned task; the task is where work is awaited, and it
//! always answers for itself (a spawned task that returns an error takes
//! the whole connection down with it).
//!
//! That is also why `session/cancel` is handled inline and `session/prompt`
//! is not: the cancel has to reach a running turn, and it can only do that
//! if the connection is still reading while that turn runs.
//!
//! # What one process serves
//!
//! Any number of ACP sessions. Each gets its own JSONL under the directory
//! `--session` names (`<dir>/<session-id>.jsonl`) and its own agent; the
//! tools, the probes, the component host and the model are process-scoped
//! and shared — built once by [`crate::setup::build`], which is why the
//! registries are `Clone`. A JSONL written here is an ordinary tau session:
//! `tau tree --session <file>` reads it, `--continue` resumes it.

mod map;
mod permission;
mod session;
mod turn;

use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, Error, Implementation, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest,
};
use agent_client_protocol::{Agent, Stdio, on_receive_notification, on_receive_request};

use crate::Cli;
use session::Sessions;

/// Serve ACP on stdio until the client closes it.
///
/// Clean EOF on stdin is the normal end of a session — the editor exited or
/// closed the pipe — so it returns `Ok`, exactly as the SDK's reactive
/// `connect_to` reports it.
pub async fn serve(cli: &Cli) -> anyhow::Result<()> {
    let harness = crate::setup::build(cli).await?;
    // No `--system` here: the harness already folded it together with
    // what the working directory offers, and every session on it starts
    // from that same prompt.
    let sessions = Arc::new(Sessions::new(harness, cli.session_dir()));

    let created = Arc::clone(&sessions);
    let prompted = Arc::clone(&sessions);
    let cancelled = Arc::clone(&sessions);
    let closed = Arc::clone(&sessions);

    Agent
        .builder()
        .name("tau")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                // v1 only. A client that asked for something else is told
                // which version it is actually talking to and left to
                // decide — the spec has the client disconnect on a version
                // it cannot speak.
                if request.protocol_version != ProtocolVersion::V1 {
                    eprintln!(
                        "[tau] acp: client offered protocol version {}, answering 1",
                        request.protocol_version.as_u16()
                    );
                }
                // The client's own capabilities are read, not used: tau
                // does its file and terminal work with its built-in tools
                // in its own working directory, and never asks the client
                // to do either (docs/acp.md). Saying so once, here, is
                // what keeps a host from wondering why the delegation it
                // advertises is ignored.
                let offered = &request.client_capabilities;
                if offered.fs.read_text_file || offered.fs.write_text_file || offered.terminal {
                    eprintln!(
                        "[tau] acp: the client offers fs (read: {}, write: {}) and terminal: {}; tau does not delegate, its built-in tools work in its own directory",
                        offered.fs.read_text_file, offered.fs.write_text_file, offered.terminal
                    );
                }
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(
                            AgentCapabilities::new()
                                // Not a maybe: session/load is not implemented.
                                // The file that would back it is already on
                                // disk, but replaying it as updates is not
                                // written, and claiming it would make an
                                // editor offer a history it cannot show.
                                .load_session(false)
                                // Both content bits are honest: an image
                                // block becomes Content::Image and an audio
                                // block Content::Audio, the same shapes the
                                // REPL's own /mic sends. The other direction
                                // is not carried — a realtime provider's
                                // output audio has no stable v1 update
                                // (docs/acp.md).
                                .prompt_capabilities(
                                    PromptCapabilities::new().image(true).audio(true),
                                ),
                        )
                        // No auth methods: credentials come from the
                        // environment the client spawned us with, so
                        // `authenticate` stays unimplemented and the SDK
                        // answers method-not-found for it.
                        .agent_info(Implementation::new("tau", env!("CARGO_PKG_VERSION"))),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, connection| {
                let sessions = Arc::clone(&created);
                // The session's permission gate asks through this same
                // connection, so it keeps a handle.
                let asking = connection.clone();
                // Off the dispatch loop: opening the store and firing
                // session_start at the loaded components both take real
                // time, and the connection may not process anything else
                // while a handler runs.
                connection.spawn(async move {
                    match sessions.create(&request, &asking).await {
                        Ok(id) => {
                            let _ = responder.respond(NewSessionResponse::new(id));
                        }
                        Err(error) => {
                            eprintln!("[tau] acp: session/new failed: {error:#}");
                            let _ = responder.respond_with_error(
                                Error::internal_error()
                                    .data(serde_json::json!(format!("{error:#}"))),
                            );
                        }
                    }
                    Ok(())
                })
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                let sessions = Arc::clone(&prompted);
                // Parsed here, run there: folding the blocks into one
                // message is a pure function and costs nothing, and the
                // notes it returns — a block tau cannot carry — are worth
                // printing even if the turn below never starts.
                let (prompt, notes) = map::prompt_message(&request.prompt);
                for note in &notes {
                    eprintln!("[tau] acp: {note}");
                }
                let id = request.session_id;
                // The task keeps a clone: the handler's own connection
                // handle is borrowed by `spawn`, and a turn needs one to
                // send `session/update` while it runs.
                let sender = connection.clone();
                connection.spawn(async move {
                    sessions.prompt(id, prompt, responder, sender).await;
                    // Never an error: returning one here would take the
                    // whole connection down over a single failed turn.
                    Ok(())
                })
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _connection| {
                // Handled inline, not spawned: this notification is only
                // useful if it lands while the turn it cancels is running,
                // and a spawned task could be scheduled after that turn
                // had already answered.
                cancelled.cancel(&notification.session_id);
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_close(async move |_connection| {
            closed.end_all().await;
            Ok(())
        })
        .connect_to(Stdio::new())
        .await
        .map_err(|error| anyhow::anyhow!("acp: the connection ended: {error}"))
}
