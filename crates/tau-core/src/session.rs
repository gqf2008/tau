//! Sessions are trees of entries stored as append-only JSONL.
//!
//! Every entry has an id and refers to its parent. The path from the root to
//! the current entry (head) is the active branch and supplies model history.
//! Continuing from an earlier entry creates another branch in the same file;
//! entries are never rewritten or deleted.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::Message;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(flatten)]
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EntryKind {
    Message { message: Message },
    /// A compaction: future branch walks yield `summary` instead of
    /// everything before this entry. The original messages stay in the
    /// tree — older branches still walk through them.
    Compaction { summary: Message },
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt session file {path} line {line}: {reason}")]
    Corrupt {
        path: PathBuf,
        line: usize,
        reason: String,
    },
    #[error("entry not found: {0}")]
    NotFound(String),
}

/// Append-only JSONL session store. Loads the whole file into memory;
/// session files are small (a long session is a few MB) — large media is
/// externalized into the blob store at write time when one is attached.
pub struct JsonlStore {
    path: PathBuf,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    blobs: Option<crate::blobs::BlobStore>,
}

impl JsonlStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();
        let mut entries = Vec::new();
        if path.exists() {
            let file = File::open(&path)?;
            for (i, line) in BufReader::new(file).lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let entry: SessionEntry =
                    serde_json::from_str(&line).map_err(|e| SessionError::Corrupt {
                        path: path.clone(),
                        line: i + 1,
                        reason: e.to_string(),
                    })?;
                entries.push(entry);
            }
        }
        let by_id = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Ok(Self {
            path,
            entries,
            by_id,
            blobs: None,
        })
    }

    /// Attach a blob store: large media is externalized to
    /// `MediaSource::Blob` at append time (see `blobs` module).
    pub fn with_blobs(mut self, store: crate::blobs::BlobStore) -> Self {
        self.blobs = Some(store);
        self
    }

    pub fn append(&mut self, mut entry: SessionEntry) -> Result<(), SessionError> {
        if let Some(parent) = &entry.parent {
            if !self.by_id.contains_key(parent) {
                return Err(SessionError::NotFound(parent.clone()));
            }
        }
        if let (Some(blobs), EntryKind::Message { message }) = (&self.blobs, &mut entry.kind) {
            crate::blobs::externalize(message, blobs)?;
        }
        let mut line = serde_json::to_string(&entry).map_err(|e| SessionError::Corrupt {
            path: self.path.clone(),
            line: self.entries.len() + 1,
            reason: e.to_string(),
        })?;
        line.push('\n');
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(line.as_bytes())?;
        self.by_id.insert(entry.id.clone(), self.entries.len());
        self.entries.push(entry);
        Ok(())
    }

    /// The most recently appended entry, i.e. the head of the latest branch.
    /// Every blob hash referenced anywhere in the tree — the GC mark
    /// set. Deliberately the whole tree, not just the active branch:
    /// old branches still walk their original messages, and a hash
    /// reachable from any entry must survive the sweep.
    pub fn live_blob_hashes(&self) -> std::collections::HashSet<String> {
        let mut live = std::collections::HashSet::new();
        for entry in &self.entries {
            match &entry.kind {
                EntryKind::Message { message } | EntryKind::Compaction { summary: message } => {
                    live.extend(crate::blobs::blob_hashes(message).map(str::to_owned));
                }
            }
        }
        live
    }

    pub fn head(&self) -> Option<&SessionEntry> {
        self.entries.last()
    }

    pub fn get(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id.get(id).map(|&i| &self.entries[i])
    }

    /// Walk parents from `head_id` to the root; returns messages oldest-first.
    pub fn active_branch(&self, head_id: &str) -> Result<Vec<Message>, SessionError> {
        let mut messages = Vec::new();
        let mut current = Some(head_id.to_string());
        while let Some(id) = current {
            let entry = self.get(&id).ok_or_else(|| SessionError::NotFound(id.clone()))?;
            match &entry.kind {
                EntryKind::Message { message } => messages.push(message.clone()),
                // Compaction boundary: the summary stands in for everything
                // before it; stop walking.
                EntryKind::Compaction { summary } => {
                    messages.push(summary.clone());
                    break;
                }
            }
            current = entry.parent.clone();
        }
        if messages.is_empty() {
            return Err(SessionError::NotFound(head_id.to_string()));
        }
        messages.reverse();
        Ok(messages)
    }
}

