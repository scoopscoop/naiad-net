//! SQLite-backed thumbnail cache (`thumbs.db`) — #118.
//!
//! Replaces the sharded one-JPEG-per-file tree: tens of thousands of tiny
//! files are the NTFS worst case (per-open MFT + filter-driver cost), while
//! SQLite reads 5–40 KB blobs faster than the filesystem. This is a *cache*:
//! corruption is answered by deleting the file and starting empty, a failed
//! write is logged and forgotten, and a read error is indistinguishable from
//! a miss. Design: docs/superpowers/specs/2026-07-25-thumbs-db-design.md.
//!
//! ## Schema: rowid table + unique index, not WITHOUT ROWID
//!
//! A rowid table B-tree keeps 20 KB blobs in ~4 KB-payload leaves; an index
//! B-tree (used by `WITHOUT ROWID`) caps local payload at ~1 KB, forcing
//! multi-page overflow chains for every thumbnail. Measured 6.7× slower point
//! reads with `WITHOUT ROWID`. `user_version = 2` reflects this amendment
//! (v1 caches used `WITHOUT ROWID` and are silently deleted and recreated on
//! first open).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};

use crate::lock::LockRecover;

/// Number of read-only connections. Scroll bursts land several cache hits at
/// once; a single reader would serialize them on cold pages. Four matches the
/// browser's per-origin connection cap, the actual arrival pattern.
const READERS: usize = 4;

/// Cache format stamp. Not a migration framework: a mismatch means "blow the
/// file away and start empty" (the recovery path in `open`). Bumped to 2 when
/// the schema changed from `WITHOUT ROWID` to a rowid table + unique index.
const USER_VERSION: i32 = 2;

/// Handle to the thumbnail cache. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct ThumbStore {
    inner: Arc<Inner>,
}

struct Inner {
    /// Round-robin read connections. Declared BEFORE `writer` so they are
    /// dropped first on shutdown: only the writer connection can checkpoint
    /// and unlink the WAL sidecars. If read-only connections are still open
    /// when the writer closes, SQLite cannot complete the checkpoint and leaves
    /// orphaned `-wal`/`-shm` files (measured 4.15 MB after a clean shutdown
    /// with the previous field order). Declaration order here is load-bearing.
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
    /// Single write connection. Declared LAST so it is dropped last.
    /// SQLite allows one writer; serializing at the handle is simpler and
    /// more predictable than letting `busy_timeout` arbitrate.
    writer: Mutex<Connection>,
}

