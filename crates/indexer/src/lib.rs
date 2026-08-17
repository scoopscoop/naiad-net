//! Folder scanning: walk a directory tree, hash each file, and yield
//! [`FileRecord`]s.
//!
//! The iterators here are lazy, sequential building blocks; the daemon's scan
//! ops fan the per-file work ([`hash_file`] via `classify`) out across cores
//! with rayon. `benches/hash.rs` measures both the sequential baseline and the
//! parallel speedup so the gain stays measurable rather than assumed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use naiad_core::{FileRecord, hash_reader_dual};
use walkdir::WalkDir;

mod metadata;
pub use metadata::extract_metadata;

mod watch;
pub use watch::{WatchEvent, Watcher, watch};

/// An error encountered while scanning a single entry, carrying the path for
/// context so the caller can report or skip it.
#[derive(Debug, thiserror::Error)]
#[error("scan error at {path}: {source}")]
pub struct ScanError {
    /// The path that failed.
    pub path: PathBuf,
    /// The underlying I/O error.
    pub source: io::Error,
}

/// A file's cheap identity: size in bytes and last-modified time (Unix seconds),
/// obtained by `stat` alone — no content read. Used to decide whether a file has
/// changed since the last scan and can therefore skip the expensive re-hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    /// File size in bytes.
    pub size: u64,
    /// Last-modified time as Unix seconds, or `None` if the platform/file can't
    /// report one.
    pub mtime: Option<i64>,
    /// Creation time as Unix seconds, or `None` if the platform/file cannot report one.
    pub created_at: Option<i64>,
}

/// The canonical set of image file extensions the indexer accepts, lowercase.
/// Anything outside this allowlist is skipped during scanning and live watching
/// so non-image files (`.dat`, `.txt`, …) never enter the index.
///
/// This list must stay in lockstep with the `image` crate's enabled decode
/// features (see the workspace `Cargo.toml`): an extension indexed here but not
/// decodable produces a silently-broken thumbnail for every such file. `.avif`
/// and `.jxl` were dropped for exactly this reason (#139) — the build carries no
/// AVIF (needs the `dav1d` C library) or JXL decoder — and should only return
/// once real decode support lands.
pub const SUPPORTED_IMAGE_EXTENSIONS: &[&str] =
    &["jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "tif"];

/// Whether `path`'s extension is a supported image format (case-insensitive).
///
/// Files with no extension, or an extension outside
/// [`SUPPORTED_IMAGE_EXTENSIONS`], return `false`.
#[must_use]
pub fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| SUPPORTED_IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// Recursively yield the path of every supported image file under `root`.
///
/// Directories, symlinks, and non-image files (anything outside
/// [`SUPPORTED_IMAGE_EXTENSIONS`]) are skipped; a walk error (e.g. permission
/// denied on a directory) yields an `Err(ScanError)` for that path while the
/// walk continues. The iterator is lazy, so large trees stream rather than
/// buffer.
pub fn walk(root: impl AsRef<Path>) -> impl Iterator<Item = Result<PathBuf, ScanError>> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| match entry {
            // Keep regular files whose extension is a supported image; skip
            // everything else (dirs, symlinks, non-image files).
            Ok(e) if e.file_type().is_file() && is_supported_image(e.path()) => {
                tracing::trace!(target: "scan", path = %e.path().display(), "walk yielded file");
                Some(Ok(e.into_path()))
            }
            Ok(_) => None,
            // walkdir error (e.g. permission denied on a dir): surface it.
            Err(err) => {
                let path = err.path().map(Path::to_path_buf).unwrap_or_default();
                tracing::warn!(target: "scan", path = %path.display(), error = %err, "walk error (entry skipped)");
                Some(Err(ScanError {
                    path,
                    source: io::Error::from(err),
                }))
            }
        })
}

/// Recursively scan `root`, yielding one result per regular file.
///
/// Each file is stat'd and hashed into a [`FileRecord`]. Unreadable entries are
/// handled gracefully (an `Err(ScanError)` for that path; the walk continues).
pub fn scan(root: impl AsRef<Path>) -> impl Iterator<Item = Result<FileRecord, ScanError>> {
    walk(root).map(|r| r.and_then(|p| hash_file(&p)))
}

