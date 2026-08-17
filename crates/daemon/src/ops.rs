//! The daemon's operation layer: synchronous functions over `&Db` that wire the
//! indexer and database together. HTTP handlers call these inside
//! `spawn_blocking`; ops tests call them directly. Kept free of HTTP/async.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use naiad_core::{FileMetadata, FileRecord, Hash, Tag};
use naiad_db::{
    BlockKind, BlockRule, Db, DeltaEdgeInput, EdgeKind, Expansion, FileListing, MergeStats,
    ReadScope, RelationEdgeRow, RelationMergeStats, ServiceRelationStatus, SharedService,
};
use naiad_indexer::{ScanError, extract_metadata, fingerprint, hash_file, walk};
use naiad_netproto::{
    Account, AuthoredEdge, Caps, DeltaEdge, DeltaMapping, EdgeStatus, HashDomain, MappingStatus,
    Op, PullMode, RelKind, RepoClient, Report, bucket_key, bucket_upper,
    effective_prefix_bits_floored,
};
use rayon::iter::{ParallelBridge, ParallelIterator};

use crate::lock::LockRecover;

// ── Caps cache ────────────────────────────────────────────────────────────────

/// Fetch-once cache for per-service `GET /repo/caps` responses. Avoids a
/// network round-trip on every reject or report for the same service. No TTL:
/// caps are stable once advertised (pre-alpha, so cache lives with the daemon).
///
/// Also owns the per-process SHA-256 backfill skip set (#152): file IDs that
/// failed to open during a backfill pass are added here and bypassed on
/// subsequent passes so repeated pulls don't re-incur open-timeout costs.
/// The skip set is owned here (not a `static`) so integration tests that
/// create a fresh [`CapsCache::new()`] get a clean skip set with no
/// cross-test contamination.
///
/// Used from `spawn_blocking` (sync context); no async primitives here.
pub struct CapsCache {
    inner: Mutex<HashMap<i64, Caps>>,
    /// Count of actual network fetches — useful for asserting cache deduplication
    /// in tests.
    fetch_count: AtomicUsize,
    /// File IDs that failed to open during a sha256 backfill pass in this
    /// process session. Bypassed on subsequent passes to avoid re-paying
    /// open-timeout costs for persistently unreadable files (e.g., files on a
    /// disconnected SMB share). Resets to empty on process restart or when a
    /// new `CapsCache` is created for tests.
    pub(crate) sha256_backfill_skip: Mutex<HashSet<i64>>,
    /// (service_id, domain) pairs already warned about a floor clamp-up this
    /// session (#179). Gates the one-shot sync-log line and toast so a repeated
    /// pull does not re-warn. Session lifetime (cleared on process restart / a
    /// fresh CapsCache), same as sha256_backfill_skip.
    pub(crate) floor_clamp_warned: Mutex<HashSet<(i64, HashDomain)>>,
    /// Per-service pending clamp notices (#179), drained by the pull handler to
    /// populate the response DTO's advisory field so the UI can toast. A notice
    /// is pushed only on the first clamp (gated by floor_clamp_warned), so
    /// draining yields at most one notice per repo+domain per session.
    pub(crate) pending_notices: Mutex<HashMap<i64, Vec<String>>>,
    /// Per-service-URL client cache. One
    /// `RepoClient` (hence one ureq connection pool) is shared by the caps
    /// fetch and every pull cycle for a given URL, so within a session the
    /// TCP+TLS handshake is paid once, not once per pull. Keyed by the
    /// trimmed service URL — the same key space `RepoClient::new` normalises
    /// to (`base_url.trim_end_matches('/')`). Session lifetime, cleared with a
    /// fresh `CapsCache` (test isolation), same as the other fields here.
    clients: Mutex<HashMap<String, Arc<RepoClient>>>,
}

impl CapsCache {
    /// Create a new, empty caps cache.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
            fetch_count: AtomicUsize::new(0),
            sha256_backfill_skip: Mutex::new(HashSet::new()),
            floor_clamp_warned: Mutex::new(HashSet::new()),
            pending_notices: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
        })
    }

    /// Cached caps for `service_id` without a network fetch. `None` if the repo
    /// has not been handshaken yet this session.
    #[must_use]
    pub fn peek(&self, service_id: i64) -> Option<Caps> {
        self.inner.lock().unwrap().get(&service_id).cloned()
    }

    /// Return the number of actual `/repo/caps` network fetches performed so far.
    ///
    /// Gated to test/debug builds only. Integration tests compile in debug mode
    /// (`debug_assertions = true`), so this is visible in all test configurations.
    #[cfg(any(test, debug_assertions))]
    #[must_use]
    pub fn fetch_count(&self) -> usize {
        self.fetch_count.load(Ordering::Relaxed)
    }

    /// Drain the first pending notice for `service_id` (#179). Returns `None`
    /// when the queue is empty (the common case) or no clamp-up has been noted
    /// for this service yet this session.
    ///
    /// Used by pull handlers to surface the advisory in their response DTO, and
    /// by integration tests to verify warn-once behaviour. At most one notice
    /// per (service, domain) per session, so draining the first is sufficient.
    pub fn drain_pending_notice(&self, service_id: i64) -> Option<String> {
        self.pending_notices
            .lock_recover()
            .get_mut(&service_id)
            .and_then(|v| {
                if v.is_empty() {
                    None
                } else {
                    Some(v.remove(0))
                }
            })
    }

    /// Return a shared client for `url`, constructing and caching one on first
    /// use. The returned `Arc<RepoClient>` shares its ureq connection pool with
    /// every other caller for the same URL this session.
    #[must_use]
    pub fn client(&self, url: &str) -> Arc<RepoClient> {
        // Key on the same normalised form RepoClient stores internally so
        // "http://h/" and "http://h" collapse to one entry.
        let key = url.trim_end_matches('/').to_string();
        let mut map = self.clients.lock_recover();
        map.entry(key)
            .or_insert_with(|| Arc::new(RepoClient::new(url)))
            .clone()
    }

    /// Return cached caps for `service_id`, or fetch from `url` and cache the
    /// result. A fetch failure is not cached — the next call will retry.
    pub fn get_or_fetch(&self, service_id: i64, url: &str) -> anyhow::Result<Caps> {
        // Fast path: already cached.
        if let Some(caps) = self.inner.lock().unwrap().get(&service_id) {
            return Ok(caps.clone());
        }
        // Slow path: fetch off-lock.
        self.fetch_count.fetch_add(1, Ordering::Relaxed);
        let caps = self
            .client(url)
            .fetch_caps()
            .with_context(|| format!("fetching caps from {url}"))?;
        tracing::debug!(target: "sync", service_id, %url, mode = ?caps.mode, "caps fetched (cache miss)");
        self.inner.lock().unwrap().insert(service_id, caps.clone());
        Ok(caps)
    }
}

impl Default for CapsCache {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            fetch_count: AtomicUsize::new(0),
            sha256_backfill_skip: Mutex::new(HashSet::new()),
            floor_clamp_warned: Mutex::new(HashSet::new()),
            pending_notices: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
        }
    }
}

/// How a scan shares the machine. `Interactive` (user-invoked `/api/scan`)
/// uses every core and pays a pre-count pass for progress totals.
/// `Background` (startup catch-up) caps hashing threads and skips the
/// pre-count so it cannot monopolize the disk the UI is reading from (#50).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanProfile {
    Interactive,
    Background,
}

/// Hashing threads for a background scan: a fraction of the machine, never
/// fewer than 2 (hashing overlaps I/O) and never more than 4.
fn background_scan_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| (n.get() / 4).clamp(2, 4))
}

/// How many classified files to accumulate before taking the DB lock to write
/// them. Bounds how long any single scan holds the lock, so other requests
/// (search, thumbnails, tag edits) interleave between bursts.
const SCAN_WRITE_BATCH: usize = 256;

/// Emit a progress log line every this many processed files during a scan.
const SCAN_PROGRESS_INTERVAL: u64 = 1000;

/// A snapshot of present locations' `(size, mtime)` keyed by path, used to skip
/// re-hashing unchanged files. See [`Db::present_fingerprints`].
type Known = HashMap<PathBuf, (u64, Option<i64>)>;

/// Per-file tag entries from a snapshot or bucket response, keyed by blake3 identity.
/// Each row is `(blake3_hash, [(tag, optional_origin_url)])`.
type TaggedEntries = Vec<(Hash, Vec<(Tag, Option<String>)>)>;

/// Outcome of importing a folder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportSummary {
    /// Files successfully hashed and stored (a location touched this scan).
    pub imported: u64,
    /// Entries that could not be read (reported, not fatal).
    pub errors: u64,
    /// Locations marked missing by the post-scan reconcile pass.
    pub marked_missing: u64,
}

/// One file's resolved write action, produced lock-free in [`classify`] and
/// applied under the DB lock in [`persist`].
enum Pending {
    /// Unchanged since the last scan: re-stamp its location, no content rewrite.
    Touch {
        path: PathBuf,
        created_at: Option<i64>,
    },
    /// New or changed: insert/upsert the content row + location, with metadata.
    Store(Box<FileRecord>, FileMetadata),
}

/// Classify one walked path **without holding the DB lock**: a file whose
/// stat'd `(size, mtime)` matches `known` is unchanged and skips the re-hash
/// (`Touch`); otherwise it is hashed and its metadata extracted (`Store`). This
/// is where the expensive, I/O-bound work lives, so callers can run it off-lock.
fn classify(path: PathBuf, known: &Known) -> std::result::Result<Pending, ScanError> {
    let fp = fingerprint(&path)?;
    if known.get(&path) == Some(&(fp.size, fp.mtime)) {
        return Ok(Pending::Touch {
            path,
            created_at: fp.created_at,
        });
    }
    let rec = hash_file(&path)?;
    let meta = extract_metadata(&rec.path);
    Ok(Pending::Store(Box::new(rec), meta))
}

/// Walk `root` and [`classify`] every entry **in parallel** on the rayon pool,
/// delivering each result to `on_result` sequentially on the calling thread.
///
/// Hashing is the scan's bottleneck and modern drives serve concurrent reads
/// well, so classification fans out across cores; the consumer side (DB writes,
/// progress callbacks) stays single-threaded, preserving the callers' locking
/// discipline. Results arrive in no particular order. The channel is bounded so
/// hashers stall rather than buffer unboundedly when the consumer is flushing.
///
/// `num_threads` is passed to rayon's [`ThreadPoolBuilder::num_threads`]; `0`
/// means rayon's default (all logical cores). Pass a small value for
/// background scans that must not starve interactive requests.
///
/// # Errors
/// Propagates the first error returned by `on_result` (a database failure);
/// per-file classify errors are delivered as `Err(ScanError)` items instead.
fn classify_walk(
    root: &Path,
    known: &Known,
    num_threads: usize,
    mut on_result: impl FnMut(std::result::Result<Pending, ScanError>) -> naiad_db::Result<()>,
) -> naiad_db::Result<()> {
    let (tx, rx) = std::sync::mpsc::sync_channel(SCAN_WRITE_BATCH);
    // A private pool, not the global one: these workers park on `tx.send`
    // whenever the DB flush is the bottleneck, and parked workers on the
    // global pool starve every other rayon user in the process — progressive
    // JPEG thumbnail decodes wedged behind a slow scan for its whole
    // duration (#65).
    let pool = rayon::ThreadPoolBuilder::new()
        .thread_name(|i| format!("naiad-scan-{i}"))
        .num_threads(num_threads)
        .build();
    std::thread::scope(|s| {
        s.spawn(move || {
            let fan_out = move || {
                walk(root).par_bridge().for_each_with(tx, |tx, entry| {
                    // A send error means the consumer bailed on a DB error and
                    // dropped the receiver; nothing to do but stop caring.
                    let _ = tx.send(entry.and_then(|p| classify(p, known)));
                });
            };
            match pool {
                Ok(pool) => pool.install(fan_out),
                // Pool creation failing (thread spawn refusal) is unheard of;
                // degrade to the old shared-pool behavior rather than failing
                // the scan.
                Err(e) => {
                    tracing::warn!(target: "scan", "scan: private rayon pool unavailable, using global: {e}");
                    // Degraded path: runs on the global rayon pool, so Background's
                    // thread cap is NOT enforced here (pool creation failed).
                    fan_out();
                }
            }
        });
        for res in rx {
            on_result(res)?;
        }
        Ok(())
    })
}

/// Apply one classified item to `db` (the caller holds the lock). Stamps the
/// touched/stored location with `marker`.
fn persist(db: &Db, item: &Pending, marker: i64) -> naiad_db::Result<()> {
    match item {
        Pending::Touch { path, created_at } => {
            db.touch_location(path, marker, *created_at)?;
        }
        Pending::Store(rec, meta) => {
            db.insert_file(rec, marker)?;
            // Best-effort metadata pass; only write when something was found so
            // non-image imports don't issue a pointless UPDATE.
            if *meta != FileMetadata::default() {
                db.update_metadata(&rec.hash, meta)?;
            }
        }
    }
    Ok(())
}

/// Emit a `scan progress` info line every [`SCAN_PROGRESS_INTERVAL`] files.
/// `last_logged` is updated to `processed` on each emission so the gate rearms.
fn log_scan_progress(root: &Path, processed: u64, last_logged: &mut u64, started: Instant) {
    if processed - *last_logged >= SCAN_PROGRESS_INTERVAL {
        let elapsed = started.elapsed().as_secs_f64();
        let rate = (processed as f64 / elapsed.max(0.001)) as u64;
        tracing::info!(
            target: "scan",
            root = %root.display(),
            processed,
            elapsed_secs = elapsed,
            rate_per_sec = rate,
            "scan progress"
        );
        *last_logged = processed;
    }
}

/// Scan `root`, hashing each file and storing it in `db`.
///
/// Unchanged files (matching size + mtime) skip the re-hash. Unreadable entries
/// are counted in [`ImportSummary::errors`] and their paths are passed to
/// `on_error`; they do not abort the import.
///
/// This direct-`&Db` form runs the whole scan against one handle (used by the
/// CLI-less in-process tests); the daemon's HTTP handler uses
/// [`scan_streaming`], which hashes off-lock.
///
/// # Errors
/// Returns an error only on a database failure (an indexing failure of a single
/// file is non-fatal and reported via `on_error`).
pub fn import_path(
    db: &Db,
    root: impl AsRef<Path>,
    mut on_error: impl FnMut(&ScanError),
) -> naiad_db::Result<ImportSummary> {
    // Absolutize so stored location paths match live watch-event paths, and
    // register the folder as a watched root.
    let root = std::path::absolute(root.as_ref()).unwrap_or_else(|_| root.as_ref().to_path_buf());
    db.add_root(&root)?;

    let known = db.present_fingerprints()?;
    // Allocate a monotonic scan marker: strictly greater than every existing
    // location's last_seen. Every location touched this scan is stamped with it,
    // so the reconcile below cleanly flips anything older.
    let marker = db.next_scan_marker()?;

    let mut summary = ImportSummary::default();
    let scan_start = Instant::now();
    let mut processed_total: u64 = 0;
    let mut last_progress_log: u64 = 0;
    // 0 = rayon default (all cores): import_path is a direct-&Db call used by
    // tests and the CLI, neither of which competes with interactive HTTP requests.
    classify_walk(&root, &known, 0, |res| {
        match res {
            Ok(item) => {
                persist(db, &item, marker)?;
                summary.imported += 1;
            }
            Err(err) => {
                on_error(&err);
                summary.errors += 1;
            }
        }
        processed_total += 1;
        log_scan_progress(&root, processed_total, &mut last_progress_log, scan_start);
        Ok(())
    })?;
    // Reconcile only this root's subtree: scanning one folder must never flip
    // another watched folder's locations to missing.
    summary.marked_missing = db.mark_missing_under_before(&root, marker)?;
    let elapsed = scan_start.elapsed().as_secs_f64();
    tracing::info!(
        target: "scan",
        root = %root.display(),
        imported = summary.imported,
        errors = summary.errors,
        marked_missing = summary.marked_missing,
        elapsed_secs = elapsed,
        "import finished"
    );
    Ok(summary)
}

/// Scan `root` while holding the DB lock only in short bursts.
///
/// The walk, hashing, and metadata extraction — the slow, I/O-bound work — run
/// entirely off-lock and fan out across the rayon pool ([`classify_walk`]); the
/// lock is taken only to snapshot fingerprints up front,
/// to flush each [`SCAN_WRITE_BATCH`]-sized batch of writes, and to reconcile at
/// the end. This keeps a long scan from freezing every other request behind the
/// single `Mutex<Db>`. Same semantics and [`ImportSummary`] as [`import_path`].
///
/// `profile` controls how the scan shares the machine:
/// - [`ScanProfile::Interactive`]: uses all cores, pays a pre-count traversal
///   so `on_progress`'s `total` argument carries an accurate file count.
/// - [`ScanProfile::Background`]: caps hashing threads (see
///   [`background_scan_threads`]) and skips the pre-count; `on_progress`'s
///   `total` is always `0` (meaning "unknown") to avoid monopolizing the disk.
///
/// # Errors
/// Returns an error only on a database failure; per-file indexing failures are
/// reported via `on_error` and do not abort the scan.
pub fn scan_streaming(
    db: &Mutex<Db>,
    root: impl AsRef<Path>,
    profile: ScanProfile,
    mut on_error: impl FnMut(&ScanError),
    mut on_progress: impl FnMut(u64, u64, u64),
) -> naiad_db::Result<ImportSummary> {
    let root = std::path::absolute(root.as_ref()).unwrap_or_else(|_| root.as_ref().to_path_buf());

    // Resolve profile → (rayon thread cap, pre-count total) together so the
    // tradeoffs are co-located. threads=0 is rayon's default (all cores).
    // total=0 means "unknown" (Background skips the extra traversal).
    let (threads, total) = match profile {
        ScanProfile::Interactive => (0, walk(&root).filter(Result::is_ok).count() as u64),
        ScanProfile::Background => (background_scan_threads(), 0),
    };

    tracing::info!(target: "scan", root = %root.display(), total, threads, "scan started");

    // Brief lock: register the root, snapshot known fingerprints, take a marker.
    let (known, marker) = {
        let db = db.lock_recover();
        db.add_root(&root)?;
        (db.present_fingerprints()?, db.next_scan_marker()?)
    };

    let mut summary = ImportSummary::default();
    let scan_start = Instant::now();
    let mut processed_total: u64 = 0;
    let mut last_progress_log: u64 = 0;
    let mut batch: Vec<Pending> = Vec::with_capacity(SCAN_WRITE_BATCH);
    classify_walk(&root, &known, threads, |res| {
        match res {
            Ok(item) => batch.push(item),
            Err(err) => {
                on_error(&err);
                summary.errors += 1;
                on_progress(summary.imported, summary.errors, total);
            }
        }
        processed_total += 1;
        log_scan_progress(&root, processed_total, &mut last_progress_log, scan_start);
        if batch.len() >= SCAN_WRITE_BATCH {
            flush(db, &batch, marker)?;
            summary.imported += batch.len() as u64;
            batch.clear();
            on_progress(summary.imported, summary.errors, total);
        }
        Ok(())
    })?;
    if !batch.is_empty() {
        flush(db, &batch, marker)?;
        summary.imported += batch.len() as u64;
        on_progress(summary.imported, summary.errors, total);
    }

    summary.marked_missing = {
        let db = db.lock_recover();
        db.mark_missing_under_before(&root, marker)?
    };
    let elapsed = scan_start.elapsed().as_secs_f64();
    tracing::info!(
        target: "scan",
        root = %root.display(),
        imported = summary.imported,
        errors = summary.errors,
        marked_missing = summary.marked_missing,
        elapsed_secs = elapsed,
        "scan finished"
    );
    Ok(summary)
}

/// A single progress observation emitted by [`rescan_roots`], at write-batch
/// cadence within a root and once at each root boundary. Carries a running,
/// cross-root tally so a caller can surface catch-up scan progress without
/// tracking per-root state itself.
#[derive(Clone, Debug)]
pub struct RescanProgress {
    /// Registered roots fully scanned so far.
    pub roots_done: usize,
    /// Total registered roots to scan.
    pub roots_total: usize,
    /// Files imported across all roots so far.
    pub imported: u64,
    /// Per-file indexing errors across all roots so far.
    pub errors: u64,
    /// The root currently being scanned, or `None` at a root boundary.
    pub current: Option<PathBuf>,
}

/// Catch-up rescan of every registered root, run at startup: picks up files an
/// interrupted import never flushed and files added while the daemon was down —
/// the live watcher only sees events that happen while it is running. Rescans
/// via [`scan_streaming`], so unchanged files take the fingerprint-only fast
/// path and other requests interleave between write bursts.
///
/// A root whose directory is currently unavailable (unmounted drive, detached
/// share) is skipped with a warning: scanning it would walk nothing and then
/// reconcile the whole subtree to missing. Per-file scan errors are logged and
/// counted, not fatal.
///
/// Progress ticks are emitted via `on_progress` at write-batch cadence within
/// each root and once at each root boundary.
///
/// # Errors
/// Returns an error only on a database failure.
pub fn rescan_roots(
    db: &Mutex<Db>,
    mut on_progress: impl FnMut(RescanProgress),
) -> naiad_db::Result<ImportSummary> {
    let roots = db.lock_recover().list_roots()?;
    let roots_total = roots.len();
    let mut total = ImportSummary::default();
    let mut roots_done = 0usize;
    for root in roots {
        if !root.is_dir() {
            tracing::warn!(target: "scan", root = %root.display(), "catch-up scan: root unavailable, skipping");
            roots_done += 1;
            on_progress(RescanProgress {
                roots_done,
                roots_total,
                imported: total.imported,
                errors: total.errors,
                current: None,
            });
            continue;
        }
        // Base tallies for this root, so the per-batch closure can report a
        // running cross-root total (scan_streaming's counts are per-root).
        let base_imported = total.imported;
        let base_errors = total.errors;
        let current = Some(root.clone());
        let s = scan_streaming(
            db,
            &root,
            ScanProfile::Background,
            |e| tracing::warn!(target: "scan", "catch-up scan: {e}"),
            |imported, errors, _total| {
                on_progress(RescanProgress {
                    roots_done,
                    roots_total,
                    imported: base_imported + imported,
                    errors: base_errors + errors,
                    current: current.clone(),
                });
            },
        )?;
        total.imported += s.imported;
        total.errors += s.errors;
        total.marked_missing += s.marked_missing;
        roots_done += 1;
        on_progress(RescanProgress {
            roots_done,
            roots_total,
            imported: total.imported,
            errors: total.errors,
            current: None,
        });
    }
    Ok(total)
}