impl ThumbStore {
    /// Open (or create) the thumbnail cache at `path`. If the existing file
    /// is corrupt or unopenable, delete it (plus `-wal`/`-shm` siblings) and
    /// retry once from scratch — a trashed cache must not stop the daemon.
    ///
    /// # Errors
    /// Returns an error only if the second (fresh-file) attempt also fails.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        match Self::try_open(path) {
            Ok(store) => Ok(store),
            Err(first) => {
                tracing::warn!(
                    target: "thumb",
                    "thumbs.db unusable ({first:#}); deleting and recreating"
                );
                // Blow away sidecars before the main file so a freshly
                // created database can never see an orphaned WAL.
                let mut any_remove_failed = false;
                for suffix in ["-shm", "-wal", ""] {
                    if let Err(e) = std::fs::remove_file(sibling(path, suffix)) {
                        // NotFound is expected for sidecars that simply do not
                        // exist; any other error (e.g. another process holding
                        // the file open on Windows) means the second open will
                        // also fail with a confusing error, so warn explicitly.
                        if e.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(
                                target: "thumb",
                                path = %sibling(path, suffix).display(),
                                "could not delete cache sidecar: {e}"
                            );
                            any_remove_failed = true;
                        }
                    }
                }
                if any_remove_failed {
                    tracing::warn!(
                        target: "thumb",
                        "could not delete cache sidecar(s); attempting fresh open anyway"
                    );
                }
                Self::try_open(path)
            }
        }
    }

    fn try_open(path: &Path) -> anyhow::Result<Self> {
        let writer = Connection::open(path)?;
        // auto_vacuum only takes effect if set before the first table is
        // created, hence before the schema below.
        writer.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        apply_pragmas(&writer)?;
        let version: i32 = writer.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => {
                // One atomic batch: DROP guards against a stale wrong-shape table
                // left by a pre-fix crash (user_version stayed 0 but the table
                // existed in a different form).  Wrapping everything in one
                // BEGIN IMMEDIATE…COMMIT means a crash cannot leave user_version=0
                // paired with a partially-created table that IF NOT EXISTS would
                // silently adopt on the next open.
                writer.execute_batch(
                    "BEGIN IMMEDIATE;
                     DROP TABLE IF EXISTS thumbs;
                     DROP INDEX IF EXISTS thumbs_key;
                     CREATE TABLE thumbs (
                       hash TEXT NOT NULL, size INTEGER NOT NULL, data BLOB NOT NULL);
                     CREATE UNIQUE INDEX thumbs_key ON thumbs(hash, size);
                     PRAGMA user_version = 2;
                     COMMIT;",
                )?;
            }
            USER_VERSION => {}
            other => anyhow::bail!("unknown thumbs.db format {other}"),
        }
        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            // journal_mode=WAL on a read-only connection is a no-op query
            // that returns the current mode; readers inherit WAL from the
            // file header automatically. Calling it here keeps apply_pragmas
            // uniform across writer and reader connections.
            apply_pragmas(&conn)?;
            readers.push(Mutex::new(conn));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                readers,
                next_reader: AtomicUsize::new(0),
                writer: Mutex::new(writer),
            }),
        })
    }

    /// Fetch a cached thumbnail. Blocking. A read error is deliberately
    /// indistinguishable from a miss: both mean "regenerate".
    pub fn get(&self, hash_hex: &str, size: u32) -> Option<Vec<u8>> {
        let idx = self.inner.next_reader.fetch_add(1, Ordering::Relaxed) % READERS;
        let conn = self.inner.readers[idx].lock_recover();
        match conn.query_row(
            "SELECT data FROM thumbs WHERE hash = ?1 AND size = ?2",
            rusqlite::params![hash_hex, size],
            |r| r.get::<_, Vec<u8>>(0),
        ) {
            Ok(bytes) => Some(bytes),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                tracing::debug!(target: "thumb", "thumbs.db read failed: {e}");
                None
            }
        }
    }

    /// Store a thumbnail. Blocking. Failures are logged, never returned — a
    /// failed cache write must not fail a thumbnail that was successfully
    /// generated (the caller already holds the bytes and will serve them).
    pub fn put(&self, hash_hex: &str, size: u32, data: &[u8]) {
        let conn = self.inner.writer.lock_recover();
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO thumbs (hash, size, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![hash_hex, size, data],
        ) {
            tracing::warn!(target: "thumb", "thumbs.db write failed: {e}");
        }
    }

    /// `get` for async callers — `spawn_blocking` wrapper so the hit path
    /// never blocks the runtime (and never touches a generation permit, #51).
    pub async fn get_async(&self, hash_hex: &str, size: u32) -> Option<Vec<u8>> {
        let store = self.clone();
        let hash = hash_hex.to_owned();
        tokio::task::spawn_blocking(move || store.get(&hash, size))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(target: "thumb", "thumb reader task panicked: {e}");
                None
            })
    }
}

/// Per-connection pragmas applied to every connection (writer and readers).
///
/// - `journal_mode=WAL`: concurrent readers never block the writer; on a
///   read-only connection this is a no-op query that returns the current mode
///   (readers inherit WAL from the file header).
/// - `synchronous=NORMAL`: WAL crash recovery makes `FULL` redundant —
///   committed transactions are durable after the next checkpoint, and this
///   is a regenerable cache where a lost write only costs a re-render.
/// - `busy_timeout=5000ms`: safety net for the rare case where OS scheduling
///   delays the writer mutex release; the Mutex already serializes writers so
///   this should never fire in practice.
fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

