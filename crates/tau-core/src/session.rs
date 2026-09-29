//! Sessions are trees of entries stored as append-only JSONL.
//!
//! Every entry has an id and refers to its parent. The path from the root to
//! the current entry (head) is the active branch and supplies model history.
//! Continuing from an earlier entry creates another branch in the same file;
//! entries are never rewritten or deleted.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::Message;

/// One node of the session tree. `parent` links make the file a tree:
/// appending under an older entry forks a branch in the same file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Unique id (time-based; see [`new_id`]).
    pub id: String,
    /// The entry this one follows; none for the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// What the entry carries.
    #[serde(flatten)]
    pub kind: EntryKind,
}

/// The payload of a [`SessionEntry`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EntryKind {
    /// A conversation message (user, assistant, tool result).
    Message {
        /// The message.
        message: Message,
    },
    /// A compaction: future branch walks yield `summary` instead of
    /// everything before this entry. The original messages stay in the
    /// tree — older branches still walk through them.
    Compaction {
        /// The replacement message yielded by future branch walks.
        summary: Message,
    },
}

/// Session store failures.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Filesystem error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A session file line is not valid JSON for a [`SessionEntry`].
    #[error("corrupt session file {path} line {line}: {reason}")]
    Corrupt {
        /// The offending file.
        path: PathBuf,
        /// 1-based line number.
        line: usize,
        /// The parse error.
        reason: String,
    },
    /// An id (or `#index`) matched no entry.
    #[error("entry not found: {0}")]
    NotFound(String),
    /// An id prefix matched more than one entry.
    #[error("ambiguous id prefix {0} — matches more than one entry")]
    Ambiguous(String),
}

/// Append-only JSONL session store. Loads the whole file into memory;
/// session files are small (a long session is a few MB) — large media is
/// externalized into the blob store at write time when one is attached.
pub struct JsonlStore {
    path: PathBuf,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    blobs: Option<crate::blobs::BlobStore>,
    torn_tail: Option<TornTail>,
}

/// What [`JsonlStore::open`] discarded from a crash-torn tail.
#[derive(Debug, Clone)]
pub struct TornTail {
    /// 1-based physical line number of the first discarded line.
    pub line: usize,
    /// Bytes discarded from the end of the file.
    pub discarded_bytes: usize,
}