/// Monotonic-ish unique entry id: time-based, no external deps.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{pid:x}-{seq:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    fn entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            parent: parent.map(str::to_string),
            kind: EntryKind::Message {
                message: Message::user(text),
            },
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tau-test-{}-{}", std::process::id(), name))
    }

    #[test]
    fn live_blob_hashes_marks_the_whole_tree() {
        use crate::types::{Content, Media};
        let path = temp_path("gc.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut store = JsonlStore::open(&path).unwrap();

        let msg = |text: &str, hash: &str| {
            let mut m = Message::user(text);
            m.content.push(Content::Image {
                media: Media::blob("image/png", hash),
            });
            m
        };
        // A blob on an old branch AND one inside a compaction summary
        // must both be marked live, though neither is on the active
        // branch's plain message walk alone.
        store
            .append(SessionEntry {
                id: "a".into(),
                parent: None,
                kind: EntryKind::Message {
                    message: msg("old", "sha256:old"),
                },
            })
            .unwrap();
        store
            .append(SessionEntry {
                id: "k".into(),
                parent: Some("a".into()),
                kind: EntryKind::Compaction {
                    summary: msg("[summary]", "sha256:summary"),
                },
            })
            .unwrap();
        store.append(entry("b", Some("k"), "new")).unwrap();

        let live = store.live_blob_hashes();
        assert!(live.contains("sha256:old"));
        assert!(live.contains("sha256:summary"));
        assert_eq!(live.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn branch_stops_at_compaction_but_older_branches_are_intact() {
        let path = temp_path("compact.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut store = JsonlStore::open(&path).unwrap();
        store.append(entry("a", None, "one")).unwrap();
        store.append(entry("b", Some("a"), "two")).unwrap();
        store.append(entry("c", Some("b"), "three")).unwrap();
        store
            .append(SessionEntry {
                id: "k".into(),
                parent: Some("c".into()),
                kind: EntryKind::Compaction {
                    summary: Message::user("[summary] one two three"),
                },
            })
            .unwrap();
        store.append(entry("d", Some("k"), "four")).unwrap();

        // The current branch sees the summary, not the covered messages.
        let branch: Vec<String> = store
            .active_branch("d")
            .unwrap()
            .iter()
            .map(|m| m.text())
            .collect();
        assert_eq!(branch, vec!["[summary] one two three", "four"]);

        // An older head still walks the originals — nothing was deleted.
        let old: Vec<String> = store
            .active_branch("c")
            .unwrap()
            .iter()
            .map(|m| m.text())
            .collect();
        assert_eq!(old, vec!["one", "two", "three"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_externalizes_large_media_and_reload_reads_blob() {
        let path = temp_path("blobs.jsonl");
        let blob_dir = temp_path("blobs-store");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&blob_dir);
        let mut store = JsonlStore::open(&path)
            .unwrap()
            .with_blobs(crate::blobs::BlobStore::new(&blob_dir));
        let mut media_entry = entry("a", None, "look");
        let EntryKind::Message { message } = &mut media_entry.kind else {
            unreachable!()
        };
        message.content.push(crate::types::Content::Image {
            media: crate::types::Media::bytes(
                "image/png",
                vec![9u8; crate::blobs::INLINE_LIMIT + 1],
            ),
        });
        store.append(media_entry).unwrap();

        // The JSONL line carries a hash, not the bytes.
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(line.contains("\"source\":\"blob\""), "line: {line}");
        assert!(!line.contains("base64"), "line: {line}");

        // Reload: the reference survives the round trip.
        let store = JsonlStore::open(&path).unwrap();
        let branch = store.active_branch("a").unwrap();
        let crate::types::Content::Image { media } = &branch[0].content[1] else {
            panic!("expected image, got {:?}", branch[0].content);
        };
        let crate::types::MediaSource::Blob { hash } = &media.source else {
            panic!("expected blob, got {:?}", media.source);
        };
        let bytes = crate::blobs::BlobStore::new(&blob_dir)
            .get(hash)
            .unwrap()
            .unwrap();
        assert_eq!(bytes, vec![9u8; crate::blobs::INLINE_LIMIT + 1]);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&blob_dir);
    }

    #[test]
    fn branch_walks_parents_oldest_first() {
        let path = temp_path("branch.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut store = JsonlStore::open(&path).unwrap();
        store.append(entry("a", None, "one")).unwrap();
        store.append(entry("b", Some("a"), "two")).unwrap();
        store.append(entry("c", Some("b"), "three")).unwrap();

        // Fork from "a": a second branch in the same file.
        store.append(entry("d", Some("a"), "fork")).unwrap();

        let main: Vec<String> = store
            .active_branch("c")
            .unwrap()
            .iter()
            .map(Message::text)
            .collect();
        assert_eq!(main, ["one", "two", "three"]);

        let fork: Vec<String> = store
            .active_branch("d")
            .unwrap()
            .iter()
            .map(Message::text)
            .collect();
        assert_eq!(fork, ["one", "fork"]);

        // Head is the latest append, regardless of branch.
        assert_eq!(store.head().unwrap().id, "d");

        // Survives reload.
        let store = JsonlStore::open(&path).unwrap();
        let fork: Vec<String> = store
            .active_branch("d")
            .unwrap()
            .iter()
            .map(Message::text)
            .collect();
        assert_eq!(fork, ["one", "fork"]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_rejects_unknown_parent() {
        let path = temp_path("orphan.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut store = JsonlStore::open(&path).unwrap();
        let err = store.append(entry("x", Some("missing"), "hi")).unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
        let _ = std::fs::remove_file(&path);
    }
}
