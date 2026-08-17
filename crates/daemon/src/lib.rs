//! `naiad-daemon` — the client daemon: owns the library (`db`+`indexer`) and
//! serves the local HTTP API plus the bundled web UI. `core`/`db`/`indexer`
//! stay synchronous; all async/HTTP/image concerns live here.

mod account;
mod catchup;
mod lock;
mod ops;
/// SHA-256 backfill and Hydrus plugin import operations. Exposed as `pub` so
/// integration tests can call `backfill_sha256` directly.
pub mod plugins;
mod read_pool;
mod server;
mod settings;
mod startup_gate;
mod tag_lane;
mod thumb;
mod thumb_queue;
pub(crate) mod thumb_store;
mod thumb_stream;
mod ui;
mod warmup;
mod watch;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use naiad_db::{Db, ReadScope};

use crate::catchup::CatchupShared;
use crate::startup_gate::StartupGate;

pub use ops::{
    BackupError, BackupResult, CapsCache, FilePullOutcome, ImportSummary, RescanProgress,
    ScanProfile, SubmitError, add_parent, add_sibling, add_tags, display_tags,
    display_tags_detailed, do_backup, import_path, list_parents, list_relations, list_repos,
    list_roots, list_siblings, list_tags, mark_missing_under, pull_relations, pull_repo,
    pull_repo_for_hashes, register_root, reindex_remove, reindex_upsert, reject_mapping,
    relation_status, remove_parent, remove_repo, remove_root, remove_sibling, remove_tags,
    report_mapping, rescan_roots, resolve_file, scan_streaming, search, set_repo_priority,
    submit_relation, submit_to_repo, tag_relations,
};
pub use server::app;
pub use settings::{
    PrivacySettings, Settings, SettingsStore, migrate_trust_floor_to_file, settings_path_for,
};
pub use thumb_queue::{LifoPermits, Permit};
pub use thumb_store::ThumbStore;

/// Concurrent thumbnail *generations* (cache hits don't take a permit). Scale
/// with the machine: generation is CPU-bound decode/resize work, so run close
/// to core count while leaving headroom for light API handlers and the writer.
fn default_thumb_concurrency() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().saturating_sub(2).max(4))
}

/// Pool size: enough lanes that a couple of slow searches, /thumb + /file
/// location lookups, and a thumbnail burst don't exhaust it, without opening
/// a file handle per core.
fn read_pool_size() -> usize {
    std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).clamp(2, 6))
}

