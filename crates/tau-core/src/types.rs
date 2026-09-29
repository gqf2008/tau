//! Core data model: messages, content blocks, and media.
//!
//! The rule that shapes everything here: **bytes are the model, base64 is
//! an encoding of the JSON edges only** — session files and provider HTTP
//! APIs speak JSON, so serde encodes there; in memory media is honest
//! `Vec<u8>`.

use serde::{Deserialize, Serialize};

/// Free-form JSON (tool call arguments, provider-specific fields).
pub type Json = serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
/// Who produced a [`Message`].
pub enum Role {
    /// The human (or the harness on their behalf).
    User,
    /// The model.
    Assistant,
    /// A tool result message (content is [`Content::ToolResult`] blocks).
    Tool,
}

/// A media payload: image, audio, video, or an arbitrary file.
///
/// In memory, media is honest bytes (`Vec<u8>`). Base64 is an encoding of
/// the JSON edges only: session-file persistence and provider HTTP APIs
/// both speak JSON, so serde emits `{"source":"base64","data":...}` and the
/// provider mappings encode at request-build time. Binary boundaries (the
/// wasm ABI, a future blob store) never see base64.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Media {
    /// MIME type, e.g. "image/png", "audio/wav", "video/mp4", "application/pdf".
    pub media_type: String,
    /// Where the bytes live.
    #[serde(flatten)]
    pub source: MediaSource,
}

/// Where a [`Media`]'s bytes live.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaSource {
    /// Raw bytes. Serialized to session JSON as base64.
    Bytes(Vec<u8>),
    /// Remote reference; providers that accept URLs use it directly.
    Url(String),
    /// Content-addressed reference into the blob store (`sha256:<hex>`);
    /// large media is externalized to keep session JSONL small (see
    /// `blobs` module). Materialized back to bytes at the request edge.
    Blob {
        /// `sha256:<hex>` of the content.
        hash: String,
    },
}

impl MediaSource {
    /// Base64 encoding for JSON edges (session file, provider HTTP).
    /// Not part of the data model — bytes are the model.
    pub fn encode_base64(&self) -> Option<String> {
        use base64::Engine;
        match self {
            Self::Bytes(bytes) => Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
            Self::Url(_) | Self::Blob { .. } => None,
        }
    }
}

impl Serialize for MediaSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "source", rename_all = "kebab-case")]
        enum Repr<'a> {
            Base64 { data: String },
            Url { url: &'a str },
            Blob { hash: &'a str },
        }
        match self {
            Self::Bytes(_) => Repr::Base64 {
                data: self.encode_base64().unwrap_or_default(),
            },
            Self::Url(url) => Repr::Url { url },
            Self::Blob { hash } => Repr::Blob { hash },
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MediaSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use base64::Engine;
        #[derive(Deserialize)]
        #[serde(tag = "source", rename_all = "kebab-case")]
        enum Repr {
            Base64 { data: String },
            Url { url: String },
            Blob { hash: String },
        }
        match Repr::deserialize(deserializer)? {
            Repr::Base64 { data } => base64::engine::general_purpose::STANDARD
                .decode(&data)
                .map(MediaSource::Bytes)
                .map_err(|e| serde::de::Error::custom(format!("invalid base64 media: {e}"))),
            Repr::Url { url } => Ok(MediaSource::Url(url)),
            Repr::Blob { hash } => Ok(MediaSource::Blob { hash }),
        }
    }
}

impl Media {
    /// A media payload from in-memory bytes.
    pub fn bytes(media_type: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self {
            media_type: media_type.into(),
            source: MediaSource::Bytes(data.into()),
        }
    }

    /// A media payload by remote reference.
    pub fn url(media_type: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            media_type: media_type.into(),
            source: MediaSource::Url(url.into()),
        }
    }

    /// A media payload by blob-store hash (`sha256:<hex>`).
    pub fn blob(media_type: impl Into<String>, hash: impl Into<String>) -> Self {
        Self {
            media_type: media_type.into(),
            source: MediaSource::Blob { hash: hash.into() },
        }
    }

    /// Data URL form ("data:image/png;base64,...") for APIs that take one.
    pub fn data_url(&self) -> Option<String> {
        match &self.source {
            MediaSource::Bytes(_) => Some(format!(
                "data:{};base64,{}",
                self.media_type,
                self.source.encode_base64()?
            )),
            MediaSource::Url(url) => Some(url.clone()),
            // No bytes on hand — a data URL cannot be formed. The agent
            // materializes blobs before providers see them.
            MediaSource::Blob { .. } => None,
        }
    }
}

