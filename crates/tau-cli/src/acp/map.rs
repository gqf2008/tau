//! ACP's vocabulary and tau's, and the mapping between them.
//!
//! Everything here is a pure function of its input — no store, no bus, no
//! connection — so the rules can be read and tested on their own and the
//! turn machinery is left with nothing but ordering and lifetimes.

use agent_client_protocol::schema::v1::{
    Content as AcpContent, ContentBlock, ContentChunk, SessionUpdate, StopReason as WireStop,
    TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use base64::Engine;
use tau_core::{Content, Media, Message, Role, StopReason};

/// Fold a prompt's blocks, in order, into the one user message tau's loop
/// takes.
///
/// A block tau cannot carry becomes a text placeholder rather than
/// vanishing, and each one is named in the returned notes so the caller
/// can put it on stderr: a client that sent an audio clip should be able
/// to find out why the model never heard it.
pub fn prompt_message(blocks: &[ContentBlock]) -> (Message, Vec<String>) {
    let mut content = Vec::new();
    let mut notes = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => content.push(Content::Text {
                text: text.text.clone(),
            }),
            ContentBlock::Image(image) => {
                match base64::engine::general_purpose::STANDARD.decode(&image.data) {
                    Ok(bytes) => content.push(Content::Image {
                        media: Media::bytes(image.mime_type.clone(), bytes),
                    }),
                    Err(error) => {
                        notes.push(format!("an image block was not valid base64: {error}"));
                        content.push(Content::Text {
                            text: "[image: undecodable base64]".to_string(),
                        });
                    }
                }
            }
            ContentBlock::ResourceLink(link) => content.push(Content::Text {
                text: format!("[resource: {}]", link.uri),
            }),
            other => {
                let name = unsupported(other);
                notes.push(format!("a {name} block was not carried"));
                content.push(Content::Text {
                    text: format!("[unsupported prompt block: {name}]"),
                });
            }
        }
    }
    (
        Message {
            role: Role::User,
            content,
        },
        notes,
    )
}

/// What to call a block tau does not carry. [`ContentBlock`] is
/// non-exhaustive, so anything a later protocol version adds lands here
/// too, as `unknown` — which is still better than a silent drop.
fn unsupported(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Audio(_) => "audio",
        ContentBlock::Resource(_) => "embedded resource",
        _ => "unknown",
    }
}

/// A fragment of the assistant's answer.
pub fn text_chunk(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text.to_string(),
    ))))
}

/// A tool call, announced, so the client can draw it before it finishes.
pub fn tool_call(id: &str, name: &str) -> SessionUpdate {
    SessionUpdate::ToolCall(
        ToolCall::new(ToolCallId::new(id.to_string()), name.to_string())
            .kind(tool_kind(name))
            .status(ToolCallStatus::InProgress)
            .name(name.to_string()),
    )
}

/// A tool call that has finished, carrying a preview of what it produced.
/// The whole output is in the session file; this is what rides the wire.
pub fn tool_result(id: &str, is_error: bool, output: &str) -> SessionUpdate {
    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::new(id.to_string()),
        ToolCallUpdateFields::new()
            .status(status)
            .content(vec![ToolCallContent::Content(AcpContent::new(
                ContentBlock::Text(TextContent::new(crate::repl::compact_preview(output))),
            ))]),
    ))
}

/// ACP's idea of what a tool does, from tau's name for it. The built-ins
/// map to what they are; everything else — an extension's tool, whose
/// behaviour tau cannot know — is `Other`, because a wrong kind is worse
/// than none: an editor renders a search as a file edit if told to.
pub fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" | "ls" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "grep" | "find" => ToolKind::Search,
        "bash" | "powershell" => ToolKind::Execute,
        _ => ToolKind::Other,
    }
}