/// How long the background cache warmup waits for the first gallery query before
/// running anyway. Bounds the deferral for a headless daemon that never issues a
/// query; the UI's first `/api/search` normally fires within a second of bind,
/// so in practice the warmup starts as soon as that query returns (#121).
const CACHE_WARMUP_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Backstop for how long the startup catch-up scan defers to the cache warmup
/// before running regardless. The scan normally starts the moment
/// [`AppState::warmup_done`] fires; this only bounds the wait if the warmup task
/// dies or stalls, so the catch-up (which the live watcher does not cover) always
/// eventually runs (#126). Generous because a cold warmup's completion walk alone
/// can take ~2 minutes on the 95k-file library.
const CATCHUP_SCAN_DEFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Backstop for how long an interactive tag read (completion / detail) waits for
/// the background warmup to build the merged relation graph before giving up and
/// letting its own connection build it. Only reached if the warmup wedges; on a
/// normal cold start the graph-ready signal fires within the build's duration,
/// well under this bound (#126).
const RELATION_GRAPH_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How often the WAL backstop looks at `naiad.db-wal` (#232). The check is a
/// single `stat` — the writer mutex is touched only when the file has actually
/// outgrown [`naiad_db::WAL_SIZE_LIMIT`], so the cadence can be tight enough
/// to catch growth during a long import.
const WAL_BACKSTOP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Shared, cheaply-clonable server state. The `Db` wraps a non-`Sync` rusqlite
/// connection, so it lives behind a `Mutex`; handlers do blocking DB and image
/// work inside `tokio::task::spawn_blocking`.
#[derive(Clone)]
pub struct AppState {
    pub(crate) db: Arc<Mutex<Db>>,
    /// Read-only connection pool for API reads and location lookups. `None` for
    /// in-memory tests (a second connection to `:memory:` would be a different,
    /// empty database), in which case reads fall back to the writer.
    pub(crate) read_pool: Option<Arc<read_pool::ReadPool>>,
    /// Dedicated, cancellable read-only lane for completion, namespace listing,
    /// and tag-detail reads. Its own connection keeps tag UI responsive when
    /// pooled readers are busy; dropping a lane request stops or skips its work.
    pub(crate) tag_db: Option<tag_lane::TagLane>,
    pub(crate) thumb_store: thumb_store::ThumbStore,
    pub(crate) thumb_size: u32,
    pub(crate) thumb_permits: Arc<LifoPermits>,
    pub(crate) ui_dir: Option<Arc<PathBuf>>,
    pub(crate) watch: Option<watch::WatchHandle>,
    /// The address the server is bound to, if known. Used by the Host-header
    /// guard to allow the server's own authority in addition to loopback names.
    /// `None` in in-process tests (which drive the router without binding).
    pub(crate) bound_addr: Option<SocketAddr>,
    /// Location of the Ed25519 key file (sibling of the DB). `None` in in-process
    /// tests that do not exercise the publish path.
    pub(crate) key_path: Option<Arc<PathBuf>>,
    /// Location-backed client settings (`naiad.toml`). `None` in in-process
    /// tests that do not exercise settings (reads then default; writes error).
    pub(crate) settings: Option<Arc<SettingsStore>>,
    /// Per-service caps cache (fetch-once per session). Shared across all
    /// handlers; avoids a redundant `/repo/caps` round-trip per reject or
    /// report for the same service.
    pub(crate) caps_cache: Arc<CapsCache>,
    /// Directory that contains the database file (`naiad.db`). Used by the
    /// backup handler to resolve the default `backups/` subdirectory.
    /// `None` for in-memory / test states that do not exercise the backup path.
    pub(crate) db_dir: Option<Arc<PathBuf>>,
    /// Full path to the database file itself (e.g. `/data/naiad.db`). The
    /// backup handler opens a fresh read-only connection here for `VACUUM INTO`
    /// so the writer mutex is never held during the snapshot.
    /// `None` for in-memory / test states that do not exercise the backup path.
    pub(crate) db_path: Option<Arc<PathBuf>>,
    /// Whether the daemon is permitted to serve non-loopback peers. Read once at
    /// startup from `[net].allow_remote` in `naiad.toml`; never re-read per
    /// request. `false` in in-process tests (the safe default).
    pub(crate) allow_remote: bool,
    /// Progress of the startup catch-up rescan, written by the
    /// `naiad-catchup-scan` thread and read by `health_handler`. Defaults to an
    /// idle status (no scan run) for in-process tests.
    pub(crate) catchup: CatchupShared,
    /// Holds the background cache warmup until the first gallery query has been
    /// served (or a fallback timeout), so the warmup's large cold-page reads do
    /// not starve the UI's first `/api/search` on a cold OS file cache (#121).
    pub(crate) startup_gate: Arc<StartupGate>,
    /// Fires when the background cache warmup finishes (relation graph built +
    /// completion pages warmed). The catch-up scan waits on this before starting
    /// its disk-saturating pass so the warmup runs on an uncontended disk (#126).
    /// Pre-fired at construction (fail-open, #132); `spawn_cache_warmup` re-arms
    /// it before launching the real warmup task, so an unarmed gate is always
    /// already fired — consumers fall through instantly rather than eating the
    /// 300s backstop.
    pub(crate) warmup_done: Arc<StartupGate>,
    /// Fires as soon as the background warmup has built the merged relation graph
    /// (before the longer completion walk). Interactive completion/detail reads
    /// wait on this — off the tag lane and off the read pool — so a cold graph
    /// build lands on the warmup connection, never on the single-connection tag
    /// lane where it would stall every other tag read (#126).
    /// Pre-fired at construction (fail-open, #132); `spawn_cache_warmup` re-arms
    /// it before launching the real warmup task, so an unarmed gate is always
    /// already fired — consumers fall through instantly rather than eating the
    /// 60s backstop.
    pub(crate) graph_ready: Arc<StartupGate>,
    /// Phase of the background cache warmup, advanced by
    /// [`spawn_cache_warmup`](AppState::spawn_cache_warmup) as it runs. Read by
    /// `health_handler` so the UI can show a "Preparing library" job (#130), and
    /// by `await_relation_graph` to tell "a warmup will fire `graph_ready`" from
    /// "no warmup was ever spawned" (#131).
    pub(crate) warmup: crate::warmup::WarmupShared,
}

impl AppState {
    /// Build server state from an open database, a thumbnail store, and the
    /// thumbnail bounding-box edge length in pixels (the longer image edge is
    /// scaled to this; aspect ratio is preserved). The UI directory defaults to off
    /// (the embedded Svelte UI is served); set it with [`AppState::with_ui_dir`].
    #[must_use]
    pub fn new(db: Db, thumb_store: thumb_store::ThumbStore, thumb_size: u32) -> Self {
        Self {
            db: Arc::new(Mutex::new(db)),
            read_pool: None,
            tag_db: None,
            thumb_store,
            thumb_size,
            thumb_permits: LifoPermits::new(default_thumb_concurrency()),
            ui_dir: None,
            watch: None,
            bound_addr: None,
            key_path: None,
            settings: None,
            caps_cache: CapsCache::new(),
            db_dir: None,
            db_path: None,
            allow_remote: false,
            catchup: crate::catchup::new_shared(),
            startup_gate: Arc::new(StartupGate::new()),
            warmup_done: Arc::new(StartupGate::fired()),
            graph_ready: Arc::new(StartupGate::fired()),
            warmup: crate::warmup::new_shared(),
        }
    }

    /// Limit concurrent thumbnail requests before they enter Tokio's global
    /// blocking pool. This keeps image decode/resize bursts from starving light
    /// API work such as tag completion.
    #[must_use]
    pub fn with_thumb_concurrency(mut self, permits: usize) -> Self {
        self.thumb_permits = LifoPermits::new(permits);
        self
    }

    /// Handle to the thumbnail generation queue. Test-visibility hook: lets
    /// integration tests occupy slots and synchronize on queue depth.
    #[doc(hidden)]
    #[must_use]
    pub fn thumb_permits(&self) -> Arc<LifoPermits> {
        self.thumb_permits.clone()
    }

    /// Handle to the thumbnail cache (`thumbs.db`). Integration tests use it
    /// to seed entries before a request or inspect the cache after one.
    #[must_use]
    pub fn thumb_store(&self) -> &thumb_store::ThumbStore {
        &self.thumb_store
    }

    /// Number of actual `/repo/caps` fetches performed so far.
    ///
    /// Gated to test/debug builds: integration tests compile in debug mode
    /// (`debug_assertions = true`), so this is reachable in all test
    /// configurations without being present in release builds.
    #[cfg(any(test, debug_assertions))]
    #[doc(hidden)]
    #[must_use]
    pub fn caps_fetch_count(&self) -> usize {
        self.caps_cache.fetch_count()
    }

    /// Attach a read-only connection pool (opened on the same DB file) for read
    /// endpoints, plus a dedicated cancellable lane for completion, namespace,
    /// and tag-detail reads. Non-fatal: failures fall back to the writer.
    #[must_use]
    pub fn with_read_db(mut self, path: &Path) -> Self {
        // One shared relation-graph cache for every read connection (pool + tag
        // lane): the ~600MB merged graph is built once on cold start, not once
        // per connection (#70).
        let cache = Db::new_relation_cache();
        match read_pool::ReadPool::open(path, read_pool_size(), &cache) {
            Ok(pool) => self.read_pool = Some(pool),
            Err(e) => {
                tracing::warn!(target: "startup", "read pool disabled (reads share the writer): {e}")
            }
        }
        match Db::open_readonly_with_cache(path, cache) {
            Ok(tag) => self.tag_db = Some(tag_lane::TagLane::new(tag)),
            Err(e) => {
                tracing::warn!(target: "startup", "tag lane disabled (completion shares the pool): {e}")
            }
        }
        self.spawn_cache_warmup();
        self
    }

    /// Warm the shared relation graph and tag-completion pages in the background.
    /// The relation graph is built first so the shared cache is populated before
    /// the long completion walk, keeping the ~34s cold build off the interactive
    /// tag lane (#126); the graph is still built once instead of by 5–6 cold
    /// readers (#70), and completion's index/table/count scans (#76) run behind it.
    /// Best-effort: runs on a checked-out pool connection, logs its elapsed time,
    /// and never blocks server bind. Fires [`AppState::warmup_done`] when the real
    /// warmup task finishes. The construction-time gates are pre-fired (#132); this
    /// re-arms them only when a real warmup task will fire them, so an unarmed gate
    /// always reads as fired — consumers fall through instantly instead of eating
    /// their backstop timeouts.
    /// Spawn the periodic WAL backstop (#232): every [`WAL_BACKSTOP_INTERVAL`]
    /// stat `naiad.db-wal`; when it has outgrown [`naiad_db::WAL_SIZE_LIMIT`],
    /// run `wal_checkpoint(TRUNCATE)` on the writer and log the result.
    ///
    /// SQLite's passive auto-checkpoints give up (rather than wait) when a
    /// pooled reader overlaps, and never shrink the file — so without this a
    /// write burst (bulk import, tag edits) leaves the WAL at its high-water
    /// mark forever, or growing while readers keep starving the passive pass.
    /// The size gate keeps the steady state free: no writer-mutex traffic
    /// until there is actually something worth reclaiming. No-op when
    /// `db_path` is unknown (in-memory tests).
    fn spawn_wal_backstop(&self) {
        let Some(db_path) = self.db_path.clone() else {
            return;
        };
        let db = self.db.clone();
        let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(WAL_BACKSTOP_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let wal_len = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
                if wal_len <= naiad_db::WAL_SIZE_LIMIT as u64 {
                    continue;
                }
                let db = db.clone();
                let res = tokio::task::spawn_blocking(move || {
                    db.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .checkpoint_wal()
                })
                .await;
                match res {
                    Ok(Ok(cp)) if cp.busy => tracing::warn!(
                        target: "db",
                        wal_bytes = wal_len,
                        log_frames = cp.log_frames,
                        checkpointed_frames = cp.checkpointed_frames,
                        "WAL backstop: checkpoint blocked by a reader; starvation if persistent"
                    ),
                    Ok(Ok(cp)) => tracing::info!(
                        target: "db",
                        wal_bytes = wal_len,
                        log_frames = cp.log_frames,
                        checkpointed_frames = cp.checkpointed_frames,
                        "WAL backstop: checkpointed and truncated"
                    ),
                    Ok(Err(e)) => tracing::warn!(
                        target: "db",
                        error = %e,
                        "WAL backstop: checkpoint failed"
                    ),
                    Err(e) => tracing::warn!(
                        target: "db",
                        error = %e,
                        "WAL backstop: blocking task failed"
                    ),
                }
            }
        });
    }

    fn spawn_cache_warmup(&mut self) {
        let Some(pool) = self.read_pool.clone() else {
            // No read pool to warm. The gates were constructed pre-fired (#132),
            // so leaving them alone already means "nothing to wait for". Leave the
            // phase at Idle — nothing was warmed, so `/api/health` reports idle
            // rather than a phantom completed warmup (#130).
            return;
        };
        // Re-arm the pre-fired construction gates: from here a real warmup task
        // owns them and will fire them as it progresses. Must happen before
        // anyone clones the gates, so assert the known ordering (`serve` calls
        // with_read_db before with_watch, which clones warmup_done).
        debug_assert!(
            self.watch.is_none(),
            "with_read_db must be called before with_watch"
        );
        self.graph_ready = Arc::new(StartupGate::new());
        self.warmup_done = Arc::new(StartupGate::new());
        // From here the warmup is real work the UI should be able to see. It is
        // parked, not working, until the startup gate releases below.
        let warmup = self.warmup.clone();
        warmup.set(crate::warmup::WarmupPhase::Queued);
        let gate = self.startup_gate.clone();
        let warmup_done = self.warmup_done.clone();
        let graph_ready = self.graph_ready.clone();
        tokio::spawn(async move {
            // Let the first gallery query go first. On a cold OS file cache the
            // warmup's large sequential reads would otherwise run alongside the
            // UI's first `/api/search` and starve it — a 40s+ blank gallery on
            // the 95k-file library (#121). The query faults in the pages it needs
            // uncontended; the warmup then runs behind it. The timeout keeps a
            // headless daemon (no first query) warming on its own.
            gate.wait(CACHE_WARMUP_GATE_TIMEOUT).await;
            // Released — either by the first query or by the backstop timeout.
            // The phase is set here, not derived from the gate, because the
            // timeout path leaves the gate unfired while the warmup is very much
            // working.
            warmup.set(crate::warmup::WarmupPhase::Graph);
            let started = std::time::Instant::now();
            // Build the relation graph FIRST, then release the graph-wait: any
            // interactive completion/detail read blocked in `await_relation_graph`
            // now proceeds to a warm cache instead of building the ~34s graph on
            // the single tag-lane connection (#126). Fire even on error so those
            // reads fall through and build it themselves rather than hang.
            match pool
                .run(move |db| db.warm_relation_graph(ReadScope::Merged))
                .await
            {
                Ok(Ok(())) => tracing::info!(
                    target: "startup",
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "relation-graph cache warmed"
                ),
                Ok(Err(e)) => {
                    tracing::warn!(target: "startup", "relation-graph warmup failed: {e}")
                }
                Err(e) => tracing::warn!(target: "startup", "relation-graph warmup panicked: {e}"),
            }
            graph_ready.fire();
            warmup.set(crate::warmup::WarmupPhase::Completion);
            // Then warm the tag-completion index/table/count pages (#76), the
            // longer walk. This runs behind the graph build so a typed prefix
            // hits warm pages without waiting on the graph first.
            match pool.run(move |db| db.warm_tag_completion()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(target: "startup", "completion warmup failed: {e}"),
                Err(e) => tracing::warn!(target: "startup", "completion warmup panicked: {e}"),
            }
            // Release the catch-up scan regardless of outcome: even if warmup
            // errored, the caches are as warm as they are going to get and the
            // scan must not be deferred forever. Mark done before firing so a
            // health read that races the gate never reports an earlier phase
            // than the gate already implies.
            warmup.set(crate::warmup::WarmupPhase::Done);
            warmup_done.fire();
        });
    }

    /// Record the address the server bound to, so the Host-header guard can
    /// accept the server's own authority (not just loopback names).
    #[must_use]
    pub fn with_bound_addr(mut self, addr: SocketAddr) -> Self {
        self.bound_addr = Some(addr);
        self
    }

    /// Set whether the daemon is allowed to serve non-loopback peers. When
    /// `false` (the default), the source-address guard rejects any connection
    /// whose peer IP is not loopback. Read once at startup from
    /// `[net].allow_remote` in `naiad.toml` and forwarded here via `serve`.
    #[must_use]
    pub fn with_allow_remote(mut self, allow_remote: bool) -> Self {
        self.allow_remote = allow_remote;
        self
    }

    /// Set the account key-file location (enables the publish/account routes).
    #[must_use]
    pub fn with_key_path(mut self, key_path: PathBuf) -> Self {
        self.key_path = Some(Arc::new(key_path));
        self
    }

    /// Set the database directory (enables the backup route's default
    /// destination: `<db_dir>/backups/naiad-YYYYMMDD-HHMMSS.db`).
    #[must_use]
    pub fn with_db_dir(mut self, db_dir: PathBuf) -> Self {
        self.db_dir = Some(Arc::new(db_dir));
        self
    }

    /// Set the full path to the database file. The backup handler opens a
    /// fresh read-only connection here so `VACUUM INTO` never holds the writer
    /// mutex. Call this together with [`AppState::with_db_dir`].
    #[must_use]
    pub fn with_db_path(mut self, path: PathBuf) -> Self {
        self.db_path = Some(Arc::new(path));
        self
    }

    /// Set the client settings-file location (`naiad.toml`), enabling the trust
    /// floor read/write path.
    #[must_use]
    pub fn with_settings_path(mut self, path: PathBuf) -> Self {
        self.settings = Some(Arc::new(SettingsStore::new(path)));
        self
    }

    /// Serve a built UI directory (e.g. `ui/dist`) at `/`. When `None` (the
    /// default), the embedded Svelte UI is served as the fallback. The API and
    /// media routes always take precedence over the static files.
    #[must_use]
    pub fn with_ui_dir(mut self, ui_dir: Option<PathBuf>) -> Self {
        self.ui_dir = ui_dir.map(Arc::new);
        self
    }

    /// Start the live filesystem watcher for the registered roots and keep its
    /// handle, then kick off a background catch-up rescan of those roots. The
    /// watcher only sees events that happen while the daemon runs, so the rescan
    /// is what picks up files an interrupted import never flushed and files
    /// added while the daemon was down; it starts *after* the watcher attaches
    /// so nothing can land in a gap between the two. Watch failures are
    /// non-fatal: they are logged and watching is left off (the rescan still
    /// runs). Call this only from a real `serve` (not in-process tests).
    #[must_use]
    pub fn with_watch(mut self) -> Self {
        match crate::watch::start(self.db.clone()) {
            Ok(handle) => self.watch = Some(handle),
            Err(e) => tracing::warn!(target: "watch", "file watching disabled: {e:#}"),
        }
        let db = self.db.clone();
        let catchup = self.catchup.clone();
        let catchup_err = self.catchup.clone();
        let warmup_done = self.warmup_done.clone();
        // Mark the scan running before anything starts, so the very first health
        // poll during startup already sees it (mirrors #110's watcher). It stays
        // "running" across the warmup deferral below — the scan is pending, not idle.
        {
            let mut c = catchup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            c.running = true;
        }
        // Defer the catch-up scan's disk-saturating pass until the cache warmup
        // has finished (bounded by a backstop timeout). On a cold OS file cache
        // the scan's 95k-file fingerprint/stat storm otherwise starves the warmup
        // and the first interactive tag completion, stalling them for tens of
        // seconds (#126). The live watcher is already attached, so deferring the
        // catch-up does not reopen an event gap — only files added while the
        // daemon was down wait a little longer to be reconciled.
        tokio::spawn(async move {
            warmup_done.wait(CATCHUP_SCAN_DEFER_TIMEOUT).await;
            if let Err(e) = std::thread::Builder::new()
                .name("naiad-catchup-scan".into())
                .spawn(move || {
                    let result = ops::rescan_roots(&db, |p| {
                        let mut c = catchup
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        c.running = true;
                        c.imported = p.imported;
                        c.errors = p.errors;
                        c.roots_total = p.roots_total;
                        c.roots_done = p.roots_done;
                        c.current = p.current.as_ref().map(|path| path.display().to_string());
                    });
                    // Latch completion no matter what — never leave `running` stuck.
                    let mut c = catchup
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    c.running = false;
                    c.current = None;
                    c.complete = true;
                    match result {
                        Ok(s) => {
                            c.imported = s.imported;
                            c.errors = s.errors;
                            drop(c);
                            tracing::info!(
                                target: "scan",
                                imported = s.imported,
                                errors = s.errors,
                                marked_missing = s.marked_missing,
                                "startup catch-up scan finished"
                            );
                        }
                        Err(e) => {
                            drop(c);
                            tracing::warn!(target: "scan", "startup catch-up scan failed: {e:#}");
                        }
                    }
                })
            {
                // Thread never launched: the scan will not run, so unlatch
                // `running` and mark complete so the UI does not wait on a scan
                // that is not coming.
                let mut c = catchup_err
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                c.running = false;
                c.complete = true;
                drop(c);
                tracing::warn!(target: "scan", "startup catch-up scan not started: {e:#}");
            }
        });
        self
    }
}