/// One block of a [`Message`]'s content.
///
/// Wire shape is internally tagged (`{"type":"text",...}`,
/// `{"type":"toolCall",...}`) — the same shape pi sessions use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Content {
    /// A plain text block.
    Text {
        /// The text.
        text: String,
    },
    /// An image (screenshot, photo, diagram).
    Image {
        /// The image payload.
        media: Media,
    },
    /// An audio clip.
    Audio {
        /// The audio payload.
        media: Media,
    },
    /// A video clip.
    Video {
        /// The video payload.
        media: Media,
    },
    /// An arbitrary file attachment (PDF, archive, source bundle, ...).
    File {
        /// The file payload.
        media: Media,
        /// Original filename, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// A tool invocation requested by the assistant.
    ToolCall {
        /// Provider-assigned call id (echoed back by [`Content::ToolResult`]).
        id: String,
        /// Registered tool name.
        name: String,
        /// Arguments matching the tool's JSON schema.
        arguments: Json,
    },
    /// The outcome of one [`Content::ToolCall`].
    ToolResult {
        /// The call this answers.
        call_id: String,
        /// Result blocks: text and/or media (tool media results since
        /// 0.3.0 — docs/tool-media.md, wit-review F4). Sessions written
        /// before 0.3.0 carry a plain string here; reads upgrade it to a
        /// single text block. Writes always use the block array.
        #[serde(deserialize_with = "string_or_blocks")]
        content: Vec<Content>,
        /// True when the tool reported failure.
        is_error: bool,
    },
}

/// Read-compat for pre-0.3.0 sessions: a tool result's `content` was a
/// plain string; upgrade it to one text block. New writes are always the
/// block array (pi's shape).
fn string_or_blocks<'de, D>(deserializer: D) -> Result<Vec<Content>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrBlocks {
        String(String),
        Blocks(Vec<Content>),
    }
    Ok(match StringOrBlocks::deserialize(deserializer)? {
        StringOrBlocks::String(text) => vec![Content::Text { text }],
        StringOrBlocks::Blocks(blocks) => blocks,
    })
}

/// A tool invocation as a named record: the ABI's `tool-call`
/// (`interface types`), which [`Content::ToolCall`] carries inline today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id (echoed back by a tool result).
    pub id: String,
    /// Registered tool name.
    pub name: String,
    /// Arguments matching the tool's JSON schema.
    pub arguments: Json,
}

/// The non-recursive half of [`Content`]: what a tool result may carry.
///
/// The ABI calls this `result-block`, and it has to be a separate type
/// there: `content` reaching itself through a tool result is refused by
/// the toolchain (`error: type `content` depends on itself`), so
/// [`Content::ToolResult`]'s `Vec<Content>` is wider than any contract can
/// be. Here the two coexist until the host converges on the narrower one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ResultBlock {
    /// A plain text block.
    Text {
        /// The text.
        text: String,
    },
    /// An image.
    Image {
        /// The image payload.
        media: Media,
    },
    /// An audio clip.
    Audio {
        /// The audio payload.
        media: Media,
    },
    /// A video clip.
    Video {
        /// The video payload.
        media: Media,
    },
    /// An arbitrary file attachment.
    File {
        /// The file payload.
        media: Media,
        /// Original filename, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl From<ResultBlock> for Content {
    fn from(block: ResultBlock) -> Self {
        match block {
            ResultBlock::Text { text } => Content::Text { text },
            ResultBlock::Image { media } => Content::Image { media },
            ResultBlock::Audio { media } => Content::Audio { media },
            ResultBlock::Video { media } => Content::Video { media },
            ResultBlock::File { media, name } => Content::File { media, name },
        }
    }
}

/// The narrowing is fallible, not lossy: a tool result never contains a
/// tool call or a nested result, so those two arms are `Invalid` — a
/// contract misuse the caller must hear about rather than a silent drop.
impl TryFrom<Content> for ResultBlock {
    type Error = crate::error::HostError;