/// `path` with `suffix` appended to the file name (`thumbs.db-wal` etc.).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &tempfile::TempDir) -> ThumbStore {
        ThumbStore::open(&dir.path().join("thumbs.db")).unwrap()
    }

    #[test]
    fn roundtrip_and_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert_eq!(store.get("aa11", 64), None, "empty store must miss");
        store.put("aa11", 64, b"jpeg-bytes");
        assert_eq!(store.get("aa11", 64).as_deref(), Some(&b"jpeg-bytes"[..]));
        assert_eq!(store.get("bb22", 64), None, "unknown hash must miss");
    }

    #[test]
    fn size_buckets_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store.put("aa11", 64, b"small");
        store.put("aa11", 360, b"large");
        assert_eq!(store.get("aa11", 64).as_deref(), Some(&b"small"[..]));
        assert_eq!(store.get("aa11", 360).as_deref(), Some(&b"large"[..]));
    }

    #[test]
    fn put_overwrites_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store.put("aa11", 64, b"first");
        store.put("aa11", 64, b"second");
        assert_eq!(store.get("aa11", 64).as_deref(), Some(&b"second"[..]));
        let count: i64 = store
            .inner
            .writer
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM thumbs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "INSERT OR REPLACE must not accumulate rows");
    }

    #[test]
    fn corrupt_file_recovers_to_empty_working_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thumbs.db");
        std::fs::write(&path, b"this is not a sqlite database, not even close").unwrap();
        let store = ThumbStore::open(&path).expect("corrupt cache must recover");
        assert_eq!(store.get("aa11", 64), None);
        store.put("aa11", 64, b"fresh");
        assert_eq!(store.get("aa11", 64).as_deref(), Some(&b"fresh"[..]));
    }

    #[test]
    fn pragmas_applied() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let conn = store.inner.writer.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let av: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(av, 2, "2 = INCREMENTAL; must be set before table creation");
        // Reader connections also inherit WAL (either from the file header or
        // from the no-op pragma query); verify at least one.
        drop(conn);
        let reader = store.inner.readers[0].lock().unwrap();
        let reader_mode: String = reader
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reader_mode.to_lowercase(), "wal", "reader must report WAL");
    }

    #[test]
    fn version_mismatch_recovers_to_empty_working_store() {
        // Simulate a future incompatible format: stamp user_version=99 on a
        // valid-but-empty database, then assert that ThumbStore::open blows it
        // away and returns a fresh, working store.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thumbs.db");
        // Create a valid store first so the file is a real SQLite database.
        let store = ThumbStore::open(&path).unwrap();
        store.put("aa11", 64, b"stale");
        drop(store);
        // Stamp an unknown user_version directly.
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99i32).unwrap();
        }
        // ThumbStore::open must recover (delete and recreate) rather than error.
        let store = ThumbStore::open(&path).expect("version mismatch must recover");
        assert_eq!(
            store.get("aa11", 64),
            None,
            "store must be empty after recovery"
        );
        store.put("aa11", 64, b"fresh");
        assert_eq!(store.get("aa11", 64).as_deref(), Some(&b"fresh"[..]));
    }

    #[test]
    fn version_zero_wrong_shape_recovers_to_v2_rowid_table() {
        // Simulate a pre-fix crash: user_version stayed 0 but the WITHOUT ROWID
        // table from an older binary already existed.  On the next open the
        // version-0 branch must DROP the stale table and recreate it as a rowid
        // table (v2 schema), not silently adopt the old shape via IF NOT EXISTS.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thumbs.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE thumbs (
                   hash TEXT NOT NULL PRIMARY KEY,
                   size INTEGER NOT NULL,
                   data BLOB NOT NULL
                 ) WITHOUT ROWID;",
            )
            .unwrap();
            // user_version intentionally left at 0 (crash simulation)
        }
        let store = ThumbStore::open(&path).expect("stale v0 wrong-shape table must recover");
        // The recovered table must be a rowid table: sqlite_master DDL must
        // not contain WITHOUT ROWID.
        let ddl: String = store
            .inner
            .writer
            .lock()
            .unwrap()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='thumbs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !ddl.to_uppercase().contains("WITHOUT ROWID"),
            "recovered table must be a rowid table, got: {ddl}"
        );
        store.put("aa11", 64, b"fresh");
        assert_eq!(store.get("aa11", 64).as_deref(), Some(&b"fresh"[..]));
    }
}