/// Whether `addr` may be bound given the `allow_remote` opt-in. A loopback IP
/// is always allowed. A non-loopback IP — including the wildcards `0.0.0.0` and
/// `::` — is allowed only when `allow_remote` is true.
fn bind_allowed(addr: SocketAddr, allow_remote: bool) -> bool {
    addr.ip().is_loopback() || allow_remote
}

/// Bind `addr` and return the listener together with its *actual* local
/// address. Resolving the bound address here (instead of echoing the requested
/// `addr`) makes an ephemeral `127.0.0.1:0` request report the real port it
/// landed on — which the Tauri desktop shell parses from the startup line.
async fn bind(addr: SocketAddr) -> std::io::Result<(tokio::net::TcpListener, SocketAddr)> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

/// Bind `addr` and serve until shutdown. `ui_dir`, when set, serves a built UI
/// directory at `/` (otherwise the embedded Svelte UI is used). `allow_remote`
/// is read once by the caller from `[net].allow_remote` and forwarded here so
/// the source-address guard never re-reads it per request.
///
/// # Errors
/// Returns an error if the address cannot be bound or the server fails.
// Plumbing entry point: each argument maps to one piece of server config and is
// forwarded straight into `AppState`; a config struct would add indirection
// without reducing the real parameter count.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    db: Db,
    addr: SocketAddr,
    thumb_store: thumb_store::ThumbStore,
    key_path: PathBuf,
    thumb_size: u32,
    ui_dir: Option<PathBuf>,
    watch: bool,
    settings_path: PathBuf,
    read_db_path: Option<PathBuf>,
    allow_remote: bool,
) -> anyhow::Result<()> {
    let mut state = AppState::new(db, thumb_store, thumb_size)
        .with_ui_dir(ui_dir)
        .with_key_path(key_path)
        .with_settings_path(settings_path)
        .with_allow_remote(allow_remote)
        .with_db_dir(
            read_db_path
                .as_deref()
                .and_then(|p| p.parent())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
        );
    // Phase 3/5.
    eprintln!("{}", startup_progress_line(3, 5, "opening read pool"));
    if let Some(path) = &read_db_path {
        state = state.with_db_path(path.clone()).with_read_db(path);
    }
    // #232: periodic size-gated WAL checkpoint on the writer.
    state.spawn_wal_backstop();
    // Phase 4/5 — emitted even when watching is off, with a variant label.
    // "preparing" (not "starting"): the watcher now constructs empty and
    // returns instantly; root registration continues in the background.
    let watch_label = if watch {
        "preparing file watcher"
    } else {
        "file watching off"
    };
    eprintln!("{}", startup_progress_line(4, 5, watch_label));
    if watch {
        state = state.with_watch();
    }
    // Phase 5/5.
    eprintln!("{}", startup_progress_line(5, 5, "binding server address"));
    let (listener, bound) = bind(addr).await?;
    state = state.with_bound_addr(bound);
    println!("naiad daemon on http://{bound}");
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Synchronous wrapper over [`serve`]: builds a multi-thread Tokio runtime and
/// blocks on it. The entry point the (synchronous) CLI calls.
///
/// # Errors
/// Returns an error if the runtime cannot be built or [`serve`] fails.
// Plumbing wrapper: forwards every argument to `serve`. See the note there.
#[allow(clippy::too_many_arguments)]
pub fn run(
    db: Db,
    addr: SocketAddr,
    thumb_store: thumb_store::ThumbStore,
    key_path: PathBuf,
    thumb_size: u32,
    ui_dir: Option<PathBuf>,
    watch: bool,
    settings_path: PathBuf,
    read_db_path: Option<PathBuf>,
    allow_remote: bool,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(
        db,
        addr,
        thumb_store,
        key_path,
        thumb_size,
        ui_dir,
        watch,
        settings_path,
        read_db_path,
        allow_remote,
    ))
}

