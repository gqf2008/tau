//! The model boundary.
//!
//! Contract (mirrors pi's `StreamFn`): an implementation must not panic or
//! return an error for request/model/transport failures. Failures are reported
//! as [`ModelEvent::Error`] followed by [`ModelEvent::Done`] with
//! [`StopReason::Error`].

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::tool::ToolDef;
use crate::types::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    Stop,
    ToolUse,
    Length,
    Error,
    Aborted,
}

/// One incremental model output. This is also the wire shape pushed by wasm
/// provider components through the `events.emit` channel (see wit/tau.wit).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ModelEvent {
    /// Incremental assistant text.
    TextDelta { text: String },
    /// Incremental tool-call data. Deltas for one call share `index`;
    /// `id`/`name` arrive (typically once) before its argument fragments.
    ToolCallDelta {
        index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments_delta: String,
    },
    Done { stop: StopReason },
    Error { message: String },
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
}

#[async_trait]
pub trait Model: Send + Sync {
    /// Stream one assistant response. See the module-level contract.
    async fn stream(&self, req: &Request) -> BoxStream<'static, ModelEvent>;
}
