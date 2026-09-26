//! Content-addressed blob store for large media.
//!
//! `MediaSource::Bytes` serializes inline as base64 — fine for small
//! images, but a 10MB photo would make every session JSONL line ~13MB and
//! get re-read every turn. Media past [`INLINE_LIMIT`] is externalized at
//! session write time into `MediaSource::Blob { hash }`: bytes live once
//! in the store, sessions carry only the hash. The agent materializes
//! blobs back to bytes at the request edge — the model contract stays
//! bytes-only and provider wire encoders never see a hash.
//!
//! The store is content-addressed (`sha256:<hex>`, shared across
//! sessions) and deliberately dumb: put, get, and a mark-and-sweep GC —
//! the caller marks the live hashes (every blob referenced anywhere in
//! the session tree, not just the active branch), [`BlobStore::sweep`]
//! removes the rest. A blob deleted too early is not fatal: materialize
//! degrades it to a text placeholder and the run continues.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::types::{Content, MediaSource, Message};

/// Media larger than this is externalized at session write time.
pub const INLINE_LIMIT: usize = 256 * 1024;

/// Content-addressed blob store: session media above [`INLINE_LIMIT`]
/// is externalized here, referenced by `sha256:<hex>`.
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    /// A store rooted at `dir` (created on first [`put`](Self::put)).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `~/.tau/blobs` — shared across sessions for dedup, consistent with
    /// the other content-addressed stores under `~/.tau`.
    pub fn default_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".tau")
            .join("blobs")
    }

    fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(hash.replace(':', "_"))
    }

    /// Store bytes; returns the `sha256:<hex>` hash. Idempotent — writing
    /// the same content twice is a no-op. The write is atomic — a temp
    /// file then a rename — so a crash mid-write tears the temp file,
    /// never the content-addressed one readers verify against.
    pub fn put(&self, bytes: &[u8]) -> io::Result<String> {
        let hash = format!("sha256:{}", hex(&Sha256::digest(bytes)));
        let path = self.path(&hash);
        if !path.is_file() {
            std::fs::create_dir_all(&self.dir)?;
            let tmp = self.dir.join(format!(
                ".tmp-{}-{}",
                std::process::id(),
                hash.replace(':', "-")
            ));
            std::fs::write(&tmp, bytes)?;
            if let Err(e) = std::fs::rename(&tmp, &path) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
        Ok(hash)
    }

    /// The bytes for `hash`, or `None` when absent. The store is
    /// content-addressed, so reads verify: a file whose bytes no longer
    /// hash to its name (disk rot, a torn write from before the atomic
    /// put, tampering) is an error, never silently served.
    pub fn get(&self, hash: &str) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(hash)) {
            Ok(bytes) => {
                let actual = format!("sha256:{}", hex(&Sha256::digest(&bytes)));
                if actual == hash {
                    Ok(Some(bytes))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("blob {hash} is corrupt (content hashes to {actual})"),
                    ))
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether `hash` is stored.
    pub fn has(&self, hash: &str) -> bool {
        self.path(hash).is_file()
    }

    /// Remove every stored blob whose hash is not in `live`. With
    /// `dry_run`, only report what would go. Files whose names do not
    /// decode back to a hash are left alone — the store dir is ours,
    /// but deleting is forever.
    pub fn sweep(&self, live: &HashSet<String>, dry_run: bool) -> io::Result<GcReport> {
        let mut report = GcReport::default();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp-") {
                // Our own incomplete put: definitionally garbage (the
                // content-addressed file it was headed for either exists
                // or will be rewritten by the next put).
                if !dry_run {
                    std::fs::remove_file(entry.path())?;
                }
                continue;
            }
            let Some(hash) = name
                .split_once('_')
                .map(|(tag, hex)| format!("{tag}:{hex}"))
            else {
                continue;
            };
            report.scanned += 1;
            if live.contains(&hash) {
                report.kept += 1;
                continue;
            }
            report.removed += 1;
            report.bytes_freed += entry.metadata()?.len();
            report.removed_hashes.push(hash);
            if !dry_run {
                std::fs::remove_file(entry.path())?;
            }
        }
        report.removed_hashes.sort();
        Ok(report)
    }
}