/// Reconcile `[[repos]]` in `naiad.toml` (the declarative source of truth at
/// startup) against the DB's subscribed shared services. Toml-only names are
/// subscribed; DB-only names are detached (NEVER purged); a name present on
/// both sides with a different URL takes the toml's URL. Two guard rails:
/// a malformed file skips reconcile entirely (data safety over sync), and a
/// file with no `repos` key at all — one predating this feature — is seeded
/// from the DB instead of read as "detach everything". Failures only warn:
/// boot must not die over a settings file.
pub(crate) fn reconcile_repos(db: &naiad_db::Db, store: &crate::settings::SettingsStore) {
    let wanted = match store.settings_strict() {
        Ok(Some(s)) => s.repos,
        Ok(None) => return, // no file yet; the scaffold has no repos to apply
        Err(e) => {
            tracing::warn!(
                target: "sync",
                "naiad.toml malformed ({e:#}); keeping current repo subscriptions"
            );
            return;
        }
    };
    let in_db = match db.list_shared_services() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "sync", "reconcile: listing repos failed: {e}");
            return;
        }
    };
    let Some(wanted) = wanted else {
        // Pre-feature file: seed [[repos]] from the DB so the file becomes
        // the source of truth without detaching anything.
        if !in_db.is_empty() {
            let entries: Vec<crate::settings::RepoEntry> = in_db
                .iter()
                .map(|s| crate::settings::RepoEntry {
                    name: s.name.clone(),
                    url: s.url.clone(),
                    max_query_bits: None,
                })
                .collect();
            match store.set_repos(&entries) {
                Ok(()) => tracing::info!(
                    target: "sync",
                    "reconcile: seeded [[repos]] with {} subscription(s)",
                    entries.len()
                ),
                Err(e) => tracing::warn!(target: "sync", "reconcile: seeding failed: {e:#}"),
            }
        }
        return;
    };
    let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for r in &wanted {
        if r.name.trim().is_empty() || r.url.trim().is_empty() {
            tracing::warn!(target: "sync", "reconcile: skipping [[repos]] entry with empty name/url");
            continue;
        }
        if !seen_names.insert(r.name.as_str()) {
            tracing::warn!(
                target: "sync",
                "reconcile: duplicate [[repos]] name {:?}; first entry wins, skipping subsequent",
                r.name
            );
            continue;
        }
        match in_db.iter().find(|s| s.name == r.name) {
            None => match db.subscribe_shared_service(&r.name, &r.url, None) {
                Ok(_) => tracing::info!(target: "sync", "reconcile: subscribed {}", r.name),
                Err(e) => {
                    tracing::warn!(target: "sync", "reconcile: subscribing {} failed: {e}", r.name)
                }
            },
            Some(s) if s.url != r.url => match db.set_service_url(s.id, &r.url) {
                Ok(()) => {
                    tracing::info!(target: "sync", "reconcile: updated url for {}", r.name)
                }
                Err(e) => {
                    tracing::warn!(target: "sync", "reconcile: url update for {} failed: {e}", r.name)
                }
            },
            Some(_) => {}
        }
    }
    for s in &in_db {
        if !wanted.iter().any(|r| r.name == s.name) {
            match db.detach_service(s.id) {
                Ok(()) => {
                    tracing::info!(target: "sync", "reconcile: detached {} (tags kept)", s.name)
                }
                Err(e) => {
                    tracing::warn!(target: "sync", "reconcile: detaching {} failed: {e}", s.name)
                }
            }
        }
    }
}

