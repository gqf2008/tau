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
    /// Uplink fact (realtime sessions): the host pushed an audio chunk
    /// toward the provider. Recorded so the session tree can
    /// reconstruct what the model heard. `data` is base64 on the JSON
    /// wire, honest bytes in memory — same posture as AudioDelta.
    InputAudioChunk {
        /// The audio bytes pushed (base64 on the JSON wire).
        #[serde(
            serialize_with = "audio_data_serialize",
            deserialize_with = "audio_data_deserialize"
        )]
        data: Vec<u8>,
        /// The uplink media type (the session's input_media_type).
        media_type: String,
    },
    /// Server VAD: the user started speaking (realtime sessions).
    SpeechStarted,
    /// Server VAD: the user stopped speaking (realtime sessions).
    SpeechStopped,
    /// Barge-in (realtime sessions): the provider truncated the
    /// in-flight assistant audio. The loop freezes the current
    /// assembly (what played is what the user heard) and playback
    /// sinks clear their buffers; later chunks start a new segment.
    Interrupted,
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

/// Configuration for opening a [`RealtimeSession`].
#[derive(Debug, Clone, Default)]
pub struct RealtimeConfig {
    /// Uplink format the host will push — a raw stream, e.g.
    /// "audio/pcm;rate=16000" (realtime has no container; a container
    /// is the opposite of a persistent session).
    pub input_media_type: String,
    /// Preferred downlink format; the provider may answer with another.
    pub output_media_type: Option<String>,
    /// Session-level instructions (the system-prompt equivalent).
    pub instructions: Option<String>,
}

/// A persistent bidirectional session — the OPTIONAL realtime
/// capability of a [`Model`] (the OpenAI Realtime / Gemini Live
/// shape): open → push chunks → events flow → interrupt/close.
/// Same module-level contract as `stream`: failures surface as
/// [`ModelEvent::Error`], never panics; the `Result` on the push
/// methods only rejects at the door (a closed session refuses a chunk
/// synchronously).
#[async_trait]
pub trait RealtimeSession: Send {
    /// Push one uplink audio chunk in the session's input_media_type.
    async fn push_audio(&mut self, bytes: Vec<u8>) -> Result<(), String>;
    /// Push one image frame (JPEG) — the video uplink, ~1fps or
    /// scene-triggered.
    async fn push_image(&mut self, jpeg: Vec<u8>) -> Result<(), String>;
    /// Barge-in: truncate the in-flight assistant response.
    async fn interrupt(&mut self) -> Result<(), String>;
    /// End the session; terminal events may still flush first.
    async fn close(&mut self) -> Result<(), String>;
    /// The session's event stream. Take it once, at open.
    fn events(&self) -> BoxStream<'static, ModelEvent>;
}

/// A streaming model. Implementations: the built-in OpenAI/Anthropic
/// providers (tau-openai, tau-anthropic crates), wasm provider components
/// (tau-ext), and [`crate::faux::FauxModel`] for tests and demos.
#[async_trait]
pub trait Model: Send + Sync {
    /// Stream one assistant response. See the module-level contract.
    async fn stream(&self, req: &Request) -> BoxStream<'static, ModelEvent>;

    /// The realtime capability (docs/realtime-av.md). `None` — the
    /// default — means request/response only; capability discovery IS
    /// this call, so plain providers carry zero burden.
    fn realtime(&self, _config: RealtimeConfig) -> Option<Box<dyn RealtimeSession>> {
        None
    }
}