impl JsonlStore {
    /// Open (or create) a session file, parsing every entry into memory.
    ///
    /// A crash between starting and finishing an append leaves a torn
    /// tail: an unparseable line with nothing parseable after it. That
    /// tail is discarded (the file is truncated to the last intact line,
    /// so appends land cleanly and the warning does not repeat) and
    /// reported via [`JsonlStore::torn_tail`] — instead of bricking every
    /// intact entry before it. A bad line with GOOD lines after it is
    /// real corruption, not a tear: the file fails with
    /// [`SessionError::Corrupt`]. So does a file whose first line is bad —
    /// recovering there would silently empty a file tau was pointed at by
    /// mistake.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();
        let mut entries = Vec::new();
        let mut torn_tail = None;
        if path.exists() {
            let bytes = std::fs::read(&path)?;
            // Non-empty physical lines as (byte offset, 1-based line
            // number, bytes). Byte-level splitting tolerates a tear that
            // cut a multi-byte UTF-8 char.
            let mut segments: Vec<(usize, usize, &[u8])> = Vec::new();
            let mut offset = 0usize;
            for (n, segment) in bytes.split(|b| *b == b'\n').enumerate() {
                let start = offset;
                offset += segment.len() + 1;
                let line = segment.strip_suffix(b"\r").unwrap_or(segment);
                if !line.trim_ascii().is_empty() {
                    segments.push((start, n + 1, line));
                }
            }
            let mut first_bad: Option<usize> = None; // index into segments
            for (i, (_, _, line)) in segments.iter().enumerate() {
                match serde_json::from_slice::<SessionEntry>(line) {
                    Ok(entry) => entries.push(entry),
                    Err(_) => {
                        first_bad = Some(i);
                        break;
                    }
                }
            }
            if let Some(i) = first_bad {
                let (bad_offset, bad_line_no, _) = segments[i];
                let recoverable = !entries.is_empty()
                    && segments[i + 1..]
                        .iter()
                        .all(|(_, _, line)| serde_json::from_slice::<SessionEntry>(line).is_err());
                if recoverable {
                    let file = std::fs::OpenOptions::new().write(true).open(&path)?;
                    file.set_len(bad_offset as u64)?;
                    torn_tail = Some(TornTail {
                        line: bad_line_no,
                        discarded_bytes: bytes.len() - bad_offset,
                    });
                } else {
                    let (_, line_no, line) = segments[i];
                    let reason = serde_json::from_slice::<SessionEntry>(line)
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_default();
                    return Err(SessionError::Corrupt {
                        path: path.clone(),
                        line: line_no,
                        reason,
                    });
                }
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
            torn_tail,
        })
    }

    /// The torn tail [`JsonlStore::open`] discarded, if any.
    pub fn torn_tail(&self) -> Option<&TornTail> {
        self.torn_tail.as_ref()
    }

    /// Attach a blob store: large media is externalized to
    /// `MediaSource::Blob` at append time (see `blobs` module).
    pub fn with_blobs(mut self, store: crate::blobs::BlobStore) -> Self {
        self.blobs = Some(store);
        self
    }

    /// Append an entry (externalizing large media when a blob store is
    /// attached). The parent, if any, must already exist.
    pub fn append(&mut self, mut entry: SessionEntry) -> Result<(), SessionError> {
        if let Some(parent) = &entry.parent
            && !self.by_id.contains_key(parent)
        {
            return Err(SessionError::NotFound(parent.clone()));
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

    /// The most recently appended entry (tip of the latest branch).
    pub fn head(&self) -> Option<&SessionEntry> {
        self.entries.last()
    }

    /// Look an entry up by exact id.
    pub fn get(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id.get(id).map(|&i| &self.entries[i])
    }

    /// All entries, in append order (for `tau tree` and fork-target
    /// pickers).
    /// Every entry in append order.
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Resolve a full id, an unambiguous prefix, or a `#index` into
    /// append order (what `tau tree` and `/fork` display) to the full id.
    /// Resolve a user-typed reference to an entry id: exact id, unique
    /// prefix, or `#index` into append order. Ambiguous prefixes fail.
    pub fn resolve_id(&self, prefix_or_id: &str) -> Result<String, SessionError> {
        if let Ok(index) = prefix_or_id.parse::<usize>() {
            return self
                .entries
                .get(index)
                .map(|entry| entry.id.clone())
                .ok_or_else(|| SessionError::NotFound(prefix_or_id.to_string()));
        }
        if self.by_id.contains_key(prefix_or_id) {
            return Ok(prefix_or_id.to_string());
        }
        let matches: Vec<&str> = self
            .by_id
            .keys()
            .map(String::as_str)
            .filter(|id| id.starts_with(prefix_or_id))
            .collect();
        match matches.as_slice() {
            [one] => Ok((*one).to_string()),
            [] => Err(SessionError::NotFound(prefix_or_id.to_string())),
            _ => Err(SessionError::Ambiguous(prefix_or_id.to_string())),
        }
    }

    /// Walk parents from `head_id` to the root; returns messages oldest-first.
    pub fn active_branch(&self, head_id: &str) -> Result<Vec<Message>, SessionError> {
        let mut messages = Vec::new();
        let mut current = Some(head_id.to_string());
        while let Some(id) = current {
            let entry = self
                .get(&id)
                .ok_or_else(|| SessionError::NotFound(id.clone()))?;
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
/// One display line for an entry: kind marker + first line of its text;
/// text-less tool traffic degrades to a variant label so `tau tree`
/// never prints blank rows.
pub fn entry_summary(entry: &SessionEntry) -> String {
    let (marker, message) = match &entry.kind {
        EntryKind::Message { message } => ("", message),
        EntryKind::Compaction { summary } => ("[compaction] ", summary),
    };
    let first = message.text().lines().next().unwrap_or("").to_string();
    if !first.is_empty() {
        return format!("{marker}{first}");
    }
    for content in &message.content {
        match content {
            crate::types::Content::ToolCall { name, .. } => {
                return format!("{marker}[tool call: {name}]");
            }
            crate::types::Content::ToolResult { is_error, .. } => {
                let label = if *is_error {
                    "tool result (error)"
                } else {
                    "tool result"
                };
                return format!("{marker}[{label}]");
            }
            _ => {}
        }
    }
    format!("{marker}(no text)")
}

/// Monotonic-ish unique entry id: timestamp prefix plus randomness, no
/// external deps. Displayed truncated; [`JsonlStore::resolve_id`] accepts
/// unique prefixes and `#index` forms.
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
    #[ignore = "scale evidence for docs/perf.md, run on demand"]
    fn scale_evidence_100k() {
        // 100k-entry chain, written directly (append per entry would
        // dominate the clock, and parsing is what we are measuring).
        let dir = std::env::temp_dir().join(format!("tau-scale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("100k.jsonl");
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&path).unwrap();
            let mut parent = None;
            for i in 0..100_000 {
                let entry = SessionEntry {
                    id: format!("{i:016x}"),
                    parent,
                    kind: EntryKind::Message {
                        message: Message::user(format!("message number {i}")),
                    },
                };
                parent = Some(entry.id.clone());
                writeln!(file, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
            }
        }
        let start = std::time::Instant::now();
        let store = JsonlStore::open(&path).unwrap();
        let open = start.elapsed();
        let head = store.head().unwrap().id.clone();
        let start = std::time::Instant::now();
        let branch = store.active_branch(&head).unwrap();
        let branch_time = start.elapsed();
        assert_eq!(branch.len(), 100_000);
        eprintln!("open 100k-entry session: {open:?}; active_branch walk: {branch_time:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_parses_large_sessions_quickly() {
        // 10k entries; the generous bound guards pathological (e.g.
        // quadratic) regressions, not micro-perf. The actual time is
        // printed for docs/perf.md.
        // Unique by construction -- a clock-derived suffix is not unique
        // on Windows (100 ns steps) and parallel test threads collide.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tau-test-perf-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.jsonl");
        {
            let mut store = JsonlStore::open(&path).unwrap();
            let mut parent = None;
            for i in 0..10_000 {
                let entry = SessionEntry {
                    id: new_id(),
                    parent,
                    kind: EntryKind::Message {
                        message: Message::user(format!("message number {i}")),
                    },
                };
                parent = Some(entry.id.clone());
                store.append(entry).unwrap();
            }
        }
        let start = std::time::Instant::now();
        let store = JsonlStore::open(&path).unwrap();
        let elapsed = start.elapsed();
        let head = store.head().unwrap().id.clone();
        assert_eq!(store.active_branch(&head).unwrap().len(), 10_000);
        eprintln!("open 10k-entry session: {elapsed:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "session open regressed pathologically: {elapsed:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn torn_tail_is_discarded_and_the_file_truncated() {
        let path = temp_path("torn.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut store = JsonlStore::open(&path).unwrap();
        store.append(entry("a", None, "first")).unwrap();
        store.append(entry("b", Some("a"), "second")).unwrap();
        drop(store);

        // A crash mid-append: a partial JSON line, no trailing newline.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"id\":\"torn\",\"parent\":\"a\",\"kind\":{\"me")
            .unwrap();
        drop(file);

        let mut store = JsonlStore::open(&path).unwrap();
        assert_eq!(store.entries().len(), 2, "intact entries survive");
        let torn = store.torn_tail().expect("the tear is reported");
        assert_eq!(torn.line, 3);
        assert!(torn.discarded_bytes > 0);

        // The garbage is gone from disk: appends land cleanly, the next
        // open reports nothing.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("torn"), "torn bytes truncated: {raw}");
        store.append(entry("c", Some("b"), "third")).unwrap();
        let store = JsonlStore::open(&path).unwrap();
        assert_eq!(store.entries().len(), 3);
        assert!(store.torn_tail().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn middle_corruption_is_an_error_not_a_tear() {
        let path = temp_path("middle-corrupt.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut store = JsonlStore::open(&path).unwrap();
        store.append(entry("a", None, "first")).unwrap();
        store.append(entry("b", Some("a"), "second")).unwrap();
        drop(store);

        // A bad line with GOOD lines after it is real corruption:
        // refuse, and leave the file untouched for forensics.
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = raw.lines().collect();
        lines.insert(1, "not json at all");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = JsonlStore::open(&path)
            .err()
            .expect("corrupt session refuses");
        assert!(
            matches!(err, SessionError::Corrupt { line: 2, .. }),
            "got: {err}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "file untouched");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_first_line_is_an_error_never_a_truncation() {
        // Recovering an all-bad file would silently empty whatever file
        // tau was pointed at by mistake. Refuse instead.
        let path = temp_path("all-bad.jsonl");
        std::fs::write(&path, b"definitely not a session\n").unwrap();
        let err = JsonlStore::open(&path).err().expect("all-bad file refuses");
        assert!(
            matches!(err, SessionError::Corrupt { line: 1, .. }),
            "got: {err}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"definitely not a session\n",
            "a refused file is never truncated"
        );
        let _ = std::fs::remove_file(&path);
    }
}