/// Outcome of a [`BlobStore::sweep`].
#[derive(Debug, Default)]
pub struct GcReport {
    /// Blob files looked at.
    pub scanned: usize,
    /// Still referenced — left in place.
    pub kept: usize,
    /// Unreferenced — deleted (or would be, on a dry run).
    pub removed: usize,
    /// Total size of the removed blobs.
    pub bytes_freed: u64,
    /// Hashes of the removed blobs, sorted.
    pub removed_hashes: Vec<String>,
}

/// The blob hashes referenced by `message` (externalized media only).
pub fn blob_hashes(message: &Message) -> impl Iterator<Item = &str> {
    message.content.iter().filter_map(|content| match content {
        Content::Image { media }
        | Content::Audio { media }
        | Content::Video { media }
        | Content::File { media, .. } => match &media.source {
            MediaSource::Blob { hash } => Some(hash.as_str()),
            _ => None,
        },
        _ => None,
    })
}

/// Rewrite large inline media in `message` into blob references.
/// Idempotent and cheap for already-externalized or small media.
pub fn externalize(message: &mut Message, store: &BlobStore) -> io::Result<()> {
    for media in media_mut(message) {
        if let MediaSource::Bytes(bytes) = &media.source
            && bytes.len() > INLINE_LIMIT
        {
            let hash = store.put(bytes)?;
            media.source = MediaSource::Blob { hash };
        }
    }
    Ok(())
}

/// Resolve blob references in `message` back to inline bytes for a
/// provider request. A missing blob degrades to a text placeholder — the
/// run continues with a gap, it does not fail.
pub fn materialize(message: &mut Message, store: &BlobStore) {
    let mut notes = Vec::new();
    for content in &mut message.content {
        let (media_type, hash) = match content {
            Content::Image { media }
            | Content::Audio { media }
            | Content::Video { media }
            | Content::File { media, .. } => match &media.source {
                MediaSource::Blob { hash } => (media.media_type.clone(), hash.clone()),
                _ => continue,
            },
            _ => continue,
        };
        match store.get(&hash) {
            Ok(Some(bytes)) => {
                let media = match content {
                    Content::Image { media }
                    | Content::Audio { media }
                    | Content::Video { media }
                    | Content::File { media, .. } => media,
                    _ => unreachable!(),
                };
                media.source = MediaSource::Bytes(bytes);
            }
            Ok(None) => {
                notes.push((
                    content.clone(),
                    format!("[media unavailable: {media_type} blob {hash} not in store]"),
                ));
            }
            Err(e) => {
                // A corrupt blob (the read verified the hash and it did
                // not match) degrades the same way — but says so.
                notes.push((
                    content.clone(),
                    format!("[media unavailable: {media_type} blob {hash}: {e}]"),
                ));
            }
        }
    }
    for (old, note) in notes {
        if let Some(slot) = message.content.iter_mut().find(|c| **c == old) {
            *slot = Content::Text { text: note };
        }
    }
}