/// `stat` a single file for its [`Fingerprint`] without reading its contents.
///
/// # Errors
/// Returns a [`ScanError`] (carrying the path) if the file cannot be stat'd.
pub fn fingerprint(path: &Path) -> Result<Fingerprint, ScanError> {
    let meta = fs::metadata(path).map_err(|source| ScanError {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Fingerprint {
        size: meta.len(),
        mtime: mtime_secs(&meta),
        created_at: created_secs(&meta),
    })
}

/// Last-modified time of `meta` as Unix seconds, if available.
fn mtime_secs(meta: &fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

fn created_secs(meta: &fs::Metadata) -> Option<i64> {
    meta.created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Stat and hash a single file into a [`FileRecord`]. The single-file entry
/// point used both by [`scan`] and by the daemon's live watcher.
///
/// # Errors
/// Returns a [`ScanError`] (carrying the path) if the file cannot be stat'd,
/// opened, or read.
pub fn hash_file(path: &Path) -> Result<FileRecord, ScanError> {
    let started = std::time::Instant::now();
    let err = |source: io::Error| {
        tracing::debug!(target: "scan", path = %path.display(), error = %source, "hash_file error");
        ScanError {
            path: path.to_path_buf(),
            source,
        }
    };

    let meta = fs::metadata(path).map_err(err)?;
    let size = meta.len();
    let mtime = mtime_secs(&meta);
    let created_at = created_secs(&meta);

    let file = fs::File::open(path).map_err(err)?;
    let (hash, sha256) = hash_reader_dual(io::BufReader::new(file)).map_err(err)?;

    tracing::trace!(
        target: "scan",
        path = %path.display(),
        size,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "hashed file",
    );
    Ok(FileRecord::new(hash, path.to_path_buf(), size, mtime)
        .with_created_at(created_at)
        .with_sha256(sha256))
}

#[cfg(test)]
mod tests {
    use super::*;
    use naiad_core::hash_bytes;
    use naiad_test_support::fixture_dir;

    #[test]
    fn scans_fixture_files_with_correct_hashes() {
        let dir = fixture_dir(&[("a.jpg", b"alpha"), ("sub/b.png", b"beta")]);

        let mut records: Vec<FileRecord> = scan(dir.path())
            .collect::<Result<_, _>>()
            .expect("scan should succeed");
        records.sort_by(|x, y| x.path.cmp(&y.path));

        assert_eq!(records.len(), 2);

        // The file named a.jpg must hash to BLAKE3("alpha").
        let a = records.iter().find(|r| r.path.ends_with("a.jpg")).unwrap();
        assert_eq!(a.hash, hash_bytes(b"alpha"));
        assert_eq!(a.size, 5);

        let b = records.iter().find(|r| r.path.ends_with("b.png")).unwrap();
        assert_eq!(b.hash, hash_bytes(b"beta"));
    }

    #[test]
    fn hash_file_hashes_one_file() {
        let dir = fixture_dir(&[("only.txt", b"solo")]);
        let rec = hash_file(&dir.path().join("only.txt")).expect("hash one file");
        assert_eq!(rec.hash, hash_bytes(b"solo"));
        assert_eq!(rec.size, 4);
        assert!(rec.path.ends_with("only.txt"));
    }

    #[test]
    fn empty_dir_yields_nothing() {
        let dir = fixture_dir(&[]);
        let count = scan(dir.path()).count();
        assert_eq!(count, 0);
    }

    #[test]
    fn walk_yields_file_paths_without_hashing() {
        let dir = fixture_dir(&[("a.jpg", b"alpha"), ("sub/b.png", b"beta")]);
        let mut paths: Vec<PathBuf> = walk(dir.path()).collect::<Result<_, _>>().unwrap();
        paths.sort();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("a.jpg"));
        assert!(paths[1].ends_with("b.png"));
    }

    #[test]
    fn supported_image_accepts_known_extensions_case_insensitively() {
        for ext in ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "tif"] {
            assert!(
                is_supported_image(Path::new(&format!("pic.{ext}"))),
                "{ext} should be accepted"
            );
            assert!(
                is_supported_image(Path::new(&format!("PIC.{}", ext.to_uppercase()))),
                "{ext} uppercase should be accepted"
            );
        }
    }

    #[test]
    fn supported_image_rejects_non_images_and_extensionless() {
        assert!(!is_supported_image(Path::new("data.dat")));
        assert!(!is_supported_image(Path::new("notes.txt")));
        assert!(!is_supported_image(Path::new("archive.zip")));
        assert!(!is_supported_image(Path::new("README")));
        assert!(!is_supported_image(Path::new("noext.")));
        // AVIF and JXL are deliberately NOT indexed: the build carries no decoder
        // for either, so indexing them would produce silently-broken thumbnails
        // (#139). If real decode support ever lands, add them back to the
        // allowlist and move these into the accepted-extensions test.
        assert!(!is_supported_image(Path::new("pic.avif")));
        assert!(!is_supported_image(Path::new("pic.jxl")));
    }

    #[test]
    fn walk_skips_non_image_files() {
        let dir = fixture_dir(&[
            ("photo.jpg", b"x"),
            ("sub/art.png", b"y"),
            ("data.dat", b"z"),
            ("notes.txt", b"w"),
            ("README", b"r"),
        ]);
        let mut paths: Vec<PathBuf> = walk(dir.path()).collect::<Result<_, _>>().unwrap();
        paths.sort();
        assert_eq!(paths.len(), 2, "only the two images should be yielded");
        assert!(paths[0].ends_with("photo.jpg"));
        assert!(paths[1].ends_with("art.png"));
    }

    #[test]
    fn fingerprint_reports_size_mtime_and_best_effort_creation_time_without_reading_contents() {
        let dir = fixture_dir(&[("only.jpg", b"solo")]);
        let path = dir.path().join("only.jpg");
        let fp = fingerprint(&path).expect("fingerprint");
        assert_eq!(fp.size, 4);
        assert!(fp.mtime.is_some());
        // Creation time is platform/filesystem dependent, but the field must exist
        // and be safe to read without turning an unavailable value into an error.
        let _ = fp.created_at;
    }

    #[test]
    fn hash_file_carries_creation_time() {
        let dir = fixture_dir(&[("only.jpg", b"solo")]);
        let rec = hash_file(&dir.path().join("only.jpg")).expect("hash one file");
        let fp = fingerprint(&rec.path).expect("fingerprint");
        assert_eq!(rec.created_at, fp.created_at);
    }
}
