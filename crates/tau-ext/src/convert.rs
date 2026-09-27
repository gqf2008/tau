//! Typed ↔ serde conversion between the WIT message trunk
//! (`tau:extension/types`) and `tau_core::types`. The host side is the
//! only implementation of this mapping; guests see WIT types, the rest
//! of tau sees core types, and the round-trip tests pin the two
//! together (docs/host-channel.md 落地清单).
//!
//! Mapping notes:
//!
//! - `Content::Image/Audio/Video/File` collapse into one `media` variant
//!   on the wire; the MIME major type carries the semantics back. A
//!   `name` is file-only — naming an image/audio/video media is a
//!   validation error (校验即错误), not a silent drop.
//! - `MediaSource::Bytes` crosses as raw `list<u8>`. Base64 exists only
//!   at tau's JSON edges (session file, provider HTTP), never on the ABI.
//! - `tool-call.arguments-json` is the one JSON leaf: model-produced
//!   arbitrary JSON, parsed here so a malformed payload is an error at
//!   the boundary rather than a surprise deeper in.

use crate::bindings::tau::extension::types as wit;
use tau_core::types::{Content, Media, MediaSource, Message, Role};

/// Bound on the total payload a single host-channel message may carry:
/// sum of text bytes and inline media bytes. Blob/url references cost
/// their reference string only. 4 MiB keeps a rogue extension from
/// stuffing the control channel while leaving room for real media.
pub const MAX_HOST_MESSAGE_BYTES: usize = 4 << 20;

/// Errors the conversion layer can raise at the ABI boundary.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConvertError {
    /// A named media block whose MIME major is not application/file
    /// semantics (names belong to files only).
    #[error("media '{0}' has a name but is not a file (names are file-only)")]
    NamedNonFile(String),
    /// A tool-call's arguments-json did not parse.
    #[error("tool-call '{0}': arguments-json is not valid JSON")]
    BadArgumentsJson(String),
    /// The message exceeds [`MAX_HOST_MESSAGE_BYTES`].
    #[error("message payload {0} bytes exceeds the {MAX_HOST_MESSAGE_BYTES}-byte host-channel limit")]
    TooLarge(usize),
}

/// WIT → core for one content block.
pub fn content_to_core(content: wit::Content) -> Result<Content, ConvertError> {
    Ok(match content {
        wit::Content::Text(text) => Content::Text { text },
        wit::Content::Media(media) => media_to_core(media)?,
        wit::Content::ToolCall(call) => Content::ToolCall {
            id: call.id,
            name: call.name.clone(),
            arguments: serde_json::from_str(&call.arguments_json)
                .map_err(|_| ConvertError::BadArgumentsJson(call.name))?,
        },
        wit::Content::ToolResult(result) => Content::ToolResult {
            call_id: result.call_id,
            content: result.content,
            is_error: result.is_error,
        },
    })
}

/// Core → WIT for one content block. Total conversion: every core
/// variant has a wire form.
pub fn content_to_wit(content: &Content) -> wit::Content {
    match content {
        Content::Text { text } => wit::Content::Text(text.clone()),
        Content::Image { media } => wit::Content::Media(media_to_wit(media, None)),
        Content::Audio { media } => wit::Content::Media(media_to_wit(media, None)),
        Content::Video { media } => wit::Content::Media(media_to_wit(media, None)),
        Content::File { media, name } => wit::Content::Media(media_to_wit(media, name.clone())),
        Content::ToolCall {
            id,
            name,
            arguments,
        } => wit::Content::ToolCall(wit::ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments_json: arguments.to_string(),
        }),
        Content::ToolResult {
            call_id,
            content,
            is_error,
        } => wit::Content::ToolResult(wit::ToolResult {
            call_id: call_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        }),
    }
}

fn media_to_core(media: wit::Media) -> Result<Content, ConvertError> {
    let core_media = Media {
        media_type: media.media_type.clone(),
        source: source_to_core(media.source),
    };
    let major = media
        .media_type
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match major.as_str() {
        "image" | "audio" | "video" => {
            if media.name.is_some() {
                return Err(ConvertError::NamedNonFile(media.media_type));
            }
            Ok(match major.as_str() {
                "image" => Content::Image { media: core_media },
                "audio" => Content::Audio { media: core_media },
                _ => Content::Video { media: core_media },
            })
        }
        _ => Ok(Content::File {
            media: core_media,
            name: media.name,
        }),
    }
}

fn media_to_wit(media: &Media, name: Option<String>) -> wit::Media {
    wit::Media {
        media_type: media.media_type.clone(),
        source: source_to_wit(&media.source),
        name,
    }
}

fn source_to_core(source: wit::MediaSource) -> MediaSource {
    match source {
        wit::MediaSource::Bytes(bytes) => MediaSource::Bytes(bytes),
        wit::MediaSource::Url(url) => MediaSource::Url(url),
        wit::MediaSource::Blob(hash) => MediaSource::Blob { hash },
    }
}

fn source_to_wit(source: &MediaSource) -> wit::MediaSource {
    match source {
        MediaSource::Bytes(bytes) => wit::MediaSource::Bytes(bytes.clone()),
        MediaSource::Url(url) => wit::MediaSource::Url(url.clone()),
        MediaSource::Blob { hash } => wit::MediaSource::Blob(hash.clone()),
    }
}

/// WIT → core for a role.
pub fn role_to_core(role: wit::Role) -> Role {
    match role {
        wit::Role::User => Role::User,
        wit::Role::Assistant => Role::Assistant,
        wit::Role::Tool => Role::Tool,
    }
}

