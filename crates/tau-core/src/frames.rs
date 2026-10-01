//! Durable run progress ("frames"): every streamed delta and landed
//! tool result of an in-flight run, written OUTSIDE the session tree so
//! a crash mid-run salvages what committed instead of losing the turn.
//!
//! The design is pi's (`harness/runtime/progress.ts` + `recovery.ts`;
//! bigfish1913/pi-rust ported it to Rust). tau's twist: frames ride a
//! SIDECAR (`<session>.frames.jsonl`), never the session file — a new
//! line type inside the session would read as corrupt to pre-recovery
//! binaries while the 0.7.x format is frozen (the same constraint that
//! put `/name` in the file name, `docs/repl.md`).
//!
//! Lifecycle: the harness points the shared [`FrameTarget`] at the
//! sidecar when a run starts (truncating), the agent's frame sink
//! appends one JSON line per frame, and a clean run retires the file
//! once its messages are session entries (ClearRun). A file that
//! survives means the run died: [`salvage`] rebuilds the committed
//! prefix as real entries — the partial assistant message with
//! [`INTERRUPTED_NOTICE`] attached, landed tool results kept, results
//! that never landed replaced by [`UNKNOWN_TOOL_OUTCOME`] — and removes
//! the file. Salvage is idempotent: the notice is the marker.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::session::{EntryKind, JsonlStore, SessionEntry, SessionError, new_id};
use crate::types::{Content, Message, Role};

/// Attached to the salvaged assistant message; verbatim from pi so a
/// reader (or a tool) recognises the case.
pub const INTERRUPTED_NOTICE: &str = "Assistant request was interrupted. The preceding content is \
the latest committed partial; newer live output may be missing and the external outcome is unknown.";

/// Synthetic result for a call whose outcome the dead run never
/// recorded — say plainly that the effect is unknown, so neither the
/// user nor the model assumes the tool did nothing.
pub const UNKNOWN_TOOL_OUTCOME: &str = "Tool execution was interrupted before its result was \
recorded. The external outcome is unknown: this tool may or may not have taken effect.";

/// One durable progress record. Audio deltas are deliberately not
/// framed (bytes-heavy; a salvaged partial is text plus tool activity).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Frame {
    /// A new model turn within the run (salvage groups by turn, so a
    /// multi-turn death rebuilds one assistant message per turn).
    TurnStart,
    /// Streamed assistant text.
    TextDelta {
        /// The streamed piece.
        text: String,
    },
    /// One streamed piece of an assistant tool call, assembled by
    /// index — the same reduce as the agent loop's own stream
    /// reassembly.
    ToolCallDelta {
        /// Which call in the turn (provider-assigned stream index).
        index: u32,
        /// Provider-assigned call id, when this piece carried it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Tool name, when this piece carried it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Fragment of the JSON arguments.
        arguments_delta: String,
    },
    /// A tool result that landed (text projection; media does not
    /// survive salvage — the interrupted notice covers the loss).
    ToolResult {
        /// The call this answers.
        call_id: String,
        /// Flattened result text.
        text: String,
        /// True when the tool reported failure.
        is_error: bool,
    },
}

/// The sidecar for `session.jsonl` is `session.frames.jsonl`.
pub fn frames_path_for(session: &Path) -> PathBuf {
    let stem = session
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string());
    session.with_file_name(format!("{stem}.frames.jsonl"))
}

/// What the agent calls per frame.
pub type FrameSink = Arc<dyn Fn(&Frame) + Send + Sync>;

/// The harness's half of the frame plumbing: points the agent's sink
/// at the current session's sidecar. Clone-shared, so store switches
/// (/new, /import, /resume…) just re-point it, and a swapped-in agent
/// (/reload) gets the same sink.
#[derive(Clone, Default)]
pub struct FrameTarget {
    path: Arc<Mutex<Option<PathBuf>>>,
}

impl FrameTarget {
    /// Aim at `path` and start a fresh run: truncate the file.
    pub fn point_at(&self, path: PathBuf) {
        let _ = std::fs::File::create(&path);
        *self.path.lock().unwrap() = Some(path);
    }

    /// The sidecar this target points at, if any.
    pub fn path(&self) -> Option<PathBuf> {
        self.path.lock().unwrap().clone()
    }