/// Open the database at `db_path` and serve it. Thumbnails are cached in
/// `thumbs.db` beside the database file. `ui_dir`, when set, serves a
/// built UI directory at `/`. The CLI's `daemon` subcommand calls this so it
/// never depends on `db`/`indexer` directly.
///
/// # Errors
/// Returns an error if the database cannot be opened or the server fails.
pub fn run_from_path(
    db_path: &Path,
    addr: SocketAddr,
    thumb_size: u32,
    ui_dir: Option<PathBuf>,
    watch: bool,
) -> anyhow::Result<()> {
    init_tracing(db_path);
    if let Some(dir) = &ui_dir {
        if !dir.join("index.html").is_file() {
            anyhow::bail!(
                "--ui-dir {}: no index.html found (did you run `npm --prefix ui run build`?)",
                dir.display()
            );
        }
    }
    // Phase 1/5. eprintln!, not tracing: the desktop shell's readiness timeout
    // treats any stderr output as liveness, so this must not be filterable via
    // RUST_LOG.
    eprintln!(
        "{}",
        startup_progress_line(1, 5, "opening database (migrations may take a while)")
    );
    let db = with_open_heartbeat("opening database (1/5)", || Db::open(db_path))
        .map_err(|e| anyhow::anyhow!("opening database {}: {e}", db_path.display()))?;
    let thumb_store = thumb_store::ThumbStore::open(
        &db_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join("thumbs.db"),
    )
    .map_err(|e| anyhow::anyhow!("opening thumbnail cache: {e:#}"))?;
    let key_path = crate::account::key_path_for(db_path);
    let settings_path = crate::settings::settings_path_for(db_path);
    let store = crate::settings::SettingsStore::new(settings_path.clone());
    // Phase 2/5.
    eprintln!("{}", startup_progress_line(2, 5, "loading settings"));
    store
        .ensure_scaffold()
        .map_err(|e| anyhow::anyhow!("scaffolding {}: {e}", settings_path.display()))?;
    crate::settings::migrate_trust_floor_to_file(&db, &store).map_err(|e| {
        anyhow::anyhow!(
            "migrating trust_floor into {}: {e}",
            settings_path.display()
        )
    })?;
    reconcile_repos(&db, &store);
    let allow_remote = store.net().allow_remote;
    if !bind_allowed(addr, allow_remote) {
        anyhow::bail!(
            "refusing to bind {addr}: it is not a loopback address.\n\
             naiad serves your library (media, file paths, and mutating endpoints) \
             with no authentication, so it binds loopback only by default.\n\
             To expose it anyway (UNSUPPORTED), set in {}:\n\
             \n    [net]\n    allow_remote = true\n",
            settings_path.display()
        );
    }
    if allow_remote && !addr.ip().is_loopback() {
        let msg = format!(
            "!! naiad is bound to a non-loopback address ({addr}).\n\
             !! Remote access is UNSUPPORTED and UNAUTHENTICATED. Anyone who can reach this\n\
             !! address can read your original media and file paths and invoke mutating\n\
             !! endpoints (tag edits, scans, imports, backups). You enabled this with\n\
             !! [net].allow_remote = true. Set it back to false to bind loopback only."
        );
        eprintln!("{msg}");
        tracing::warn!(target: "startup", "{msg}");
    }
    run(
        db,
        addr,
        thumb_store,
        key_path,
        thumb_size,
        ui_dir,
        watch,
        settings_path,
        Some(db_path.to_path_buf()),
        allow_remote,
    )
}

