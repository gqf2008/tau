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
/// Why a streamed response ended ([`ModelEvent::Done`]).
pub enum StopReason {
    /// The model finished naturally.
    Stop,
    /// The model wants tool calls executed; the loop continues after results.
    ToolUse,
    /// Truncated by a token limit.
    Length,
    /// Ended by a failure (preceded by [`ModelEvent::Error`]).
    Error,
    /// Cancelled from outside (e.g. a steering interrupt).
    Aborted,
}

/// One incremental model output. This is also the wire shape pushed by wasm
/// provider components through the `events.emit` channel (see wit/tau.wit).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ModelEvent {
    /// Incremental assistant text.
    TextDelta {
        /// The new text fragment.
        text: String,
    },
    /// Incremental tool-call data. Deltas for one call share `index`;
    /// `id`/`name` arrive (typically once) before its argument fragments.
    ToolCallDelta {
        /// Groups the deltas of one call (calls may interleave).
        index: u32,
        /// The call's id, on its first delta.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// The tool's name, on its first delta.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// A fragment of the JSON arguments text.
        arguments_delta: String,
    },
    /// Incremental assistant audio (realtime-style providers). Chunks of
    /// one contiguous segment share a media type; concatenating them in
    /// order must yield valid content of that type. `data` is base64 on
    /// the JSON wire, honest bytes in memory.
    AudioDelta {
        /// The audio bytes (base64 on the JSON wire).
        #[serde(
            serialize_with = "audio_data_serialize",
            deserialize_with = "audio_data_deserialize"
        )]
        data: Vec<u8>,
        /// MIME type shared by the contiguous segment this chunk belongs to.
        media_type: String,
    },
    /// Terminal: the response ended successfully for the given reason.
    Done {
        /// Why it ended.
        stop: StopReason,
    },
    /// Terminal failure — always followed by
    /// `Done { stop: StopReason::Error }`; never an Err or a panic.
    Error {
        /// What failed, human-readable.
        message: String,
    },
}

fn audio_data_serialize<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    use base64::Engine;
    s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn audio_data_deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    use base64::Engine;
    let text = String::deserialize(d)?;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(serde::de::Error::custom)
}

/// One request to a model: system prompt, history, available tools.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// System prompt, if any.
    pub system: Option<String>,
    /// Conversation history (the session's active branch).
    pub messages: Vec<Message>,
    /// Tools the model may call, sorted by name.
    pub tools: Vec<ToolDef>,
}

/// A streaming model. Implementations: the built-in OpenAI/Anthropic
/// providers (tau-openai, tau-anthropic crates), wasm provider components
/// (tau-ext), and [`crate::faux::FauxModel`] for tests and demos.
#[async_trait]
pub trait Model: Send + Sync {
    /// Stream one assistant response. See the module-level contract.
    async fn stream(&self, req: &Request) -> BoxStream<'static, ModelEvent>;
}