/// tau's stop → the protocol's.
///
/// Two arms are unreachable through the turn, which checks the run's own
/// `Result` first: `Error` arrives as `RunError` and is answered with a
/// JSON-RPC error, and `ToolUse` is not a stop the loop can end on — it
/// continues after the tool results. Both are mapped rather than
/// panicked, so the table stays total.
pub fn stop_reason(stop: StopReason) -> WireStop {
    match stop {
        StopReason::Stop => WireStop::EndTurn,
        StopReason::Length => WireStop::MaxTokens,
        StopReason::Aborted => WireStop::Cancelled,
        StopReason::Error => WireStop::Refusal,
        StopReason::ToolUse => WireStop::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ImageContent, ResourceLink};

    fn text_of(message: &Message) -> Vec<String> {
        message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_prompt_folds_block_by_block_in_order() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("look at")),
            ContentBlock::Image(ImageContent::new("aGk=", "image/png")),
            ContentBlock::Text(TextContent::new("this")),
        ];
        let (message, notes) = prompt_message(&blocks);
        assert_eq!(message.role, Role::User);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(message.content.len(), 3);
        assert_eq!(text_of(&message), vec!["look at", "this"]);
        match &message.content[1] {
            Content::Image { media } => {
                assert_eq!(media.media_type, "image/png");
                // The wire carries base64; the message carries the bytes.
                assert_eq!(media.source, tau_core::MediaSource::Bytes(b"hi".to_vec()));
            }
            other => panic!("expected the image block, got {other:?}"),
        }
    }

    #[test]
    fn blocks_tau_cannot_carry_are_placed_and_named() {
        let blocks = vec![
            ContentBlock::ResourceLink(ResourceLink::new("notes", "file:///notes.md")),
            ContentBlock::Audio(agent_client_protocol::schema::v1::AudioContent::new(
                "aGk=",
                "audio/wav",
            )),
        ];
        let (message, notes) = prompt_message(&blocks);
        let texts = text_of(&message);
        assert_eq!(
            texts,
            vec![
                "[resource: file:///notes.md]".to_string(),
                "[unsupported prompt block: audio]".to_string(),
            ],
            "the placeholder is where the block was, and says what it was"
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("audio"), "{notes:?}");
    }

    #[test]
    fn an_image_that_is_not_base64_is_reported_and_placed() {
        let blocks = vec![ContentBlock::Image(ImageContent::new(
            "not base64!",
            "image/png",
        ))];
        let (message, notes) = prompt_message(&blocks);
        assert_eq!(text_of(&message), vec!["[image: undecodable base64]"]);
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn every_stop_has_an_answer() {
        assert_eq!(stop_reason(StopReason::Stop), WireStop::EndTurn);
        assert_eq!(stop_reason(StopReason::Length), WireStop::MaxTokens);
        assert_eq!(stop_reason(StopReason::Aborted), WireStop::Cancelled);
        // Not reachable through a turn (see the function), but mapped.
        assert_eq!(stop_reason(StopReason::Error), WireStop::Refusal);
        assert_eq!(stop_reason(StopReason::ToolUse), WireStop::EndTurn);
    }

    #[test]
    fn built_ins_get_their_kind_and_strangers_do_not_guess() {
        assert_eq!(tool_kind("read"), ToolKind::Read);
        assert_eq!(tool_kind("ls"), ToolKind::Read);
        assert_eq!(tool_kind("write"), ToolKind::Edit);
        assert_eq!(tool_kind("edit"), ToolKind::Edit);
        assert_eq!(tool_kind("grep"), ToolKind::Search);
        assert_eq!(tool_kind("find"), ToolKind::Search);
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("powershell"), ToolKind::Execute);
        // An extension's tool: tau has no idea what it does.
        assert_eq!(tool_kind("upper"), ToolKind::Other);
    }

    #[test]
    fn a_tool_result_carries_a_flattened_preview() {
        let update = tool_result("c1", false, "line one\n\n  line two");
        let SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected a tool call update");
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
        let content = update.fields.content.expect("a preview");
        let ToolCallContent::Content(content) = &content[0] else {
            panic!("expected a content block");
        };
        let ContentBlock::Text(text) = &content.content else {
            panic!("expected text");
        };
        assert_eq!(text.text, "line one line two");
    }
}