// Logging convention (see docs/superpowers/specs/2026-07-06-expanded-logging-toml-control-design.md):
//   ERROR — an operation failed and the user loses something.
//   WARN  — degraded but continuing (feature disabled, input rejected).
//   INFO  — curated activity feed: one line per user-meaningful event, no
//           per-item spam.
//   DEBUG — dev diagnostics: per-file, per-request, timings.
//   TRACE — firehose: raw events, SQL, wire payloads.
// Every call sets an explicit `target:` from the reserved set so RUST_LOG can
// tune one subsystem: scan | tags | watch | thumb | search | hydrus | settings
// | startup | db (per-op DB lock-wait/work timings) | sync (pull-path
// row discards) (trust reserved for later).

/// Which tier produced the resolved log directive.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LogDirectiveSource {
    RustLog,
    Toml,
    Default,
}

impl LogDirectiveSource {
    fn name(&self) -> &'static str {
        match self {
            LogDirectiveSource::RustLog => "RUST_LOG env",
            LogDirectiveSource::Toml => "naiad.toml [log].level",
            LogDirectiveSource::Default => "default",
        }
    }
}

/// Resolution result for the log directive.
struct LogDirectiveResolution {
    directive: String,
    source: LogDirectiveSource,
    overridden: Vec<(LogDirectiveSource, String)>,
}

/// Resolve the `tracing` filter directive: `RUST_LOG` wins when set and
/// non-blank (it may carry per-target directives); otherwise the `naiad.toml`
/// `[log].level`; otherwise `info`. Returns the resolution struct including
/// any tiers that were set but overridden by a higher-priority tier.
fn resolve_log_directive(
    rust_log: Option<&str>,
    toml_level: Option<&str>,
) -> LogDirectiveResolution {
    let rust_log_val = rust_log.filter(|s| !s.trim().is_empty());
    let toml_val = toml_level.map(str::trim).filter(|s| !s.is_empty());

    let (source, directive) = if let Some(s) = rust_log_val {
        (LogDirectiveSource::RustLog, s.to_string())
    } else if let Some(t) = toml_val {
        (LogDirectiveSource::Toml, t.to_string())
    } else {
        (LogDirectiveSource::Default, "info".to_string())
    };

    // Collect lower-priority tiers that differ from the winner.
    let mut overridden: Vec<(LogDirectiveSource, String)> = Vec::new();
    if source == LogDirectiveSource::RustLog {
        if let Some(t) = toml_val {
            if t != directive {
                overridden.push((LogDirectiveSource::Toml, t.to_string()));
            }
        }
    }

    LogDirectiveResolution {
        directive,
        source,
        overridden,
    }
}

/// Resolve the `[log].file` spec to an absolute path: an absolute spec is used
/// as-is; a relative spec is placed next to the database (its parent dir), the
/// same directory that holds `naiad.toml` and `naiad.key`.
fn resolve_log_file(spec: &str, db_path: &Path) -> PathBuf {
    let p = Path::new(spec);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        db_path.parent().unwrap_or_else(|| Path::new(".")).join(p)
    }
}

