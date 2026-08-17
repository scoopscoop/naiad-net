use std::path::PathBuf;

use crate::{FileState, Hash};

/// A file as Naiad knows it: its content [`Hash`] plus filesystem metadata.
///
/// This is the shared shape produced by the indexer and persisted by the
/// database, so it lives in `core` rather than in either of those crates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    /// BLAKE3-256 content hash — the file's identity.
    pub hash: Hash,
    /// Absolute path the file was found at on this machine.
    pub path: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// Last-modified time as Unix seconds (UTC), if available from the OS.
    pub mtime: Option<i64>,
    /// File creation time as Unix seconds (UTC), if available from the OS.
    pub created_at: Option<i64>,
    /// SHA-256 interop key (64-char lowercase hex). `None` until computed.
    pub sha256: Option<String>,
}

impl FileRecord {
    /// Create a new record.
    #[must_use]
    pub fn new(hash: Hash, path: PathBuf, size: u64, mtime: Option<i64>) -> Self {
        Self {
            hash,
            path,
            size,
            mtime,
            created_at: None,
            sha256: None,
        }
    }

    /// Attach the SHA-256 interop hex.
    #[must_use]
    pub fn with_sha256(mut self, sha256: String) -> Self {
        self.sha256 = Some(sha256);
        self
    }

    /// Attach a best-effort filesystem creation timestamp.
    #[must_use]
    pub fn with_created_at(mut self, created_at: Option<i64>) -> Self {
        self.created_at = created_at;
        self
    }
}

/// A row of the `files` table: one distinct piece of content, identified by its
/// BLAKE3 hash. Metadata fields are `None` until a later extraction pass fills
/// them; Phase 1 leaves them unset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContent {
    /// Database row id.
    pub id: i64,
    /// Content hash (the unique key).
    pub hash: Hash,
    /// Size in bytes.
    pub size: u64,
    /// Detected MIME type (filled post-hash; `None` in Phase 1).
    pub mime: Option<String>,
    /// Pixel width (filled post-hash; `None` in Phase 1).
    pub width: Option<u32>,
    /// Pixel height (filled post-hash; `None` in Phase 1).
    pub height: Option<u32>,
    /// Duration in milliseconds for A/V (filled post-hash; `None` in Phase 1).
    pub duration_ms: Option<i64>,
    /// Lifecycle state (`Active` in Phase 1).
    pub state: FileState,
    /// Unix seconds when this content was first imported.
    pub imported_at: i64,
}

/// Intrinsic metadata extracted from a file's *content* after hashing — the
/// `mime`/`width`/`height` columns on `files`. Extraction is best-effort and
/// never blocks import, so every field is optional.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileMetadata {
    /// Detected MIME type (e.g. `image/png`).
    pub mime: Option<String>,
    /// Pixel width.
    pub width: Option<u32>,
    /// Pixel height.
    pub height: Option<u32>,
}

/// A row of the `file_locations` table: one place a copy of some content lives.
/// Many locations can point at one [`FileContent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// Filesystem path (stored as raw OS bytes; see `core::pathenc`).
    pub path: std::path::PathBuf,
    /// Last-modified time as Unix seconds, if known.
    pub mtime: Option<i64>,
    /// File creation time as Unix seconds, if known for this location.
    pub created_at: Option<i64>,
    /// Whether the copy was seen on disk at the last scan.
    pub present: bool,
    /// Unix seconds when this location was last touched by a scan.
    pub last_seen: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileState, hash_bytes};
    use std::path::PathBuf;

    #[test]
    fn file_content_holds_metadata_and_state() {
        let c = FileContent {
            id: 7,
            hash: hash_bytes(b"alpha"),
            size: 5,
            mime: None,
            width: None,
            height: None,
            duration_ms: None,
            state: FileState::Active,
            imported_at: 1700,
        };
        assert_eq!(c.id, 7);
        assert_eq!(c.state, FileState::Active);
        assert_eq!(c.mime, None);
    }

    #[test]
    fn location_holds_path_and_presence() {
        let loc = Location {
            path: PathBuf::from("/lib/a.png"),
            mtime: Some(42),
            created_at: Some(41),
            present: true,
            last_seen: 1700,
        };
        assert_eq!(loc.path, PathBuf::from("/lib/a.png"));
        assert_eq!(loc.created_at, Some(41));
        assert!(loc.present);
    }

    #[test]
    fn file_record_holds_created_time() {
        let rec = FileRecord::new(
            hash_bytes(b"alpha"),
            PathBuf::from("/lib/a.png"),
            5,
            Some(10),
        )
        .with_created_at(Some(7));
        assert_eq!(rec.created_at, Some(7));
        assert_eq!(rec.mtime, Some(10));
    }
}