    /// ClearRun: the run's messages are entries — retire the frames.
    pub fn retire(&self) {
        let path = self.path.lock().unwrap().take();
        if let Some(path) = path {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The sink the agent writes frames through (no-op until pointed).
    /// Appends one JSON line per frame, opening the file per write —
    /// no persistent handle, so a crash can only tear the last line
    /// (which [`salvage`] discards).
    pub fn sink(&self) -> FrameSink {
        let path = self.path.clone();
        Arc::new(move |frame: &Frame| {
            let Some(path) = path.lock().unwrap().clone() else {
                return;
            };
            let Ok(mut line) = serde_json::to_string(frame) else {
                return;
            };
            line.push('\n');
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = file.write_all(line.as_bytes());
            }
        })
    }
}

/// What a salvage recovered, for the harness's user-facing note.
#[derive(Debug, Clone, PartialEq)]
pub struct Salvage {
    /// Session entries appended (one assistant message per salvaged
    /// turn, plus a tool message per turn that had calls).
    pub entries: usize,
    /// Committed assistant text recovered, in chars.
    pub text_chars: usize,
    /// Tool calls whose result never landed (unknown outcome).
    pub unknown_outcomes: usize,
}

/// One dead turn's committed frames, grouped for reconstruction.
#[derive(Default)]
struct TurnFrames {
    text: String,
    calls: std::collections::BTreeMap<u32, (String, String, String)>,
    results: Vec<(String, String, bool)>,
}

/// Recover a dead run's committed frames into `store` as real entries,
/// then remove the sidecar. No-op (`None`) when there is nothing to
/// recover — including a second call after a successful salvage.
pub fn salvage(store: &mut JsonlStore) -> Result<Option<Salvage>, SessionError> {
    let path = frames_path_for(store.path());
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    // Parse tolerantly: a torn final line is progress that never
    // committed — discard it, keep the committed prefix.
    let frames: Vec<Frame> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    // Idempotency: the notice is the marker. A crash between appending
    // and removing the file would otherwise double-append on reopen.
    let noticed = [
        store.head(),
        store
            .head()
            .and_then(|h| h.parent.as_ref().and_then(|id| store.get(id))),
    ]
    .into_iter()
    .flatten()
    .any(|entry| match &entry.kind {
        EntryKind::Message { message } => message.text().contains(INTERRUPTED_NOTICE),
        _ => false,
    });
    if noticed {
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }

    let mut turns: Vec<TurnFrames> = Vec::new();
    for frame in &frames {
        if matches!(frame, Frame::TurnStart) || turns.is_empty() {
            turns.push(TurnFrames::default());
            if matches!(frame, Frame::TurnStart) {
                continue;
            }
        }
        let turn = turns.last_mut().expect("non-empty after push");
        match frame {
            Frame::TurnStart => unreachable!("handled above"),
            Frame::TextDelta { text } => turn.text.push_str(text),
            Frame::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let call = turn.calls.entry(*index).or_default();
                if let Some(id) = id {
                    call.0.clone_from(id);
                }
                if let Some(name) = name {
                    call.1.clone_from(name);
                }
                call.2.push_str(arguments_delta);
            }
            Frame::ToolResult {
                call_id,
                text,
                is_error,
            } => turn
                .results
                .push((call_id.clone(), text.clone(), *is_error)),
        }
    }

