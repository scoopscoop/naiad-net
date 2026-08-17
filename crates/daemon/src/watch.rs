//! Live-watching glue: start the indexer's filesystem watcher and apply its
//! events to the shared database on a background thread.
//!
//! The watcher is constructed with **zero roots** so startup never blocks on
//! `notify-debouncer-full`'s per-root fingerprint walk (that walk dominates
//! startup on a large library). The persisted roots are then fed in over the
//! roots channel and registered one at a time on a background thread, with
//! per-root progress recorded in [`WatchStatus`] for the health payload.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use naiad_db::Db;
use naiad_indexer::WatchEvent;

use crate::lock::LockRecover;
use crate::ops;

/// One root that could not be registered with the watcher (path gone,
/// permission denied). Previously these were logged on the roots thread and
/// otherwise lost; now they surface in the health payload.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct WatchFailure {
    pub path: String,
    pub error: String,
}

/// Background registration status for the persisted roots, updated by the
/// roots thread as it drains the channel. Serialized into `/api/health`.
///
/// # Post-startup invariant
/// `complete` latches to `true` once every startup root is accounted for and
/// never resets. Post-startup calls to [`WatchHandle::register`] flow through
/// the same channel and thread, but `finish_ok`/`finish_err` are no-ops once
/// `complete` is latched, so `done` never overflows past `total`. This keeps
/// the health payload meaningful as "startup registration progress" rather than
/// a running tally of all roots ever watched.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct WatchStatus {
    /// Persisted roots to register at startup.
    pub total: usize,
    /// Successfully registered so far.
    pub done: usize,
    /// Root currently being registered, or `None`.
    pub current: Option<String>,
    /// Roots that failed to register.
    pub failed: Vec<WatchFailure>,
    /// `done + failed.len() >= total`. Latches true and never resets.
    pub complete: bool,
}

impl Default for WatchStatus {
    fn default() -> Self {
        Self::new(0)
    }
}

impl WatchStatus {
    /// A status with `total` roots pending. Zero roots is already complete.
    pub(crate) fn new(total: usize) -> Self {
        Self {
            total,
            done: 0,
            current: None,
            failed: Vec::new(),
            complete: total == 0,
        }
    }

    /// Mark that registration of `path` has begun.
    fn begin_root(&mut self, path: &Path) {
        self.current = Some(path.display().to_string());
    }

    /// Record a successful registration.
    /// No-op (except clearing `current`) if startup is already complete; see
    /// the struct-level doc for the post-startup invariant.
    fn finish_ok(&mut self) {
        self.current = None;
        if self.complete {
            return;
        }
        self.done += 1;
        self.refresh_complete();
    }

    /// Record a failed registration (does not abort the loop).
    /// No-op (except clearing `current`) if startup is already complete; see
    /// the struct-level doc for the post-startup invariant.
    fn finish_err(&mut self, path: &Path, error: String) {
        self.current = None;
        if self.complete {
            return;
        }
        self.failed.push(WatchFailure {
            path: path.display().to_string(),
            error,
        });
        self.refresh_complete();
    }

    /// `complete` latches true once every startup root is accounted for.
    fn refresh_complete(&mut self) {
        if self.done + self.failed.len() >= self.total {
            self.complete = true;
        }
    }
}

/// A cheap, clonable handle for registering new roots with a running watcher
/// and reading background-registration status.
#[derive(Clone)]
pub(crate) struct WatchHandle {
    roots_tx: Sender<PathBuf>,
    status: Arc<Mutex<WatchStatus>>,
}

impl WatchHandle {
    /// Ask the watcher to begin watching `path` (best-effort; errors are
    /// recorded in [`WatchStatus::failed`] by the roots thread).
    pub(crate) fn register(&self, path: PathBuf) {
        let _ = self.roots_tx.send(path);
    }

    /// A snapshot of the current background-registration status.
    pub(crate) fn status(&self) -> WatchStatus {
        self.status.lock_recover().clone()
    }
}