/// Flush a batch of classified items under one brief lock acquisition, inside
/// a single transaction — one WAL commit per batch instead of one per
/// statement, which is what makes imports write-bound-cheap rather than
/// fsync-bound.
fn flush(db: &Mutex<Db>, batch: &[Pending], marker: i64) -> naiad_db::Result<()> {
    let db = db.lock_recover();
    db.with_tx(|db| {
        for item in batch {
            persist(db, item, marker)?;
        }
        Ok(())
    })
}

/// Reindex a single created/modified path: hash it and upsert its location. If
/// the file vanished between the watch event and now, fall through to marking it
/// missing (handling the rename/delete race).
///
/// # Errors
/// Returns an error on a hash/IO failure that is not "file not found", or on a
/// database failure.
pub fn reindex_upsert(db: &Db, path: &Path) -> Result<()> {
    match hash_file(path) {
        Ok(record) => {
            db.insert_file(&record, db.next_scan_marker()?)?;
            let meta = extract_metadata(&record.path);
            if meta != FileMetadata::default() {
                db.update_metadata(&record.hash, &meta)?;
            }
            Ok(())
        }
        Err(e) if e.source.kind() == std::io::ErrorKind::NotFound => {
            db.mark_missing_path(path)?;
            Ok(())
        }
        Err(e) => Err(anyhow!(e).context(format!("reindexing {}", path.display()))),
    }
}

/// Reindex a removed path: mark its location, and any descendants, missing.
///
/// # Errors
/// Returns an error on a database failure.
pub fn reindex_remove(db: &Db, path: &Path) -> Result<()> {
    db.mark_missing_path(path)?;
    Ok(())
}

/// Register `path` as a watched root.
///
/// # Errors
/// Returns an error on a database failure.
pub fn register_root(db: &Db, path: &Path) -> Result<()> {
    db.add_root(path)?;
    tracing::info!(target: "scan", root = %path.display(), "root registered");
    Ok(())
}

/// List watched roots.
///
/// # Errors
/// Returns an error on a database failure.
pub fn list_roots(db: &Db) -> Result<Vec<PathBuf>> {
    Ok(db.list_roots()?)
}

/// Stop watching `path`. Returns whether a root was removed.
///
/// # Errors
/// Returns an error on a database failure.
pub fn remove_root(db: &Db, path: &Path) -> Result<bool> {
    let removed = db.remove_root(path)?;
    if removed {
        tracing::info!(target: "scan", root = %path.display(), "root removed");
    }
    Ok(removed)
}

/// Mark every indexed location under `path` missing (`present = 0`) so it drops
/// out of search results. Reversible via a re-scan; never deletes rows. Returns
/// the number of locations newly hidden.
///
/// # Errors
/// Returns an error on a database failure.
pub fn mark_missing_under(db: &Db, path: &Path) -> Result<u64> {
    Ok(db.mark_missing_path(path)?)
}

/// Resolve a file reference to a `files.id`. A 64-char lowercase-hex string is
/// treated as a BLAKE3 hash; anything else is treated as a filesystem path.
///
/// # Errors
/// Returns an error if the reference is an invalid hash, the query fails, or no
/// file matches (the file must be scanned into the library first).
pub fn resolve_file(db: &Db, reference: &str) -> Result<i64> {
    let is_hash = reference.len() == 64
        && reference
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() && b <= b'f');
    let found = if is_hash {
        let hash = reference
            .parse::<Hash>()
            .with_context(|| format!("parsing hash {reference}"))?;
        db.file_id_by_hash(&hash)?
    } else {
        db.file_id_by_path(Path::new(reference))?
    };
    found.ok_or_else(|| anyhow!("{reference}: not in library; scan it first"))
}

/// Add each tag in `tags` to the referenced file, in the local service.
/// Tags are normalized via [`naiad_core::Tag::parse`]; re-adding is idempotent.
///
/// # Errors
/// Returns an error if the file cannot be resolved, a tag fails to parse, or a
/// database operation fails.
pub fn add_tags(db: &Db, reference: &str, tags: &[String]) -> Result<()> {
    let file_id = resolve_file(db, reference)?;
    let service_id = db.local_service_id()?;
    for raw in tags {
        let tag = Tag::parse(raw).with_context(|| format!("parsing tag {raw:?}"))?;
        let tag_id = db.intern_tag(&tag)?;
        db.add_mapping(file_id, tag_id, service_id)?;
    }
    tracing::info!(target: "tags", file = %reference, ?tags, "tags added");
    Ok(())
}

/// Remove each tag in `tags` from the referenced file's local-service mappings.
/// Removing a tag the file does not have is a no-op.
///
/// # Errors
/// Returns an error if the file cannot be resolved, a tag fails to parse, or a
/// database operation fails.
pub fn remove_tags(db: &Db, reference: &str, tags: &[String]) -> Result<()> {
    let file_id = resolve_file(db, reference)?;
    let service_id = db.local_service_id()?;
    for raw in tags {
        let tag = Tag::parse(raw).with_context(|| format!("parsing tag {raw:?}"))?;
        let tag_id = db.intern_tag(&tag)?;
        db.remove_mapping(file_id, tag_id, service_id)?;
    }
    tracing::info!(target: "tags", file = %reference, ?tags, "tags removed");
    Ok(())
}

/// List the referenced file's tags, ordered by namespace then subtag.
///
/// # Errors
/// Returns an error if the file cannot be resolved or the query fails.
pub fn list_tags(db: &Db, reference: &str) -> Result<Vec<Tag>> {
    let file_id = resolve_file(db, reference)?;
    Ok(db.tags_of(file_id)?)
}

/// Parse and intern a tag string, returning its id.
fn intern_ref(db: &Db, raw: &str) -> Result<i64> {
    let tag = Tag::parse(raw).with_context(|| format!("parsing tag {raw:?}"))?;
    Ok(db.intern_tag(&tag)?)
}

/// List the referenced file's effective (computed) tags under `scope`, each
/// rendered as a display string. Pulled-only tags are prefixed with `*` so the
/// CLI/gallery can show provenance without a richer wire type.
///
/// # Errors
/// Returns an error if the file cannot be resolved or a query fails.
pub fn display_tags(db: &Db, reference: &str, scope: ReadScope) -> Result<Vec<String>> {
    let file_id = resolve_file(db, reference)?;
    let tags = db.display_tags_of(file_id, scope)?;
    Ok(tags.into_iter().map(render_tag).collect())
}

/// Render an effective tag with a leading `*` iff it is pulled-only.
fn render_tag(t: naiad_db::TagWithPresence) -> String {
    match t.presence {
        naiad_db::TagPresence::Pulled => format!("*{}", t.tag),
        _ => t.tag.to_string(),
    }
}

/// The referenced file's effective tags with per-tag supporting authors and
/// their current trust weights — the detailed analogue of [`display_tags`].
///
/// # Errors
/// Returns an error if the file cannot be resolved or a query fails.
pub fn display_tags_detailed(
    db: &Db,
    reference: &str,
    scope: ReadScope,
) -> Result<Vec<naiad_db::TagDetail>> {
    let file_id = resolve_file(db, reference)?;
    Ok(db.display_tags_detailed(file_id, scope)?)
}

/// Relation sections for one tag: aliases, parents, and children, each capped
/// to `cap` items (clamped to `1..=10` by the caller). Optionally anchored to
/// `file` (a 64-char hex hash string) to determine `via_alias`.
///
/// A malformed or unknown file hash resolves to `None` — not an error. A
/// malformed `tag` propagates as an error (maps to 400 by the HTTP layer).
///
/// # Errors
/// Returns an error if `tag` fails to parse or a database query fails.
pub fn tag_relations(
    db: &Db,
    tag: &str,
    file: Option<&str>,
    scope: ReadScope,
    cap: usize,
) -> Result<naiad_db::TagRelations> {
    let tag = Tag::parse(tag).with_context(|| format!("parsing tag {tag:?}"))?;
    // Optional file: parse as a hex hash; silently map unknown refs to None.
    let file_id: Option<i64> = match file {
        Some(hex) => {
            if let Ok(hash) = hex.parse::<Hash>() {
                db.file_id_by_hash(&hash)?
            } else {
                None
            }
        }
        None => None,
    };
    Ok(db.tag_relations(&tag, file_id, scope, cap)?)
}

/// Alias `bad` to `ideal` on the local service.
///
/// # Errors
/// Returns an error if a tag fails to parse, the tags are equal, or a database
/// operation fails.
pub fn add_sibling(db: &Db, bad: &str, ideal: &str) -> Result<()> {
    let service_id = db.local_service_id()?;
    let bad_id = intern_ref(db, bad)?;
    let ideal_id = intern_ref(db, ideal)?;
    db.add_sibling(bad_id, ideal_id, service_id)?;
    Ok(())
}

/// Remove the sibling alias for `bad` on the local service.
///
/// # Errors
/// Returns an error if the tag fails to parse or a database operation fails.
pub fn remove_sibling(db: &Db, bad: &str) -> Result<()> {
    let service_id = db.local_service_id()?;
    let bad_id = intern_ref(db, bad)?;
    db.remove_sibling(bad_id, service_id)?;
    Ok(())
}

/// List the local service's sibling aliases as `(bad, ideal)` pairs.
///
/// # Errors
/// Returns an error if a query fails.
pub fn list_siblings(db: &Db) -> Result<Vec<(Tag, Tag)>> {
    let service_id = db.local_service_id()?;
    Ok(db.list_siblings(service_id)?)
}

/// Imply `parent` from `child` on the local service.
///
/// # Errors
/// Returns an error if a tag fails to parse, the tags are equal, or a database
/// operation fails.
pub fn add_parent(db: &Db, child: &str, parent: &str) -> Result<()> {
    let service_id = db.local_service_id()?;
    let child_id = intern_ref(db, child)?;
    let parent_id = intern_ref(db, parent)?;
    db.add_parent(child_id, parent_id, service_id)?;
    Ok(())
}

/// Remove the implication `child -> parent` on the local service.
///
/// # Errors
/// Returns an error if a tag fails to parse or a database operation fails.
pub fn remove_parent(db: &Db, child: &str, parent: &str) -> Result<()> {
    let service_id = db.local_service_id()?;
    let child_id = intern_ref(db, child)?;
    let parent_id = intern_ref(db, parent)?;
    db.remove_parent(child_id, parent_id, service_id)?;
    Ok(())
}

/// List the local service's parent implications as `(child, parent)` pairs.
///
/// # Errors
/// Returns an error if a query fails.
pub fn list_parents(db: &Db) -> Result<Vec<(Tag, Tag)>> {
    let service_id = db.local_service_id()?;
    Ok(db.list_parents(service_id)?)
}

/// Every relation edge across all services, with provenance (`relation list`).
///
/// # Errors
/// Returns an error if a query fails.
pub fn list_relations(db: &Db) -> Result<Vec<RelationEdgeRow>> {
    Ok(db.list_relation_edges()?)
}

/// Per-service relation counts and last-pull time (`relation status`).
///
/// # Errors
/// Returns an error if a query fails.
pub fn relation_status(db: &Db) -> Result<Vec<ServiceRelationStatus>> {
    Ok(db.relation_status()?)
}

/// Add a block rule. `kind` is `"tag"`, `"tag_pattern"`, or `"author"`.
///
/// # Errors
/// Returns an error if `kind` is unknown or `target` is invalid for it.
pub fn add_block(db: &Db, kind: &str, target: &str, note: Option<&str>) -> Result<i64> {
    let kind = BlockKind::parse(kind)?;
    Ok(db.add_block_rule(kind, target, note)?)
}

/// Every block rule (`block list`).
///
/// # Errors
/// Returns an error if a query fails.
pub fn list_blocks(db: &Db) -> Result<Vec<BlockRule>> {
    Ok(db.list_block_rules()?)
}