    let mut parent = store.head().map(|h| h.id.clone());
    let mut appended = 0;
    let mut text_chars = 0;
    let mut unknown = 0;
    let last = turns.len().saturating_sub(1);
    for (index, turn) in turns.into_iter().enumerate() {
        let mut content: Vec<Content> = Vec::new();
        if !turn.text.is_empty() {
            text_chars += turn.text.chars().count();
            content.push(Content::Text { text: turn.text });
        }
        for (id, name, arguments) in turn.calls.values() {
            let arguments = if arguments.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "__invalidJson": arguments }))
            };
            content.push(Content::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments,
            });
        }
        if content.is_empty() {
            continue;
        }
        if index == last {
            content.push(Content::Text {
                text: INTERRUPTED_NOTICE.to_string(),
            });
        }
        let entry = SessionEntry {
            id: new_id(),
            parent,
            kind: EntryKind::Message {
                message: Message {
                    role: Role::Assistant,
                    content,
                },
            },
        };
        parent = Some(entry.id.clone());
        store.append(entry)?;
        appended += 1;

        if !turn.calls.is_empty() {
            let results: Vec<Content> = turn
                .calls
                .values()
                .map(
                    |(id, _, _)| match turn.results.iter().find(|(cid, _, _)| cid == id) {
                        Some((_, text, is_error)) => Content::ToolResult {
                            call_id: id.clone(),
                            content: vec![Content::Text { text: text.clone() }],
                            is_error: *is_error,
                        },
                        None => {
                            unknown += 1;
                            Content::ToolResult {
                                call_id: id.clone(),
                                content: vec![Content::Text {
                                    text: UNKNOWN_TOOL_OUTCOME.to_string(),
                                }],
                                is_error: false,
                            }
                        }
                    },
                )
                .collect();
            let entry = SessionEntry {
                id: new_id(),
                parent,
                kind: EntryKind::Message {
                    message: Message {
                        role: Role::Tool,
                        content: results,
                    },
                },
            };
            parent = Some(entry.id.clone());
            store.append(entry)?;
            appended += 1;
        }
    }

    let _ = std::fs::remove_file(&path);
    if appended == 0 {
        return Ok(None);
    }
    Ok(Some(Salvage {
        entries: appended,
        text_chars,
        unknown_outcomes: unknown,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_user_turn() -> (tempfile::TempDir, JsonlStore) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlStore::open(dir.path().join("session.jsonl")).unwrap();
        store
            .append(SessionEntry {
                id: new_id(),
                parent: None,
                kind: EntryKind::Message {
                    message: Message::user("do the thing"),
                },
            })
            .unwrap();
        (dir, store)
    }

    fn write_frames(dir: &Path, frames: &[Frame]) {
        let path = frames_path_for(&dir.join("session.jsonl"));
        let mut text = String::new();
        for frame in frames {
            text.push_str(&serde_json::to_string(frame).unwrap());
            text.push('\n');
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn salvage_rebuilds_an_interrupted_run() {
        let (dir, mut store) = store_with_user_turn();
        write_frames(
            dir.path(),
            &[
                Frame::TurnStart,
                Frame::TextDelta {
                    text: "working on it… ".into(),
                },
                Frame::TextDelta {
                    text: "first half done.".into(),
                },
                Frame::ToolCallDelta {
                    index: 0,
                    id: Some("c1".into()),
                    name: Some("read".into()),
                    arguments_delta: "{\"path\":\"a.txt\"}".into(),
                },
                Frame::ToolCallDelta {
                    index: 1,
                    id: Some("c2".into()),
                    name: Some("write".into()),
                    arguments_delta: "{\"path\":".into(),
                },
                Frame::ToolResult {
                    call_id: "c1".into(),
                    text: "file contents".into(),
                    is_error: false,
                },
            ],
        );

        let recovered = salvage(&mut store).unwrap().expect("frames to recover");
        assert_eq!(recovered.entries, 2, "assistant + tool message");
        assert!(recovered.text_chars > 0);
        assert_eq!(recovered.unknown_outcomes, 1, "c2 never landed");

        let head = store.head().unwrap().id.clone();
        let branch = store.active_branch(&head).unwrap();
        assert_eq!(branch.len(), 3, "user + salvaged assistant + salvaged tool");
        let assistant = &branch[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert!(assistant.text().contains("first half done."));
        assert!(assistant.text().contains(INTERRUPTED_NOTICE));
        let calls: Vec<_> = assistant.tool_calls().collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].2,
            &serde_json::json!({"__invalidJson": "{\"path\":"})
        );
        let tool = &branch[2];
        assert_eq!(tool.role, Role::Tool);
        let tool_text: String = tool
            .content
            .iter()
            .filter_map(|block| match block {
                Content::ToolResult { content, .. } => {
                    Some(crate::types::tool_result_text(content))
                }
                _ => None,
            })
            .collect();
        assert!(tool_text.contains("file contents"), "landed result kept");
        assert!(
            tool_text.contains(UNKNOWN_TOOL_OUTCOME),
            "c2 outcome unknown"
        );
        assert!(
            !frames_path_for(&dir.path().join("session.jsonl")).exists(),
            "sidecar retired"
        );
    }

    #[test]
    fn salvage_groups_frames_by_turn() {
        let (dir, mut store) = store_with_user_turn();
        write_frames(
            dir.path(),
            &[
                Frame::TurnStart,
                Frame::TextDelta {
                    text: "turn one ".into(),
                },
                Frame::ToolCallDelta {
                    index: 0,
                    id: Some("c1".into()),
                    name: Some("ls".into()),
                    arguments_delta: "{}".into(),
                },
                Frame::ToolResult {
                    call_id: "c1".into(),
                    text: "a.txt".into(),
                    is_error: false,
                },
                Frame::TurnStart,
                Frame::TextDelta {
                    text: "turn two partial".into(),
                },
            ],
        );

        let recovered = salvage(&mut store).unwrap().expect("frames to recover");
        assert_eq!(recovered.entries, 3, "two assistants + one tool message");
        let head = store.head().unwrap().id.clone();
        let branch = store.active_branch(&head).unwrap();
        let roles: Vec<_> = branch.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert!(!branch[1].text().contains(INTERRUPTED_NOTICE));
        assert!(branch[3].text().contains(INTERRUPTED_NOTICE));
    }

    #[test]
    fn salvage_is_idempotent() {
        let (dir, mut store) = store_with_user_turn();
        write_frames(
            dir.path(),
            &[Frame::TextDelta {
                text: "partial".into(),
            }],
        );
        assert!(salvage(&mut store).unwrap().is_some());
        let entries = store.entries().len();
        // A crash between appending and retiring leaves the file behind:
        std::fs::write(
            frames_path_for(&dir.path().join("session.jsonl")),
            "{\"type\":\"textDelta\",\"text\":\"partial\"}\n",
        )
        .unwrap();
        assert_eq!(
            salvage(&mut store).unwrap(),
            None,
            "the notice marks the salvage"
        );
        assert_eq!(store.entries().len(), entries, "no double append");
        assert!(!frames_path_for(&dir.path().join("session.jsonl")).exists());
    }

    #[test]
    fn salvage_ignores_empty_and_torn_frames() {
        let (dir, mut store) = store_with_user_turn();
        let path = frames_path_for(&dir.path().join("session.jsonl"));
        std::fs::write(&path, "{\"type\":\"textDelta\",\"text\":\"par").unwrap();
        let entries = store.entries().len();
        assert_eq!(salvage(&mut store).unwrap(), None, "only a torn line");
        assert_eq!(store.entries().len(), entries);
        assert!(!path.exists());
    }
}
