//! Typed ↔ core conversion between the WIT message trunk
//! (`tau:extension/types`) and `tau_core::types`, and between the probe
//! payloads (`interface probes`) and `tau_core::probe_payload`.
//!
//! 0.7.0 made both trunks typed, so this module no longer infers
//! anything: a guest says which arm it means, and the arm is the intent.
//! The MIME-major inference and its "naming an image is a validation
//! error" rule went away with the four-arm `media` record — `image`,
//! `audio`, `video` and `file` are separate arms now, and only `file`
//! carries a name. JSON survives at exactly one leaf: model-produced tool
//! arguments, parsed here so a malformed payload is an error at the
//! boundary rather than a surprise deeper in.
//!
//! The host side is the only implementation of this mapping; guests see
//! WIT types, the rest of tau sees core types, and the round-trip tests
//! pin the two together (docs/host-channel.md 落地清单).

use serde_json::Value as Json;

use crate::bindings::tau::extension::types as wit;
use crate::bindings::exports::tau::extension::{probes as wit_probes, tools as wit_tools};
use tau_core::model::StopReason;
use tau_core::probe::ProbePoint;
use tau_core::probe_payload::{
    AssembledContext, AssembledResponse, BeforeRun, Branch, Compaction, FinalRequest, Navigation,
    ProbePayload, RunEnd, SessionFacts, ToolOutcome,
};
use tau_core::tool::ToolDef;
use tau_core::types::{Content, Media, MediaSource, Message, ResultBlock, Role, ToolCall};

/// Bound on the total payload a single host-channel message may carry:
/// sum of text bytes and inline media bytes. Blob/url references cost
/// their reference string only. 4 MiB keeps a rogue extension from
/// stuffing the control channel while leaving room for real media.
pub const MAX_HOST_MESSAGE_BYTES: usize = 4 << 20;

/// Errors the conversion layer can raise at the ABI boundary.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConvertError {
    /// A tool-call's arguments-json did not parse.
    #[error("tool-call '{0}': arguments-json is not valid JSON")]
    BadArgumentsJson(String),
    /// A probe's `final-request` table carried a tool whose
    /// parameters-json did not parse. The same strictness as load time: an
    /// open schema would let the model free-wheel arguments past a broken
    /// contract.
    #[error("probe tool '{0}': parameters-json is not valid JSON")]
    BadToolSchema(String),
    /// The message exceeds [`MAX_HOST_MESSAGE_BYTES`].
    #[error("message payload {0} bytes exceeds the {MAX_HOST_MESSAGE_BYTES}-byte host-channel limit")]
    TooLarge(usize),
    /// A probe answered with a payload belonging to another point. The
    /// contract makes the pairing the host's to validate (`invalid`); the
    /// adapters degrade it to `continue` and say so on stderr, because a
    /// broken extension must not wedge the run.
    #[error("probe answered the {point:?} point with a {got:?} payload")]
    WrongPayload {
        /// The point that was probed.
        point: ProbePoint,
        /// The point the returned payload belongs to.
        got: ProbePoint,
    },
}

// ---------------------------------------------------------------------------
// Message trunk
// ---------------------------------------------------------------------------

/// WIT → core for one content block.
pub fn content_to_core(content: wit::Content) -> Result<Content, ConvertError> {
    Ok(match content {
        wit::Content::Text(text) => Content::Text { text },
        wit::Content::Image(media) => Content::Image {
            media: media_to_core(media),
        },
        wit::Content::Audio(media) => Content::Audio {
            media: media_to_core(media),
        },
        wit::Content::Video(media) => Content::Video {
            media: media_to_core(media),
        },
        wit::Content::File(file) => {
            let (media, name) = file_to_core(file);
            Content::File { media, name }
        }
        wit::Content::ToolCall(call) => Content::ToolCall {
            id: call.id,
            name: call.name.clone(),
            arguments: parse_arguments(&call.name, &call.arguments_json)?,
        },
        wit::Content::ToolResult(result) => Content::ToolResult {
            call_id: result.call_id,
            content: result_blocks_to_core(result.content)?
                .into_iter()
                .map(Content::from)
                .collect(),
            is_error: result.is_error,
        },
    })
}