/// Core → WIT for a role.
pub fn role_to_wit(role: Role) -> wit::Role {
    match role {
        Role::User => wit::Role::User,
        Role::Assistant => wit::Role::Assistant,
        Role::Tool => wit::Role::Tool,
    }
}

/// WIT → core for a block list (host.notify payloads), enforcing the
/// size limit across the whole list.
pub fn contents_to_core(content: Vec<wit::Content>) -> Result<Vec<Content>, ConvertError> {
    let mut blocks = Vec::with_capacity(content.len());
    let mut bytes = 0usize;
    for block in content {
        bytes += block_bytes(&block);
        if bytes > MAX_HOST_MESSAGE_BYTES {
            return Err(ConvertError::TooLarge(bytes));
        }
        blocks.push(content_to_core(block)?);
    }
    Ok(blocks)
}

/// WIT → core for a whole message, enforcing the size limit.
pub fn message_to_core(message: wit::Message) -> Result<Message, ConvertError> {
    let role = role_to_core(message.role);
    let content = contents_to_core(message.content)?;
    Ok(Message { role, content })
}

/// Core → WIT for a whole message.
pub fn message_to_wit(message: &Message) -> wit::Message {
    wit::Message {
        role: role_to_wit(message.role),
        content: message.content.iter().map(content_to_wit).collect(),
    }
}

/// What a block costs against [`MAX_HOST_MESSAGE_BYTES`]: text bytes,
/// inline media bytes, and the reference strings for the rest.
fn block_bytes(content: &wit::Content) -> usize {
    match content {
        wit::Content::Text(text) => text.len(),
        wit::Content::Media(media) => {
            media.media_type.len()
                + media.name.as_ref().map_or(0, String::len)
                + match &media.source {
                    wit::MediaSource::Bytes(bytes) => bytes.len(),
                    wit::MediaSource::Url(url) => url.len(),
                    wit::MediaSource::Blob(hash) => hash.len(),
                }
        }
        wit::Content::ToolCall(call) => call.id.len() + call.name.len() + call.arguments_json.len(),
        wit::Content::ToolResult(result) => result.call_id.len() + result.content.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Vec<Content> {
        vec![
            Content::Text { text: String::new() },
            Content::Text {
                text: "héllo 你好 🦀\nwith\nlines".into(),
            },
            Content::Image {
                media: Media {
                    media_type: "image/png".into(),
                    source: MediaSource::Bytes(vec![0, 1, 2, 255, 254]),
                },
            },
            Content::Audio {
                media: Media {
                    media_type: "audio/pcm;rate=24000".into(),
                    source: MediaSource::Bytes(vec![0xAB; 1000]),
                },
            },
            Content::Video {
                media: Media {
                    media_type: "video/mp4".into(),
                    source: MediaSource::Url("https://example.com/v.mp4".into()),
                },
            },
            Content::File {
                media: Media {
                    media_type: "application/pdf".into(),
                    source: MediaSource::Blob {
                        hash: "sha256:deadbeef".into(),
                    },
                },
                name: Some("报告.pdf".into()),
            },
            Content::File {
                media: Media {
                    media_type: "application/zip".into(),
                    source: MediaSource::Bytes(vec![1, 2, 3]),
                },
                name: None,
            },
            Content::ToolCall {
                id: "call_1".into(),
                name: "upper".into(),
                arguments: serde_json::json!({"text": "hi", "n": 42, "nested": {"a": [1, 2]}}),
            },
            Content::ToolResult {
                call_id: "call_1".into(),
                content: "HI".into(),
                is_error: false,
            },
            Content::ToolResult {
                call_id: "call_2".into(),
                content: "boom".into(),
                is_error: true,
            },
        ]
    }

    #[test]
    fn content_round_trip_is_identity() {
        for block in corpus() {
            let wire = content_to_wit(&block);
            let back = content_to_core(wire).expect("corpus converts back");
            assert_eq!(back, block, "round trip changed the block");
        }
    }

    #[test]
    fn message_round_trip_is_identity() {
        let message = Message {
            role: Role::User,
            content: corpus(),
        };
        for role in [Role::User, Role::Assistant, Role::Tool] {
            let m = Message {
                role,
                content: message.content.clone(),
            };
            let back = message_to_core(message_to_wit(&m)).expect("message converts back");
            assert_eq!(back, m);
        }
    }

    #[test]
    fn named_non_file_media_is_rejected() {
        let block = wit::Content::Media(wit::Media {
            media_type: "image/png".into(),
            source: wit::MediaSource::Bytes(vec![1]),
            name: Some("sneaky.png".into()),
        });
        assert_eq!(
            content_to_core(block),
            Err(ConvertError::NamedNonFile("image/png".into()))
        );
    }

    #[test]
    fn bad_arguments_json_is_rejected() {
        let block = wit::Content::ToolCall(wit::ToolCall {
            id: "c".into(),
            name: "t".into(),
            arguments_json: "{ not json".into(),
        });
        assert_eq!(
            content_to_core(block),
            Err(ConvertError::BadArgumentsJson("t".into()))
        );
    }

    #[test]
    fn oversized_message_is_rejected() {
        let block = wit::Content::Text("x".repeat(MAX_HOST_MESSAGE_BYTES + 1));
        let message = wit::Message {
            role: wit::Role::User,
            content: vec![block],
        };
        assert!(matches!(
            message_to_core(message),
            Err(ConvertError::TooLarge(_))
        ));
    }

    #[test]
    fn media_major_match_is_case_insensitive() {
        let block = wit::Content::Media(wit::Media {
            media_type: "IMAGE/PNG".into(),
            source: wit::MediaSource::Bytes(vec![1]),
            name: None,
        });
        assert!(matches!(
            content_to_core(block),
            Ok(Content::Image { .. })
        ));
    }
}