    fn try_from(content: Content) -> Result<Self, Self::Error> {
        match content {
            Content::Text { text } => Ok(ResultBlock::Text { text }),
            Content::Image { media } => Ok(ResultBlock::Image { media }),
            Content::Audio { media } => Ok(ResultBlock::Audio { media }),
            Content::Video { media } => Ok(ResultBlock::Video { media }),
            Content::File { media, name } => Ok(ResultBlock::File { media, name }),
            Content::ToolCall { .. } | Content::ToolResult { .. } => Err(
                crate::error::HostError::invalid(
                    "a tool result block cannot be a tool call or a nested tool result",
                ),
            ),
        }
    }
}

/// One message in the conversation: a role plus ordered content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who produced it.
    pub role: Role,
    /// Ordered blocks: text, media, tool calls/results.
    pub content: Vec<Content>,
}

impl Message {
    /// Shorthand for a single-text-block user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![Content::Text { text: text.into() }],
        }
    }

    /// All tool calls in this message (assistant messages).
    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Json)> {
        self.content.iter().filter_map(|c| match c {
            Content::ToolCall {
                id,
                name,
                arguments,
            } => Some((id.as_str(), name.as_str(), arguments)),
            _ => None,
        })
    }

    /// Concatenated text blocks; media blocks are skipped.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

}

/// Text projection of tool-result blocks: text concatenated, media as
/// `[image: mime]`-style placeholders. Used for the ToolCallEnd event,
/// the after_tool probe payload, and anywhere a string is needed off
/// the wire.
pub fn tool_result_text(blocks: &[Content]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            Content::Text { text } => out.push_str(text),
            Content::Image { media } => {
                out.push_str(&format!("[image: {}]", media.media_type))
            }
            Content::Audio { media } => {
                out.push_str(&format!("[audio: {}]", media.media_type))
            }
            Content::Video { media } => {
                out.push_str(&format!("[video: {}]", media.media_type))
            }
            Content::File { media, name } => out.push_str(&format!(
                "[file: {}]",
                name.as_deref().unwrap_or(&media.media_type)
            )),
            Content::ToolCall { name, .. } => {
                out.push_str(&format!("[tool-call: {name}]"))
            }
            Content::ToolResult { call_id, .. } => {
                out.push_str(&format!("[tool-result: {call_id}]"))
            }
        }
    }
    out
}

impl Message {
    /// Media blocks (image/audio/video/file), in order.
    pub fn media(&self) -> impl Iterator<Item = &Media> {
        self.content.iter().filter_map(|c| match c {
            Content::Image { media }
            | Content::Audio { media }
            | Content::Video { media }
            | Content::File { media, .. } => Some(media),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_round_trips_through_session_json() {
        // In the model: bytes. On the wire/disk: base64. Both ways.
        let message = Message {
            role: Role::User,
            content: vec![Content::Image {
                media: Media::bytes("image/png", b"hello"),
            }],
        };
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains(r#""source":"base64""#));
        assert!(json.contains(r#""data":"aGVsbG8=""#));

        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(message, back);

        let url_message = Message {
            role: Role::User,
            content: vec![Content::Image {
                media: Media::url("image/png", "https://x.test/a.png"),
            }],
        };
        let back: Message =
            serde_json::from_str(&serde_json::to_string(&url_message).unwrap()).unwrap();
        assert_eq!(url_message, back);
    }

    #[test]
    fn legacy_string_tool_result_reads_as_text_block() {
        // Sessions written before 0.3.0 carry a tool result's content as
        // a plain string; reads upgrade it to one text block and writes
        // re-emit the block array (docs/tool-media.md).
        let json = r#"{"role":"tool","content":[{"type":"toolResult","call_id":"c1","content":"plain output","is_error":false}]}"#;
        let message: Message = serde_json::from_str(json).unwrap();
        assert_eq!(
            message.content[0],
            Content::ToolResult {
                call_id: "c1".into(),
                content: vec![Content::Text {
                    text: "plain output".into()
                }],
                is_error: false,
            }
        );
        let out = serde_json::to_string(&message).unwrap();
        assert!(out.contains(r#""content":[{"type":"text","text":"plain output"}]"#));
    }

    #[test]
    fn corrupt_base64_in_session_is_rejected() {
        let json = r#"{"role":"user","content":[{"type":"image","media":{"mediaType":"image/png","source":"base64","data":"!!!"}}]}"#;
        // mediaType camelCase: rename_all camelCase on Media struct? Media has
        // no rename_all, so the field is media_type. Fix the probe JSON:
        let json = json.replace("mediaType", "media_type");
        assert!(serde_json::from_str::<Message>(&json).is_err());
    }
}