/// A cloneable [`io::Write`] over a shared log file. Each `tracing` event locks
/// the file briefly so concurrent writers cannot interleave a line. Used as a
/// `MakeWriter` via a `move ||` closure that clones the `Arc`.
#[derive(Clone)]
struct FileWriter(Arc<Mutex<std::fs::File>>);

impl std::io::Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::Write::write(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()), buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Install the global tracing subscriber: human-readable lines on stderr (and,
/// when `[log].file` is set, appended to that file too), level resolved by
/// `resolve_log_directive` (RUST_LOG > naiad.toml `[log].level` > info). stderr
/// keeps stdout clean for the `naiad daemon on http://…` handshake line the
/// desktop shell parses, and stays on even with a file sink so the shell's
/// liveness detection still sees output. ANSI is off because the output is
/// usually relayed into a plain console window (and a log file wants no color
/// codes). `try_init` so a second call (tests) is a no-op instead of a panic.
fn init_tracing(db_path: &Path) {
    use std::io::Write as _;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::writer::MakeWriterExt;

    let log =
        crate::settings::SettingsStore::new(crate::settings::settings_path_for(db_path)).log();
    let resolution = resolve_log_directive(
        std::env::var("RUST_LOG").ok().as_deref(),
        log.level.as_deref(),
    );
    let filter =
        EnvFilter::try_new(&resolution.directive).unwrap_or_else(|_| EnvFilter::new("info"));

    // Optional file sink. Additive: stderr always stays. On failure we warn to
    // stderr (tracing is not up yet) and carry on with stderr only.
    let file_sink = log.file.as_deref().and_then(|spec| {
        let path = resolve_log_file(spec, db_path);
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "--- naiad log session start ---");
                Some(FileWriter(Arc::new(Mutex::new(f))))
            }
            Err(e) => {
                eprintln!("naiad: could not open log file {}: {e}", path.display());
                None
            }
        }
    });

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    let _ = match file_sink {
        Some(sink) => builder
            .with_writer(std::io::stderr.and(move || sink.clone()))
            .try_init(),
        None => builder.with_writer(std::io::stderr).try_init(),
    };

    // Emit cross-tier override warnings now that the subscriber is up.
    for (loser_source, loser_val) in &resolution.overridden {
        tracing::warn!(
            target: "startup",
            "log level: {} ({}) overrides {} ({})",
            resolution.source.name(), &resolution.directive,
            loser_source.name(), loser_val
        );
    }
}

/// Format one machine-parseable startup progress line for stderr:
/// `naiad-startup <step>/<total> <label>`. The daemon emits progress via this
/// exact function so the Tauri shell's `parse_startup_progress` and the
/// format-pinning test share one source of truth. `total` is always 5.
/// Emitted with a plain `eprintln!` (never `tracing`) so the line is immune to
/// `RUST_LOG` filtering and format-stable across runs. See docs/logging.md.
fn startup_progress_line(step: u32, total: u32, label: &str) -> String {
    format!("naiad-startup {step}/{total} {label}")
}

