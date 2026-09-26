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
//! sessions) and deliberately dumb: put, get, no GC yet.

use std::io;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::types::{Content, MediaSource, Message};

/// Media larger than this is externalized at session write time.
pub const INLINE_LIMIT: usize = 256 * 1024;

pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
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
    /// the same content twice is a no-op.
    pub fn put(&self, bytes: &[u8]) -> io::Result<String> {
        let hash = format!("sha256:{}", hex(&Sha256::digest(bytes)));
        let path = self.path(&hash);
        if !path.is_file() {
            std::fs::create_dir_all(&self.dir)?;
            std::fs::write(path, bytes)?;
        }
        Ok(hash)
    }

    pub fn get(&self, hash: &str) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn has(&self, hash: &str) -> bool {
        self.path(hash).is_file()
    }
}

/// Rewrite large inline media in `message` into blob references.
/// Idempotent and cheap for already-externalized or small media.
pub fn externalize(message: &mut Message, store: &BlobStore) -> io::Result<()> {
    for media in media_mut(message) {
        if let MediaSource::Bytes(bytes) = &media.source {
            if bytes.len() > INLINE_LIMIT {
                let hash = store.put(bytes)?;
                media.source = MediaSource::Blob { hash };
            }
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
            _ => {
                notes.push((
                    content.clone(),
                    format!("[media unavailable: {media_type} blob {hash} not in store]"),
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
    message.content.iter_mut().filter_map(|content| match content {
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
        assert!(matches!(&lost.content[1], Content::Text { text } if text.contains("not in store")));
    }
}
