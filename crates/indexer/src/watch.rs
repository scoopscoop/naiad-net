//! Live filesystem watching: turn raw `notify` events into the two actions the
//! daemon cares about ([`WatchEvent`]), debounced so editor/atomic-save bursts
//! collapse into a single event per path.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};

/// A coalesced change reduced to the daemon's two reindex actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A file was created or modified — re-hash and upsert it.
    Upsert(PathBuf),
    /// A file or directory was removed — mark it (and any descendants) missing.
    Remove(PathBuf),
}

/// Owns the debouncer. Dropping it stops watching and closes the event channel.
pub struct Watcher {
    debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
}

impl Watcher {
    /// Begin watching an additional root recursively on a running watcher.
    ///
    /// # Errors
    /// Returns the `notify` error if the path cannot be watched.
    pub fn watch_root(&mut self, root: &Path) -> notify::Result<()> {
        tracing::info!(target: "watch", root = %root.display(), "watching new root");
        self.debouncer
            .watch(absolute(root), RecursiveMode::Recursive)
    }
}

/// Start watching every path in `roots` recursively. Returns the [`Watcher`]
/// (keep it alive) and a receiver of debounced, translated [`WatchEvent`]s.
///
/// # Errors
/// Returns a `notify` error if the backend cannot be initialized or a root
/// cannot be watched.
pub fn watch(roots: &[PathBuf]) -> notify::Result<(Watcher, Receiver<WatchEvent>)> {
    let (tx, rx) = mpsc::channel();
    let mut debouncer = new_debouncer(
        Duration::from_millis(500),
        None,
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                for ev in events {
                    translate(&ev.kind, &ev.paths, &tx);
                }
            }
            Err(errors) => {
                tracing::warn!(target: "watch", count = errors.len(), "debouncer reported errors (dropped)");
            }
        },
    )
    .map_err(|e| {
        tracing::error!(target: "watch", error = %e, "file watcher failed to start");
        e
    })?;
    for root in roots {
        debouncer
            .watch(absolute(root), RecursiveMode::Recursive)
            .map_err(|e| {
                tracing::error!(target: "watch", error = %e, "file watcher failed to start");
                e
            })?;
    }
    tracing::info!(target: "watch", roots = roots.len(), debounce_ms = 500u64, "file watcher started");
    Ok((Watcher { debouncer }, rx))
}

/// Map one `notify` event kind + its paths to zero or more [`WatchEvent`]s.
fn translate(kind: &EventKind, paths: &[PathBuf], tx: &Sender<WatchEvent>) {
    let send = |ev: WatchEvent| {
        tracing::trace!(target: "watch", event = ?ev, "watch event");
        let _ = tx.send(ev);
    };
    // Upserts feed the index, so they are gated on the image allowlist — a
    // non-image dropped into a watched folder is ignored. Removes are never
    // filtered: a directory removal has no image extension yet must still
    // propagate to mark its descendants missing.
    let upsert = |p: &Path| {
        if crate::is_supported_image(p) {
            send(WatchEvent::Upsert(absolute(p)));
        } else {
            tracing::trace!(target: "watch", path = %p.display(), "skipped non-image upsert");
        }
    };
    match kind {
        EventKind::Create(_) => {
            for p in paths {
                upsert(p);
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // paths = [from, to]
            if let [from, to] = paths {
                send(WatchEvent::Remove(absolute(from)));
                upsert(to);
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            for p in paths {
                send(WatchEvent::Remove(absolute(p)));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            for p in paths {
                upsert(p);
            }
        }
        EventKind::Modify(_) => {
            for p in paths {
                upsert(p);
            }
        }
        EventKind::Remove(_) => {
            for p in paths {
                send(WatchEvent::Remove(absolute(p)));
            }
        }
        // Access / Any / Other carry no reindex action.
        _ => {}
    }
}

/// Lexically absolutize a path (no filesystem or symlink resolution), falling
/// back to the input on the rare error so an event is never dropped.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructing the watcher with no roots must not walk any tree — it
    /// returns instantly — and a root can then be registered on the running
    /// debouncer via `watch_root`. This is the guarantee the daemon relies on
    /// to keep the file watcher off the startup critical path.
    #[test]
    fn empty_roots_constructs_then_registers() {
        let (mut watcher, _events_rx) = watch(&[]).expect("empty-roots watch must construct");
        let dir = tempfile::tempdir().expect("tempdir");
        watcher
            .watch_root(dir.path())
            .expect("registering a root on a running watcher must succeed");
        // Release the OS watch handle before `dir` drops so its cleanup
        // succeeds on Windows.
        drop(watcher);
    }
}