/// Run `f` while a background thread prints a keep-alive line to stderr every
/// few seconds. The desktop shell treats any daemon output as liveness and
/// only gives up after a stretch of *silence*, so a long schema migration
/// inside [`Db::open`] must not go quiet — a shell that thinks the daemon is
/// dead kills it, rolling back the migration to be retried (and killed again)
/// on every launch. Plain threads, not Tokio: this runs before the runtime.
fn with_open_heartbeat<T>(phase: &str, f: impl FnOnce() -> T) -> T {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let phase = phase.to_string();
    let beat = std::thread::spawn(move || {
        while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
            stop_rx.recv_timeout(std::time::Duration::from_secs(5))
        {
            eprintln!("still working: {phase}...");
        }
    });
    let out = f();
    drop(stop_tx);
    let _ = beat.join();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pool_and_tag_db_absent_for_in_memory_state() {
        let thumbs = tempfile::tempdir().unwrap();
        let state = AppState::new(
            Db::open_in_memory().unwrap(),
            thumb_store::ThumbStore::open(&thumbs.path().join("thumbs.db")).unwrap(),
            128,
        );
        assert!(state.read_pool.is_none());
        assert!(state.tag_db.is_none());
    }

    // --- bind_allowed decision table ---

    #[test]
    fn bind_allowed_loopback_always_permitted() {
        // Loopback v4 — allowed regardless of allow_remote.
        let lo4: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert!(bind_allowed(lo4, false));
        assert!(bind_allowed(lo4, true));
        // In-range loopback (127.4.5.6 is still 127.0.0.0/8 → loopback).
        let lo4b: SocketAddr = "127.4.5.6:0".parse().unwrap();
        assert!(bind_allowed(lo4b, false));
        assert!(bind_allowed(lo4b, true));
        // Loopback v6.
        let lo6: SocketAddr = "[::1]:8080".parse().unwrap();
        assert!(bind_allowed(lo6, false));
        assert!(bind_allowed(lo6, true));
    }

    #[test]
    fn bind_allowed_wildcards_gated_by_allow_remote() {
        let wild4: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert!(!bind_allowed(wild4, false));
        assert!(bind_allowed(wild4, true));
        let wild6: SocketAddr = "[::]:8080".parse().unwrap();
        assert!(!bind_allowed(wild6, false));
        assert!(bind_allowed(wild6, true));
    }

    #[test]
    fn bind_allowed_lan_gated_by_allow_remote() {
        let lan: SocketAddr = "192.168.1.50:8080".parse().unwrap();
        assert!(!bind_allowed(lan, false));
        assert!(bind_allowed(lan, true));
    }

    #[test]
    fn resolve_log_directive_precedence() {
        // RUST_LOG wins when present and non-empty.
        assert_eq!(
            resolve_log_directive(Some("debug"), Some("warn")).directive,
            "debug"
        );
        // Falls back to the toml level when RUST_LOG is unset/blank.
        assert_eq!(resolve_log_directive(None, Some("warn")).directive, "warn");
        assert_eq!(
            resolve_log_directive(Some(""), Some("warn")).directive,
            "warn"
        );
        assert_eq!(
            resolve_log_directive(Some("   "), Some("warn")).directive,
            "warn"
        );
        // Falls back to info when neither is usable.
        assert_eq!(resolve_log_directive(None, None).directive, "info");
        assert_eq!(resolve_log_directive(None, Some("  ")).directive, "info");
    }

    #[test]
    fn resolve_log_directive_override_warning_when_tiers_differ() {
        let r = resolve_log_directive(Some("debug"), Some("warn"));
        assert_eq!(r.source, LogDirectiveSource::RustLog);
        assert_eq!(r.overridden.len(), 1);
        assert_eq!(r.overridden[0].0, LogDirectiveSource::Toml);
        assert_eq!(r.overridden[0].1, "warn");
    }

    #[test]
    fn resolve_log_directive_no_override_when_tiers_agree() {
        let r = resolve_log_directive(Some("info"), Some("info"));
        assert!(r.overridden.is_empty(), "agreeing tiers must not warn");
    }

    #[test]
    fn resolve_log_directive_no_override_when_only_one_tier_set() {
        let r = resolve_log_directive(None, Some("debug"));
        assert_eq!(r.source, LogDirectiveSource::Toml);
        assert!(r.overridden.is_empty());
    }

    #[test]
    fn resolve_log_file_places_relative_beside_the_db() {
        let db = Path::new("/var/lib/naiad/naiad.db");
        // Relative spec → resolved into the DB's parent directory.
        assert_eq!(
            resolve_log_file("naiad.log", db),
            Path::new("/var/lib/naiad/naiad.log")
        );
        // Absolute spec → used verbatim, ignoring the DB directory.
        let abs = if cfg!(windows) {
            r"C:\logs\naiad.log"
        } else {
            "/logs/naiad.log"
        };
        assert_eq!(resolve_log_file(abs, db), Path::new(abs));
    }

    #[test]
    fn startup_progress_line_format_is_pinned() {
        // The shell's parse_startup_progress and the loading page depend on this
        // exact byte sequence; the emitter uses the same function, so this test is
        // the single guard against format drift.
        assert_eq!(
            startup_progress_line(3, 5, "opening read pool"),
            "naiad-startup 3/5 opening read pool"
        );
        // Stage 4 label moved from "starting" to "preparing file watcher" when
        // watch startup became non-blocking; the wire shape is unchanged.
        assert_eq!(
            startup_progress_line(4, 5, "preparing file watcher"),
            "naiad-startup 4/5 preparing file watcher"
        );
    }

    #[test]
    fn with_open_heartbeat_returns_the_closure_result_and_stops() {
        // No timing assertions — just that the value passes through and the
        // heartbeat thread is joined (the call returning proves both).
        assert_eq!(with_open_heartbeat("test phase", || 42), 42);
    }

    #[tokio::test]
    async fn bind_resolves_ephemeral_port_to_a_real_one() {
        let (_listener, bound) = bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind ephemeral");
        assert!(bound.ip().is_loopback());
        assert_ne!(
            bound.port(),
            0,
            "ephemeral bind must resolve to a real port"
        );
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use crate::settings::SettingsStore;
    use naiad_db::Db;

    fn store_with(content: &str) -> (tempfile::TempDir, SettingsStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        std::fs::write(&path, content).unwrap();
        (dir, SettingsStore::new(path))
    }

    #[test]
    fn reconcile_subscribes_toml_only_and_detaches_db_only() {
        let db = Db::open_in_memory().unwrap();
        db.add_shared_service("stale", "http://old", None).unwrap();
        let (_d, store) = store_with("[[repos]]\nname = \"fresh\"\nurl = \"http://new\"\n");

        reconcile_repos(&db, &store);

        let names: Vec<String> = db
            .list_shared_services()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            vec!["fresh".to_string()],
            "stale detached, fresh subscribed"
        );

        // Idempotent: a second run changes nothing.
        reconcile_repos(&db, &store);
        assert_eq!(db.list_shared_services().unwrap().len(), 1);
    }

    #[test]
    fn reconcile_updates_a_changed_url() {
        let db = Db::open_in_memory().unwrap();
        db.add_shared_service("r", "http://old", None).unwrap();
        let (_d, store) = store_with("[[repos]]\nname = \"r\"\nurl = \"http://new\"\n");
        reconcile_repos(&db, &store);
        assert_eq!(
            db.shared_service_by_name("r").unwrap().unwrap().url,
            "http://new"
        );
    }

    #[test]
    fn reconcile_skips_a_malformed_file() {
        let db = Db::open_in_memory().unwrap();
        db.add_shared_service("keep", "http://x", None).unwrap();
        let (_d, store) = store_with("not { valid ===");
        reconcile_repos(&db, &store);
        assert_eq!(
            db.list_shared_services().unwrap().len(),
            1,
            "a broken toml never detaches anything"
        );
    }

    #[test]
    fn reconcile_seeds_the_toml_when_the_repos_key_is_absent() {
        // Upgrade path: a pre-feature naiad.toml (no repos key) with live DB
        // subscriptions must be seeded, not treated as "detach everything".
        let db = Db::open_in_memory().unwrap();
        db.add_shared_service("existing", "http://x", None).unwrap();
        let (_d, store) = store_with("[log]\nlevel = \"info\"\n");
        reconcile_repos(&db, &store);
        assert_eq!(
            db.list_shared_services().unwrap().len(),
            1,
            "nothing detached"
        );
        let repos = store.settings().repos.unwrap();
        assert_eq!(repos.len(), 1, "toml seeded from the DB");
        assert_eq!(repos[0].name, "existing");
    }

    #[test]
    fn reconcile_first_entry_wins_on_duplicate_names() {
        // Two [[repos]] entries with the same name but different URLs: the first
        // must win (subscribe or keep its URL), the second must be skipped.
        let db = Db::open_in_memory().unwrap();
        let toml = "[[repos]]\nname = \"r\"\nurl = \"http://first\"\n\
                    [[repos]]\nname = \"r\"\nurl = \"http://second\"\n";
        let (_d, store) = store_with(toml);
        reconcile_repos(&db, &store);
        let svc = db.shared_service_by_name("r").unwrap().unwrap();
        assert_eq!(
            svc.url, "http://first",
            "first entry must win on duplicate name"
        );
        assert_eq!(
            db.list_shared_services().unwrap().len(),
            1,
            "only one subscription"
        );
    }
}