/// Start watching the database's registered roots without blocking on the
/// per-root fingerprint walk. Spawns three detached threads: one applies
/// events to `db`, one owns the watcher and registers roots off the channel,
/// one feeds the persisted roots into that channel. All wind down when the
/// returned [`WatchHandle`] (held in `AppState`) is dropped.
///
/// # Errors
/// Returns an error if the roots cannot be read or the watcher backend cannot
/// be initialized.
pub(crate) fn start(db: Arc<Mutex<Db>>) -> anyhow::Result<WatchHandle> {
    let roots = db.lock_recover().list_roots()?;
    // Empty-roots construction: instant, no tree walk (see indexer::watch).
    let (mut watcher, events_rx) = naiad_indexer::watch(&[])?;
    let status = Arc::new(Mutex::new(WatchStatus::new(roots.len())));

    // Thread A: apply debounced events to the shared DB.
    let db_apply = db.clone();
    std::thread::Builder::new()
        .name("naiad-watch-apply".into())
        .spawn(move || {
            for ev in events_rx {
                let db = db_apply.lock_recover();
                let outcome = match &ev {
                    WatchEvent::Upsert(p) => ops::reindex_upsert(&db, p),
                    WatchEvent::Remove(p) => ops::reindex_remove(&db, p),
                };
                match outcome {
                    Ok(()) => tracing::debug!(target: "watch", "watch: applied {ev:?}"),
                    Err(e) => {
                        tracing::warn!(target: "watch", "watch: applying {ev:?} failed: {e:#}")
                    }
                }
            }
        })?;

    // Thread B: own the watcher (keeping the event stream alive) and register
    // roots as they arrive, recording per-root progress into `status`.
    let (roots_tx, roots_rx) = std::sync::mpsc::channel::<PathBuf>();
    let status_b = status.clone();
    std::thread::Builder::new()
        .name("naiad-watch-roots".into())
        .spawn(move || {
            for path in roots_rx {
                status_b.lock_recover().begin_root(&path);
                match watcher.watch_root(&path) {
                    Ok(()) => status_b.lock_recover().finish_ok(),
                    Err(e) => {
                        tracing::warn!(target: "watch", "watch: cannot watch {}: {e:#}", path.display());
                        status_b.lock_recover().finish_err(&path, format!("{e:#}"));
                    }
                }
            }
        })?;

    // Thread C: feed the persisted roots into the channel in the background so
    // `start` returns immediately and the server can bind. Runtime additions
    // still arrive via `WatchHandle::register`.
    let feed_tx = roots_tx.clone();
    std::thread::Builder::new()
        .name("naiad-watch-feed".into())
        .spawn(move || {
            for root in roots {
                if feed_tx.send(root).is_err() {
                    break; // watcher dropped; stop feeding.
                }
            }
        })?;

    Ok(WatchHandle { roots_tx, status })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn status_empty_is_complete() {
        let s = WatchStatus::new(0);
        assert_eq!(s.total, 0);
        assert!(s.complete, "zero roots is immediately complete");
        assert!(s.failed.is_empty());
    }

    #[test]
    fn status_counts_success_and_failure() {
        let mut s = WatchStatus::new(2);
        assert!(!s.complete);

        let ok = PathBuf::from("D:/img/newstuff");
        s.begin_root(&ok);
        assert_eq!(s.current.as_deref(), Some("D:/img/newstuff"));
        s.finish_ok();
        assert_eq!(s.done, 1);
        assert!(s.current.is_none());
        assert!(!s.complete, "one of two roots done");

        let bad = PathBuf::from("E:/gone");
        s.begin_root(&bad);
        s.finish_err(&bad, "path not found".into());
        assert_eq!(s.failed.len(), 1);
        assert_eq!(s.failed[0].path, "E:/gone");
        assert_eq!(s.failed[0].error, "path not found");
        assert!(
            s.complete,
            "1 done + 1 failed == 2 total, should be complete"
        );
    }

    #[test]
    fn status_completes_when_all_roots_accounted() {
        let mut s = WatchStatus::new(2);
        let a = PathBuf::from("A");
        let b = PathBuf::from("B");
        s.begin_root(&a);
        s.finish_ok();
        s.begin_root(&b);
        s.finish_err(&b, "boom".into());
        assert!(s.complete, "1 done + 1 failed == 2 total");
        assert_eq!(s.done, 1);
        assert_eq!(s.failed.len(), 1);
    }

    /// Post-startup `register` calls must not overflow `done` past `total`:
    /// once `complete` latches, `finish_ok` / `finish_err` update `current`
    /// but leave the counters frozen so the health payload stays meaningful.
    #[test]
    fn post_startup_register_does_not_overflow_done() {
        let mut s = WatchStatus::new(1);
        let root = PathBuf::from("A");
        s.begin_root(&root);
        s.finish_ok();
        assert!(s.complete);
        assert_eq!(s.done, 1);

        // Simulate a runtime WatchHandle::register call arriving after startup.
        let extra = PathBuf::from("B");
        s.begin_root(&extra);
        s.finish_ok();
        assert_eq!(
            s.done, 1,
            "done must not exceed total after complete latches"
        );
        assert!(s.complete);
        assert!(s.current.is_none(), "current cleared even when clamped");

        // Same for a runtime registration that fails.
        s.begin_root(&extra);
        s.finish_err(&extra, "gone".into());
        assert_eq!(s.failed.len(), 0, "post-startup failures not recorded");
        assert!(s.current.is_none());
    }
}