fn media_mut(message: &mut Message) -> impl Iterator<Item = &mut crate::types::Media> {
    message
        .content
        .iter_mut()
        .filter_map(|content| match content {
            Content::Image { media }
            | Content::Audio { media }
            | Content::Video { media }
            | Content::File { media, .. } => Some(media),
            _ => None,
        })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Media;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        (dir, blobs)
    }

    #[test]
    fn put_get_round_trip_and_dedup() {
        let (_dir, store) = store();
        let hash = store.put(b"hello").unwrap();
        assert!(hash.starts_with("sha256:"));
        assert_eq!(store.get(&hash).unwrap(), Some(b"hello".to_vec()));
        // Same content, same hash, still one file.
        assert_eq!(store.put(b"hello").unwrap(), hash);
        assert_eq!(store.get("sha256:missing").unwrap(), None);
        assert!(!store.has("sha256:missing"));
        assert!(store.has(&hash));
    }

    #[test]
    fn externalize_only_large_media() {
        let (_dir, store) = store();
        let mut message = Message::user("look");
        message.content.push(Content::Image {
            media: Media::bytes("image/png", vec![1u8; INLINE_LIMIT + 1]),
        });
        message.content.push(Content::Image {
            media: Media::bytes("image/png", vec![2u8; 16]),
        });
        externalize(&mut message, &store).unwrap();
        match &message.content[1] {
            Content::Image { media } => match &media.source {
                MediaSource::Blob { hash } => assert!(store.has(hash)),
                other => panic!("expected blob, got {other:?}"),
            },
            _ => panic!("expected image"),
        }
        match &message.content[2] {
            Content::Image { media } => {
                assert!(matches!(media.source, MediaSource::Bytes(_)))
            }
            _ => panic!("expected image"),
        }
    }

    #[test]
    fn sweep_removes_unreferenced_and_keeps_live() {
        let (_dir, store) = store();
        let keep = store.put(b"keep me").unwrap();
        let drop_a = store.put(b"drop a").unwrap();
        let drop_b = store.put(b"drop b").unwrap();
        let live: HashSet<String> = [keep.clone()].into_iter().collect();

        // Dry run reports but deletes nothing.
        let report = store.sweep(&live, true).unwrap();
        assert_eq!(report.scanned, 3);
        assert_eq!(report.kept, 1);
        assert_eq!(report.removed, 2);
        assert!(report.bytes_freed >= 11);
        assert_eq!(report.removed_hashes, {
            let mut v = vec![drop_a.clone(), drop_b.clone()];
            v.sort();
            v
        });
        assert!(store.has(&drop_a) && store.has(&drop_b));

        // Real sweep deletes exactly the unreferenced blobs.
        let report = store.sweep(&live, false).unwrap();
        assert_eq!(report.removed, 2);
        assert!(store.has(&keep));
        assert!(!store.has(&drop_a) && !store.has(&drop_b));

        // Sweeping an absent store is a no-op, not an error.
        let empty = BlobStore::new(_dir.path().join("nope"));
        assert_eq!(empty.sweep(&live, false).unwrap().scanned, 0);
    }

    #[test]
    fn materialize_restores_bytes_and_degrades_missing() {
        let (_dir, store) = store();
        let mut message = Message::user("look");
        message.content.push(Content::Image {
            media: Media::bytes("image/png", vec![7u8; INLINE_LIMIT + 1]),
        });
        externalize(&mut message, &store).unwrap();
        materialize(&mut message, &store);
        match &message.content[1] {
            Content::Image { media } => match &media.source {
                MediaSource::Bytes(bytes) => assert_eq!(bytes.len(), INLINE_LIMIT + 1),
                other => panic!("expected bytes, got {other:?}"),
            },
            _ => panic!("expected image"),
        }

        // A blob whose file is gone degrades to a text note.
        let mut lost = Message::user("look");
        lost.content.push(Content::Image {
            media: Media::blob("image/png", "sha256:gone"),
        });
        materialize(&mut lost, &store);
        assert!(
            matches!(&lost.content[1], Content::Text { text } if text.contains("not in store"))
        );
    }

    #[test]
    fn get_verifies_content_and_rejects_corrupt_blobs() {
        let (_dir, store) = store();
        let hash = store.put(b"honest bytes").unwrap();
        assert_eq!(
            store.get(&hash).unwrap().as_deref(),
            Some(&b"honest bytes"[..])
        );

        // Disk rot / a torn write from before the atomic put / tampering:
        // the file's bytes no longer hash to its name. Never serve that.
        std::fs::write(store.path(&hash), b"evil bytes").unwrap();
        let err = store.get(&hash).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("corrupt"), "got: {err}");

        // materialize degrades a corrupt blob like a missing one — but
        // says it is corrupt, not "not in store".
        let mut message = Message::user("look");
        message.content.push(Content::Image {
            media: Media::blob("image/png", &hash),
        });
        materialize(&mut message, &store);
        assert!(
            matches!(&message.content[1], Content::Text { text } if text.contains("corrupt")),
            "got: {:?}",
            message.content[1]
        );
    }

    #[test]
    fn put_leaves_no_temp_files_and_sweep_cleans_orphaned_temps() {
        let (_dir, store) = store();
        let hash = store.put(b"atomic").unwrap();
        let names: Vec<String> = std::fs::read_dir(&store.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![hash.replace(':', "_")], "no .tmp- litter");

        // A crashed put leaves a .tmp- orphan: dry-run leaves it (pure
        // report), a real sweep removes it. It never counts as a blob.
        std::fs::write(store.dir.join(".tmp-1-sha256-deadbeef"), b"torn").unwrap();
        let live: HashSet<String> = [hash.clone()].into_iter().collect();
        let report = store.sweep(&live, true).unwrap();
        assert_eq!(report.scanned, 1, "tmp files are not blobs");
        assert!(store.dir.join(".tmp-1-sha256-deadbeef").exists());
        let report = store.sweep(&live, false).unwrap();
        assert_eq!(report.scanned, 1);
        assert!(!store.dir.join(".tmp-1-sha256-deadbeef").exists());
        assert!(store.has(&hash));
    }
}
