//! tau-core: the headless agent harness.
//!
//! Design follows the pi agent harness (MIT, earendil-works/pi):
//! a session is a tree of entries stored as JSONL; the path from the root to
//! the current entry is the active branch and supplies model history.

pub mod agent;
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
pub use bus::{new_bus, EventBus, EventStream};
pub use control::{Control, ControlTx};
pub use model::{Model, ModelEvent, Request, StopReason};
pub use probe::{ProbeHandler, ProbePoint, ProbeRegistry, Verdict};
pub use session::{EntryKind, JsonlStore, SessionEntry, SessionError};
pub use tool::{Tool, ToolDef, ToolOutput, ToolRegistry};
pub use types::{Content, Media, MediaSource, Message, Role};
