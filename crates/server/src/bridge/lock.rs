//! Single-writer guard for bridge write paths (issue #193).
//!
//! `bridge sync` and a bridge-enabled `serve` both open `bridge-state.db` +
//! `repo.db` read-write and advance the same PTR cursor. WAL keeps that safe
//! from corruption, but two writers duplicate every fetch and interleave logs.
//! This lock makes "the bridge writer" a single role: whoever holds an
//! exclusive transaction on the sibling `bridge.lock` file is it.
//!
//! Deliberately NOT acquired by `bridge seed` (hard constraint from #193:
//! seed writes defs tables and must never be blocked), nor by the read-only
//! `status` / `parity-audit` paths.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rusqlite::Connection;

/// Typed contention marker: another bridge writer already holds the lock.
///
/// Callers detect it with `err.is::<Contended>()` and map it to their own
/// policy (exit code 4 for `bridge sync`, log-and-degrade for `spawn_follow`).
#[derive(Debug, thiserror::Error)]
#[error("another bridge process appears to be running")]
pub struct Contended;

/// Derive the lock-file path: a sibling of the resolved `state_db` named
/// `bridge.lock`. Deriving from `state_db` (not from `db` or config) means a
/// relocated state DB keeps its lock beside it automatically.
pub fn lock_path(state_db: &Path) -> PathBuf {
    state_db.with_file_name("bridge.lock")
}

/// Holds SQLite's EXCLUSIVE lock on the dedicated lock file for the lifetime
/// of the value. The lock is an OS file lock tied to the open handle: dropped
/// on `Drop` and on process death alike, so no stale-lock state can exist.
pub struct BridgeLock {
    // Held only for its open EXCLUSIVE transaction; never queried.
    _conn: Connection,
    // Retained for symmetric acquire/release tracing in Drop.
    path: PathBuf,
}

impl std::fmt::Debug for BridgeLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeLock").finish_non_exhaustive()
    }
}

impl BridgeLock {
    /// Open (creating if absent) the lock database at `path` and take the
    /// EXCLUSIVE lock immediately.
    ///
    /// `busy_timeout(0)` makes contention fail instantly instead of waiting —
    /// the deliberate opposite of the 10 s timeout on the working DBs.
    ///
    /// # Errors
    /// Returns [`Contended`] (test with `err.is::<Contended>()`) when another
    /// process/connection holds the lock; any other failure (create, disk,
    /// permission) is an ordinary error and must not be treated as contention.
    pub fn acquire(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening bridge lock {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::ZERO)
            .context("setting lock busy timeout")?;
        match conn.execute_batch("BEGIN EXCLUSIVE") {
            Ok(()) => {
                tracing::debug!(target: "bridge", path = %path.display(), "bridge writer lock acquired");
                Ok(Self {
                    _conn: conn,
                    path: path.to_path_buf(),
                })
            }
            Err(e) if is_busy(&e) => Err(anyhow::Error::new(Contended)),
            Err(e) => Err(anyhow::Error::new(e).context("acquiring bridge lock")),
        }
    }
}

impl Drop for BridgeLock {
    // Mirrors the acquire debug line for symmetric writer-role visibility:
    // one log when the role is taken, one when it is relinquished.
    fn drop(&mut self) {
        tracing::debug!(target: "bridge", path = %self.path.display(), "bridge writer lock released");
    }
}

// `DatabaseLocked` is belt-and-suspenders: it cannot occur across separate
// non-shared-cache connections; real contention surfaces as `DatabaseBusy`.
fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_acquire_succeeds_and_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.lock");
        let _guard = BridgeLock::acquire(&path).unwrap();
        assert!(path.exists(), "lock file must be created");
    }

    #[test]
    fn second_acquire_is_contended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.lock");
        let _guard = BridgeLock::acquire(&path).unwrap();
        // A second Connection is a separate handle, so same-process contention
        // is real contention — no second OS process needed.
        let err = BridgeLock::acquire(&path).unwrap_err();
        assert!(err.is::<Contended>(), "expected Contended, got: {err:#}");
    }

    #[test]
    fn drop_releases_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.lock");
        let guard = BridgeLock::acquire(&path).unwrap();
        drop(guard);
        let _re = BridgeLock::acquire(&path).unwrap();
    }

    #[test]
    fn lock_path_is_sibling_of_state_db() {
        let p = lock_path(Path::new("/data/some-dir/bridge-state.db"));
        assert_eq!(p, Path::new("/data/some-dir/bridge.lock"));
    }

    #[test]
    fn acquire_missing_parent_errors_not_contended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-subdir").join("bridge.lock");
        let err = BridgeLock::acquire(&path).unwrap_err();
        assert!(
            !err.is::<Contended>(),
            "missing-parent error must not be Contended, got: {err:#}"
        );
    }
}