/// Core → WIT for one content block. Total conversion: every core variant
/// has a wire form.
pub fn content_to_wit(content: &Content) -> wit::Content {
    match content {
        Content::Text { text } => wit::Content::Text(text.clone()),
        Content::Image { media } => wit::Content::Image(media_to_wit(media)),
        Content::Audio { media } => wit::Content::Audio(media_to_wit(media)),
        Content::Video { media } => wit::Content::Video(media_to_wit(media)),
        Content::File { media, name } => wit::Content::File(wit::File {
            media: media_to_wit(media),
            name: name.clone(),
        }),
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
            content: content.iter().map(content_block_to_result_block).collect(),
            is_error: *is_error,
        }),
    }
}

/// The wire's `result-block` is deliberately narrower than `content` (the
/// toolchain refuses a recursive `content`; see the contract's comment on
/// `result-block`), so a nested call or result degrades to its text
/// projection — the posture the JSON carrier had before 0.7.0.
fn content_block_to_result_block(block: &Content) -> wit::ResultBlock {
    match ResultBlock::try_from(block.clone()) {
        Ok(block) => result_block_to_wit(&block),
        Err(_) => wit::ResultBlock::Text(tau_core::types::tool_result_text(std::slice::from_ref(
            block,
        ))),
    }
}

/// WIT → core for one result block (the non-recursive half of `content`).
pub fn result_block_to_core(block: wit::ResultBlock) -> ResultBlock {
    match block {
        wit::ResultBlock::Text(text) => ResultBlock::Text { text },
        wit::ResultBlock::Image(media) => ResultBlock::Image {
            media: media_to_core(media),
        },
        wit::ResultBlock::Audio(media) => ResultBlock::Audio {
            media: media_to_core(media),
        },
        wit::ResultBlock::Video(media) => ResultBlock::Video {
            media: media_to_core(media),
        },
        wit::ResultBlock::File(file) => {
            let (media, name) = file_to_core(file);
            ResultBlock::File { media, name }
        }
    }
}

/// Core → WIT for one result block.
pub fn result_block_to_wit(block: &ResultBlock) -> wit::ResultBlock {
    match block {
        ResultBlock::Text { text } => wit::ResultBlock::Text(text.clone()),
        ResultBlock::Image { media } => wit::ResultBlock::Image(media_to_wit(media)),
        ResultBlock::Audio { media } => wit::ResultBlock::Audio(media_to_wit(media)),
        ResultBlock::Video { media } => wit::ResultBlock::Video(media_to_wit(media)),
        ResultBlock::File { media, name } => wit::ResultBlock::File(wit::File {
            media: media_to_wit(media),
            name: name.clone(),
        }),
    }
}

