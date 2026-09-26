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
/// session files are small (a long session is a few MB).
pub struct JsonlStore {
    path: PathBuf,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
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
        })
    }

    pub fn append(&mut self, entry: SessionEntry) -> Result<(), SessionError> {
        if let Some(parent) = &entry.parent {
            if !self.by_id.contains_key(parent) {
                return Err(SessionError::NotFound(parent.clone()));
            }
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
