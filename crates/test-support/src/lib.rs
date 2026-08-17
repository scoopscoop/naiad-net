//! Shared test helpers for the Naiad workspace.
//!
//! Per `AGENTS.md`, tests hit a **real** SQLite file (never a mock) and use real
//! files on disk. These helpers build those throwaway fixtures so the setup is
//! written once here rather than copied across test modules.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::thread::JoinHandle;

use naiad_db::Db;
use tempfile::TempDir;

/// A real SQLite [`Db`] backed by a file in a fresh temp directory.
///
/// The returned [`TempDir`] owns the directory; keep it alive for the test's
/// duration (dropping it deletes the database). Using a real file — not
/// `:memory:` — exercises the on-disk path, including WAL and migrations.
#[must_use]
pub fn temp_db() -> (Db, TempDir) {
    let dir = TempDir::new().expect("create temp dir");
    let db = Db::open(dir.path().join("naiad.db")).expect("open temp db");
    (db, dir)
}

/// Create a temp directory populated with the given `(relative_path, contents)`
/// fixtures. Intermediate subdirectories are created as needed.
///
/// ```ignore
/// let dir = fixture_dir(&[("a.txt", b"alpha"), ("sub/b.txt", b"beta")]);
/// ```
#[must_use]
pub fn fixture_dir(files: &[(&str, &[u8])]) -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture subdir");
        }
        write_file(&path, contents);
    }
    dir
}

fn write_file(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("write fixture file");
}

/// A throwaway [`naiad_daemon::ThumbStore`] backed by `thumbs.db` inside
/// `dir`. The store is created fresh and lives as long as `dir` is alive.
/// Tests use this to seed cached thumbnails before a request or to inspect
/// the cache afterwards.
#[must_use]
pub fn test_thumb_store(dir: &TempDir) -> naiad_daemon::ThumbStore {
    naiad_daemon::ThumbStore::open(&dir.path().join("thumbs.db")).expect("open test thumb store")
}

/// A daemon running on an ephemeral loopback port for the duration of a test.
///
/// The server runs on a background thread with its own current-thread Tokio
/// runtime; it has no graceful shutdown, so it is torn down when the test
/// process exits. Keep this value alive for the test's duration: dropping it
/// drops the temp directory that holds `thumbs.db` and `naiad.toml`.
pub struct TestDaemon {
    /// The bound address — point a client here.
    pub addr: SocketAddr,
    _thumbs: TempDir,
    _handle: JoinHandle<()>,
}

/// Start a daemon serving `db` on `127.0.0.1:0` (an OS-chosen free port) with a
/// throwaway `thumbs.db`. Blocks until the listener is bound, then
/// returns the [`TestDaemon`] (whose `addr` is ready to connect to). Live
/// file-watching is OFF.
#[must_use]
pub fn spawn_test_daemon(db: Db, thumb_size: u32) -> TestDaemon {
    spawn(db, thumb_size, false)
}

/// Like [`spawn_test_daemon`], but with the live filesystem watcher enabled.
/// The throwaway `thumbs.db` and `naiad.toml` are placed in the same temp dir.
#[must_use]
pub fn spawn_test_daemon_watching(db: Db, thumb_size: u32) -> TestDaemon {
    spawn(db, thumb_size, true)
}

fn spawn(db: Db, thumb_size: u32, watch: bool) -> TestDaemon {
    let thumbs = TempDir::new().expect("create thumbs dir");
    let thumbs_path = thumbs.path().to_path_buf();
    let store =
        naiad_daemon::ThumbStore::open(&thumbs_path.join("thumbs.db")).expect("open thumb store");
    let (tx, rx) = std::sync::mpsc::channel();

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind test daemon");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send bound addr");
            let mut state = naiad_daemon::AppState::new(db, store, thumb_size)
                .with_settings_path(thumbs_path.join("naiad.toml"));
            if watch {
                state = state.with_watch();
            }
            axum::serve(listener, naiad_daemon::app(state))
                .await
                .expect("serve test daemon");
        });
    });

    let addr = rx.recv().expect("test daemon failed to bind");
    TestDaemon {
        addr,
        _thumbs: thumbs,
        _handle: handle,
    }
}

/// A repository node running on an ephemeral loopback port for a test's
/// duration. Mirrors [`spawn_test_daemon`]: a background thread owns a
/// current-thread runtime; no graceful shutdown (torn down at process exit).
pub struct TestRepo {
    /// The bound address — point a `RepoClient` (or repo URL) here.
    pub addr: SocketAddr,
    _handle: JoinHandle<()>,
}

/// Serve `store` on `127.0.0.1:0` with the default crowd floor (`k = 1000`).
#[must_use]
pub fn spawn_test_repo(store: naiad_server::RepoStore) -> TestRepo {
    spawn_test_repo_with_k(store, 1000)
}

/// Like [`spawn_test_repo`], but with an explicit crowd floor `k` (set it low to
/// exercise the bucketed pull path over a small store). Blocks until bound, then
/// returns the [`TestRepo`] whose `addr` is ready to connect to.
#[must_use]
pub fn spawn_test_repo_with_k(store: naiad_server::RepoStore, k: u64) -> TestRepo {
    use std::sync::{Arc, Mutex};

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind test repo");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send bound addr");
            axum::serve(
                listener,
                naiad_server::app(Arc::new(Mutex::new(store)), k)
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("serve test repo");
        });
    });
    let addr = rx.recv().expect("test repo failed to bind");
    TestRepo {
        addr,
        _handle: handle,
    }
}