/// Convert a guest tool result's blocks (0.3.0 multi-block contract,
/// docs/tool-media.md), enforcing the same total size cap as the host
/// channel: oversize fails closed, it never silently truncates.
pub fn result_blocks_to_core(blocks: Vec<wit::ResultBlock>) -> Result<Vec<ResultBlock>, ConvertError> {
    let mut total = 0usize;
    for block in &blocks {
        total += result_block_bytes(block);
        if total > MAX_HOST_MESSAGE_BYTES {
            return Err(ConvertError::TooLarge(total));
        }
    }
    Ok(blocks.into_iter().map(result_block_to_core).collect())
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

/// WIT → core for a stop reason.
pub fn stop_to_core(stop: wit::StopReason) -> StopReason {
    match stop {
        wit::StopReason::Stop => StopReason::Stop,
        wit::StopReason::ToolUse => StopReason::ToolUse,
        wit::StopReason::Length => StopReason::Length,
        wit::StopReason::Error => StopReason::Error,
        wit::StopReason::Aborted => StopReason::Aborted,
    }
}

/// Core → WIT for a stop reason.
pub fn stop_to_wit(stop: StopReason) -> wit::StopReason {
    match stop {
        StopReason::Stop => wit::StopReason::Stop,
        StopReason::ToolUse => wit::StopReason::ToolUse,
        StopReason::Length => wit::StopReason::Length,
        StopReason::Error => wit::StopReason::Error,
        StopReason::Aborted => wit::StopReason::Aborted,
    }
}

/// WIT → core for a tool call.
pub fn tool_call_to_core(call: wit::ToolCall) -> Result<ToolCall, ConvertError> {
    Ok(ToolCall {
        id: call.id,
        name: call.name.clone(),
        arguments: parse_arguments(&call.name, &call.arguments_json)?,
    })
}

/// Core → WIT for a tool call.
pub fn tool_call_to_wit(call: &ToolCall) -> wit::ToolCall {
    wit::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments_json: call.arguments.to_string(),
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

// ---------------------------------------------------------------------------
// Probe payloads
// ---------------------------------------------------------------------------

/// WIT → core for a probe point. Total: the contract's `point` enum is
/// the host's point set, arm for arm.
pub fn point_to_core(point: wit_probes::Point) -> ProbePoint {
    match point {
        wit_probes::Point::BeforeRun => ProbePoint::BeforeRun,
        wit_probes::Point::TransformContext => ProbePoint::TransformContext,
        wit_probes::Point::BeforeRequest => ProbePoint::BeforeRequest,
        wit_probes::Point::AfterResponse => ProbePoint::AfterResponse,
        wit_probes::Point::BeforeTool => ProbePoint::BeforeTool,
        wit_probes::Point::AfterTool => ProbePoint::AfterTool,
        wit_probes::Point::BeforeRunEnd => ProbePoint::BeforeRunEnd,
        wit_probes::Point::BeforeCompaction => ProbePoint::BeforeCompaction,
        wit_probes::Point::BeforeNavigation => ProbePoint::BeforeNavigation,
        wit_probes::Point::SessionStart => ProbePoint::SessionStart,
        wit_probes::Point::Branch => ProbePoint::Branch,
        wit_probes::Point::SessionEnd => ProbePoint::SessionEnd,
    }
}

/// Core → WIT for a probe point.
pub fn point_to_wit(point: ProbePoint) -> wit_probes::Point {
    match point {
        ProbePoint::BeforeRun => wit_probes::Point::BeforeRun,
        ProbePoint::TransformContext => wit_probes::Point::TransformContext,
        ProbePoint::BeforeRequest => wit_probes::Point::BeforeRequest,
        ProbePoint::AfterResponse => wit_probes::Point::AfterResponse,
        ProbePoint::BeforeTool => wit_probes::Point::BeforeTool,
        ProbePoint::AfterTool => wit_probes::Point::AfterTool,
        ProbePoint::BeforeRunEnd => wit_probes::Point::BeforeRunEnd,
        ProbePoint::BeforeCompaction => wit_probes::Point::BeforeCompaction,
        ProbePoint::BeforeNavigation => wit_probes::Point::BeforeNavigation,
        ProbePoint::SessionStart => wit_probes::Point::SessionStart,
        ProbePoint::Branch => wit_probes::Point::Branch,
        ProbePoint::SessionEnd => wit_probes::Point::SessionEnd,
    }
}

/// Core → WIT for a probe firing's payload: one arm per point, the same
/// arm the point carries.
pub fn payload_to_wit(payload: &ProbePayload) -> wit_probes::Payload {
    match payload {
        ProbePayload::BeforeRun(p) => {
            wit_probes::Payload::BeforeRun(message_to_wit(&p.prompt))
        }
        ProbePayload::TransformContext(p) => {
            wit_probes::Payload::TransformContext(wit_probes::AssembledContext {
                system: p.system.clone(),
                messages: p.messages.iter().map(message_to_wit).collect(),
            })
        }
        ProbePayload::BeforeRequest(p) => wit_probes::Payload::BeforeRequest(wit_probes::FinalRequest {
            system: p.system.clone(),
            messages: p.messages.iter().map(message_to_wit).collect(),
            tools: p.tools.iter().map(definition_to_wit).collect(),
        }),
        ProbePayload::AfterResponse(p) => {
            wit_probes::Payload::AfterResponse(wit_probes::AssembledResponse {
                message: message_to_wit(&p.message),
                stop: stop_to_wit(p.stop),
            })
        }
        ProbePayload::BeforeTool(call) => wit_probes::Payload::BeforeTool(tool_call_to_wit(call)),
        ProbePayload::AfterTool(p) => wit_probes::Payload::AfterTool(wit_probes::ToolOutcome {
            call: tool_call_to_wit(&p.call),
            content: p.content.iter().map(result_block_to_wit).collect(),
            is_error: p.is_error,
        }),
        ProbePayload::BeforeRunEnd(p) => wit_probes::Payload::BeforeRunEnd(wit_probes::RunEnd {
            messages: p.messages.iter().map(message_to_wit).collect(),
            stop: stop_to_wit(p.stop),
        }),
        ProbePayload::BeforeCompaction(p) => {
            wit_probes::Payload::BeforeCompaction(wit_probes::Compaction {
                reason: p.reason.clone(),
                messages: p.messages.iter().map(message_to_wit).collect(),
            })
        }
        ProbePayload::BeforeNavigation(p) => {
            wit_probes::Payload::BeforeNavigation(wit_probes::Navigation {
                target: p.target.clone(),
                summary: p.summary.clone(),
            })
        }
        ProbePayload::SessionStart(p) => {
            wit_probes::Payload::SessionStart(session_facts_to_wit(p))
        }
        ProbePayload::Branch(p) => wit_probes::Payload::Branch(wit_probes::Branch {
            previous: p.previous.clone(),
            to: p.to.clone(),
        }),
        ProbePayload::SessionEnd(p) => wit_probes::Payload::SessionEnd(session_facts_to_wit(p)),
    }
}

/// WIT → core for a probe answer, validating the pairing: a `before-tool`
/// probe answering with a `navigation` payload is `WrongPayload`, which
/// the adapters report and ignore (a broken extension must not wedge the
/// run).
pub fn payload_from_point(
    point: ProbePoint,
    payload: wit_probes::Payload,
) -> Result<ProbePayload, ConvertError> {
    let core = match payload {
        wit_probes::Payload::BeforeRun(message) => ProbePayload::BeforeRun(BeforeRun {
            prompt: message_to_core(message)?,
        }),
        wit_probes::Payload::TransformContext(p) => {
            ProbePayload::TransformContext(AssembledContext {
                system: p.system,
                messages: messages_to_core(p.messages)?,
            })
        }
        wit_probes::Payload::BeforeRequest(p) => ProbePayload::BeforeRequest(FinalRequest {
            system: p.system,
            messages: messages_to_core(p.messages)?,
            tools: p.tools.into_iter().map(definition_to_core).collect::<Result<Vec<_>, _>>()?,
        }),
        wit_probes::Payload::AfterResponse(p) => {
            ProbePayload::AfterResponse(AssembledResponse {
                message: message_to_core(p.message)?,
                stop: stop_to_core(p.stop),
            })
        }
        wit_probes::Payload::BeforeTool(call) => ProbePayload::BeforeTool(tool_call_to_core(call)?),
        wit_probes::Payload::AfterTool(p) => ProbePayload::AfterTool(ToolOutcome {
            call: tool_call_to_core(p.call)?,
            content: result_blocks_to_core(p.content)?,
            is_error: p.is_error,
        }),
        wit_probes::Payload::BeforeRunEnd(p) => ProbePayload::BeforeRunEnd(RunEnd {
            messages: messages_to_core(p.messages)?,
            stop: stop_to_core(p.stop),
        }),
        wit_probes::Payload::BeforeCompaction(p) => {
            ProbePayload::BeforeCompaction(Compaction {
                reason: p.reason,
                messages: messages_to_core(p.messages)?,
            })
        }
        wit_probes::Payload::BeforeNavigation(p) => {
            ProbePayload::BeforeNavigation(Navigation {
                target: p.target,
                summary: p.summary,
            })
        }
        wit_probes::Payload::SessionStart(p) => ProbePayload::SessionStart(session_facts_to_core(p)),
        wit_probes::Payload::Branch(p) => ProbePayload::Branch(Branch {
            previous: p.previous,
            to: p.to,
        }),
        wit_probes::Payload::SessionEnd(p) => ProbePayload::SessionEnd(session_facts_to_core(p)),
    };
    if core.point() != point {
        return Err(ConvertError::WrongPayload {
            point,
            got: core.point(),
        });
    }
    Ok(core)
}

fn messages_to_core(messages: Vec<wit::Message>) -> Result<Vec<Message>, ConvertError> {
    messages.into_iter().map(message_to_core).collect()
}

fn session_facts_to_wit(p: &SessionFacts) -> wit_probes::SessionFacts {
    wit_probes::SessionFacts {
        session: p.session.clone(),
        cwd: p.cwd.clone(),
        model: p.model.clone(),
    }
}

fn session_facts_to_core(p: wit_probes::SessionFacts) -> SessionFacts {
    SessionFacts {
        session: p.session,
        cwd: p.cwd,
        model: p.model,
    }
}

/// Core → WIT for a tool definition (the `before_request` tool table).
pub fn definition_to_wit(def: &ToolDef) -> wit_tools::Definition {
    wit_tools::Definition {
        name: def.name.clone(),
        description: def.description.clone(),
        parameters_json: def.parameters.to_string(),
    }
}

/// WIT → core for a tool definition. The JSON Schema leaf is parsed by
/// [`crate::tool_def_strict`] -- the strictness of load time, so a probe
/// payload can never hand the model an open schema.
pub fn definition_to_core(def: wit_tools::Definition) -> Result<ToolDef, ConvertError> {
    let name = def.name;
    crate::tool_def_strict(name.clone(), def.description, &def.parameters_json)
        .map_err(|_| ConvertError::BadToolSchema(name))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn parse_arguments(name: &str, arguments_json: &str) -> Result<Json, ConvertError> {
    serde_json::from_str(arguments_json)
        .map_err(|_| ConvertError::BadArgumentsJson(name.to_string()))
}

fn media_to_core(media: wit::Media) -> Media {
    Media {
        media_type: media.media_type,
        source: source_to_core(media.source),
    }
}

fn media_to_wit(media: &Media) -> wit::Media {
    wit::Media {
        media_type: media.media_type.clone(),
        source: source_to_wit(&media.source),
    }
}

fn file_to_core(file: wit::File) -> (Media, Option<String>) {
    (media_to_core(file.media), file.name)
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

/// What a block costs against [`MAX_HOST_MESSAGE_BYTES`]: text bytes,
/// inline media bytes, and the reference strings for the rest.
fn result_block_bytes(block: &wit::ResultBlock) -> usize {
    match block {
        wit::ResultBlock::Text(text) => text.len(),
        wit::ResultBlock::Image(media)
        | wit::ResultBlock::Audio(media)
        | wit::ResultBlock::Video(media) => media_bytes(media),
        wit::ResultBlock::File(file) => {
            media_bytes(&file.media) + file.name.as_ref().map_or(0, String::len)
        }
    }
}

fn media_bytes(media: &wit::Media) -> usize {
    media.media_type.len()
        + match &media.source {
            wit::MediaSource::Bytes(bytes) => bytes.len(),
            wit::MediaSource::Url(url) => url.len(),
            wit::MediaSource::Blob(hash) => hash.len(),
        }
}

fn block_bytes(content: &wit::Content) -> usize {
    match content {
        wit::Content::Text(text) => text.len(),
        wit::Content::Image(media)
        | wit::Content::Audio(media)
        | wit::Content::Video(media) => media_bytes(media),
        wit::Content::File(file) => {
            media_bytes(&file.media) + file.name.as_ref().map_or(0, String::len)
        }
        wit::Content::ToolCall(call) => call.id.len() + call.name.len() + call.arguments_json.len(),
        wit::Content::ToolResult(result) => {
            result.call_id.len() + result.content.iter().map(result_block_bytes).sum::<usize>()
        }
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
                content: vec![tau_core::Content::Text { text: "HI".into() }],
                is_error: false,
            },
            Content::ToolResult {
                call_id: "call_2".into(),
                content: vec![tau_core::Content::Text { text: "boom".into() }],
                is_error: true,
            },
            // 0.3.0 (docs/tool-media.md): a tool result carrying real
            // media — bytes, url, and blob sources must all round-trip.
            Content::ToolResult {
                call_id: "call_3".into(),
                content: vec![
                    tau_core::Content::Text {
                        text: "the dot: ".into(),
                    },
                    tau_core::Content::Image {
                        media: Media::bytes("image/png", b"\x89PNG"),
                    },
                    tau_core::Content::Video {
                        media: Media::url("video/mp4", "https://example.com/v.mp4"),
                    },
                    tau_core::Content::File {
                        media: Media {
                            media_type: "application/pdf".into(),
                            source: MediaSource::Blob {
                                hash: "sha256:deadbeef".into(),
                            },
                        },
                        name: Some("out.pdf".into()),
                    },
                ],
                is_error: false,
            },
            // The narrowing the wire forces: a tool result that carries a
            // nested call/result degrades to its text projection.
            Content::ToolResult {
                call_id: "call_4".into(),
                content: vec![tau_core::Content::ToolCall {
                    id: "inner".into(),
                    name: "t".into(),
                    arguments: serde_json::json!({}),
                }],
                is_error: false,
            },
        ]
    }

    #[test]
    fn content_round_trip_is_identity_or_the_documented_narrowing() {
        for block in corpus() {
            let wire = content_to_wit(&block);
            let back = content_to_core(wire).expect("corpus converts back");
            if let Content::ToolResult { content, .. } = &block
                && content
                    .iter()
                    .any(|c| matches!(c, Content::ToolCall { .. } | Content::ToolResult { .. }))
            {
                assert_ne!(back, block, "the narrowing must be visible, not silent");
                continue;
            }
            assert_eq!(back, block, "round trip changed the block");
        }
    }

    /// The wire's `result-block` is narrower than `content`, so a nested
    /// call inside a result has no wire form of its own and degrades to its
    /// text projection (`tau_core::types::tool_result_text`). Generated WIT
    /// types have no `PartialEq` (wasmtime's bindgen derives it only for
    /// enums), so the arms are read out one by one.
    #[test]
    fn a_nested_tool_call_in_a_result_degrades_to_text() {
        let block = Content::ToolResult {
            call_id: "c".into(),
            content: vec![Content::ToolCall {
                id: "call-1".into(),
                name: "t".into(),
                arguments: serde_json::json!({}),
            }],
            is_error: false,
        };
        let wit::Content::ToolResult(result) = content_to_wit(&block) else {
            panic!("a tool result stays a tool result");
        };
        assert_eq!(result.call_id, "c");
        assert!(!result.is_error);
        let [wit::ResultBlock::Text(text)] = result.content.as_slice() else {
            panic!("the nested call degrades to exactly one text block");
        };
        assert_eq!(text, "[tool-call: t]");
    }

    #[test]
    fn oversize_tool_result_fails_closed() {
        // The host-channel cap applies to tool-result blocks too: one
        // giant block or a total over the limit both fail closed.
        let huge = vec![wit::ResultBlock::Text("x".repeat(MAX_HOST_MESSAGE_BYTES + 1))];
        assert!(matches!(
            result_blocks_to_core(huge),
            Err(ConvertError::TooLarge(_))
        ));
        let half = "x".repeat(MAX_HOST_MESSAGE_BYTES / 2 + 1);
        let blocks = vec![
            wit::ResultBlock::Text(half.clone()),
            wit::ResultBlock::Text(half),
        ];
        assert!(matches!(
            result_blocks_to_core(blocks),
            Err(ConvertError::TooLarge(_))
        ));
    }

    #[test]
    fn message_round_trip_is_identity() {
        let content: Vec<Content> = corpus()
            .into_iter()
            .filter(|block| {
                !matches!(block, Content::ToolResult { content, .. } if content.iter().any(|c| matches!(c, Content::ToolCall { .. } | Content::ToolResult { .. })))
            })
            .collect();
        for role in [Role::User, Role::Assistant, Role::Tool] {
            let m = Message {
                role,
                content: content.clone(),
            };
            let back = message_to_core(message_to_wit(&m)).expect("message converts back");
            assert_eq!(back, m);
        }
    }

    #[test]
    fn a_named_image_is_no_longer_an_error() {
        // 0.7.0: the arm is the intent, so `file` is where a name lives
        // and an image arm simply has no name field. The old rule ("a
        // name means it is really a file") died with the four-arm media.
        let file = wit::Content::File(wit::File {
            media: wit::Media {
                media_type: "image/png".into(),
                source: wit::MediaSource::Bytes(vec![1]),
            },
            name: Some("sneaky.png".into()),
        });
        assert_eq!(
            content_to_core(file).expect("converts"),
            Content::File {
                media: Media {
                    media_type: "image/png".into(),
                    source: MediaSource::Bytes(vec![1]),
                },
                name: Some("sneaky.png".into()),
            }
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

    /// Every point's payload makes the round trip intact, and the point
    /// itself is what the payload says it is.
    #[test]
    fn every_payload_arm_round_trips() {
        for payload in payload_corpus() {
            let point = payload.point();
            let wire = payload_to_wit(&payload);
            let back = payload_from_point(point, wire).expect("payload converts back");
            assert_eq!(back.point(), point);
            assert_eq!(back.to_json(), payload.to_json(), "arm payload changed");
        }
    }

    #[test]
    fn a_payload_answering_the_wrong_point_is_refused() {
        let wire = payload_to_wit(&payload_corpus()[0]); // before_run
        let err = payload_from_point(ProbePoint::BeforeTool, wire).expect_err("mismatch");
        assert_eq!(
            err,
            ConvertError::WrongPayload {
                point: ProbePoint::BeforeTool,
                got: ProbePoint::BeforeRun,
            }
        );
    }

    fn payload_corpus() -> Vec<ProbePayload> {
        let message = Message {
            role: Role::User,
            content: vec![Content::Text { text: "hi".into() }],
        };
        let call = ToolCall {
            id: "c1".into(),
            name: "upper".into(),
            arguments: serde_json::json!({"text": "hi"}),
        };
        let facts = SessionFacts {
            session: "s1".into(),
            cwd: "/w".into(),
            model: "m".into(),
        };
        vec![
            ProbePayload::BeforeRun(BeforeRun {
                prompt: message.clone(),
            }),
            ProbePayload::TransformContext(AssembledContext {
                system: Some("sys".into()),
                messages: vec![message.clone()],
            }),
            ProbePayload::BeforeRequest(FinalRequest {
                system: None,
                messages: vec![message.clone()],
                tools: vec![ToolDef {
                    name: "upper".into(),
                    description: "shout".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }],
            }),
            ProbePayload::AfterResponse(AssembledResponse {
                message: message.clone(),
                stop: StopReason::ToolUse,
            }),
            ProbePayload::BeforeTool(call.clone()),
            ProbePayload::AfterTool(ToolOutcome {
                call: call.clone(),
                content: vec![ResultBlock::Text { text: "HI".into() }],
                is_error: false,
            }),
            ProbePayload::BeforeRunEnd(RunEnd {
                messages: vec![message.clone()],
                stop: StopReason::Stop,
            }),
            ProbePayload::BeforeCompaction(Compaction {
                reason: "manual".into(),
                messages: vec![message.clone()],
            }),
            ProbePayload::BeforeNavigation(Navigation {
                target: "e1".into(),
                summary: "first".into(),
            }),
            ProbePayload::SessionStart(facts.clone()),
            ProbePayload::Branch(Branch {
                previous: Some("e0".into()),
                to: "e1".into(),
            }),
            ProbePayload::SessionEnd(facts),
        ]
    }
}