/// Remove a block rule by id (`block remove`). Returns `false` if no rule had
/// that id (so the caller can surface a 404), `true` if one was removed.
///
/// # Errors
/// Returns an error if the delete statement fails.
pub fn remove_block(db: &Db, id: i64) -> Result<bool> {
    match db.remove_block_rule(id) {
        Ok(()) => Ok(true),
        Err(naiad_db::Error::NotFound(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Reject one pulled mapping. The DB write runs under a brief lock; the
/// reports capability is then checked via the **cached** caps — a dead repo
/// must not fail a purely local act (the reject row has already been committed).
///
/// Returns `true` if the repo advertises the reports capability (per cached
/// caps), `false` if the repo is unreachable or does not advertise it.
///
/// # Errors
/// Returns [`SubmitError::BadRequest`] if the service is unknown (including
/// the local service — local tags are deleted, not rejected), the file is not
/// in the library, or the tag is unparseable.
pub fn reject_mapping(
    db: &Mutex<Db>,
    caps_cache: &CapsCache,
    service: &str,
    file: &str,
    tag: &str,
    note: Option<&str>,
) -> Result<bool, SubmitError> {
    // Brief lock: resolve all inputs and write the rejection row atomically.
    let (svc_id, url) = (|| -> anyhow::Result<(i64, String)> {
        let db = db.lock_recover();
        let svc = db.shared_service_by_name(service)?.ok_or_else(|| {
            anyhow!("no such shared repo: {service} (local tags are deleted, not rejected)")
        })?;
        let file_id = resolve_file(&db, file)?;
        let tag = Tag::parse(tag).with_context(|| format!("parsing tag {tag:?}"))?;
        let tag_id = db.intern_tag(&tag)?;
        db.add_rejection(svc.id, file_id, tag_id, note)?;
        Ok((svc.id, svc.url))
    })()
    .map_err(SubmitError::BadRequest)?;

    // Off-lock: consult the cached caps (one fetch per service, ever). An
    // unreachable repo returns false — the local rejection has already succeeded.
    let reports = caps_cache
        .get_or_fetch(svc_id, &url)
        .map(|caps| caps.reports)
        .unwrap_or(false);
    Ok(reports)
}

/// Undo a rejection for one pulled mapping. Idempotent: if no rejection row
/// exists (already undone), this is a no-op.
///
/// # Errors
/// Returns [`SubmitError::BadRequest`] if the service is unknown, the file is
/// not in the library, or the tag is unparseable.
pub fn undo_rejection(db: &Db, service: &str, file: &str, tag: &str) -> Result<(), SubmitError> {
    (|| -> anyhow::Result<()> {
        let svc = db
            .shared_service_by_name(service)?
            .ok_or_else(|| anyhow!("no such shared repo: {service}"))?;
        let file_id = resolve_file(db, file)?;
        let tag = Tag::parse(tag).with_context(|| format!("parsing tag {tag:?}"))?;
        let tag_id = db.intern_tag(&tag)?;
        db.remove_rejection(svc.id, file_id, tag_id)?;
        Ok(())
    })()
    .map_err(SubmitError::BadRequest)
}

/// File an anonymous report against a pulled mapping, forwarding it to the
/// originating repository (fire-and-forget: no local record, no polling).
///
/// - Resolves the service URL and checks the **cached** caps; returns
///   [`SubmitError::Unsupported`] when the repo does not advertise `reports`.
/// - Derives the contributor account and signs the request via the
///   `x-naiad-*` auth headers.
/// - NOTHING is written to the client DB.
///
/// # Errors
/// Returns [`SubmitError::BadRequest`] if the service is unknown or the tag is
/// unparseable. Returns [`SubmitError::Unsupported`] if the repo does not
/// advertise reports. Returns [`SubmitError::Upstream`] on key/network errors.
pub fn report_mapping(
    db: &Mutex<Db>,
    caps_cache: &CapsCache,
    key_path: &Path,
    service: &str,
    file: &str,
    tag: &str,
    note: Option<&str>,
) -> Result<(), SubmitError> {
    // Brief lock: resolve the service URL, id, file hash, and tag.
    let (svc_id, url, hash) = (|| -> anyhow::Result<_> {
        let db = db.lock_recover();
        let svc = db.shared_service_by_name(service)?.ok_or_else(|| {
            anyhow!("no such shared repo: {service} (local tags cannot be reported)")
        })?;
        let file_id = resolve_file(&db, file)?;
        let hash = db
            .file_hash(file_id)?
            .ok_or_else(|| anyhow!("file {file} has no content hash"))?;
        // Tag parsed and discarded (normalization check only).
        Tag::parse(tag).with_context(|| format!("parsing tag {tag:?}"))?;
        Ok((svc.id, svc.url, hash))
    })()
    .map_err(SubmitError::BadRequest)?;

    // Off-lock: check caps (cached). Return Unsupported if the repo does not
    // advertise reports — this is a clean user-facing error, not a 500.
    let caps = caps_cache
        .get_or_fetch(svc_id, &url)
        .with_context(|| format!("checking caps for {url}"))
        .map_err(SubmitError::Upstream)?;
    if !caps.reports {
        return Err(SubmitError::Unsupported(anyhow!(
            "repo {service} does not advertise the reports capability"
        )));
    }

    // Off-lock: derive the contributor account and submit the signed report.
    let account = contributor_account_for(db, caps_cache, key_path, svc_id, &url)
        .with_context(|| format!("resolving contributor account for {url}"))
        .map_err(SubmitError::Upstream)?;

    let report = Report {
        version: naiad_netproto::PROTOCOL_VERSION,
        hash: hash.to_hex(),
        tag: tag.to_string(),
        note: note.map(str::to_string),
    };
    RepoClient::new(&url)
        .report(&account, &report)
        .with_context(|| format!("reporting to {url}"))
        .map_err(SubmitError::Upstream)?;
    Ok(())
}

/// List rejections, optionally scoped to one file (identified by a
/// path-or-hash reference). Pass `None` to list all rejections.
///
/// # Errors
/// Returns an error if the file cannot be resolved (when `file` is `Some`)
/// or a query fails.
pub fn list_rejections_op(db: &Db, file: Option<&str>) -> Result<Vec<naiad_db::Rejection>> {
    let file_id = file.map(|f| resolve_file(db, f)).transpose()?;
    Ok(db.list_rejections(file_id)?)
}

/// Parse `tokens` into a query (via [`naiad_core::parse_query`]) and run it,
/// returning matching files.
///
/// # Errors
/// Returns an error if the query is invalid or a database operation fails.
pub fn search(
    db: &Db,
    tokens: &[String],
    scope: ReadScope,
    expansion: Expansion,
) -> Result<Vec<FileListing>> {
    let query = naiad_core::parse_query(tokens)?;
    Ok(db.search(&query, scope, expansion)?)
}

/// Set a subscribed repository's (shared service's) priority.
///
/// # Errors
/// Returns an error if the repo is unknown or the update fails.
pub fn set_repo_priority(db: &Db, name: &str, priority: i64) -> Result<()> {
    let svc = db
        .shared_service_by_name(name)?
        .with_context(|| format!("no repository named {name:?}"))?;
    db.set_service_priority(svc.id, priority)?;
    Ok(())
}

/// List subscribed repositories.
///
/// # Errors
/// Returns an error on a database failure.
pub fn list_repos(db: &Db) -> Result<Vec<SharedService>> {
    Ok(db.list_shared_services()?)
}

/// Unsubscribe from a repository. By default this **detaches**: the service
/// row and every pulled tag are kept (still displayed, marked by source),
/// only the subscription/URL is dropped. `purge` deletes the service and all
/// of its tags — the explicit, irreversible path. Returns whether a
/// subscribed repo was found.
///
/// # Errors
/// Returns an error on a database failure.
pub fn remove_repo(db: &Db, name: &str, purge: bool) -> Result<bool> {
    match db.shared_service_by_name(name)? {
        Some(svc) => {
            if purge {
                db.drop_service(svc.id)?;
            } else {
                db.detach_service(svc.id)?;
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Translate repo entries keyed by SHA-256 hex into `(blake3 Hash, tags)` using
/// the local `sha256-hex → blake3` map. Entries whose sha256 is not locally
/// present are dropped (the file isn't in the library) — mirroring the blake3
/// path's own "not in library" skip in `merge_pulled_mappings`.
fn translate_sha256_entries(
    entries: TaggedEntries,
    sha_to_blake3: &HashMap<String, Hash>,
) -> TaggedEntries {
    entries
        .into_iter()
        .filter_map(|(sha_as_hash, tags)| {
            let sha_hex = sha_as_hash.to_hex();
            sha_to_blake3.get(&sha_hex).map(|blake| (*blake, tags))
        })
        .collect()
}

/// Translate delta rows keyed by SHA-256 into blake3-identity rows, preserving
/// each row's status and seq. Rows whose sha256 is not locally owned are dropped
/// (the file isn't in the library), exactly as [`translate_sha256_entries`] does
/// for snapshot rows. Tombstones (`status = Deleted`) translate identically —
/// the map keys on the sha256 hex regardless of status.
fn translate_sha256_delta_inputs(
    inputs: Vec<naiad_db::MappingDeltaInput>,
    sha_to_blake3: &HashMap<String, Hash>,
) -> Vec<naiad_db::MappingDeltaInput> {
    inputs
        .into_iter()
        .filter_map(|mut inp| {
            let sha_hex = inp.hash.to_hex();
            sha_to_blake3.get(&sha_hex).map(|blake| {
                inp.hash = *blake;
                inp
            })
        })
        .collect()
}

/// #194: Detect a store-generation change in `caps` and, when found, reset all
/// cursors for `service_id` so the next pull restarts from seq 0.
///
/// Called from BOTH `pull_repo` and `pull_repo_for_hashes` — both paths fetch
/// caps and both advance `service_domain_pull_state`, so both must check.
///
/// # Behaviour
/// - `caps` has no `store_generation` (pre-feature server): no-op; fall back to
///   the backwards-cursor guard.
/// - First time we see a generation: record it, no reset.
/// - Same generation as last time: no-op.
/// - Different generation: reset all cursors, record the new id, log INFO.
fn reconcile_store_generation(
    db: &Mutex<Db>,
    service_id: i64,
    caps: &Caps,
    name: &str,
) -> Result<()> {
    let Some(ref new_gen) = caps.store_generation else {
        return Ok(());
    };
    let stored_gen = {
        let db = db.lock_recover();
        db.service_store_generation(service_id)?
    };
    match stored_gen.as_deref() {
        Some(old_gen) if old_gen != new_gen.as_str() => {
            {
                let db = db.lock_recover();
                db.reset_service_cursors(service_id)?;
                db.set_service_store_generation(service_id, new_gen)?;
            }
            tracing::info!(
                target: "sync",
                repo = %name,
                old = %old_gen,
                new = %new_gen,
                "store re-seeded; re-pulling from zero"
            );
        }
        None => {
            let db = db.lock_recover();
            db.set_service_store_generation(service_id, new_gen)?;
        }
        Some(_) => {}
    }
    Ok(())
}

/// Pull `name` using the repo's advertised mode — bucketed (k-anonymity) or a
/// whole-repo fallback — filter to owned hashes, and merge the matches.
///
/// `max_query_bits` is the privacy ceiling: the repo-advertised `prefix_bits`
/// is clamped to this value before any query is issued, so a hostile repo
/// advertising 256 bits can never receive exact-hash-precision queries.
///
/// `key_path` is reserved for future use and is currently unused; tests and
/// callers that do not exercise any key-dependent path pass `None`.
///
/// The HTTP fetch runs **off** the DB lock: the lock is taken only briefly to
/// resolve the repo URL, again to read owned hashes (bucketed mode), and again
/// to merge or reconcile, so a slow pull never freezes other requests behind the
/// single `Mutex<Db>` (same discipline as [`scan_streaming`]).
///
/// # Errors
/// Returns an error if the repo is unknown, the handshake or fetch fails, or a
/// database operation fails. A response entry with an unparseable hash or no
/// parseable tags is skipped, not fatal.
pub fn pull_repo(
    db: &Mutex<Db>,
    caps_cache: &CapsCache,
    name: &str,
    max_query_bits: u32,
    _key_path: Option<&Path>,
) -> Result<MergeStats> {
    // Brief lock: resolve the subscribed repo.
    let svc = {
        let db = db.lock_recover();
        db.shared_service_by_name(name)?
            .ok_or_else(|| anyhow!("no such repo: {name}"))?
    };

    let started = Instant::now();
    let client = caps_cache.client(&svc.url);

    // Off-lock: handshake the repo's pull mode via the caps cache (one network
    // call per service per daemon session). `caps` drives the pull strategy
    // (whole-repo vs bucketed, incremental vs full) and carries `repo_key` for
    // anchor derivation.
    let caps = caps_cache
        .get_or_fetch(svc.id, &svc.url)
        .with_context(|| format!("handshaking {}", svc.url))?;

    // #194: detect a re-seeded store via store_generation. Factored into a
    // shared helper so pull_repo_for_hashes can call the same logic without
    // duplicating it.
    reconcile_store_generation(db, svc.id, &caps, name)?;
    // If caps has no store_generation (pre-feature server), leave cursors
    // untouched and rely on the backwards-cursor guard (~L2804).

    // #141: a SHA-256 domain derives its bucket keys from `files.sha256`. A
    // library imported before eager dual-hashing (ADR 0018) has NULL there,
    // derives ZERO bucket keys, and the pull "succeeds" having pulled nothing.
    // Fill the interop column before the first fetch. On an old library this is
    // a long pass, so it is announced on the curated INFO activity feed at both
    // ends — it must never look like a hang. The pass re-runs on every pull
    // while files remain missing because unreadability is usually transient on
    // this platform (files held under exclusive handles, disconnected shares); a
    // persistent give-up marker would strand files that become readable later.
    //
    // #157: there are two distinct sub-populations of NULL-sha256 files:
    //
    //   backfillable — file has at least one present location; naiad can open
    //     and hash it now.
    //   offline     — file has NO present location; it cannot be hashed until
    //     its volume comes back and a rescan fills in the sha256. These files
    //     still derive no bucket key and are silently excluded from the pull,
    //     but they were already excluded from count_files_missing_sha256 (the
    //     old present-only form), so callers never saw them in the count and
    //     assumed the pull was complete. The count is now authoritative (all
    //     files), while the backfill work list remains present-only.
    if caps.serves(HashDomain::Sha256) {
        let (total_missing, backfillable) = {
            let db = db.lock_recover();
            let total = db.count_files_missing_sha256()?;
            let present = db.count_files_missing_sha256_present()?;
            (total, present)
        };
        // offline = files with no present location that cannot be hashed yet.
        let offline = total_missing.saturating_sub(backfillable);
        if total_missing > 0 {
            tracing::info!(
                target: "sync",
                repo = %name,
                total = total_missing,
                backfillable,
                offline,
                "sha256 domain: files without interop hash before pull; \
                 starting backfill for present files"
            );
        }
        if backfillable > 0 {
            // Take the skip set out and put it back rather than holding the
            // guard across the backfill: the pass can run for minutes of file
            // I/O on a slow volume, and holding this lock would stall every
            // concurrent pull. `lock_recover` (not `lock().expect()`) so a
            // panic anywhere in the backfill cannot poison the mutex and make
            // every later pull panic here for the life of the process.
            let mut skip = {
                let mut guard = caps_cache.sha256_backfill_skip.lock_recover();
                std::mem::take(&mut *guard)
            };
            let filled = crate::plugins::backfill_sha256(db, &mut skip);
            *caps_cache.sha256_backfill_skip.lock_recover() = skip;
            let filled = filled.context("backfilling sha256 for a sha256-domain repo")?;
            let (remaining_total, remaining_present) = {
                let db = db.lock_recover();
                let total = db.count_files_missing_sha256()?;
                let present = db.count_files_missing_sha256_present()?;
                (total, present)
            };
            let remaining_offline = remaining_total.saturating_sub(remaining_present);
            if filled > 0 {
                tracing::info!(
                    target: "sync",
                    repo = %name,
                    filled,
                    remaining_total,
                    remaining_present,
                    remaining_offline,
                    "sha256 backfill finished"
                );
            } else {
                tracing::debug!(
                    target: "sync",
                    repo = %name,
                    filled,
                    remaining_total,
                    remaining_present,
                    remaining_offline,
                    "sha256 backfill finished (nothing new filled)"
                );
            }
            // #144: files the backfill could not read keep no interop hash; log
            // the two classes so an operator can distinguish "needs a rescan"
            // (present but failed to open) from "volume offline" (no present
            // location). Both are transient on normal deployments.
            if remaining_present > 0 {
                tracing::warn!(
                    target: "sync",
                    repo = %name,
                    files = remaining_present,
                    "sha256 backfill: present files still missing interop hash \
                     (open/read failed); their tags cannot pull from the sha256 \
                     domain — try a rescan once the files are accessible"
                );
            }
            if remaining_offline > 0 {
                tracing::warn!(
                    target: "sync",
                    repo = %name,
                    files = remaining_offline,
                    "sha256 backfill: offline files have no interop hash (no \
                     present location); their tags cannot pull from the sha256 \
                     domain until the volume comes back and a rescan runs"
                );
            }
        } else if offline > 0 {
            // No present-backfillable files, but offline files exist. The
            // backfill does not run, but the operator should know why the
            // count is non-zero.
            tracing::warn!(
                target: "sync",
                repo = %name,
                files = offline,
                "sha256 domain: all files missing an interop hash are offline (no \
                 present location); their tags cannot pull until volumes come back \
                 and a rescan runs"
            );
        }
    }

    // Off-lock: fetch a snapshot (whole-repo), bucket snapshot, or bucket
    // delta. Capture the stats before returning them to the caller.
    //
    // Branch on the repo's advertised DOMAIN LIST, not on the single
    // `hash_domain` field: a dual-domain repo reports `blake3` there for the
    // benefit of old clients, and inferring from it would silently skip the
    // added SHA-256 domain.
    let domains = caps.domains();
    // Sort and dedup the domain list before matching so a repo that
    // accidentally advertises `["blake3","blake3"]` (or any other repeated
    // set) still hits the correct single-domain arm rather than falling
    // through to the `_` multi-domain arm and losing the incremental delta
    // path. `as_str()` gives a stable `&'static str` ordering that agrees
    // with the enum's textual representation.
    let domains = {
        let mut d = domains;
        d.sort_by_key(|dom| dom.as_str());
        d.dedup();
        d
    };
    // §5.3 / §5.4: detect a floor clamp-up before starting the pull, so the
    // notice is generated exactly once and the #169 doom warn is suppressed for
    // legs that the floor raises to a serveable width.
    if let (
        PullMode::Bucketed {
            prefix_bits: advertised,
        },
        Some(floor),
    ) = (&caps.mode, caps.min_query_bits)
    {
        let base = (*advertised).min(max_query_bits);
        for &domain in &domains {
            // #195: the floor applies to sha256 regardless of whether it is
            // native (mirror) or added (snapshot), so notify on sha256.
            if domain == HashDomain::Sha256 && base < floor {
                // Compute the actual effective width (caps `.min(advertised)` to
                // defend against a hostile floor > advertised) so the notice
                // message agrees with what the client will actually query at.
                let effective = base.max(floor).min(*advertised);
                note_floor_clamp_up(
                    caps_cache,
                    svc.id,
                    name,
                    domain,
                    max_query_bits,
                    floor,
                    effective,
                );
            }
        }
    }
    let stats = match domains.as_slice() {
        [HashDomain::Blake3] => match caps.mode {
            PullMode::WholeRepo => {
                tracing::debug!(target: "sync", repo = %name, "pull mode: whole-repo snapshot");
                let pull =
                    fetch_blake3_entries(db, &client, &caps, name, &svc.url, max_query_bits)?;
                let db_guard = db.lock_recover();
                merge_domain_entries(&db_guard, svc.id, HashDomain::Blake3, pull)?
            }
            PullMode::Bucketed {
                prefix_bits: advertised,
            } if caps.serves_deltas(HashDomain::Blake3) => {
                tracing::debug!(target: "sync", repo = %name, advertised, "bucketed pull is incremental (delta)");
                pull_domain_delta(
                    db,
                    &client,
                    &caps,
                    &svc,
                    name,
                    advertised,
                    max_query_bits,
                    HashDomain::Blake3,
                )?
            }
            PullMode::Bucketed {
                prefix_bits: advertised,
            } => {
                tracing::debug!(target: "sync", repo = %name, advertised, "pull mode: bucketed (full)");
                let pull =
                    fetch_blake3_entries(db, &client, &caps, name, &svc.url, max_query_bits)?;
                let db_guard = db.lock_recover();
                merge_domain_entries(&db_guard, svc.id, HashDomain::Blake3, pull)?
            }
        },

        // A repo whose only domain is SHA-256: the mirror-mode bridge.
        // Incremental delta path when the server advertises it; full pull
        // otherwise (snapshot-mode and old servers).
        [HashDomain::Sha256] => match caps.mode {
            PullMode::Bucketed {
                prefix_bits: advertised,
            } if caps.serves_deltas(HashDomain::Sha256) => {
                tracing::debug!(target: "sync", repo = %name, advertised, "sha256-domain pull is incremental (delta)");
                pull_domain_delta(
                    db,
                    &client,
                    &caps,
                    &svc,
                    name,
                    advertised,
                    max_query_bits,
                    HashDomain::Sha256,
                )?
            }
            _ => {
                tracing::debug!(target: "sync", repo = %name, mode = ?caps.mode, "sha256-domain pull (full)");
                let pull =
                    fetch_sha256_entries(db, &client, &caps, name, &svc.url, max_query_bits)?;
                let db_guard = db.lock_recover();
                merge_domain_entries(&db_guard, svc.id, HashDomain::Sha256, pull)?
            }
        },

        // Dual-domain: pull each advertised domain INDEPENDENTLY (#151).
        //
        // Every merge below is scoped to its own domain's provenance bit
        // (migration 0034), so neither leg can delete rows the other supplies.
        // That is what retires the old one-merge rule, and with it the full
        // snapshot merge this arm used to pay on every pull, forever — the
        // BLAKE3 leg now takes exactly the same incremental delta path a
        // single-domain repo gets.
        //
        // The legs are also independent on failure: each merge commits with its
        // own cursor, so a SHA-256 fetch that fails after the BLAKE3 leg
        // committed leaves BLAKE3's progress intact and retries only SHA-256
        // next pull. The previous all-or-nothing behaviour threw away a
        // successful domain's work whenever the other one failed.
        _ => {
            tracing::debug!(
                target: "sync",
                repo = %name,
                domains = ?domains,
                "dual-domain pull: one independent, domain-scoped merge per domain"
            );

            // ── BLAKE3 leg: incremental where the repo supports it ──────────
            if domains.contains(&HashDomain::Blake3) {
                match caps.mode {
                    PullMode::Bucketed {
                        prefix_bits: advertised,
                    } if caps.serves_deltas(HashDomain::Blake3) => {
                        tracing::debug!(target: "sync", repo = %name, advertised, "dual-domain blake3 leg: incremental (delta)");
                        pull_domain_delta(
                            db,
                            &client,
                            &caps,
                            &svc,
                            name,
                            advertised,
                            max_query_bits,
                            HashDomain::Blake3,
                        )?
                    }
                    _ => {
                        tracing::debug!(target: "sync", repo = %name, mode = ?caps.mode, "dual-domain blake3 leg: full");
                        let pull = fetch_blake3_entries(
                            db,
                            &client,
                            &caps,
                            name,
                            &svc.url,
                            max_query_bits,
                        )?;
                        let db_guard = db.lock_recover();
                        merge_domain_entries(&db_guard, svc.id, HashDomain::Blake3, pull)?
                    }
                };
            }

            // ── SHA-256 leg: incremental when served, else full ─────────────
            if domains.contains(&HashDomain::Sha256) {
                if let PullMode::Bucketed {
                    prefix_bits: advertised,
                } = caps.mode
                {
                    if caps.serves_deltas(HashDomain::Sha256) {
                        tracing::debug!(target: "sync", repo = %name, advertised, "dual-domain sha256 leg: incremental (delta)");
                        pull_domain_delta(
                            db,
                            &client,
                            &caps,
                            &svc,
                            name,
                            advertised,
                            max_query_bits,
                            HashDomain::Sha256,
                        )?;
                    } else {
                        let pull = fetch_sha256_entries(
                            db,
                            &client,
                            &caps,
                            name,
                            &svc.url,
                            max_query_bits,
                        )?;
                        let db_guard = db.lock_recover();
                        merge_domain_entries(&db_guard, svc.id, HashDomain::Sha256, pull)?;
                    }
                } else {
                    let pull =
                        fetch_sha256_entries(db, &client, &caps, name, &svc.url, max_query_bits)?;
                    let db_guard = db.lock_recover();
                    merge_domain_entries(&db_guard, svc.id, HashDomain::Sha256, pull)?;
                }
            }

            // Report the service's post-merge totals rather than summing the two
            // legs. Summing would double-count a file both domains matched — the
            // reason this arm used to coalesce entries by hash before merging —
            // and the BLAKE3 leg's delta stats describe only the rows that
            // changed, which is not comparable with the SHA-256 leg's full-pull
            // stats. One read of the merged result is both cheaper and honest.
            let db_guard = db.lock_recover();
            db_guard.service_mapping_stats(svc.id)?
        }
    };

    // Petition status polling removed in v6 (petitions endpoint deleted).

    tracing::info!(
        target: "sync",
        repo = %name,
        domains = ?domains,
        mode = ?caps.mode,
        matched = stats.matched_files,
        mappings = stats.mappings,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "pull finished",
    );
    Ok(stats)
}

/// The result of a per-file pull ([`pull_repo_for_hashes`]).
///
/// Carries the ordinary merge stats plus the requested hashes that could not
/// be queried at all. The list is the observable symptom behind #141/#143:
/// without it the caller cannot tell "upstream has no tags for this file" from
/// "this file has no SHA-256 interop hash, so we never asked" (spec §6).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FilePullOutcome {
    /// Merge statistics for the scoped merge.
    pub stats: MergeStats,
    /// Requested BLAKE3 hashes that could not be resolved to a SHA-256 interop
    /// hash — either the `files.sha256` column is NULL, the stored value is
    /// unparseable, or the hash is not in the local library at all (the API
    /// accepts arbitrary hex, so callers can request unowned files). Only the
    /// NULL case is repairable by a library backfill; the others are not.
    /// Always empty for a repo that serves no SHA-256 domain.
    pub missing_sha256: Vec<Hash>,
}

/// Fetch one scoped (non-incremental) snapshot for `domain` from `client`.
///
/// Bucketed mode masks `keys_from` to the privacy ceiling (`max_query_bits`)
/// and issues a bucket fetch; `WholeRepo` falls back to a full snapshot.
/// Used by [`pull_repo_for_hashes`] for both the BLAKE3 and SHA-256 legs so
/// the `match caps.mode { … }` pattern lives in exactly one place.
///
/// # Errors
/// Propagates any network or decode error from the underlying client call.
fn fetch_scoped_snapshot(
    client: &RepoClient,
    caps: &Caps,
    url: &str,
    domain: HashDomain,
    keys_from: &[Hash],
    max_query_bits: u32,
    observer: &dyn naiad_netproto::PullObserver,
) -> Result<naiad_netproto::Snapshot> {
    let wire = caps.wire_domain(domain);
    match caps.mode {
        PullMode::WholeRepo => client
            .fetch_snapshot_in(wire, observer)
            .with_context(|| format!("pulling from {url}")),
        PullMode::Bucketed {
            prefix_bits: advertised,
        } => {
            // #195: apply the floor to the sha256 domain whether it is native
            // (mirror mode) or added (snapshot mode). Blake3 domain: never floored.
            let floor = caps.min_query_bits.filter(|_| domain == HashDomain::Sha256);
            let prefix_bits = effective_prefix_bits_floored(advertised, max_query_bits, floor);
            // Full pulls warn via clamped_prefix_bits when the ceiling fires;
            // scoped pulls log at debug to avoid spam (per-file pulls are
            // high-frequency, so a warn here would flood the log).
            if prefix_bits != advertised {
                tracing::debug!(
                    target: "sync",
                    domain = domain.as_str(),
                    advertised,
                    negotiated = prefix_bits,
                    "scoped pull: clamped to negotiated width"
                );
            }
            let mut keys: Vec<String> = keys_from
                .iter()
                .map(|h| bucket_key(h, prefix_bits))
                .collect();
            keys.sort();
            keys.dedup();
            let hint = seed_ms_per_bucket(caps, domain, advertised, prefix_bits);
            tracing::debug!(
                target: "sync",
                domain = domain.as_str(),
                advertised,
                requested_bits = prefix_bits,
                hint_bits = ?caps.serve_hint.get(domain.as_str()).and_then(|h| h.hint_bits),
                seed_ms = ?hint,
                "first-window seed"
            );
            with_clamp_hint(
                client
                    .fetch_buckets_in(prefix_bits, &keys, wire, hint, observer, caps.streaming)
                    .with_context(|| format!("pulling buckets from {url}")),
                caps,
                advertised,
                prefix_bits,
            )
        }
    }
}

/// Pull tags for specific owned `hashes` from one subscribed repo — the
/// per-file counterpart of [`pull_repo`]. Bucketed mode masks the requested
/// hashes to the same privacy-clamped prefix a full pull would use, so a
/// per-file pull never reveals finer precision than `max_query_bits`.
/// WholeRepo mode fetches the snapshot and filters. The merge is scoped
/// (authoritative only for `hashes`) and deliberately does NOT touch the
/// incremental cursor / pull-state: a one-file pull must never masquerade as
/// a full sync.
///
/// Every domain the repo advertises is queried, and all domains' entries feed
/// **one** scoped merge — `merge_pulled_mappings_for_files` is authoritative
/// for the requested files, so two sequential merges would have the second
/// wipe the first (#143, and the same rule as the dual-domain full pull).
///
/// # Errors
/// Returns an error if the repo is unknown/detached, the handshake or fetch
/// fails, or a database operation fails.
pub fn pull_repo_for_hashes(
    db: &Mutex<Db>,
    caps_cache: &CapsCache,
    name: &str,
    max_query_bits: u32,
    hashes: &[Hash],
    observer: &dyn naiad_netproto::PullObserver,
) -> Result<FilePullOutcome> {
    // Brief lock: resolve the subscribed repo.
    let svc = {
        let db = db.lock_recover();
        db.shared_service_by_name(name)?
            .ok_or_else(|| anyhow!("no such repo: {name}"))?
    };
    let started = Instant::now();
    tracing::debug!(target: "sync", repo = %name, hashes = hashes.len(), "per-file pull starting");
    let client = caps_cache.client(&svc.url);
    let caps = caps_cache
        .get_or_fetch(svc.id, &svc.url)
        .with_context(|| format!("handshaking {}", svc.url))?;

    // #194: same generation-reconciliation that pull_repo performs — this path
    // also advances service_domain_pull_state cursors, so it must check too.
    reconcile_store_generation(db, svc.id, &caps, name)?;

    // §5.3: detect a floor clamp-up before starting the per-file pull.
    if let (
        PullMode::Bucketed {
            prefix_bits: advertised,
        },
        Some(floor),
    ) = (&caps.mode, caps.min_query_bits)
    {
        let base = (*advertised).min(max_query_bits);
        for domain in caps.domains() {
            // #195: notify on sha256 regardless of native/added (mirror or snapshot).
            if domain == HashDomain::Sha256 && base < floor {
                let effective = base.max(floor).min(*advertised);
                note_floor_clamp_up(
                    caps_cache,
                    svc.id,
                    name,
                    domain,
                    max_query_bits,
                    floor,
                    effective,
                );
            }
        }
    }

    // Each domain's entries stay in their own bucket: they are merged
    // separately, under their own provenance bit (#151), so that neither
    // domain's authoritative removals touch the other's rows.
    let mut blake3_entries: TaggedEntries = Vec::new();
    let mut sha256_entries: TaggedEntries = Vec::new();
    let mut missing_sha256: Vec<Hash> = Vec::new();

    // ── Native BLAKE3 domain ────────────────────────────────────────────────
    // Scoped pulls always use the plain (non-incremental) fetch even against an
    // incremental repo — deltas are keyed to the full-pull cursor, which a
    // scoped pull must not consume or advance.
    if caps.serves(HashDomain::Blake3) {
        observer.set_domain(Some("blake3"));
        let snapshot = fetch_scoped_snapshot(
            &client,
            &caps,
            &svc.url,
            HashDomain::Blake3,
            hashes,
            max_query_bits,
            observer,
        )?;
        blake3_entries.extend(mapping_snapshot_entries(snapshot));
    }

    // ── SHA-256 domain (#143) ───────────────────────────────────────────────
    if caps.serves(HashDomain::Sha256) {
        // Look up each requested file's interop hash. A file that cannot be
        // resolved to a SHA-256 — NULL in the DB, an unparseable stored value,
        // or simply not in the library — is reported in `missing_sha256` and
        // skipped. Only the NULL case is repairable by a library backfill;
        // unowned and corrupt entries stay missing regardless.
        let mut sha_to_blake3: HashMap<String, Hash> = HashMap::new();
        let mut sha_keys: Vec<Hash> = Vec::new();
        let mut unparseable_n: u64 = 0;
        let mut unparseable_samples: Vec<String> = Vec::new();
        {
            let db_guard = db.lock_recover();
            for h in hashes {
                match db_guard.sha256_of(h)? {
                    Some(sha_hex) => {
                        let sha_hex = sha_hex.to_lowercase();
                        match sha_hex.parse::<Hash>() {
                            Ok(sha_as_hash) => {
                                sha_keys.push(sha_as_hash);
                                sha_to_blake3.insert(sha_hex, *h);
                            }
                            Err(_) => {
                                unparseable_n += 1;
                                if unparseable_samples.len() < 3 {
                                    unparseable_samples.push(truncate_for_log(&sha_hex));
                                }
                                missing_sha256.push(*h);
                            }
                        }
                    }
                    None => missing_sha256.push(*h),
                }
            }
        }
        if unparseable_n > 0 {
            tracing::warn!(
                target: "sync",
                repo = %name,
                files = unparseable_n,
                sample = %unparseable_samples.join(", "),
                "per-file pull: files have an unparseable sha256 stored in the DB \
                 (data integrity issue); skipping their sha256-domain query"
            );
        }
        if !missing_sha256.is_empty() {
            tracing::warn!(
                target: "sync",
                repo = %name,
                files = missing_sha256.len(),
                "per-file pull: some files could not be resolved to a SHA-256 (NULL, \
                 unparseable stored value, or not in the library); no sha256-domain query \
                 was issued for them — the NULL case is repairable by a library backfill, \
                 others are not"
            );
        }
        if !sha_keys.is_empty() {
            observer.set_domain(Some("sha256"));
            let snapshot = fetch_scoped_snapshot(
                &client,
                &caps,
                &svc.url,
                HashDomain::Sha256,
                &sha_keys,
                max_query_bits,
                observer,
            )?;
            // Translate sha256-keyed rows back to BLAKE3 identities.
            let sha_entries = mapping_snapshot_entries(snapshot);
            sha256_entries.extend(translate_sha256_entries(sha_entries, &sha_to_blake3));
        }
    }

    // Coalesce each domain's entries by hash. A single domain can still emit a
    // hash more than once (bucket responses are not deduplicated), and the
    // file-scoped merge keys its per-file tag set by file_id, so a duplicate
    // hash would otherwise have the later group overwrite the earlier one —
    // the #143 regression. Coalescing is now per domain rather than across
    // domains, since the two are merged separately.
    let coalesce = |entries: TaggedEntries| -> TaggedEntries {
        let mut by_hash: HashMap<Hash, Vec<(Tag, Option<String>)>> =
            HashMap::with_capacity(entries.len());
        for (hash, tags) in entries {
            by_hash.entry(hash).or_default().extend(tags);
        }
        by_hash.into_iter().collect()
    };
    let blake3_entries = coalesce(blake3_entries);
    let sha256_entries = coalesce(sha256_entries);

    // Brief lock: scoped, idempotent, domain-scoped merges — authoritative only
    // for `hashes` within each domain, never touching pull-state / incremental
    // cursor. Running one per domain is safe because each is confined to its own
    // provenance bit; before #151 both had to be coalesced into a single call.
    observer.set_domain(None);
    observer.on_phase(naiad_netproto::PullPhase::Merging);
    let db_guard = db.lock_recover();
    let mut added = 0u64;
    if caps.serves(HashDomain::Blake3) {
        added += db_guard
            .merge_pulled_mappings_for_files_in_domain(
                svc.id,
                hashes,
                HashDomain::Blake3.as_str(),
                &blake3_entries,
            )?
            .mappings;
    }
    if caps.serves(HashDomain::Sha256) {
        added += db_guard
            .merge_pulled_mappings_for_files_in_domain(
                svc.id,
                hashes,
                HashDomain::Sha256.as_str(),
                &sha256_entries,
            )?
            .mappings;
    }
    // `mappings` feeds the wire's `mappings_added`, which is additive by
    // contract — so it stays the sum of what the legs actually wrote, and a
    // repeat pull still reports zero. `matched_files` is read back instead of
    // summed, because a file both domains matched would otherwise be counted
    // once per domain (the double-count the pre-#151 coalesce prevented).
    let stats = MergeStats {
        matched_files: db_guard.file_mapping_stats(svc.id, hashes)?.matched_files,
        mappings: added,
    };
    tracing::debug!(
        target: "sync",
        repo = %name,
        matched = stats.matched_files,
        mappings = stats.mappings,
        missing_sha256 = missing_sha256.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "per-file pull finished"
    );
    observer.on_phase(naiad_netproto::PullPhase::Done);
    Ok(FilePullOutcome {
        stats,
        missing_sha256,
    })
}

/// One domain's worth of a pull: entries already translated to BLAKE3 file
/// identities, the cursor the repo returned, and a file-id watermark.
struct DomainPull {
    entries: TaggedEntries,
    cursor: u64,
    /// File-id watermark read atomically with the owned-hash / sha256-map query so
    /// the stored pull-state marker never overshoots what was actually requested.
    /// `None` is only correct for the BLAKE3 WholeRepo arm: a whole-repo blake3
    /// fetch resolves file ids at merge time and genuinely covers every file in
    /// existence, so [`record_domain_cursor`] may safely re-read `max_file_id`.
    /// SHA-256 arms must always carry `Some(file_marker)` because the translation
    /// map (`sha_to_blake3`) is snapshotted at T1 — files imported after T1 are
    /// absent from the map and silently dropped from the entries regardless of mode.
    marker: Option<i64>,
}

/// Record one `(service, domain)` pair's pull state after a merge.
///
/// `cursor == 0` means the repo is pre-incremental, stopped advertising, or —
/// in snapshot mode — has no sequence at all. A stale stored cursor could
/// outlive a repo rebuild and dodge the reset guard, so drop the state and
/// start over next time.
///
/// `marker` is the domain's watermark read atomically with the owned-key set
/// (`files.id` for BLAKE3, `sha256_seq` for SHA-256); when `None` (BLAKE3
/// WholeRepo only) the watermark is re-read as `max_file_id` under this lock
/// because a whole-repo blake3 fetch covers every file that could exist at the
/// query time. SHA-256 callers must always pass `Some` — the `None` fallback is
/// in file-id units and would store the wrong unit for a sha256_seq marker.
fn record_domain_cursor(
    db_guard: &Db,
    service_id: i64,
    domain: HashDomain,
    cursor: u64,
    marker: Option<i64>,
) -> Result<()> {
    if cursor > 0 {
        let m = match marker {
            Some(m) => m,
            None => db_guard.max_file_id()?,
        };
        db_guard.set_mapping_pull_state(service_id, domain.as_str(), cursor, m)?;
    } else {
        db_guard.clear_mapping_pull_state(service_id, domain.as_str())?;
    }
    Ok(())
}

/// Authoritatively merge one domain's entries into a service and record that
/// domain's cursor.
///
/// The merge is scoped to `domain`'s provenance bit, so it is authoritative for
/// that domain across the whole service and inert with respect to every other
/// domain's rows. Calling it once per domain in a single pull is therefore safe
/// — the one-merge contract this function carried before #151 is retired.
fn merge_domain_entries(
    db_guard: &Db,
    service_id: i64,
    domain: HashDomain,
    pull: DomainPull,
) -> Result<MergeStats> {
    let stats =
        db_guard.merge_pulled_mappings_in_domain(service_id, domain.as_str(), &pull.entries)?;
    record_domain_cursor(db_guard, service_id, domain, pull.cursor, pull.marker)?;
    Ok(stats)
}

/// Pull one hash domain incrementally (bucket deltas) and merge it, scoped to
/// that domain's provenance bit so it composes with an independent leg for the
/// other domain in the same pull.
///
/// Parameterised — not forked — from the old `pull_blake3_delta`: exactly four
/// concerns vary by domain (key source, new-keys query, marker read, entry
/// translation), and every subtle part (the same-lock marker read, the
/// first-pull `since` fill, the cursor-reset guard, the min-cursor chunk merge
/// in `fetch_bucket_delta_in`) is shared, so a fix to any of them lands once.
/// `pull_domain_delta` never asks WHICH domains a repo serves — only how to key
/// the one it was handed — which is what lets "sha256 is the only domain" and
/// "sha256 is an added domain" share this path (spec decision 2).
#[allow(clippy::too_many_arguments)] // 8 params: domain-generic parameterisation; grouping would obscure the strategy
fn pull_domain_delta(
    db: &Mutex<Db>,
    client: &RepoClient,
    caps: &Caps,
    svc: &SharedService,
    name: &str,
    advertised: u32,
    max_query_bits: u32,
    domain: HashDomain,
) -> Result<MergeStats> {
    let started = Instant::now();
    // #195: apply the floor to the sha256 domain whether it is native (mirror
    // mode) or added (snapshot mode). Blake3 domain: never floored.
    let floor = caps.min_query_bits.filter(|_| domain == HashDomain::Sha256);
    let prefix_bits = clamped_prefix_bits(caps, domain, name, advertised, max_query_bits, floor);

    // One brief lock reads everything that must be mutually consistent: owned
    // keys, the stored cursor+marker, the CURRENT marker, and the new-keys set —
    // so a file that gains a key mid-read can't land inside `current_marker`
    // while missing from the requested buckets. The sha256 arm also snapshots
    // its sha256→blake3 translation map here, against the same marker
    // (max_sha256_seq), so the map and the marker agree.
    let (owned, sha_to_blake3, stored_cursor, stored_marker, current_marker, new_keys) = {
        let db = db.lock_recover();
        let cursor = db.mapping_cursor(svc.id, domain.as_str())?;
        let marker = db.last_pull_file_marker(svc.id, domain.as_str())?;
        match domain {
            HashDomain::Sha256 => {
                let (keys, map, _malformed) = db.sha256_domain_pull_inputs()?;
                let current = db.max_sha256_seq()?;
                let new_keys =
                    db.owned_sha256_bucket_keys_after_seq(prefix_bits, marker.unwrap_or(0))?;
                (keys, Some(map), cursor, marker, current, new_keys)
            }
            _ => {
                let owned = db.owned_hashes()?;
                let current = db.max_file_id()?;
                let new_keys =
                    db.owned_bucket_keys_after_file_id(prefix_bits, marker.unwrap_or(0))?;
                (owned, None, cursor, marker, current, new_keys)
            }
        }
    };
    let mut keys: Vec<String> = owned.iter().map(|h| bucket_key(h, prefix_bits)).collect();
    keys.sort();
    keys.dedup();

    let wire = caps.wire_domain(domain);
    let cursor = stored_cursor.unwrap_or(0).max(0) as u64;
    let mut since: Vec<u64> = keys
        .iter()
        .map(|k| {
            if new_keys.binary_search(k).is_ok() {
                0
            } else {
                cursor
            }
        })
        .collect();
    if stored_marker.is_none() {
        since.fill(0);
    }

    let mut delta = client
        .fetch_bucket_delta_in(prefix_bits, &keys, &since, wire)
        .with_context(|| format!("pulling bucket deltas from {}", svc.url))?;

    if stored_cursor.is_some_and(|c| delta.cursor < c as u64) {
        since.fill(0);
        delta = client
            .fetch_bucket_delta_in(prefix_bits, &keys, &since, wire)
            .with_context(|| format!("re-syncing bucket deltas from {}", svc.url))?;
    }

    let full_buckets: Vec<(String, String)> = keys
        .iter()
        .zip(since.iter())
        .filter_map(|(k, s)| {
            if *s == 0 {
                let lo = k.parse::<Hash>().ok()?;
                Some((bucket_key(&lo, prefix_bits), bucket_upper(&lo, prefix_bits)))
            } else {
                None
            }
        })
        .collect();
    let inputs = mapping_delta_inputs(delta.changes);
    // Translate to blake3 identities before the merge. Blake3 is identity;
    // sha256 maps each row through the snapshotted map (dropping unowned rows),
    // so `merge_mapping_delta`'s per-row path stays blake3-keyed and unchanged.
    let changes = match &sha_to_blake3 {
        Some(map) => translate_sha256_delta_inputs(inputs, map),
        None => inputs,
    };
    let db_guard = db.lock_recover();
    let stats = db_guard.merge_mapping_delta(
        svc.id,
        domain.as_str(),
        &changes,
        &full_buckets,
        delta.cursor,
        current_marker,
    )?;
    tracing::debug!(
        target: "sync",
        repo = %name,
        domain = domain.as_str(),
        prefix_bits,
        keys = keys.len(),
        since_zero = since.iter().filter(|s| **s == 0).count(),
        changes = changes.len(),
        mappings = stats.mappings,
        cursor = delta.cursor,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "domain delta pull finished"
    );
    Ok(stats)
}

/// Detect and record a floor clamp-up for `(svc_id, domain)` the first time it
/// fires this session. Emits one `warn!(target: "sync", …)` line and pushes a
/// UI-toast notice into `caps_cache.pending_notices` (#179 §5.3).
/// Subsequent calls for the same `(svc_id, domain)` are silent (warn-once).
fn note_floor_clamp_up(
    caps_cache: &CapsCache,
    svc_id: i64,
    name: &str,
    domain: HashDomain,
    ceiling: u32,
    floor: u32,
    effective: u32,
) {
    let first = caps_cache
        .floor_clamp_warned
        .lock_recover()
        .insert((svc_id, domain));
    if !first {
        return; // already warned this session
    }
    // `effective` is the actual query width (`base.max(floor).min(advertised)`),
    // which agrees with the floor when the server caps are well-formed but caps
    // at `advertised` on a hostile floor > advertised advertisement.
    let msg = format!(
        "repo {name}: your privacy ceiling for the {domain} domain \
         (max_query_bits = {ceiling}) is below this repo's minimum query width \
         ({floor}); querying at {effective} bits (still k-anonymous) so the pull \
         can proceed. Raise max_query_bits for this repo in [[repos]] to silence this.",
        domain = domain.as_str()
    );
    tracing::warn!(
        target: "sync",
        repo = %name,
        domain = domain.as_str(),
        ceiling,
        floor,
        effective,
        "privacy ceiling below repo floor; clamping up"
    );
    caps_cache
        .pending_notices
        .lock_recover()
        .entry(svc_id)
        .or_default()
        .push(msg);
}

/// Clamp `advertised` to `max_query_bits` (the caller's privacy ceiling) and
/// emit a structured log event when the clamp fires. Shared by
/// [`fetch_blake3_entries`] and [`fetch_sha256_entries`] so the message stays
/// consistent and `domain` is always present as a field.
///
/// The clamp fires on two qualitatively different repo kinds:
///
/// * **Non-snapshot repos** advertise a finite prefix width (e.g. 24 bits) that
///   the server can scan efficiently; a clamp here is purely a local privacy
///   choice and proceeds safely → logged at `debug!`.
/// * **Snapshot-inferred repos** advertise 256 bits (the full hash width) because
///   they cannot do bounded bucket scans — the snapshot backend enumerates the
///   whole store. The client default ceiling of 24 bits will therefore clamp
///   every pull, forcing wide range scans that predictably blow the HTTP
///   timeout (#169). This condition is logged at `warn!` so operators notice
///   and can add `max_query_bits = 256` to the `[[repos]]` entry.
fn clamped_prefix_bits(
    caps: &naiad_netproto::Caps,
    domain: HashDomain,
    name: &str,
    advertised: u32,
    max_query_bits: u32,
    floor: Option<u32>,
) -> u32 {
    let prefix_bits = effective_prefix_bits_floored(advertised, max_query_bits, floor);
    // §5.4: only fire the #169 clamp-down warn when the result was NOT produced
    // by the floor raise. When the floor raised the width, the §5.3
    // note_floor_clamp_up path (called by pull_repo / pull_repo_for_hashes) owns
    // the advisory — the "coarse buckets will time out" prophecy does not apply
    // when the server has explicitly declared the floor serveable.
    // §5.4: fire the #169 clamp-down warn only when the result was NOT produced
    // by the floor raise. Gate: `prefix_bits < advertised` (any clamp happened)
    // AND `floor.is_none_or(|f| prefix_bits > f)` (the clamp-down result is
    // above the floor, i.e. not floor-raised). When base == floor (ceiling ==
    // floor), prefix_bits == f, so `prefix_bits > f` is false → no warn, which
    // is correct: the width is already at the floor, not a doomed coarse scan.
    if prefix_bits < advertised && floor.is_none_or(|f| prefix_bits > f) {
        if caps.snapshot_inferred() {
            tracing::warn!(
                target: "sync",
                repo = %name,
                domain = domain.as_str(),
                advertised,
                ceiling = max_query_bits,
                effective = prefix_bits,
                "privacy ceiling clamps this snapshot-backed repo's queries; \
                 coarse buckets force slow range scans and can time out — \
                 consider raising max_query_bits for this repo in [[repos]]"
            );
        } else {
            tracing::debug!(
                target: "sync",
                repo = %name,
                domain = domain.as_str(),
                advertised,
                ceiling = max_query_bits,
                effective = prefix_bits,
                "repo advertised a finer query prefix than the privacy ceiling; \
                 clamping (pull proceeds with wider buckets, more download)"
            );
        }
    }
    prefix_bits
}

/// Pessimistic first-window seed (ms/bucket) for a coarse pull with no measured
/// hint (#178 §5.3). Large enough that `round(WINDOW_TARGET_MS / this)` is far
/// below `MIN_WINDOW`, so netproto's `.max(MIN_WINDOW)` clamp seeds the first
/// window at exactly the 32-bucket floor instead of the body-budget maximum.
/// Models "unmeasured coarse bucket ⇒ assume expensive," which #170 proved
/// correct at ≤ 24 bits on the PTR snapshot.
const COARSE_BOOTSTRAP_MS: f64 = 1.0e6;

/// The first-window `ms_per_bucket` seed to pass to `fetch_buckets_in` for a
/// bucketed pull of `domain` at `requested_bits`, given the repo `caps` and
/// the width it advertised (`advertised`, i.e. the `caps.mode` prefix bits)
/// (#178, spec §6.2 — that table is normative).
///
/// * A usable hint (`caps.serve_hint[domain].ms_per_bucket > 0`) is re-scaled
///   from the width it was measured at onto `requested_bits` along the cost
///   curve `cost(b) ∝ 2^(-b)`:
///   `scaled = ms × 2^(hint_bits − requested_bits)`.
///   `hint_bits` defaults to `advertised` when the server did not stamp it
///   (pre-#178 or non-normalising server — §5.2 fallback).
/// * No usable hint and the pull is clamped COARSER than advertised
///   (`requested_bits < advertised`) → the §5.3 bootstrap: a pessimistic seed
///   that collapses netproto's first window to `MIN_WINDOW`.
/// * No usable hint and not clamped coarse → `None` (netproto's body-budget-max
///   first window, today's behaviour — fine-width first windows are cheap).
/// * Malformed hint (`ms_per_bucket ≤ 0` or non-finite) is treated as absent.
fn seed_ms_per_bucket(
    caps: &naiad_netproto::Caps,
    domain: HashDomain,
    advertised: u32,
    requested_bits: u32,
) -> Option<f64> {
    match caps.serve_hint.get(domain.as_str()) {
        Some(h) if h.ms_per_bucket > 0.0 && h.ms_per_bucket.is_finite() => {
            let hint_bits = h.hint_bits.unwrap_or(advertised);
            let e = (hint_bits as i32 - requested_bits as i32).clamp(
                -naiad_netproto::HINT_SHIFT_CLAMP,
                naiad_netproto::HINT_SHIFT_CLAMP,
            );
            Some(h.ms_per_bucket * 2f64.powi(e))
        }
        _ if requested_bits < advertised => Some(COARSE_BOOTSTRAP_MS),
        _ => None,
    }
}

/// Append the #169 remediation hint to a failed bucket fetch, but only when
/// this pull was actually clamped (`effective < advertised`) against a repo
/// that looks snapshot-backed. A coarse-width scan on a snapshot backend can
/// exceed the per-request HTTP timeout; the hint names the exact config key
/// that fixes it. On any other failure the chain is passed through untouched.
fn with_clamp_hint<T>(
    res: Result<T>,
    caps: &naiad_netproto::Caps,
    advertised: u32,
    effective: u32,
) -> Result<T> {
    if effective < advertised && caps.snapshot_inferred() {
        res.with_context(|| {
            format!(
                "pull was clamped to {effective}-bit buckets (repo supports {advertised}); \
                 snapshot repos scan slowly at coarse widths — consider \
                 `max_query_bits = {advertised}` for this repo in [[repos]]"
            )
        })
    } else {
        res
    }
}

/// Fetch the BLAKE3 domain non-incrementally (whole-repo or bucketed) and
/// return its entries. The incremental delta path deliberately lives inline in
/// [`pull_repo`]'s single-domain arm: deltas cannot be concatenated with
/// another domain's snapshot into one authoritative merge.
fn fetch_blake3_entries(
    db: &Mutex<Db>,
    client: &RepoClient,
    caps: &naiad_netproto::Caps,
    name: &str,
    url: &str,
    max_query_bits: u32,
) -> Result<DomainPull> {
    let wire = caps.wire_domain(HashDomain::Blake3);
    let (snapshot, marker) = match caps.mode {
        PullMode::WholeRepo => {
            let snap = client
                .fetch_snapshot_in(wire, &naiad_netproto::NoopObserver)
                .with_context(|| format!("pulling from {url}"))?;
            (snap, None)
        }
        PullMode::Bucketed {
            prefix_bits: advertised,
        } => {
            // Blake3 is always the native domain; the floor never applies here.
            let prefix_bits = clamped_prefix_bits(
                caps,
                HashDomain::Blake3,
                name,
                advertised,
                max_query_bits,
                None,
            );
            // Read max_file_id in the SAME lock as owned_hashes so no file
            // imported between the two reads can fall inside the marker while
            // missing from the queried buckets.
            let (owned, file_marker) = {
                let db = db.lock_recover();
                (db.owned_hashes()?, db.max_file_id()?)
            };
            let mut keys: Vec<String> = owned.iter().map(|h| bucket_key(h, prefix_bits)).collect();
            keys.sort();
            keys.dedup();
            let hint = seed_ms_per_bucket(caps, HashDomain::Blake3, advertised, prefix_bits);
            tracing::debug!(
                target: "sync",
                domain = HashDomain::Blake3.as_str(),
                advertised,
                requested_bits = prefix_bits,
                hint_bits = ?caps.serve_hint.get(HashDomain::Blake3.as_str()).and_then(|h| h.hint_bits),
                seed_ms = ?hint,
                "first-window seed"
            );
            let snap = with_clamp_hint(
                client
                    .fetch_buckets_in(
                        prefix_bits,
                        &keys,
                        wire,
                        hint,
                        &naiad_netproto::NoopObserver,
                        caps.streaming,
                    )
                    .with_context(|| format!("pulling buckets from {url}")),
                caps,
                advertised,
                prefix_bits,
            )?;
            (snap, Some(file_marker))
        }
    };
    let cursor = snapshot.cursor;
    let entries = mapping_snapshot_entries(snapshot);
    tracing::debug!(
        target: "sync",
        repo = %name,
        domain = HashDomain::Blake3.as_str(),
        entries = entries.len(),
        cursor,
        "domain fetch complete"
    );
    Ok(DomainPull {
        entries,
        cursor,
        marker,
    })
}

/// Fetch the SHA-256 domain (full pull), derive bucket keys from `files.sha256`,
/// and translate the returned sha256-keyed entries back to BLAKE3 identities.
/// The full-pull marker is `max_sha256_seq`; the incremental path lives in
/// `pull_domain_delta`.
fn fetch_sha256_entries(
    db: &Mutex<Db>,
    client: &RepoClient,
    caps: &naiad_netproto::Caps,
    name: &str,
    url: &str,
    max_query_bits: u32,
) -> Result<DomainPull> {
    let wire = caps.wire_domain(HashDomain::Sha256);
    // Read sha256_domain_pull_inputs and max_sha256_seq in one brief lock so
    // the map and the marker are consistent. The call now returns a
    // malformed-row count (#158); the WARN is already emitted by the DB layer.
    // The full-pull marker MUST be max_sha256_seq (the meta counter), read
    // in this same lock so it never overshoots the key set actually offered.
    // A files.id marker was unsound: a file backfilled after import keeps its
    // id but gains a NEW sha256 key, which a files.id marker would classify
    // "already covered" and never pull (spec §Watermark).
    let (sha_owned, sha_to_blake3, _malformed, sha_marker) = {
        let db = db.lock_recover();
        let (sha_owned, sha_to_blake3, malformed) = db.sha256_domain_pull_inputs()?;
        let sha_marker = db.max_sha256_seq()?;
        (sha_owned, sha_to_blake3, malformed, sha_marker)
    };
    let (snapshot, marker) = match caps.mode {
        PullMode::WholeRepo => {
            let snap = client
                .fetch_snapshot_in(wire, &naiad_netproto::NoopObserver)
                .with_context(|| format!("pulling from {url}"))?;
            // sha_to_blake3 was snapshotted at T1; files imported after T1 are absent
            // from the map and dropped from entries, so the marker must be T1.
            (snap, Some(sha_marker))
        }
        PullMode::Bucketed {
            prefix_bits: advertised,
        } => {
            // #195: sha256 domain always carries the floor — mirror (native) or
            // snapshot (added). The domain here is hardcoded to Sha256.
            let floor = caps.min_query_bits;
            let prefix_bits = clamped_prefix_bits(
                caps,
                HashDomain::Sha256,
                name,
                advertised,
                max_query_bits,
                floor,
            );
            let mut keys: Vec<String> = sha_owned
                .iter()
                .map(|h| bucket_key(h, prefix_bits))
                .collect();
            keys.sort();
            keys.dedup();
            let hint = seed_ms_per_bucket(caps, HashDomain::Sha256, advertised, prefix_bits);
            tracing::debug!(
                target: "sync",
                domain = HashDomain::Sha256.as_str(),
                advertised,
                requested_bits = prefix_bits,
                hint_bits = ?caps.serve_hint.get(HashDomain::Sha256.as_str()).and_then(|h| h.hint_bits),
                seed_ms = ?hint,
                "first-window seed"
            );
            let snap = with_clamp_hint(
                client
                    .fetch_buckets_in(
                        prefix_bits,
                        &keys,
                        wire,
                        hint,
                        &naiad_netproto::NoopObserver,
                        caps.streaming,
                    )
                    .with_context(|| format!("pulling buckets from {url}")),
                caps,
                advertised,
                prefix_bits,
            )?;
            (snap, Some(sha_marker))
        }
    };
    let cursor = snapshot.cursor;
    let sha_entries = mapping_snapshot_entries(snapshot);
    let entries = translate_sha256_entries(sha_entries, &sha_to_blake3);
    tracing::debug!(
        target: "sync",
        repo = %name,
        domain = HashDomain::Sha256.as_str(),
        entries = entries.len(),
        cursor,
        "domain fetch complete"
    );
    Ok(DomainPull {
        entries,
        cursor,
        marker,
    })
}

/// Truncate a peer-supplied string to 64 characters for log sampling.
/// Prevents a hostile peer from flooding the log with arbitrarily long values.
fn truncate_for_log(s: &str) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(64).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

fn mapping_snapshot_entries(snapshot: naiad_netproto::Snapshot) -> TaggedEntries {
    let mut entries: TaggedEntries = Vec::with_capacity(snapshot.tags.len());
    let mut discards: u64 = 0;
    let mut samples: Vec<String> = Vec::new();
    for (hex, origin_tags) in snapshot.tags {
        let hash = match hex.parse::<Hash>() {
            Ok(h) => h,
            Err(_) => {
                discards += 1;
                if samples.len() < 3 {
                    samples.push(truncate_for_log(&hex));
                }
                continue;
            }
        };
        let mut parsed: Vec<(Tag, Option<String>)> = Vec::new();
        for ot in origin_tags {
            match Tag::parse(&ot.tag) {
                Ok(t) => parsed.push((t, ot.origin)),
                Err(_) => {
                    discards += 1;
                    if samples.len() < 3 {
                        samples.push(truncate_for_log(&ot.tag));
                    }
                }
            }
        }
        if !parsed.is_empty() {
            entries.push((hash, parsed));
        }
    }
    if discards > 0 {
        tracing::warn!(
            target: "sync",
            discards,
            sample = %samples.join(", "),
            "discarded {} unparsable rows during pull",
            discards
        );
    }
    entries
}

fn mapping_delta_inputs(changes: Vec<DeltaMapping>) -> Vec<naiad_db::MappingDeltaInput> {
    let mut out: Vec<naiad_db::MappingDeltaInput> = Vec::new();
    let mut discards: u64 = 0;
    let mut samples: Vec<String> = Vec::new();
    for m in changes {
        let hash = match m.hash.parse::<Hash>() {
            Ok(h) => h,
            Err(_) => {
                discards += 1;
                if samples.len() < 3 {
                    samples.push(truncate_for_log(&m.hash));
                }
                continue;
            }
        };
        let tag = match Tag::parse(&m.tag) {
            Ok(t) => t,
            Err(_) => {
                discards += 1;
                if samples.len() < 3 {
                    samples.push(truncate_for_log(&m.tag));
                }
                continue;
            }
        };
        out.push(naiad_db::MappingDeltaInput {
            hash,
            tag,
            status: match m.status {
                MappingStatus::Current => naiad_db::MappingDeltaStatus::Current,
                MappingStatus::Deleted => naiad_db::MappingDeltaStatus::Deleted,
            },
            seq: m.seq,
            origin: m.origin,
        });
    }
    if discards > 0 {
        tracing::warn!(
            target: "sync",
            discards,
            sample = %samples.join(", "),
            "discarded {} unparsable rows during pull",
            discards
        );
    }
    out
}

/// Why a [`submit_to_repo`] call failed, so the HTTP layer can pick 4xx vs 5xx.
#[derive(Debug)]
pub enum SubmitError {
    /// The caller's request was bad: unknown repo, file not in the library, or
    /// an unparseable tag. Maps to 400.
    BadRequest(anyhow::Error),
    /// A server-side or upstream failure: local key IO, or the remote repo being
    /// unreachable or rejecting the submission. Maps to 500.
    Upstream(anyhow::Error),
    /// The remote repo does not support the requested capability (e.g. no
    /// reports capability). Maps to a clean 4xx, not a 500.
    Unsupported(anyhow::Error),
}

/// Resolve the signing account for one shared service: the key is derived from
/// the master seed (`naiad.master`) and the FROZEN `repo_anchor` for this service.
///
/// Anchor resolution (lazy, write-once via `freeze_repo_anchor`):
/// 0. If `repo_anchor` is already frozen in the DB, return immediately — no
///    network call, no caps fetch. The frozen value is permanent.
/// 1. Otherwise fetch caps (via the cache, one network call per service per
///    session); use `caps.repo_key` when the server advertises one.
/// 2. Fall back to `normalize_repo_url(url)` when the repo advertises no key.
///
/// Once frozen, the anchor never changes — same service always yields the
/// same pseudonym; different anchors yield different pseudonyms.
///
/// Lock discipline: the DB mutex is NOT held across the `fetch_caps` network
/// call. After freezing, we re-read the stored anchor so that two racing callers
/// always derive the same pseudonym — the SQL write-once guard means whichever
/// caller froze first wins, and the other's local variable is discarded.
fn contributor_account_for(
    db: &Mutex<Db>,
    caps_cache: &CapsCache,
    key_path: &Path,
    service_id: i64,
    url: &str,
) -> anyhow::Result<Account> {
    let ident = {
        let db = db.lock_recover();
        db.contributor_identity(service_id)?
    };

    let anchor = match ident.repo_anchor {
        Some(a) => a,
        None => {
            // First resolve (off-lock): v6 uses the repo_key directly as the
            // anchor (no rotation chain / genesis_key walk). Use the cached caps
            // (one fetch per service per session) to get repo_key; fall back to
            // the normalized URL when the repo advertises no repo_key.
            let resolved_anchor = match caps_cache
                .get_or_fetch(service_id, url)
                .ok()
                .and_then(|c| c.repo_key)
            {
                Some(key) => key,
                None => crate::account::normalize_repo_url(url),
            };
            // Write-once: SQL WHERE repo_anchor IS NULL ensures the first writer wins.
            {
                let db = db.lock_recover();
                db.freeze_repo_anchor(service_id, &resolved_anchor)?;
            }
            // Re-read to get the winner — a concurrent caller may have frozen a
            // different value first (shouldn't happen with the same URL, but the
            // database is the authority).
            let db = db.lock_recover();
            db.contributor_identity(service_id)?
                .repo_anchor
                .ok_or_else(|| anyhow!("anchor disappeared immediately after freeze"))?
        }
    };

    let master_path = crate::account::master_path_for(key_path);
    let master = crate::account::load_or_create_master(&master_path)
        .with_context(|| format!("loading master seed from {}", master_path.display()))?;
    Ok(crate::account::derive_contributor(&master, &anchor))
}

/// Sign one tag operation for an owned file and submit it to a subscribed repo.
///
/// The HTTP submit runs **off** the DB lock (same discipline as [`pull_repo`]):
/// a brief lock resolves the repo URL and the file's content hash, then the
/// account signs and POSTs without holding the lock. The submitted tag is not
/// stored locally — it returns to the library on the next pull.
///
/// # Errors
/// Returns [`SubmitError::BadRequest`] if the repo is unknown, the file is not
/// in the library, or the tag is unparseable. Returns [`SubmitError::Upstream`]
/// if the key cannot be loaded or the repo rejects the submission.
pub fn submit_to_repo(
    db: &Mutex<Db>,
    caps_cache: &CapsCache,
    key_path: &Path,
    name: &str,
    file: &str,
    tag: &str,
    op: Op,
) -> Result<(), SubmitError> {
    // Brief lock: resolve the repo URL, service id, the owned file's hash, the
    // tag, and the wire origin. Provenance-by-location (ADR 0026): the origin is
    // the SOURCE local service's `services.origin`. Promoting a tag into manual
    // "my tags" (origin NULL) publishes it as manual — deliberately, even if it
    // was autotagged elsewhere. Origin is asserted, not proven.
    let (svc_id, url, hash, tag, origin) = (|| -> anyhow::Result<_> {
        let db = db.lock_recover();
        let svc = db
            .shared_service_by_name(name)?
            .ok_or_else(|| anyhow!("no such repo: {name}"))?;
        let file_id = resolve_file(&db, file)?;
        let hash = db
            .file_hash(file_id)?
            .ok_or_else(|| anyhow!("file {file} has no content hash"))?;
        let tag = Tag::parse(tag).with_context(|| format!("parsing tag {tag:?}"))?;
        let tag_id = db.intern_tag(&tag)?;
        let origin = db.origin_of_local_mapping(file_id, tag_id)?;
        Ok((svc.id, svc.url, hash, tag, origin))
    })()
    .map_err(SubmitError::BadRequest)?;

    // Off-lock: resolve the contributor account (may touch the network on first
    // derive), sign, and submit with auth headers. Failures here are server/upstream.
    let account = contributor_account_for(db, caps_cache, key_path, svc_id, &url)
        .with_context(|| format!("resolving contributor account for {url}"))
        .map_err(SubmitError::Upstream)?;
    let submission = account.sign_with_origin(op, &hash, &tag, origin.as_deref());
    RepoClient::new(&url)
        .submit(&account, &submission)
        .with_context(|| format!("submitting to {url}"))
        .map_err(SubmitError::Upstream)?;
    tracing::info!(target: "sync", repo = %name, file = %file, tag = %tag, op = ?op, "submitted tag op to repo");
    Ok(())
}

/// Convert pulled authored edges to `(from, to, author)` tag triples, skipping
/// any edge whose tags do not parse.
fn parse_edges(edges: Vec<AuthoredEdge>) -> Vec<(Tag, Tag, String)> {
    edges
        .into_iter()
        .filter_map(|e| {
            let from = Tag::parse(&e.from).ok()?;
            let to = Tag::parse(&e.to).ok()?;
            Some((from, to, e.author))
        })
        .collect()
}

/// Convert wire delta edges to db staging inputs, skipping any edge whose tags do
/// not parse (mirrors [`parse_edges`]'s tolerance). Note: the cursor still
/// advances past a skipped edge, so a permanently-malformed edge is never
/// reflected locally — acceptable because the repo validates tags on signed
/// submission, so a wire tag failing to parse implies corruption, not a normal case.
fn delta_inputs(edges: Vec<DeltaEdge>) -> Vec<DeltaEdgeInput> {
    edges
        .into_iter()
        .filter_map(|e| {
            let from = Tag::parse(&e.from).ok()?;
            let to = Tag::parse(&e.to).ok()?;
            let kind = match e.kind {
                RelKind::Sibling => EdgeKind::Sibling,
                RelKind::Parent => EdgeKind::Parent,
            };
            Some(DeltaEdgeInput {
                kind,
                from,
                to,
                author: e.author,
                deleted: matches!(e.status, EdgeStatus::Deleted),
                seq: e.seq,
            })
        })
        .collect()
}

/// Sign one relation operation and submit it to a subscribed repo.
///
/// Off-lock discipline mirrors [`submit_to_repo`]: a brief lock resolves the repo
/// URL and parses both tags (relations are library-independent — no file or
/// owned-hash check), then the account signs and POSTs without holding the lock.
/// The relation is not stored locally — it returns on the next relation pull.
///
/// # Errors
/// Returns [`SubmitError::BadRequest`] if the repo is unknown or a tag is
/// unparseable. Returns [`SubmitError::Upstream`] if the key cannot be loaded or
/// the repo rejects the submission.
#[allow(clippy::too_many_arguments)]
pub fn submit_relation(
    db: &Mutex<Db>,
    caps_cache: &CapsCache,
    key_path: &Path,
    name: &str,
    kind: RelKind,
    from: &str,
    to: &str,
    op: Op,
) -> Result<(), SubmitError> {
    // Brief lock: resolve the repo URL, service id, and parse both tags
    // (relations are library-independent — no file or owned-hash check).
    let (svc_id, url, from_tag, to_tag) = (|| -> anyhow::Result<_> {
        let db = db.lock_recover();
        let svc = db
            .shared_service_by_name(name)?
            .ok_or_else(|| anyhow!("no such repo: {name}"))?;
        let from_tag = Tag::parse(from).with_context(|| format!("parsing tag {from:?}"))?;
        let to_tag = Tag::parse(to).with_context(|| format!("parsing tag {to:?}"))?;
        Ok((svc.id, svc.url, from_tag, to_tag))
    })()
    .map_err(SubmitError::BadRequest)?;

    // Off-lock: resolve the contributor account (same pseudonym as tag
    // submissions — a derived-mode service must never mix the global key with
    // the per-repo pseudonym, or the repo can link them), sign, and submit.
    let account = contributor_account_for(db, caps_cache, key_path, svc_id, &url)
        .with_context(|| format!("resolving contributor account for {url}"))
        .map_err(SubmitError::Upstream)?;
    let submission = account.sign_relation(op, kind, &from_tag, &to_tag);
    RepoClient::new(&url)
        .submit_relation(&submission)
        .with_context(|| format!("submitting relation to {url}"))
        .map_err(SubmitError::Upstream)?;
    tracing::info!(target: "sync", repo = %name, kind = ?kind, from = %from, to = %to, op = ?op, "submitted relation to repo");
    Ok(())
}

/// Pull a subscribed repo's relation graph, negotiating incremental deltas when
/// the repo advertises `caps.relation_incremental`.
///
/// **Incremental path** (repo supports it): reads the stored cursor, fetches
/// only edges since that seq, converts wire edges to db inputs, and calls
/// [`Db::merge_relation_delta`]. Includes a reset guard: if the returned
/// `delta.cursor` is smaller than the stored cursor (the repo rebuilt its DB and
/// seq restarted), re-fetches from 0 with `full_reset = true`.
///
/// **Fallback path** (old repo or one not advertising deltas): full-graph
/// replace via [`Db::merge_pulled_relations`], unchanged behavior. Leaves
/// `relation_cursor` NULL.
///
/// Off-lock discipline mirrors [`pull_repo`]: the lock is taken briefly to
/// resolve the repo URL, to read the stored cursor, and to merge — HTTP fetches
/// run entirely off the lock.
///
/// # Errors
/// Returns an error if the repo is unknown, the fetch fails, or a database
/// operation fails. Edges with unparseable tags are skipped, not fatal.
pub fn pull_relations(
    db: &Mutex<Db>,
    caps_cache: &CapsCache,
    name: &str,
) -> Result<RelationMergeStats> {
    // Brief lock: resolve the subscribed repo.
    let svc = {
        let db = db.lock_recover();
        db.shared_service_by_name(name)?
            .ok_or_else(|| anyhow!("no such repo: {name}"))?
    };
    let started = Instant::now();
    let client = caps_cache.client(&svc.url);

    // Negotiate: does the repo serve incremental relation deltas? Use the
    // cached caps (one fetch per service per session).
    let caps = caps_cache
        .get_or_fetch(svc.id, &svc.url)
        .with_context(|| format!("fetching caps from {}", svc.url))?;

    if caps.relation_incremental {
        tracing::debug!(target: "sync", repo = %name, "relation pull is incremental (delta)");
        // Brief lock: read the stored cursor.
        let stored = {
            let db = db.lock_recover();
            db.relation_cursor(svc.id)?
        };
        let since = stored.unwrap_or(0).max(0) as u64;
        let mut delta = client
            .fetch_relations_since(since)
            .with_context(|| format!("pulling relation delta from {}", svc.url))?;
        let mut full_reset = since == 0;
        // Reset guard: a cursor that went backwards means the repo's seq restarted
        // (DB rebuilt) → clear staging and re-pull the full set.
        if let Some(c) = stored {
            if delta.cursor < c as u64 {
                tracing::warn!(target: "sync", repo = %name, stored = c, returned = delta.cursor, "repo seq restarted, re-syncing relations from 0");
                delta = client
                    .fetch_relations_since(0)
                    .with_context(|| format!("re-syncing relations from {}", svc.url))?;
                full_reset = true;
            }
        }
        let edges = delta_inputs(delta.edges);
        let stats = {
            let db = db.lock_recover();
            db.merge_relation_delta(svc.id, full_reset, delta.cursor, &edges)?
        };
        tracing::info!(target: "sync", repo = %name, mode = "incremental", siblings = stats.siblings, parents = stats.parents, elapsed_ms = started.elapsed().as_millis() as u64, "relation pull finished");
        Ok(stats)
    } else {
        // Fallback: old repo (or one not advertising deltas) → full-graph replace,
        // unchanged. Leaves relation_cursor NULL.
        let graph = client
            .fetch_relations()
            .with_context(|| format!("pulling relations from {}", svc.url))?;
        let siblings = parse_edges(graph.siblings);
        let parents = parse_edges(graph.parents);
        let db = db.lock_recover();
        let stats = db.merge_pulled_relations(svc.id, &siblings, &parents)?;
        tracing::info!(target: "sync", repo = %name, mode = "full", siblings = stats.siblings, parents = stats.parents, elapsed_ms = started.elapsed().as_millis() as u64, "relation pull finished");
        Ok(stats)
    }
}

/// Format a UTC timestamp as `naiad-YYYYMMDD-HHMMSS.db` for a default backup
/// filename. Uses UTC (not local time) because the daemon runs without a
/// timezone dependency; callers supplying an explicit `dest` bypass this.
fn backup_filename_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, min, sec) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );
    let (y, m, d) = civil_from_days_utc(days);
    format!("naiad-{y:04}{m:02}{d:02}-{hour:02}{min:02}{sec:02}.db")
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's `civil_from_days`.
fn civil_from_days_utc(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Why a [`do_backup`] call failed, so the HTTP layer can pick 4xx vs 5xx.
#[derive(Debug)]
pub enum BackupError {
    /// The caller's request was bad: destination exists, its parent is missing,
    /// or the path is not valid UTF-8. Maps to 400.
    BadRequest(anyhow::Error),
    /// A server-side failure: SQLite error, I/O error creating the `backups/`
    /// directory, or stat-ing the output file. Maps to 500.
    Internal(anyhow::Error),
}

/// Outcome of a successful [`do_backup`] call.
#[derive(Debug)]
pub struct BackupResult {
    /// Absolute path of the backup file that was written.
    pub dest: PathBuf,
    /// Size of the backup file in bytes.
    pub bytes: u64,
    /// Wall-clock duration of the `VACUUM INTO` in milliseconds.
    pub duration_ms: u64,
}

/// Back up the database to `dest` (explicit) or a timestamped file under
/// `<db_dir>/backups/` (default). Opens a fresh read-only connection to
/// `src_db_path` for the `VACUUM INTO` so the writer mutex is never held
/// during the (potentially long-running) snapshot.
///
/// **Destination validation (explicit `dest`):**
/// - Its parent directory must already exist.
/// - It must not already exist.
///
/// **Default destination:** `<db_dir>/backups/naiad-YYYYMMDD-HHMMSS.db`
/// (UTC). The `backups/` directory is created on demand. Returns
/// [`BackupError::BadRequest`] if the computed path already exists (e.g. two
/// backups within the same UTC second).
///
/// On any SQLite failure only a file **newly created by this call** is
/// removed; a pre-existing file at `dest_path` is never touched.
///
/// # Errors
/// Returns [`BackupError::BadRequest`] for validation failures (map to 400)
/// and [`BackupError::Internal`] for SQLite or I/O failures (map to 500).
pub fn do_backup(
    src_db_path: &Path,
    db_dir: &Path,
    dest: Option<&str>,
) -> std::result::Result<BackupResult, BackupError> {
    let dest_path: PathBuf = match dest {
        Some(d) if !d.trim().is_empty() => {
            let p = PathBuf::from(d);
            let parent = p
                .parent()
                .filter(|pp| !pp.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            if !parent.is_dir() {
                return Err(BackupError::BadRequest(anyhow::anyhow!(
                    "backup destination parent directory does not exist: {}",
                    parent.display()
                )));
            }
            if p.exists() {
                return Err(BackupError::BadRequest(anyhow::anyhow!(
                    "backup destination already exists: {}",
                    p.display()
                )));
            }
            p
        }
        _ => {
            let backups_dir = db_dir.join("backups");
            std::fs::create_dir_all(&backups_dir)
                .with_context(|| format!("creating backups directory {}", backups_dir.display()))
                .map_err(BackupError::Internal)?;
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let p = backups_dir.join(backup_filename_utc(secs));
            // Guard against two backups in the same UTC second: the computed
            // filename would collide with the earlier run's output file.
            if p.exists() {
                return Err(BackupError::BadRequest(anyhow::anyhow!(
                    "backup destination already exists: {}",
                    p.display()
                )));
            }
            p
        }
    };

    // Open a fresh read-only connection for VACUUM INTO. SQLite's WAL mode
    // lets a read-only connection take a consistent snapshot while the writer
    // continues, so the Db mutex never needs to be held here.
    let src_db = Db::open_readonly(src_db_path)
        .with_context(|| {
            format!(
                "opening source database for backup: {}",
                src_db_path.display()
            )
        })
        .map_err(BackupError::Internal)?;

    // Record whether the destination existed before this call. If vacuum_into
    // fails we only clean up a file *we* created; a pre-existing file (e.g.
    // from a concurrent backup that just finished) must be left untouched.
    let dest_existed_before = dest_path.exists();

    let started = std::time::Instant::now();
    if let Err(e) = src_db.vacuum_into(&dest_path) {
        if !dest_existed_before {
            // Best-effort removal of any partial file we may have written.
            let _ = std::fs::remove_file(&dest_path);
        }
        // Error::Invalid means the destination already existed (client error);
        // everything else is a server-side I/O or SQLite failure.
        return Err(match e {
            naiad_db::Error::Invalid(_) => BackupError::BadRequest(anyhow::Error::from(e)),
            _ => BackupError::Internal(anyhow::Error::from(e)),
        });
    }
    let duration_ms = started.elapsed().as_millis() as u64;

    let bytes = std::fs::metadata(&dest_path)
        .with_context(|| format!("stat-ing backup file {}", dest_path.display()))
        .map_err(BackupError::Internal)?
        .len();

    tracing::info!(
        target: "db",
        dest = %dest_path.display(),
        bytes,
        duration_ms,
        "backup completed"
    );

    Ok(BackupResult {
        dest: dest_path,
        bytes,
        duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use naiad_core::{FileRecord, hash_bytes};
    use naiad_netproto::{Op, RepoClient};

    #[test]
    fn backup_filename_utc_epoch() {
        // 2023-11-14 22:13:20 UTC = 1_700_000_000 secs
        assert_eq!(
            backup_filename_utc(1_700_000_000),
            "naiad-20231114-221320.db"
        );
        // Epoch itself
        assert_eq!(backup_filename_utc(0), "naiad-19700101-000000.db");
    }

    #[test]
    fn do_backup_default_creates_backups_dir_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("naiad.db");
        let db = Db::open(&db_path).unwrap();
        // Add a root so the DB has rows to verify in the backup.
        db.add_root(tmp.path()).unwrap();
        drop(db); // release the writer before opening read-only inside do_backup

        let result = do_backup(&db_path, tmp.path(), None).unwrap();

        // backups/ subdir must have been created.
        assert!(
            tmp.path().join("backups").is_dir(),
            "backups/ directory must be created"
        );
        // The dest file must exist inside backups/.
        assert!(
            result.dest.exists(),
            "backup file must exist at {}",
            result.dest.display()
        );
        assert!(
            result.dest.starts_with(tmp.path().join("backups")),
            "default dest must be inside backups/"
        );
        assert!(result.bytes > 0, "backup file must not be empty");
        // Filename must match the naiad-YYYYMMDD-HHMMSS.db pattern.
        let name = result.dest.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("naiad-") && name.ends_with(".db"),
            "filename must match naiad-YYYYMMDD-HHMMSS.db, got {name}"
        );
    }

    #[test]
    fn do_backup_existing_file_returns_bad_request() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("naiad.db");
        let db = Db::open(&db_path).unwrap();
        drop(db);
        let dest = tmp.path().join("existing.db");
        std::fs::write(&dest, b"nonempty").unwrap();

        let err = do_backup(&db_path, tmp.path(), Some(dest.to_str().unwrap())).unwrap_err();
        assert!(
            matches!(err, BackupError::BadRequest(_)),
            "existing dest must produce BackupError::BadRequest"
        );
    }

    #[test]
    fn do_backup_missing_parent_returns_bad_request() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("naiad.db");
        let db = Db::open(&db_path).unwrap();
        drop(db);
        let dest = tmp.path().join("nonexistent_dir").join("backup.db");

        let err = do_backup(&db_path, tmp.path(), Some(dest.to_str().unwrap())).unwrap_err();
        assert!(
            matches!(err, BackupError::BadRequest(_)),
            "missing parent must produce BackupError::BadRequest"
        );
    }

    /// A pre-existing file at the default (timestamped) destination must be
    /// preserved and the call must return `BackupError::BadRequest`, not
    /// `BackupError::Internal`. This guards against the regression where the
    /// error cleanup path unconditionally removed the file.
    #[test]
    fn do_backup_default_dest_collision_preserves_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("naiad.db");
        let db = Db::open(&db_path).unwrap();
        drop(db);

        // Manually plant a "previous backup" at the path do_backup would choose
        // for the current second, so the timestamped name collides.
        let backups_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let existing = backups_dir.join(backup_filename_utc(secs));
        let sentinel = b"previous good backup";
        std::fs::write(&existing, sentinel).unwrap();

        // do_backup must fail with BadRequest (not Internal/500) …
        let err = do_backup(&db_path, tmp.path(), None).unwrap_err();
        assert!(
            matches!(err, BackupError::BadRequest(_)),
            "timestamped-name collision must produce BackupError::BadRequest, got {err:?}"
        );

        // … and must leave the pre-existing file untouched.
        assert!(
            existing.exists(),
            "pre-existing backup file must not be deleted on collision"
        );
        let contents = std::fs::read(&existing).unwrap();
        assert_eq!(
            contents, sentinel,
            "pre-existing backup file contents must be unchanged"
        );
    }

    /// Backup via a fresh read-only connection (the new non-mutex path) must
    /// produce a valid, non-empty SQLite file even while the writer connection
    /// is open alongside it (WAL-mode concurrency).
    #[test]
    fn do_backup_fresh_connection_concurrent_with_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("naiad.db");
        let db = Db::open(&db_path).unwrap();
        db.add_root(tmp.path()).unwrap();
        // Keep `db` (the writer) open while do_backup opens its own connection.
        // This verifies WAL-mode allows concurrent read + VACUUM INTO.
        let result = do_backup(&db_path, tmp.path(), None).unwrap();
        drop(db);

        assert!(result.dest.exists(), "backup file must exist");
        assert!(result.bytes > 0, "backup file must not be empty");
        // The output must be a valid SQLite database (first 16 bytes = magic).
        let header = std::fs::read(&result.dest).unwrap();
        assert_eq!(
            &header[..16],
            b"SQLite format 3\0",
            "backup file must have the SQLite magic header"
        );
    }

    #[test]
    fn background_scan_threads_stays_within_bounds() {
        let n = background_scan_threads();
        assert!((2..=4).contains(&n), "got {n}");
    }

    #[test]
    fn background_profile_imports_all_files_and_emits_zero_totals() {
        // Background profile must: import every file in the directory, report
        // no errors, and always pass total=0 (unknown — no pre-count traversal)
        // in every progress callback.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Distinct-content files so dedup doesn't collapse them.
        for i in 0u8..5 {
            std::fs::write(root.join(format!("f{i}.png")), vec![i; 16]).unwrap();
        }

        let db = Mutex::new(Db::open_in_memory().unwrap());
        let mut progress_totals: Vec<u64> = Vec::new();
        let summary = scan_streaming(
            &db,
            root,
            ScanProfile::Background,
            |e| panic!("unexpected scan error: {e}"),
            |_imported, _errors, total| progress_totals.push(total),
        )
        .unwrap();

        assert_eq!(summary.imported, 5);
        assert_eq!(summary.errors, 0);
        assert_eq!(summary.marked_missing, 0);
        assert!(
            progress_totals.iter().all(|&t| t == 0),
            "Background passes total=0 in every progress callback; got {progress_totals:?}"
        );
        assert_eq!(db.lock().unwrap().file_count().unwrap(), 5);
    }

    #[test]
    fn background_profile_marks_missing_after_deletion() {
        // Background profile must flip a deleted file's location to missing on
        // the next scan, and leave the content row intact.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("keep.png"), b"keep").unwrap();
        std::fs::write(root.join("gone.png"), b"gone").unwrap();

        let db = Mutex::new(Db::open_in_memory().unwrap());
        let first = scan_streaming(
            &db,
            root,
            ScanProfile::Background,
            |e| panic!("first scan error: {e}"),
            |_, _, _| (),
        )
        .unwrap();
        assert_eq!(first.imported, 2);
        assert_eq!(first.marked_missing, 0);

        // Delete one file; the next Background scan must reconcile it missing.
        std::fs::remove_file(root.join("gone.png")).unwrap();
        let second = scan_streaming(
            &db,
            root,
            ScanProfile::Background,
            |e| panic!("second scan error: {e}"),
            |_, _, _| (),
        )
        .unwrap();
        assert_eq!(second.imported, 1, "only keep.png seen");
        assert_eq!(
            second.marked_missing, 1,
            "gone.png location flipped missing"
        );

        let db = db.lock().unwrap();
        let gone = db.locations_of(&hash_bytes(b"gone")).unwrap();
        assert!(
            !gone.is_empty(),
            "content row for gone.png must still exist"
        );
        assert!(!gone[0].present, "gone.png location must be marked missing");
        let keep = db.locations_of(&hash_bytes(b"keep")).unwrap();
        assert!(keep[0].present, "keep.png must still be present");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn submit_signs_and_the_repo_accepts_then_serves_it() {
        // A repo to receive the submission.
        let repo_store = naiad_server::RepoStore::open_in_memory().unwrap();
        let repo = naiad_test_support::spawn_test_repo(repo_store);
        let repo_url = format!("http://{}", repo.addr);

        // A library owning one file, subscribed to the repo.
        let owned = hash_bytes(b"owned");
        let db = Db::open_in_memory().unwrap();
        db.insert_file(&FileRecord::new(owned, "/lib/a.txt".into(), 5, Some(1)), 1)
            .unwrap();
        db.add_shared_service("ptr", &repo_url, None).unwrap();
        let db = Mutex::new(db);

        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("naiad.key");

        // Submit an add, off the main thread (blocking HTTP).
        let owned_hex = owned.to_hex();
        let key_clone = key.clone();
        let cache = CapsCache::new();
        tokio::task::spawn_blocking(move || {
            submit_to_repo(
                &db,
                &cache,
                &key_clone,
                "ptr",
                &owned_hex,
                "character:samus",
                Op::Add,
            )
        })
        .await
        .unwrap()
        .unwrap();

        // The repo now serves it.
        let snap = tokio::task::spawn_blocking(move || RepoClient::new(&repo_url).fetch_snapshot())
            .await
            .unwrap()
            .unwrap();
        // v8 snapshot: BTreeMap<hash, Vec<OriginTag>>.
        let tags = snap.tags.get(&owned.to_hex()).unwrap();
        assert_eq!(tags[0].tag, "character:samus");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn incremental_pull_sets_cursor_then_advances_on_a_later_submission() {
        // A repo seeded with one sibling edge.
        let repo_store = naiad_server::RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let bad = Tag::parse("a:bad").unwrap();
        let ideal = Tag::parse("a:aaa").unwrap();
        repo_store
            .apply_relation(&acct.sign_relation(Op::Add, RelKind::Sibling, &bad, &ideal))
            .unwrap();
        let repo = naiad_test_support::spawn_test_repo(repo_store);
        let repo_url = format!("http://{}", repo.addr);

        // A client subscribed to it.
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("ptr", &repo_url, None).unwrap();
        let db = Mutex::new(db);

        // First pull → incremental path: cursor set, sibling materialized.
        let cache = CapsCache::new();
        let s1 = pull_relations(&db, &cache, "ptr").unwrap();
        assert_eq!(s1.siblings, 1, "sibling materialized via delta");
        let c1 = db.lock().unwrap().relation_cursor(svc).unwrap();
        assert!(
            c1.is_some(),
            "incremental path set a cursor (not the NULL fallback)"
        );

        // Submit a NEW parent edge to the live repo, then pull again.
        let child = Tag::parse("a:child").unwrap();
        let parent = Tag::parse("a:parent").unwrap();
        let sub = acct.sign_relation(Op::Add, RelKind::Parent, &child, &parent);
        RepoClient::new(&repo_url).submit_relation(&sub).unwrap();

        let s2 = pull_relations(&db, &cache, "ptr").unwrap();
        assert_eq!(s2.siblings, 1);
        assert_eq!(s2.parents, 1, "the new parent arrived via the delta");
        let c2 = db.lock().unwrap().relation_cursor(svc).unwrap();
        assert!(c2 > c1, "cursor advanced: {c1:?} -> {c2:?}");
    }

    /// contributor_account_for: derived mode without a repo_key falls back to the
    /// normalized URL as anchor. Tests via two separate DBs that get the same master
    /// → same key; a different URL → different key.
    #[tokio::test(flavor = "multi_thread")]
    async fn derived_contributor_account_for_url_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("naiad.key");

        // Repo with no identity key configured (no repo_key in caps).
        let repo_store = naiad_server::RepoStore::open_in_memory().unwrap();
        let repo = naiad_test_support::spawn_test_repo(repo_store);
        let repo_url = format!("http://{}", repo.addr);

        // DB 1: derived service, let contributor_account_for resolve and freeze anchor.
        let db1 = {
            let db = Db::open_in_memory().unwrap();
            let svc_id = db.add_shared_service("ptr", &repo_url, None).unwrap();
            // repo_anchor is NULL until contributor_account_for resolves it.
            assert!(
                db.contributor_identity(svc_id)
                    .unwrap()
                    .repo_anchor
                    .is_none()
            );
            Mutex::new(db)
        };

        let key1 = key_path.clone();
        let url1 = repo_url.clone();
        let (acct1, svc_id) = tokio::task::spawn_blocking(move || {
            let svc_id = db1
                .lock_recover()
                .shared_service_by_name("ptr")
                .unwrap()
                .unwrap()
                .id;
            let cache = CapsCache::new();
            let acct = contributor_account_for(&db1, &cache, &key1, svc_id, &url1).unwrap();
            (acct, svc_id)
        })
        .await
        .unwrap();

        // DB 2: pre-frozen with the same URL anchor → same master → same key.
        let normalized_url = crate::account::normalize_repo_url(&repo_url);
        let db2 = {
            let db = Db::open_in_memory().unwrap();
            let id = db.add_shared_service("ptr", &repo_url, None).unwrap();
            db.freeze_repo_anchor(id, &normalized_url).unwrap();
            Mutex::new(db)
        };
        let key2 = key_path.clone();
        let url2 = repo_url.clone();
        let acct2 = tokio::task::spawn_blocking(move || {
            let id = db2
                .lock_recover()
                .shared_service_by_name("ptr")
                .unwrap()
                .unwrap()
                .id;
            contributor_account_for(&db2, &CapsCache::new(), &key2, id, &url2).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(
            acct1.public_hex(),
            acct2.public_hex(),
            "URL-fallback anchor must be deterministic"
        );

        // DB 3: a different URL → different anchor → different pseudonym.
        let db3 = {
            let db = Db::open_in_memory().unwrap();
            let id = db
                .add_shared_service("other", "http://other.repo:9090", None)
                .unwrap();
            db.freeze_repo_anchor(id, "http://other.repo:9090").unwrap();
            Mutex::new(db)
        };
        let key3 = key_path.clone();
        let acct3 = tokio::task::spawn_blocking(move || {
            let id = db3
                .lock_recover()
                .shared_service_by_name("other")
                .unwrap()
                .unwrap()
                .id;
            contributor_account_for(&db3, &CapsCache::new(), &key3, id, "http://other.repo:9090")
                .unwrap()
        })
        .await
        .unwrap();
        assert_ne!(
            acct1.public_hex(),
            acct3.public_hex(),
            "different URLs must yield different pseudonyms"
        );

        // The derived key must differ from the global naiad.key.
        let global = Account::load_or_create(&key_path).unwrap();
        assert_ne!(
            acct1.public_hex(),
            global.public_hex(),
            "derived key must not equal the global naiad.key"
        );
        let _ = svc_id;
    }

    /// contributor_account_for: derived mode uses the repo_key directly as the
    /// anchor (no rotation chain in v6). Same repo_key → same pseudonym.
    #[tokio::test(flavor = "multi_thread")]
    async fn derived_contributor_anchor_is_stable_across_calls() {
        // Two calls with the same service (same URL / repo_key) must return the
        // same pseudonym. The rotation chain walk was removed in v6; the anchor
        // is the repo_key itself (or the normalized URL when absent).
        let master = [42u8; 32];
        let anchor = "some-repo-key-or-url";
        let key1 = crate::account::derive_contributor(&master, anchor);
        let key2 = crate::account::derive_contributor(&master, anchor);
        assert_eq!(
            key1.public_hex(),
            key2.public_hex(),
            "same anchor must produce the same pseudonym"
        );

        // Different anchor → different pseudonym.
        let key3 = crate::account::derive_contributor(&master, "other-anchor");
        assert_ne!(
            key1.public_hex(),
            key3.public_hex(),
            "different anchor must produce a different pseudonym"
        );
    }

    /// Spawn a real naiad-server on an ephemeral port, advertising the given
    /// `repo_key` in caps. Returns the bound addr; keep the JoinHandle alive for
    /// the server's lifetime. May be called from `spawn_blocking`.
    fn spawn_repo_with_caps_key(repo_key: Option<String>) -> std::net::SocketAddr {
        use std::net::SocketAddr;
        use std::sync::{Arc, Mutex};
        let store = Arc::new(Mutex::new(
            naiad_server::RepoStore::open_in_memory().unwrap(),
        ));
        let (tx, rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tx.send(addr).unwrap();
                let router = naiad_server::app_split(
                    store,
                    None,
                    1000,
                    repo_key,
                    None,
                    naiad_netproto::HashDomain::Blake3,
                );
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            });
        });
        rx.recv().unwrap()
    }

    /// contributor_account_for: when the server advertises a `repo_key` in caps,
    /// the frozen anchor must equal that key — NOT the normalized URL — and the
    /// derived pseudonym must differ from what URL-anchoring would produce.
    #[tokio::test(flavor = "multi_thread")]
    async fn repo_key_in_caps_used_as_anchor() {
        // A 64-hex-char string the server advertises as its repo_key.
        let advertised_key = "ab".repeat(32);

        let addr = tokio::task::spawn_blocking({
            let k = advertised_key.clone();
            move || spawn_repo_with_caps_key(Some(k))
        })
        .await
        .unwrap();
        let repo_url = format!("http://{addr}");

        let db = Db::open_in_memory().unwrap();
        let svc_id = db.add_shared_service("ptr", &repo_url, None).unwrap();
        assert!(
            db.contributor_identity(svc_id)
                .unwrap()
                .repo_anchor
                .is_none(),
            "anchor must be NULL before first contact"
        );
        let db = Mutex::new(db);

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("naiad.key");
        let url = repo_url.clone();

        // Call contributor_account_for: caps fetch returns repo_key, freezes it.
        let (acct_pubkey, frozen_anchor) = tokio::task::spawn_blocking(move || {
            let cache = CapsCache::new();
            let acct = contributor_account_for(&db, &cache, &key_path, svc_id, &url).unwrap();
            let anchor = db
                .lock_recover()
                .contributor_identity(svc_id)
                .unwrap()
                .repo_anchor
                .expect("anchor must be frozen after first call");
            (acct.public_hex(), anchor)
        })
        .await
        .unwrap();

        assert_eq!(
            frozen_anchor, advertised_key,
            "frozen anchor must be the advertised repo_key, not the normalized URL"
        );

        // Verify the derived key equals derive(master, repo_key), not derive(master, url).
        let master_path = dir.path().join("naiad.master");
        let master = crate::account::load_or_create_master(&master_path).unwrap();
        let from_key = crate::account::derive_contributor(&master, &advertised_key).public_hex();
        let from_url = crate::account::derive_contributor(
            &master,
            &crate::account::normalize_repo_url(&repo_url),
        )
        .public_hex();

        assert_eq!(
            acct_pubkey, from_key,
            "account must be derived from the repo_key anchor"
        );
        assert_ne!(
            acct_pubkey, from_url,
            "account must NOT match the URL-anchored derivation"
        );
    }

    /// contributor_account_for: once the anchor is frozen to the normalized URL
    /// (from first contact with a no-key server), subsequent calls with a fresh
    /// CapsCache against a key-advertising server must leave the anchor and
    /// derived pubkey UNCHANGED — the write-once freeze is permanent.
    #[tokio::test(flavor = "multi_thread")]
    async fn frozen_url_anchor_persists_when_caps_later_advertise_repo_key() {
        use std::sync::Arc;

        // Step 1: Contact a server with NO repo_key → anchor freezes to normalized URL.
        let addr_no_key = tokio::task::spawn_blocking(|| spawn_repo_with_caps_key(None))
            .await
            .unwrap();
        let repo_url = format!("http://{addr_no_key}");
        let expected_anchor = crate::account::normalize_repo_url(&repo_url);

        // Use Arc so the db survives across two spawn_blocking calls.
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let svc_id = db
            .lock_recover()
            .add_shared_service("ptr", &repo_url, None)
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("naiad.key");

        // First call: anchor should freeze to normalize_repo_url(repo_url).
        let (pubkey_first, anchor_first) = {
            let db = Arc::clone(&db);
            let kp = key_path.clone();
            let url = repo_url.clone();
            tokio::task::spawn_blocking(move || {
                let cache = CapsCache::new();
                let acct = contributor_account_for(&db, &cache, &kp, svc_id, &url).unwrap();
                let anchor = db
                    .lock_recover()
                    .contributor_identity(svc_id)
                    .unwrap()
                    .repo_anchor
                    .expect("anchor must be set after first call");
                (acct.public_hex(), anchor)
            })
            .await
            .unwrap()
        };

        assert_eq!(
            anchor_first, expected_anchor,
            "first contact with no-key server anchors to normalized URL"
        );

        // Step 2: spin up a NEW server that DOES advertise a repo_key. Use a fresh
        // CapsCache (no cached entry for this service_id) so a live caps fetch
        // WOULD see a repo_key — but the frozen anchor must win.
        let advertised_key = "cd".repeat(32);
        let addr_with_key = tokio::task::spawn_blocking({
            let k = advertised_key.clone();
            move || spawn_repo_with_caps_key(Some(k))
        })
        .await
        .unwrap();
        let url_with_key = format!("http://{addr_with_key}");

        let pubkey_second = {
            let db = Arc::clone(&db);
            let kp = key_path.clone();
            tokio::task::spawn_blocking(move || {
                // Fresh CapsCache: no cached entry, so a live fetch would return repo_key.
                // The frozen anchor for svc_id must short-circuit before any fetch.
                let fresh_cache = CapsCache::new();
                contributor_account_for(&db, &fresh_cache, &kp, svc_id, &url_with_key)
                    .unwrap()
                    .public_hex()
            })
            .await
            .unwrap()
        };

        // Anchor must remain the URL anchor frozen in step 1.
        let anchor_after = db
            .lock_recover()
            .contributor_identity(svc_id)
            .unwrap()
            .repo_anchor
            .unwrap();
        assert_eq!(
            anchor_after, expected_anchor,
            "anchor must remain the frozen URL anchor even when caps later advertise a repo_key"
        );
        assert_eq!(
            pubkey_second, pubkey_first,
            "derived pubkey must be unchanged — write-once freeze is permanent"
        );
    }

    /// contributor_account_for + submit: derived-key submissions verify at the
    /// repo exactly as before (no wire change).
    #[tokio::test(flavor = "multi_thread")]
    async fn derived_submit_verifies_at_repo() {
        use naiad_core::{FileRecord, hash_bytes};

        let repo_store = naiad_server::RepoStore::open_in_memory().unwrap();
        let repo = naiad_test_support::spawn_test_repo(repo_store);
        let repo_url = format!("http://{}", repo.addr);

        let owned = hash_bytes(b"owned-derived");
        let db = Db::open_in_memory().unwrap();
        db.insert_file(&FileRecord::new(owned, "/lib/d.txt".into(), 5, Some(1)), 1)
            .unwrap();
        db.add_shared_service("ptr", &repo_url, None).unwrap();
        // New services default to 'derived' — no mode change needed.
        let db = Mutex::new(db);

        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("naiad.key");
        let owned_hex = owned.to_hex();
        let key_clone = key.clone();
        let url_clone = repo_url.clone();

        // submit_to_repo should succeed (derived-key sig verifies at the repo).
        let cache = CapsCache::new();
        tokio::task::spawn_blocking(move || {
            submit_to_repo(
                &db,
                &cache,
                &key_clone,
                "ptr",
                &owned_hex,
                "character:samus",
                Op::Add,
            )
        })
        .await
        .unwrap()
        .unwrap();

        // The repo serves it. v8 snapshot: BTreeMap<hash, Vec<OriginTag>>.
        let snap =
            tokio::task::spawn_blocking(move || RepoClient::new(&url_clone).fetch_snapshot())
                .await
                .unwrap()
                .unwrap();
        let tags = snap.tags.get(&owned.to_hex()).unwrap();
        assert_eq!(tags[0].tag, "character:samus");
        // The repo accepted the submission (tag present) which means the derived
        // key's signature verified. The master seed key (naiad.master) must exist
        // on disk; naiad.key is never created in derived mode.
        let master_path = dir.path().join("naiad.master");
        assert!(
            master_path.exists(),
            "master seed created alongside account resolution"
        );
    }

    /// Relations submitted via `submit_relation` use the SAME per-repo pseudonym
    /// as tag submissions from `submit_to_repo`. If they used the global naiad.key
    /// instead, the repo could trivially link the pseudonym to the global identity,
    /// defeating the unlinkability guarantee of ADR 0020 §6.
    #[tokio::test(flavor = "multi_thread")]
    async fn relation_and_tag_use_the_same_derived_pseudonym() {
        use std::sync::Arc;

        use naiad_core::{FileRecord, hash_bytes};
        use naiad_netproto::RelKind;

        let repo_store = naiad_server::RepoStore::open_in_memory().unwrap();
        let repo = naiad_test_support::spawn_test_repo(repo_store);
        let repo_url = format!("http://{}", repo.addr);

        let owned = hash_bytes(b"owned-rel-linkage");
        let db = Db::open_in_memory().unwrap();
        db.insert_file(
            &FileRecord::new(owned, "/lib/rel.txt".into(), 5, Some(1)),
            1,
        )
        .unwrap();
        db.add_shared_service("ptr", &repo_url, None).unwrap();
        // New services default to 'derived'.
        let db = Arc::new(Mutex::new(db));

        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("naiad.key");

        // Submit a tag.
        let owned_hex = owned.to_hex();
        {
            let db = Arc::clone(&db);
            let key = key.clone();
            let cache = CapsCache::new();
            tokio::task::spawn_blocking(move || {
                submit_to_repo(
                    &db,
                    &cache,
                    &key,
                    "ptr",
                    &owned_hex,
                    "character:samus",
                    Op::Add,
                )
            })
            .await
            .unwrap()
            .unwrap();
        }

        // Submit a relation using the same derived pseudonym.
        {
            let db = Arc::clone(&db);
            let key = key.clone();
            let cache = CapsCache::new();
            tokio::task::spawn_blocking(move || {
                submit_relation(
                    &db,
                    &cache,
                    &key,
                    "ptr",
                    RelKind::Sibling,
                    "character:samus_aran",
                    "character:samus",
                    Op::Add,
                )
            })
            .await
            .unwrap()
            .unwrap();
        }

        // v6 snapshot has no author info; verify via relations + master key file.
        // Pull snapshot: assert the tag arrived.
        let url = repo_url.clone();
        let snap = tokio::task::spawn_blocking(move || RepoClient::new(&url).fetch_snapshot())
            .await
            .unwrap()
            .unwrap();
        assert!(
            snap.tags
                .get(&owned.to_hex())
                .map(|ts| ts.iter().any(|ot| ot.tag == "character:samus"))
                .unwrap_or(false),
            "character:samus must appear in the snapshot after submit"
        );

        // Pull relations: the relation author is a valid pubkey hex.
        let url2 = repo_url.clone();
        let graph = tokio::task::spawn_blocking(move || RepoClient::new(&url2).fetch_relations())
            .await
            .unwrap()
            .unwrap();
        let rel_author = graph.siblings[0].author.clone();
        assert_eq!(
            rel_author.len(),
            64,
            "relation author is a valid pubkey hex"
        );

        // naiad.key must not be created in derived mode.
        assert!(
            !key.exists(),
            "naiad.key must not be created in derived mode"
        );
    }

    #[test]
    fn remove_repo_detaches_by_default_and_purges_on_request() {
        use naiad_core::{Hash, Tag, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("r", "http://x", None).unwrap();
        let h: Hash = hash_bytes(b"f");
        db.insert_file(
            &naiad_core::FileRecord::new(h, "/lib/f".into(), 1, Some(1)),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        db.merge_pulled_mappings(svc, &[(h, vec![Tag::parse("a:b").unwrap()])])
            .unwrap();
        let fid = db.file_id_by_hash(&h).unwrap().unwrap();

        // Default: detach — subscription gone, tags kept.
        assert!(remove_repo(&db, "r", false).unwrap());
        assert!(db.shared_service_by_name("r").unwrap().is_none());
        assert_eq!(
            db.tags_of(fid).unwrap().len(),
            1,
            "tags survive a plain remove"
        );

        // Removing a detached name is a no-op "not found" (it is not subscribed).
        assert!(!remove_repo(&db, "r", false).unwrap());

        // Re-attach, then purge: everything goes.
        db.subscribe_shared_service("r", "http://x", None).unwrap();
        assert!(remove_repo(&db, "r", true).unwrap());
        assert!(
            db.tags_of(fid).unwrap().is_empty(),
            "purge removes the tags"
        );
    }

    /// Spawn a real dual-domain naiad-server: a native BLAKE3 store plus a
    /// snapshot-mode SHA-256 backend over a fixture Hydrus snapshot. Returns
    /// the bound address; the returned `TempDir` owns the snapshot and must
    /// stay alive for the server's lifetime.
    fn spawn_dual_domain_repo(
        blake3_seed: Vec<(String, String)>,
        sha256_hex: &str,
    ) -> (std::net::SocketAddr, tempfile::TempDir) {
        use std::net::SocketAddr;
        use std::sync::{Arc, Mutex as StdMutex};

        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_snapshot(
            dir.path(),
            9,
            &[(sha256_hex, "character:samus")],
        )
        .unwrap();
        let backend =
            naiad_server::SnapshotBackend::open(dir.path(), Some(9)).expect("open snapshot");
        let domains = naiad_server::DomainConfig {
            native: naiad_netproto::HashDomain::Blake3,
            added_sha256: Some(Arc::new(backend) as Arc<dyn naiad_server::Sha256Backend>),
            max_query_bits: 256,
            min_query_bits: 8, // SNAPSHOT_MIN_QUERY_BITS
        };

        let store = naiad_server::RepoStore::open_in_memory().unwrap();
        for (hash, tag) in blake3_seed {
            store.apply_mappings_bulk(vec![(hash, tag, false)]).unwrap();
        }
        let store = Arc::new(StdMutex::new(store));

        let (tx, rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                let router = naiad_server::app_domains(store, None, 1, None, None, domains);
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            });
        });
        (rx.recv().unwrap(), dir)
    }

    /// A repo that serves BOTH domains lands both sets of tags on the same
    /// file, in one authoritative merge, and records two independent cursors.
    #[test]
    fn dual_domain_pull_merges_both_domains_and_keeps_two_cursors() {
        use naiad_core::{FileRecord, hash_reader_dual};

        // A file whose sha256 is the fixture's tagged hash. We construct the
        // library row directly so the two digests are exactly what we want.
        let content = b"dual-domain-file";
        let (blake3_hash, sha256_hex) = hash_reader_dual(&content[..]).unwrap();

        // Native (blake3) side of the repo: one tag on the same file, plus a
        // filler hash so the repo has >= 2 distinct hashes.
        let (filler_blake3, _) = hash_reader_dual(&b"dual-filler"[..]).unwrap();
        let (addr, _snapshot_dir) = spawn_dual_domain_repo(
            vec![
                (blake3_hash.to_hex(), "series:metroid".to_string()),
                (filler_blake3.to_hex(), "filler:tag".to_string()),
            ],
            &sha256_hex,
        );
        let url = format!("http://{addr}");

        let db = Db::open_in_memory().unwrap();
        db.insert_file(
            &FileRecord::new(
                blake3_hash,
                "/lib/dual.png".into(),
                content.len() as u64,
                Some(1),
            )
            .with_sha256(sha256_hex.clone()),
            1,
        )
        .unwrap();
        let svc = db.add_shared_service("dual", &url, None).unwrap();
        let db = Mutex::new(db);

        // The socket is bound before spawn_dual_domain_repo returns (tx.send
        // fires before axum::serve), so pull_repo can connect immediately.
        // max_query_bits = 256 so the client honours the snapshot repo's
        // exact-hash advertisement.
        let cache = CapsCache::new();
        let stats = pull_repo(&db, &cache, "dual", 256, None).unwrap();
        assert_eq!(stats.matched_files, 1, "the file must match: {stats:?}");

        let guard = db.lock_recover();
        let fid = guard.file_id_by_hash(&blake3_hash).unwrap().unwrap();
        let tags: Vec<String> = guard
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert!(
            tags.contains(&"series:metroid".to_string()),
            "the native blake3 tag must survive the sha256 merge: {tags:?}"
        );
        assert!(
            tags.contains(&"character:samus".to_string()),
            "the snapshot-backed sha256 tag must land on the blake3 identity: {tags:?}"
        );

        // Two independent cursor rows, neither clobbering the other. The
        // snapshot backend reports cursor 0 (a static snapshot has no
        // sequence), so its state is cleared rather than stored — that is the
        // documented shape, not a bug.
        assert_eq!(
            guard.mapping_cursor(svc, "sha256").unwrap(),
            None,
            "snapshot mode reports no cursor"
        );
        assert!(
            guard.mapping_cursor(svc, "blake3").unwrap().is_some(),
            "the native domain still records its cursor"
        );
    }

    /// #141: a library whose rows predate eager dual-hashing has
    /// `files.sha256 IS NULL`, derives ZERO bucket keys, and would pull nothing
    /// at all. Subscribing to a repo that serves the sha256 domain must
    /// backfill first and then pull real mappings.
    #[test]
    fn sha256_backfill_runs_before_a_sha256_domain_pull() {
        use naiad_core::{FileRecord, hash_reader_dual};
        use std::io::Write as _;

        // A REAL file on disk — backfill_sha256 re-hashes from the filesystem.
        let lib = tempfile::tempdir().unwrap();
        let path = lib.path().join("needs-backfill.bin");
        let content = b"backfill-me";
        std::fs::File::create(&path)
            .unwrap()
            .write_all(content)
            .unwrap();
        let (blake3_hash, sha256_hex) = hash_reader_dual(&content[..]).unwrap();

        let (filler_blake3, _) = hash_reader_dual(&b"backfill-filler"[..]).unwrap();
        let (addr, _snapshot_dir) = spawn_dual_domain_repo(
            vec![(filler_blake3.to_hex(), "filler:tag".to_string())],
            &sha256_hex,
        );
        let url = format!("http://{addr}");

        let db = Db::open_in_memory().unwrap();
        // NOTE: no `.with_sha256(..)` — this is the #141 shape.
        db.insert_file(
            &FileRecord::new(blake3_hash, path.clone(), content.len() as u64, Some(1)),
            1,
        )
        .unwrap();
        assert_eq!(
            db.count_files_missing_sha256().unwrap(),
            1,
            "precondition: the interop hash is missing"
        );
        db.add_shared_service("needs-backfill", &url, None).unwrap();
        let db = Mutex::new(db);

        // spawn_dual_domain_repo returns only after the listener is bound (tx.send
        // fires before axum::serve), so pull_repo can connect immediately — same
        // guarantee as the sibling dual_domain_pull_merges_both_domains_and_keeps_two_cursors.
        let cache = CapsCache::new();
        let stats = pull_repo(&db, &cache, "needs-backfill", 256, None).unwrap();
        assert_eq!(
            stats.matched_files, 1,
            "exactly one library file must match the sha256-domain snapshot: {stats:?}"
        );
        assert!(
            stats.mappings > 0,
            "the pull must land real mappings, not silently pull nothing: {stats:?}"
        );

        let guard = db.lock_recover();
        assert_eq!(
            guard.count_files_missing_sha256().unwrap(),
            0,
            "the backfill filled the interop column"
        );
        let fid = guard.file_id_by_hash(&blake3_hash).unwrap().unwrap();
        let tags: Vec<String> = guard
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert!(
            tags.contains(&"character:samus".to_string()),
            "the sha256-domain tag landed on the blake3 identity: {tags:?}"
        );
    }

    /// #143: a per-file pull against a repo that serves the sha256 domain must
    /// return real stats, and must hand back the requested hashes it could not
    /// query because the file has no interop hash yet — the caller needs that
    /// list to prompt a backfill, and "no tags" must be distinguishable from
    /// "not implemented".
    #[test]
    fn per_file_pull_in_sha256_domain_returns_real_stats_and_reports_null_sha() {
        use naiad_core::{FileRecord, hash_reader_dual};

        let content = b"per-file-sha-domain";
        let (blake3_hash, sha256_hex) = hash_reader_dual(&content[..]).unwrap();
        let (filler_blake3, _) = hash_reader_dual(&b"per-file-filler"[..]).unwrap();
        let (addr, _snapshot_dir) = spawn_dual_domain_repo(
            vec![(filler_blake3.to_hex(), "filler:tag".to_string())],
            &sha256_hex,
        );
        let url = format!("http://{addr}");

        let db = Db::open_in_memory().unwrap();
        // File A: has its interop hash (a normal, eagerly dual-hashed import).
        db.insert_file(
            &FileRecord::new(
                blake3_hash,
                "/lib/a.png".into(),
                content.len() as u64,
                Some(1),
            )
            .with_sha256(sha256_hex.clone()),
            1,
        )
        .unwrap();
        // File B: no interop hash and no readable file on disk, so no backfill
        // can rescue it — exactly the case that must be REPORTED.
        let (null_blake3, _) = hash_reader_dual(&b"no-interop-hash"[..]).unwrap();
        db.insert_file(
            &FileRecord::new(null_blake3, "/lib/gone.png".into(), 16, Some(1)),
            1,
        )
        .unwrap();
        let svc = db.add_shared_service("perfile", &url, None).unwrap();
        let db = Mutex::new(db);

        // spawn_dual_domain_repo returns only after the listener is bound, so
        // no retry loop is needed (same guarantee as the sibling tests).
        let cache = CapsCache::new();
        let outcome = pull_repo_for_hashes(
            &db,
            &cache,
            "perfile",
            256,
            &[blake3_hash, null_blake3],
            &naiad_netproto::NoopObserver,
        )
        .unwrap();

        assert!(
            outcome.stats.mappings > 0,
            "a per-file sha256-domain pull must return real stats, not the v1 no-op: {:?}",
            outcome.stats
        );
        assert_eq!(
            outcome.missing_sha256,
            vec![null_blake3],
            "the NULL-sha256 hash must be reported to the caller, not dropped"
        );

        let guard = db.lock_recover();
        let fid = guard.file_id_by_hash(&blake3_hash).unwrap().unwrap();
        let tags: Vec<String> = guard
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert!(
            tags.contains(&"character:samus".to_string()),
            "the sha256-domain tag landed on the blake3 identity: {tags:?}"
        );

        // A scoped pull must never masquerade as a full sync.
        assert_eq!(guard.mapping_cursor(svc, "blake3").unwrap(), None);
        assert_eq!(guard.mapping_cursor(svc, "sha256").unwrap(), None);
        assert_eq!(guard.last_pull_file_marker(svc, "sha256").unwrap(), None);
    }

    /// Helper for unit tests: create a distinct [`Hash`] from a small integer.
    /// Uses BLAKE3 of the single byte `n`, giving a stable deterministic value.
    fn h(n: u8) -> Hash {
        hash_bytes(&[n])
    }

    #[test]
    fn translate_sha256_delta_inputs_maps_and_preserves_status() {
        use naiad_db::{MappingDeltaInput, MappingDeltaStatus};
        let blake = h(1);
        let sha: Hash = "aa00000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
        let mut map = std::collections::HashMap::new();
        map.insert(sha.to_hex(), blake);
        let inputs = vec![
            MappingDeltaInput {
                hash: sha,
                tag: "foo".parse().unwrap(),
                status: MappingDeltaStatus::Deleted,
                seq: 7,
                origin: None,
            },
            // Unowned sha → dropped.
            MappingDeltaInput {
                hash: h(9),
                tag: "bar".parse().unwrap(),
                status: MappingDeltaStatus::Current,
                seq: 8,
                origin: None,
            },
        ];
        let out = translate_sha256_delta_inputs(inputs, &map);
        assert_eq!(out.len(), 1, "unowned sha row dropped");
        assert_eq!(out[0].hash, blake, "sha translated to blake3 identity");
        assert!(
            matches!(out[0].status, MappingDeltaStatus::Deleted),
            "status preserved (tombstone)"
        );
        assert_eq!(out[0].seq, 7, "seq preserved");
    }

    /// A file tagged in BOTH domains via the per-file path must land BOTH tags
    /// on the blake3 identity and count as exactly ONE matched_file, not two.
    /// Without entry coalescing the second domain's entry would overwrite the
    /// first (HashMap::insert keyed by file_id in merge_pulled_mappings_for_files),
    /// deleting the blake3-native tag and double-counting the file.
    #[test]
    fn per_file_pull_dual_domain_coalesces_both_tag_sets_into_one_file() {
        use naiad_core::{FileRecord, hash_reader_dual};

        let content = b"per-file-dual-coalesce";
        let (blake3_hash, sha256_hex) = hash_reader_dual(&content[..]).unwrap();

        // Seed the blake3 store with the SAME file carrying a native tag, and
        // the sha256 snapshot with its sha256 carrying "character:samus".
        let (addr, _snapshot_dir) = spawn_dual_domain_repo(
            vec![(blake3_hash.to_hex(), "native:blake3-tag".to_string())],
            &sha256_hex,
        );
        let url = format!("http://{addr}");

        let db = Db::open_in_memory().unwrap();
        db.insert_file(
            &FileRecord::new(
                blake3_hash,
                "/lib/dual.png".into(),
                content.len() as u64,
                Some(1),
            )
            .with_sha256(sha256_hex.clone()),
            1,
        )
        .unwrap();
        let _svc = db.add_shared_service("dual-perfile", &url, None).unwrap();
        let db = Mutex::new(db);

        let cache = CapsCache::new();
        let outcome = pull_repo_for_hashes(
            &db,
            &cache,
            "dual-perfile",
            256,
            &[blake3_hash],
            &naiad_netproto::NoopObserver,
        )
        .unwrap();

        let guard = db.lock_recover();
        let fid = guard.file_id_by_hash(&blake3_hash).unwrap().unwrap();
        let tags: Vec<String> = guard
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert!(
            tags.contains(&"native:blake3-tag".to_string()),
            "blake3-domain native tag must survive alongside the sha256 tag: {tags:?}"
        );
        assert!(
            tags.contains(&"character:samus".to_string()),
            "sha256-domain tag must land on the blake3 identity: {tags:?}"
        );
        assert_eq!(
            outcome.stats.matched_files, 1,
            "one file tagged in two domains must count as exactly one matched_file, not two: {:?}",
            outcome.stats
        );
        assert_eq!(
            outcome.stats.mappings, 2,
            "both domain tags must be counted in mappings: {:?}",
            outcome.stats
        );
    }

    /// Build a snapshot-inferred Caps: serves sha256 but NOT incrementally.
    fn snapshot_caps() -> naiad_netproto::Caps {
        naiad_netproto::Caps {
            version: 8,
            mode: naiad_netproto::PullMode::Bucketed { prefix_bits: 256 },
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: naiad_netproto::HashDomain::Blake3,
            hash_domains: vec![
                naiad_netproto::HashDomain::Blake3,
                naiad_netproto::HashDomain::Sha256,
            ],
            // sha256 is NOT in incremental_domains → snapshot_inferred() == true
            incremental_domains: Some(vec!["blake3".to_string()]),
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            name: None,
        }
    }

    /// Build a plain (non-snapshot-inferred) Caps: sha256 IS incremental.
    fn plain_caps() -> naiad_netproto::Caps {
        naiad_netproto::Caps {
            version: 8,
            mode: naiad_netproto::PullMode::Bucketed { prefix_bits: 256 },
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: naiad_netproto::HashDomain::Blake3,
            hash_domains: vec![
                naiad_netproto::HashDomain::Blake3,
                naiad_netproto::HashDomain::Sha256,
            ],
            // sha256 IS in incremental_domains → snapshot_inferred() == false
            incremental_domains: Some(vec!["blake3".to_string(), "sha256".to_string()]),
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            name: None,
        }
    }

    /// Records (active_domain, phase) so a test can assert the daemon drives
    /// set_domain around each leg and emits the Merging/Done bookends.
    #[derive(Default)]
    struct LegRecorder {
        domain: std::cell::Cell<Option<&'static str>>,
        seen: std::cell::RefCell<Vec<(Option<&'static str>, &'static str)>>,
    }
    impl naiad_netproto::PullObserver for LegRecorder {
        fn set_domain(&self, d: Option<&'static str>) {
            self.domain.set(d);
        }
        fn on_phase(&self, p: naiad_netproto::PullPhase) {
            use naiad_netproto::PullPhase::*;
            let tag = match p {
                RequestSent { .. } => "request",
                ChunkReceived { .. } => "chunk",
                Merging => "merging",
                Done => "done",
                // Within-window streaming row tick (#176): not a distinct
                // stage from the recorder's perspective; skip it.
                RowReceived { .. } => return,
                // Window shrink-retry (#177): not a distinct stage from the
                // recorder's perspective; skip it.
                WindowRetry { .. } => return,
            };
            self.seen.borrow_mut().push((self.domain.get(), tag));
        }
    }

    #[test]
    fn per_file_pull_reports_stages_and_bookends() {
        use naiad_core::{FileRecord, hash_reader_dual};

        let content = b"per-file-sha-domain";
        let (blake3_hash, sha256_hex) = hash_reader_dual(&content[..]).unwrap();
        let (filler_blake3, _) = hash_reader_dual(&b"per-file-filler"[..]).unwrap();
        let (addr, _snapshot_dir) = spawn_dual_domain_repo(
            vec![(filler_blake3.to_hex(), "filler:tag".to_string())],
            &sha256_hex,
        );
        let url = format!("http://{addr}");

        let db = Db::open_in_memory().unwrap();
        db.insert_file(
            &FileRecord::new(
                blake3_hash,
                "/lib/a.png".into(),
                content.len() as u64,
                Some(1),
            )
            .with_sha256(sha256_hex.clone()),
            1,
        )
        .unwrap();
        let _svc = db.add_shared_service("perfile-stages", &url, None).unwrap();
        let db = Mutex::new(db);

        let cache = CapsCache::new();
        let rec = LegRecorder::default();
        pull_repo_for_hashes(&db, &cache, "perfile-stages", 256, &[blake3_hash], &rec).unwrap();
        let seen = rec.seen.borrow();
        assert!(
            seen.iter()
                .any(|(d, t)| *d == Some("blake3") && *t == "chunk"),
            "should have at least one blake3 chunk event: {seen:?}"
        );
        // Merging then Done, in order, at the end, with domain cleared.
        let tail: Vec<&str> = seen.iter().rev().take(2).map(|(_, t)| *t).collect();
        assert_eq!(
            tail,
            vec!["done", "merging"],
            "tail must be [merging, done] (rev): {seen:?}"
        );
        assert!(
            seen.iter()
                .all(|(d, t)| !(*t == "merging" || *t == "done") || d.is_none()),
            "merging/done phases must have domain cleared: {seen:?}"
        );
    }

    #[test]
    fn per_file_pull_dual_domain_legs_in_order() {
        use naiad_core::{FileRecord, hash_reader_dual};

        let content = b"per-file-dual-coalesce";
        let (blake3_hash, sha256_hex) = hash_reader_dual(&content[..]).unwrap();

        let (addr, _snapshot_dir) = spawn_dual_domain_repo(
            vec![(blake3_hash.to_hex(), "native:blake3-tag".to_string())],
            &sha256_hex,
        );
        let url = format!("http://{addr}");

        let db = Db::open_in_memory().unwrap();
        db.insert_file(
            &FileRecord::new(
                blake3_hash,
                "/lib/dual.png".into(),
                content.len() as u64,
                Some(1),
            )
            .with_sha256(sha256_hex.clone()),
            1,
        )
        .unwrap();
        let _svc = db
            .add_shared_service("dual-perfile-legs", &url, None)
            .unwrap();
        let db = Mutex::new(db);

        let cache = CapsCache::new();
        let rec = LegRecorder::default();
        pull_repo_for_hashes(&db, &cache, "dual-perfile-legs", 256, &[blake3_hash], &rec).unwrap();
        let seen = rec.seen.borrow();
        let domains: Vec<&'static str> = seen
            .iter()
            .filter(|(_, t)| *t == "chunk" || *t == "request")
            .filter_map(|(d, _)| *d)
            .collect();
        let first_b3 = domains.iter().position(|d| *d == "blake3");
        let first_sha = domains.iter().position(|d| *d == "sha256");
        assert!(
            first_b3.is_some() && first_sha.is_some(),
            "both domains must appear in request/chunk events: {seen:?}"
        );
        assert!(
            first_b3 < first_sha,
            "blake3 leg must precede sha256 leg: {seen:?}"
        );
    }

    #[test]
    fn clamp_hint_wraps_only_when_clamped_and_inferred() {
        use anyhow::anyhow;
        let snapshot_caps = snapshot_caps();
        let plain_caps = plain_caps();

        // clamped + inferred -> hint present
        let e =
            with_clamp_hint(Err::<(), _>(anyhow!("boom")), &snapshot_caps, 256, 24).unwrap_err();
        let chain = format!("{e:#}");
        assert!(chain.contains("clamped to 24-bit buckets"), "{chain}");
        assert!(chain.contains("max_query_bits = 256"), "{chain}");

        // not clamped -> no hint
        let e =
            with_clamp_hint(Err::<(), _>(anyhow!("boom")), &snapshot_caps, 256, 256).unwrap_err();
        assert!(!format!("{e:#}").contains("clamped"));

        // clamped but not snapshot-inferred -> no hint
        let e = with_clamp_hint(Err::<(), _>(anyhow!("boom")), &plain_caps, 256, 24).unwrap_err();
        assert!(!format!("{e:#}").contains("clamped"));

        // Ok passes through untouched
        assert!(with_clamp_hint(Ok::<_, anyhow::Error>(7), &snapshot_caps, 256, 24).is_ok());
    }

    // ── #178 §6.2 decision table for the first-window seed ────────────────────

    /// Build a Caps with `serve_hint[sha256] = ServeHint { ms_per_bucket, hint_bits }`.
    fn caps_with_serve_hint(ms: f64, hint_bits: Option<u32>) -> naiad_netproto::Caps {
        let mut caps = plain_caps();
        caps.mode = naiad_netproto::PullMode::Bucketed { prefix_bits: 32 };
        caps.serve_hint.insert(
            "sha256".to_string(),
            naiad_netproto::ServeHint {
                ms_per_bucket: ms,
                hint_bits,
            },
        );
        caps
    }

    /// Build a Caps with empty serve_hint, advertised at `prefix_bits = 32`.
    fn caps_with_empty_serve_hint() -> naiad_netproto::Caps {
        let mut caps = plain_caps();
        caps.mode = naiad_netproto::PullMode::Bucketed { prefix_bits: 32 };
        caps
    }

    /// #178 §6.2 decision table for the first-window seed.
    #[test]
    fn seed_ms_per_bucket_decision_table() {
        let d = naiad_netproto::HashDomain::Sha256;

        // Row 1: hint with hint_bits=Some(32), coarse request bits=24 → scale up
        // by 2^(32-24) = 256: 0.2 × 256 = 51.2.
        let c = caps_with_serve_hint(0.2, Some(32));
        let result = seed_ms_per_bucket(&c, d, 32, 24);
        assert!(
            (result.unwrap() - 51.2).abs() < 1e-9,
            "Row 1 coarse: expected 51.2, got {result:?}"
        );

        // Row 1: at-width request → unscaled (2^0 = 1).
        let result = seed_ms_per_bucket(&c, d, 32, 32);
        assert!(
            (result.unwrap() - 0.2).abs() < 1e-12,
            "Row 1 at-width: expected 0.2, got {result:?}"
        );

        // Row 2: hint present, hint_bits=None → fall back to advertised (32) as
        // the measured width; same math as Row 1.
        let c = caps_with_serve_hint(0.2, None);
        let result = seed_ms_per_bucket(&c, d, 32, 24);
        assert!(
            (result.unwrap() - 51.2).abs() < 1e-9,
            "Row 2 (no hint_bits fallback): expected 51.2, got {result:?}"
        );

        // Row 3: no hint at all, requested_bits < advertised → COARSE_BOOTSTRAP_MS.
        let c = caps_with_empty_serve_hint();
        assert_eq!(
            seed_ms_per_bucket(&c, d, 32, 24),
            Some(COARSE_BOOTSTRAP_MS),
            "Row 3: no hint + coarse must return COARSE_BOOTSTRAP_MS"
        );

        // Row 4: no hint, requested_bits >= advertised → None (today's behaviour).
        assert_eq!(
            seed_ms_per_bucket(&c, d, 32, 32),
            None,
            "Row 4: no hint + at-width must return None"
        );

        // Row 5: malformed hint (ms_per_bucket = 0.0) → treated as absent →
        // bootstrap on the coarse path.
        let c = caps_with_serve_hint(0.0, Some(32));
        assert_eq!(
            seed_ms_per_bucket(&c, d, 32, 24),
            Some(COARSE_BOOTSTRAP_MS),
            "Row 5 (zero ms): malformed hint must fall through to bootstrap"
        );

        // Row 5: NaN hint → bootstrap.
        let c = caps_with_serve_hint(f64::NAN, Some(32));
        assert_eq!(
            seed_ms_per_bucket(&c, d, 32, 24),
            Some(COARSE_BOOTSTRAP_MS),
            "Row 5 (NaN ms): malformed hint must fall through to bootstrap"
        );

        // Row 5: negative hint → bootstrap.
        let c = caps_with_serve_hint(-1.0, Some(32));
        assert_eq!(
            seed_ms_per_bucket(&c, d, 32, 24),
            Some(COARSE_BOOTSTRAP_MS),
            "Row 5 (negative ms): malformed hint must fall through to bootstrap"
        );
    }

    /// #178 §5.3: the bootstrap seed collapses netproto's first window to
    /// `MIN_WINDOW`. Mirrors netproto's formula:
    /// `W0 = round(WINDOW_TARGET_MS / ms).max(MIN_WINDOW)`.
    #[test]
    fn coarse_bootstrap_lands_on_min_window() {
        let w0 = (naiad_netproto::WINDOW_TARGET_MS as f64 / COARSE_BOOTSTRAP_MS).round() as usize;
        assert_eq!(
            w0.max(naiad_netproto::MIN_WINDOW),
            naiad_netproto::MIN_WINDOW,
            "bootstrap seed must collapse W0 to MIN_WINDOW={}, got raw w0={w0}",
            naiad_netproto::MIN_WINDOW
        );
    }

    // ── #195 floor-domain tests ───────────────────────────────────────────────

    /// Build a mirror-mode Caps: hash_domain = Sha256 (native), min_query_bits
    /// = Some(floor). This is what a full-PTR mirror advertises after #195.
    fn mirror_caps(floor: u32) -> naiad_netproto::Caps {
        naiad_netproto::Caps {
            version: 8,
            mode: naiad_netproto::PullMode::Bucketed { prefix_bits: 256 },
            relation_incremental: true,
            mapping_incremental: true,
            reports: false,
            repo_key: None,
            hash_domain: naiad_netproto::HashDomain::Sha256,
            hash_domains: vec![naiad_netproto::HashDomain::Sha256],
            incremental_domains: Some(vec!["sha256".to_string()]),
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: Some(floor),
            store_generation: None,
            name: None,
        }
    }

    /// #195: for a mirror-mode repo (native sha256, min_query_bits = Some(floor)),
    /// the floor must be applied when pulling the sha256 domain.
    ///
    /// `effective_prefix_bits_floored(advertised, max_query_bits, floor)` raises
    /// `base = advertised.min(max_query_bits)` to `floor` when `base < floor`.
    /// The key scenario: server advertises 256, client ceiling < floor → raise.
    ///
    /// With the OLD gate (`filter(|_| domain != caps.hash_domain)`):
    ///   domain == caps.hash_domain → filter returns None → floor not applied.
    /// With the NEW gate (`filter(|_| domain == HashDomain::Sha256)`):
    ///   domain == Sha256 → filter returns Some(floor) → floor applied.
    #[test]
    fn mirror_sha256_floor_applied_to_sha256_domain() {
        let floor = 16u32;
        let caps = mirror_caps(floor);
        let domain = naiad_netproto::HashDomain::Sha256;
        // Server advertises prefix_bits = 256; client ceiling is below the floor.
        let advertised = 256u32;
        let client_ceiling = 8u32; // client max_query_bits below the floor of 16

        // The NEW gate produces Some(floor) for the sha256 domain.
        let new_floor: Option<u32> = caps.min_query_bits.filter(|_| domain == HashDomain::Sha256);
        assert_eq!(
            new_floor,
            Some(floor),
            "new gate must yield Some(floor) for sha256 domain in mirror mode"
        );

        // effective_prefix_bits_floored(advertised, max_query_bits, floor):
        //   base = advertised.min(client_ceiling) = 256.min(8) = 8
        //   floor = Some(16): 8.max(16).min(256) = 16
        let prefix_bits = effective_prefix_bits_floored(advertised, client_ceiling, new_floor);
        assert_eq!(
            prefix_bits, floor,
            "floor must raise client_ceiling={client_ceiling} to floor={floor}"
        );

        // Confirm the OLD gate would have returned None (the bug this fixes).
        let old_floor: Option<u32> = caps.min_query_bits.filter(|_| domain != caps.hash_domain);
        assert_eq!(
            old_floor, None,
            "old gate incorrectly returns None for native sha256 (mirror mode)"
        );
        let old_prefix_bits = effective_prefix_bits_floored(advertised, client_ceiling, old_floor);
        assert_eq!(
            old_prefix_bits, client_ceiling,
            "old code would have sent client_ceiling={client_ceiling} unraised (the bug)"
        );
    }

    /// `CapsCache::client` returns the same `Arc<RepoClient>` for the same URL
    /// and a different one for a different URL.
    #[test]
    fn caps_cache_client_reuse() {
        let cache = CapsCache::new();
        let url_a = "http://127.0.0.1:19999/";
        let url_b = "http://127.0.0.1:29999/";

        let c1 = cache.client(url_a);
        let c2 = cache.client(url_a);
        assert!(
            Arc::ptr_eq(&c1, &c2),
            "same URL must yield the same Arc<RepoClient> (connection pool reused)"
        );

        let c3 = cache.client(url_b);
        assert!(
            !Arc::ptr_eq(&c1, &c3),
            "different URL must yield a different Arc<RepoClient>"
        );
    }

    /// #195: for a blake3-native repo, the floor must NEVER be applied even when
    /// caps.min_query_bits is Some (which a snapshot-backed blake3 repo advertises).
    /// The sha256 gate (`filter(|_| domain == HashDomain::Sha256)`) on the blake3
    /// domain returns None, so blake3 queries are unaffected.
    #[test]
    fn blake3_native_pull_never_floored() {
        let floor = 16u32;
        // Build a snapshot-backed blake3-native caps (has min_query_bits, but native is blake3).
        let mut caps = snapshot_caps();
        caps.min_query_bits = Some(floor);
        let domain = naiad_netproto::HashDomain::Blake3;
        let advertised = 256u32;
        let client_ceiling = 4u32; // way below any floor

        // The gate on sha256 domain: blake3 → filter returns None → no floor.
        let new_floor: Option<u32> = caps.min_query_bits.filter(|_| domain == HashDomain::Sha256);
        assert_eq!(
            new_floor, None,
            "floor must not apply to blake3 domain (#195)"
        );
        let prefix_bits = effective_prefix_bits_floored(advertised, client_ceiling, new_floor);
        assert_eq!(
            prefix_bits, client_ceiling,
            "blake3 query uses raw client_ceiling={client_ceiling} — not raised by sha256 floor"
        );
    }
}
