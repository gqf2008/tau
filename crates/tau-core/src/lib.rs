//! tau-core: the headless agent harness.
//!
//! Design follows the pi agent harness (MIT, earendil-works/pi):
//! a session is a tree of entries stored as JSONL; the path from the root to
//! the current entry is the active branch and supplies model history.
//!
//! Everything an embedding needs is re-exported at the crate root:
//! [`Agent`] (the loop), [`Model`] / [`Request`] / [`ModelEvent`] (the
//! model boundary), [`Tool`] / [`ToolRegistry`] (tools), [`JsonlStore`] /
//! [`SessionEntry`] (sessions), [`ProbePoint`] / [`Verdict`] (extension
//! influence points) and the [`types`] data model.
//!
//! ## Embedding quick start
//!
//! ```
//! use tau_core::faux::FauxModel;
//! use tau_core::{Agent, Message, ToolRegistry};
//!
//! let runtime = tokio::runtime::Builder::new_current_thread()
//!     .enable_all()
//!     .build()
//!     .unwrap();
//! runtime.block_on(async {
//!     // FauxModel is the scripted stand-in — no API key needed; swap in
//!     // tau_openai / tau_anthropic / a wasm provider for a real model.
//!     let agent = Agent::new(Box::new(FauxModel::demo()), ToolRegistry::new());
//!     let produced = agent
//!         .run(&[], Message::user("hello"))
//!         .await
//!         .expect("run");
//!     assert!(produced.iter().any(|m| !m.text().is_empty()));
//! });
//! ```

pub mod agent;
pub mod blobs;
pub mod bus;
pub mod control;
pub mod faux;
pub mod model;
pub mod probe;
pub mod session;
pub mod sse;
pub mod tool;
pub mod types;

pub use agent::{Agent, AgentError, AgentEvent};
pub use blobs::{BlobStore, INLINE_LIMIT};
pub use bus::{EventBus, EventStream, new_bus};
pub use control::{Control, ControlTx};
pub use model::{Model, ModelEvent, RealtimeConfig, RealtimeSession, Request, StopReason};
pub use probe::{ProbeHandler, ProbePoint, ProbeRegistry, Verdict};
pub use session::{EntryKind, JsonlStore, SessionEntry, SessionError};
pub use tool::{Tool, ToolDef, ToolOutput, ToolRegistry};
pub use types::{Content, Media, MediaSource, Message, Role};
