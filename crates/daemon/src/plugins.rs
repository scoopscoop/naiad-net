//! Plugin registry construction, the `Db`-backed `Sink`, and the import/lookup/
//! backfill operations the HTTP handlers call.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use naiad_core::{Hash, Tag, hash_reader_dual};
use naiad_db::{Db, TagCache};
use naiad_plugin::{
    FileRef, MappingRecord, PluginError, RecordStatus, RelationKind, RelationRecord, Sink, Source,
    Tagger,
};
use naiad_plugin_hydrus::HydrusPlugin;

use crate::lock::LockRecover;

const HYDRUS_SERVICE_NAME: &str = "Hydrus: imported tags";

/// How many records to buffer before flushing to the DB in a single
/// transaction. Each buffer (mappings, siblings, parents) flushes
/// independently when it reaches this threshold.
///
/// 4096 is chosen to match [`RELATIONS_PROGRESS_EVERY`] and sit comfortably
/// above `SCAN_WRITE_BATCH` (256). One transaction of 4096 cached upserts is a
/// single WAL fsync that holds the writer mutex for only a few ms — the
/// fairness budget. Tune upward for more throughput, downward for shorter lock
/// holds; the benchmark's lock-wait metric is the guardrail.
const IMPORT_WRITE_BATCH: usize = 4096;

/// Buffering [`Sink`] that accumulates records into three independent `Vec`
/// buffers (mappings, siblings, parents) and flushes each to the DB in a
/// single `unchecked_transaction` whenever the buffer reaches
/// [`IMPORT_WRITE_BATCH`].  Holding `&Mutex<Db>` (not a borrowed guard)
/// lets it lock and unlock per flush — no lock is held between flushes.
///
/// The [`TagCache`] is owned by the sink and persists across every flush for
/// the lifetime of the import: the first occurrence of each tag pays one
/// intern SQL round-trip; subsequent occurrences are HashMap hits.  Drop the
/// sink (and its cache) when the import finishes to release memory.
///
/// ## Counts semantics
///
/// - `mappings_ct` increments by one **at buffer time** for each `mapping()`
///   call, mirroring the old `DbSink`'s per-record increment.
/// - `siblings_ct` and `parents_ct` accumulate the **return value** of each
///   [`Db::add_siblings_batch`] / [`Db::add_parents_batch`] call.  Those
///   methods return the count of applied rows, excluding self-relations — the
///   same semantics as the old `DbSink` which swallowed
///   [`naiad_db::Error::SelfRelation`] without incrementing the counter.
///
/// ## Error handling
///
/// A flush error is stored in `self.err`; the `Sink` method returns
/// [`PluginError`] to stop the bulk import.  Partial committed batches survive
/// — an idempotent re-run converges (same behaviour as `DbSink`).
struct BatchSink<'a> {
    db: &'a Mutex<Db>,
    service_id: i64,
    /// Tag→id cache, persists across all flushes for this import.
    cache: TagCache,
    mappings: Vec<(String, Tag, &'static str)>,
    siblings: Vec<(Tag, Tag)>,
    parents: Vec<(Tag, Tag)>,
    /// Running applied-count for mappings (incremented at buffer time).
    mappings_ct: u64,
    /// Running applied-count for siblings (accumulated from flush returns).
    siblings_ct: u64,
    /// Running applied-count for parents (accumulated from flush returns).
    parents_ct: u64,
    err: Option<naiad_db::Error>,
}

impl<'a> BatchSink<'a> {
    fn new(db: &'a Mutex<Db>, service_id: i64) -> Self {
        Self {
            db,
            service_id,
            cache: TagCache::new(),
            mappings: Vec::new(),
            siblings: Vec::new(),
            parents: Vec::new(),
            mappings_ct: 0,
            siblings_ct: 0,
            parents_ct: 0,
            err: None,
        }
    }

    /// Flush the mappings buffer: lock → `stage_mappings_batch` → unlock → clear.
    fn flush_mappings(&mut self) {
        if self.mappings.is_empty() || self.err.is_some() {
            return;
        }
        let items: Vec<(String, Tag, &str)> = self
            .mappings
            .iter()
            .map(|(sha, tag, status)| (sha.clone(), tag.clone(), *status))
            .collect();
        let guard = self.db.lock_recover();
        match guard.stage_mappings_batch(self.service_id, &items, &mut self.cache) {
            Ok(_) => {}
            Err(e) => {
                self.err = Some(e);
            }
        }
        self.mappings.clear();
    }

    /// Flush the siblings buffer: lock → `add_siblings_batch` → unlock → clear.
    fn flush_siblings(&mut self) {
        if self.siblings.is_empty() || self.err.is_some() {
            return;
        }
        let guard = self.db.lock_recover();
        match guard.add_siblings_batch(self.service_id, &self.siblings, &mut self.cache) {
            Ok(applied) => {
                self.siblings_ct += applied;
            }
            Err(e) => {
                self.err = Some(e);
            }
        }
        self.siblings.clear();
    }

    /// Flush the parents buffer: lock → `add_parents_batch` → unlock → clear.
    fn flush_parents(&mut self) {
        if self.parents.is_empty() || self.err.is_some() {
            return;
        }
        let guard = self.db.lock_recover();
        match guard.add_parents_batch(self.service_id, &self.parents, &mut self.cache) {
            Ok(applied) => {
                self.parents_ct += applied;
            }
            Err(e) => {
                self.err = Some(e);
            }
        }
        self.parents.clear();
    }

    /// Flush all three partial buffers. Call after `bulk_import` returns and
    /// before checking `self.err` or reading the counts.
    fn finish(&mut self) {
        self.flush_mappings();
        self.flush_siblings();
        self.flush_parents();
    }
}

impl Sink for BatchSink<'_> {
    fn mapping(&mut self, rec: MappingRecord) -> naiad_plugin::Result<()> {
        if self.err.is_some() {
            return Err(PluginError("db sink error".into()));
        }
        let status = match rec.status {
            RecordStatus::Current => "current",
            RecordStatus::Deleted => "deleted",
        };
        self.mappings.push((rec.sha256, rec.tag, status));
        // Count per buffered record, parity with old DbSink which counted per
        // successful stage_mapping call (always succeeds for non-error cases).
        self.mappings_ct += 1;
        if self.mappings.len() >= IMPORT_WRITE_BATCH {
            self.flush_mappings();
        }
        if self.err.is_some() {
            Err(PluginError("db sink mapping failed".into()))
        } else {
            Ok(())
        }
    }

    fn relation(&mut self, rec: RelationRecord) -> naiad_plugin::Result<()> {
        if self.err.is_some() {
            return Err(PluginError("db sink error".into()));
        }
        // Mirror DbSink: skip Deleted relations at buffer time.
        if rec.status == RecordStatus::Deleted {
            return Ok(());
        }
        match rec.kind {
            RelationKind::Sibling => {
                self.siblings.push((rec.from, rec.to));
                if self.siblings.len() >= IMPORT_WRITE_BATCH {
                    self.flush_siblings();
                }
            }
            RelationKind::Parent => {
                self.parents.push((rec.from, rec.to));
                if self.parents.len() >= IMPORT_WRITE_BATCH {
                    self.flush_parents();
                }
            }
        }
        if self.err.is_some() {
            Err(PluginError("db sink relation failed".into()))
        } else {
            Ok(())
        }
    }
}

/// Hydrus import configuration set via `POST /api/hydrus/configure`.
#[derive(Debug, Clone, Default)]
pub struct HydrusConfig {
    pub dir: Option<PathBuf>,
    pub tag_services: Vec<i64>,
}

fn hydrus_plugin(cfg: &HydrusConfig) -> Result<HydrusPlugin, PluginError> {
    let dir = cfg
        .dir
        .clone()
        .ok_or_else(|| PluginError("Hydrus not configured (set the DB directory)".into()))?;
    Ok(HydrusPlugin::new(dir, cfg.tag_services.clone()))
}

/// Plugin descriptors for `GET /api/plugins`. Static for Step 1 (one plugin).
pub fn list_plugins() -> Vec<(String, String, bool, bool, bool)> {
    vec![("hydrus".into(), "Hydrus importer".into(), true, false, true)]
}

/// Result of a bulk import. `siblings`/`parents` count **applied**
/// (non-deleted, non-self) relations from the db sink, not the plugin's raw
/// streamed records (#44 corrected this; the raw stream includes
/// self-relations that are never applied).
pub struct ImportOutcome {
    pub mappings_staged: u64,
    pub mappings_resolved: u64,
    pub siblings: u64,
    pub parents: u64,
    pub sha256_backfilled: u64,
}

/// Result of a relations-only import (issue #41): applied (non-deleted,
/// non-self) relation counts from the db sink.
pub struct RelationsOutcome {
    pub siblings: u64,
    pub parents: u64,
}

/// Emit a relations-import progress tick every this many streamed edges.
/// Coarse on purpose: at ~614k edges (PTR) that is ~150 ticks.
const RELATIONS_PROGRESS_EVERY: u64 = 4096;

/// `Sink` decorator that counts streamed relation records and reports
/// `(done, total, siblings, parents)` every [`RELATIONS_PROGRESS_EVERY`]
/// edges. `done` counts records streamed (including deleted/unapplied ones,
/// matching the `count_relations` total); siblings/parents are the inner
/// sink's applied counts.
struct ProgressSink<'a, 'f> {
    inner: BatchSink<'a>,
    done: u64,
    total: u64,
    on_progress: &'f mut dyn FnMut(u64, u64, u64, u64),
}

impl Sink for ProgressSink<'_, '_> {
    fn mapping(&mut self, rec: MappingRecord) -> naiad_plugin::Result<()> {
        self.inner.mapping(rec)
    }

    fn relation(&mut self, rec: RelationRecord) -> naiad_plugin::Result<()> {
        let res = self.inner.relation(rec);
        self.done += 1;
        if self.done % RELATIONS_PROGRESS_EVERY == 0 {
            (self.on_progress)(
                self.done,
                self.total,
                self.inner.siblings_ct,
                self.inner.parents_ct,
            );
        }
        res
    }
}

/// How many rows [`backfill_sha256`] loads per page of its work list.
///
/// 8 192 rows bounds the *returned list* to roughly:
///   8 192 × (avg PathBuf ~128 B) ≈ 1 MB of path data
///   8 192 × (64 B sha256 hex)    ≈ 0.5 MB of pending writes
/// Well inside the desktop memory budget. Larger values mean fewer DB-lock
/// bursts at the cost of more bytes in flight before the first write; smaller
/// values increase lock contention for minimal memory gain. 8 192 is also 2×
/// the [`IMPORT_WRITE_BATCH`] flush size, so every page produces at least two
/// write bursts, which gives observable progress on a slow storage device.
///
/// This bounds what the caller holds, not what SQLite does: the query still
/// groups over every matching row per page (see
/// [`naiad_db::Db::files_missing_sha256_after`]).
const BACKFILL_PASS_LIMIT: usize = 8192;

/// Backfill SHA-256 for present files lacking it. Per-file read errors are
/// skipped; file IDs that fail to open are inserted into `skip` so the caller
/// can bypass them on subsequent pulls without re-paying open-timeout costs.
///
/// `skip` is owned by the caller (typically stored inside [`CapsCache`] so it
/// persists across pulls within the same process session but resets cleanly on
/// process restart or in tests). A disconnected SMB share or a file held under
/// an exclusive handle returns an error immediately on Windows; without the
/// skip set every pull would re-incur those costs.
///
/// **Progress is durable**: the function writes every [`IMPORT_WRITE_BATCH`]
/// results immediately, so progress survives a crash or restart. A previous
/// version accumulated all results in memory before writing, meaning an
/// 800k-file library restarting at 90% lost all progress.
///
/// **The whole work list is covered in one call.** The function pages through
/// it internally with an ascending id cursor, so callers do not loop — an
/// earlier revision bounded a single pass to [`BACKFILL_PASS_LIMIT`] and left
/// the looping to the caller, which no caller did: a library import silently
/// tagged only the first 8 192 files and reported success.
///
/// **Memory is bounded**: at most [`BACKFILL_PASS_LIMIT`] rows plus one
/// [`IMPORT_WRITE_BATCH`] of pending writes are held at any moment, regardless
/// of library size.
///
/// The naiad lock is held only in short bursts: one burst to read the bounded
/// work list and one burst per [`IMPORT_WRITE_BATCH`] writes. File I/O runs
/// lock-free, so other writers can interleave freely.
///
/// # Errors
/// Returns an error if a db statement fails.
pub fn backfill_sha256(db: &Mutex<Db>, skip: &mut HashSet<i64>) -> Result<u64, naiad_db::Error> {
    // Total up front, for a progress log that means something. Cheap COUNT.
    let total = {
        let guard = db.lock_recover();
        guard.count_files_missing_sha256_present()?
    };
    if total == 0 {
        return Ok(0);
    }
    tracing::info!(
        target: "sync",
        total,
        "sha256 backfill: starting"
    );

    // Off-lock: open and hash each file; flush to DB every IMPORT_WRITE_BATCH
    // records so progress is durable. Files in the skip set are silently
    // bypassed; files that fail to open are added to it so future passes skip
    // them without paying open-timeout costs again.
    let mut pending: Vec<(i64, String)> = Vec::with_capacity(IMPORT_WRITE_BATCH);
    let mut written: u64 = 0;
    let mut done: u64 = 0;
    let mut newly_skipped: Vec<i64> = Vec::new();
    // Ascending id cursor. Paging by id rather than re-issuing a bare LIMIT is
    // what makes the loop terminate *and* make progress: a row that cannot be
    // hashed stays in the result set, so a LIMIT-only query would hand back the
    // same lowest ids every time and never reach the files behind them.
    let mut after_id: i64 = 0;

    loop {
        let page = {
            let guard = db.lock_recover();
            guard.files_missing_sha256_after(after_id, BACKFILL_PASS_LIMIT)?
        };
        if page.is_empty() {
            break;
        }
        // Advance before any I/O: ids ascend, so the last is the high-water
        // mark whether or not each file in this page resolved.
        after_id = page.last().map_or(after_id, |(id, _)| *id);

        for (file_id, path) in page {
            done += 1;

            if skip.contains(&file_id) {
                continue;
            }

            let file = match fs::File::open(&path) {
                Ok(f) => f,
                Err(_) => {
                    // Record for the skip set; do not abort the pass.
                    newly_skipped.push(file_id);
                    continue;
                }
            };
            let Ok((_, sha)) = hash_reader_dual(io::BufReader::new(file)) else {
                // Read error after a successful open. This is more unusual than
                // a failed open (truncated file, I/O error mid-read). Don't add
                // to the skip set — the condition may clear — but skip this file.
                continue;
            };
            pending.push((file_id, sha));

            // Flush immediately when the pending buffer is full so we never
            // hold more than IMPORT_WRITE_BATCH results in memory at once.
            if pending.len() >= IMPORT_WRITE_BATCH {
                let guard = db.lock_recover();
                written += guard.set_sha256_batch(&pending)?;
                pending.clear();
                tracing::info!(
                    target: "sync",
                    done,
                    total,
                    written,
                    "sha256 backfill: progress"
                );
            }
        }
    }

    // Flush any remainder (fewer than IMPORT_WRITE_BATCH entries).
    if !pending.is_empty() {
        let guard = db.lock_recover();
        written += guard.set_sha256_batch(&pending)?;
        // pending is about to be dropped; no need to clear.
    }

    // Commit newly failed IDs to the caller's skip set so the next pull
    // bypasses them without re-paying open-timeout costs.
    skip.extend(newly_skipped);

    Ok(written)
}

fn ensure_service(db: &Db) -> Result<i64, PluginError> {
    match db.service_id_by_name(HYDRUS_SERVICE_NAME) {
        Ok(Some(id)) => Ok(id),
        Ok(None) => db
            .add_local_service(HYDRUS_SERVICE_NAME, None)
            .map_err(|e| PluginError(format!("create service: {e}"))),
        Err(e) => Err(PluginError(format!("lookup service: {e}"))),
    }
}

/// Run the full Hydrus bulk import: backfill sha256, stream records, resolve.
///
/// The naiad lock is held only in short bursts:
///
/// 1. **Setup burst** — `ensure_service`.
/// 2. **Backfill** — [`backfill_sha256`]: a snapshot burst, off-lock hashing,
///    then `set_sha256_batch` write burst(s) in [`IMPORT_WRITE_BATCH`] chunks.
/// 3. **Streaming** — `bulk_import` through [`BatchSink`]; the sink flushes per
///    buffer under its own brief lock bursts.
/// 4. **Resolve burst** — `resolve_staged_mappings` (one `INSERT … SELECT`;
///    accepted single burst, not a fairness regression).
///
/// # Errors
/// Returns an error if the import or any db step fails.
pub fn run_import(db: &Mutex<Db>, cfg: &HydrusConfig) -> Result<ImportOutcome, PluginError> {
    tracing::info!(target: "hydrus", "hydrus import started");
    let plugin = hydrus_plugin(cfg)?;

    // Burst 1: ensure the service row exists.
    let service_id = {
        let guard = db.lock_recover();
        ensure_service(&guard)?
    };

    // Backfill burst(s): snapshot work list, hash off-lock, write in chunks.
    // A fresh skip set per import: the Hydrus import is a one-shot operation
    // so cross-call persistence doesn't help here.
    let sha256_backfilled = backfill_sha256(db, &mut HashSet::new())
        .map_err(|e| PluginError(format!("backfill: {e}")))?;

    // Streaming: bulk_import drives BatchSink, which flushes per buffer under
    // brief lock bursts.  The TagCache persists across all flushes.
    let mut sink = BatchSink::new(db, service_id);
    let import_res = plugin.bulk_import(&mut sink);
    // Flush the three partial buffers remaining after bulk_import.
    sink.finish();
    // A flush failure mid-stream aborts bulk_import with a generic message;
    // sink.err holds the underlying db error, so surface it first.
    if let Some(e) = sink.err.take() {
        return Err(PluginError(format!("db sink: {e}")));
    }
    import_res?;
    let (staged, siblings, parents) = (sink.mappings_ct, sink.siblings_ct, sink.parents_ct);

    // Burst 3: resolve staged mappings (one INSERT … SELECT over the whole
    // staging table; unavoidable single burst, accepted).
    let resolved = {
        let guard = db.lock_recover();
        guard
            .resolve_staged_mappings(service_id, "sha256")
            .map_err(|e| PluginError(format!("resolve: {e}")))?
    };

    let outcome = ImportOutcome {
        mappings_staged: staged,
        mappings_resolved: resolved,
        siblings,
        parents,
        sha256_backfilled,
    };
    tracing::info!(
        target: "hydrus",
        staged = outcome.mappings_staged,
        resolved = outcome.mappings_resolved,
        siblings = outcome.siblings,
        parents = outcome.parents,
        sha256_backfilled = outcome.sha256_backfilled,
        "hydrus import finished"
    );
    Ok(outcome)
}

/// Pull the full Hydrus sibling/parent graph — no mapping work, no sha256
/// backfill (issue #41). Relations land on the same service as the other
/// Hydrus imports, so they canonicalize mappings pulled before *or* after.
/// Idempotent: re-runs upsert into the same rows; an interrupted run leaves a
/// valid partial graph. Convenience wrapper with no progress callback.
///
/// # Errors
/// Returns an error if the import or any db step fails.
pub fn run_relations_import(
    db: &Mutex<Db>,
    cfg: &HydrusConfig,
) -> Result<RelationsOutcome, PluginError> {
    run_relations_import_with_progress(db, cfg, |_, _, _, _| {})
}

/// [`run_relations_import`] with progress: `on_progress(edges_done,
/// edges_total, siblings, parents)` fires every [`RELATIONS_PROGRESS_EVERY`]
/// streamed edges, plus once at completion with `done == total`. The total is
/// known up front from the Hydrus relation tables, so the bar is determinate.
///
/// The naiad lock is held only in short bursts: once at startup for
/// `ensure_service`, then once per [`IMPORT_WRITE_BATCH`] relation records
/// flushed by the inner [`BatchSink`].
///
/// # Errors
/// Returns an error if the import or any db step fails.
pub fn run_relations_import_with_progress(
    db: &Mutex<Db>,
    cfg: &HydrusConfig,
    mut on_progress: impl FnMut(u64, u64, u64, u64),
) -> Result<RelationsOutcome, PluginError> {
    let plugin = hydrus_plugin(cfg)?;
    let total = plugin.count_relations()?;

    // Burst 1: ensure the service row exists.
    let service_id = {
        let guard = db.lock_recover();
        ensure_service(&guard)?
    };

    let mut sink = ProgressSink {
        inner: BatchSink::new(db, service_id),
        done: 0,
        total,
        on_progress: &mut on_progress,
    };
    let import_res = plugin.import_relations_only(&mut sink);
    // Flush the partial siblings/parents buffers remaining after the stream.
    sink.inner.finish();
    // A flush failure mid-stream aborts the import with a generic message;
    // sink.err holds the underlying db error, so surface it first.
    if let Some(e) = sink.inner.err.take() {
        return Err(PluginError(format!("db sink: {e}")));
    }
    import_res?;
    let (siblings, parents) = (sink.inner.siblings_ct, sink.inner.parents_ct);
    drop(sink);
    on_progress(total, total, siblings, parents);
    Ok(RelationsOutcome { siblings, parents })
}

/// How many `(file_id, tag)` pairs to commit per transaction during a library
/// import. Large enough to amortize per-commit cost, small enough that an
/// interrupted import loses little and progress ticks feel live.
const LIBRARY_IMPORT_BATCH: usize = 512;

/// Run a **library-scoped** Hydrus import: see
/// [`run_library_import_with_progress`]. Convenience wrapper with no progress
/// callback (used by the CLI and tests).
///
/// # Errors
/// Returns an error if the import or any db step fails.
pub fn run_library_import(
    db: &Arc<Mutex<Db>>,
    cfg: &HydrusConfig,
) -> Result<ImportOutcome, PluginError> {
    run_library_import_with_progress(db, cfg, |_, _, _| {})
}

/// Run a library-scoped Hydrus import, reporting progress as it goes.
///
/// Unlike the full [`run_import`] (which stages every Hydrus-owned record then
/// resolves once at the end), this owns its files, so it applies tags **directly,
/// in batches** — the import lands incrementally and a mid-run interruption keeps
/// whatever was already committed. No relations: bounded by library size, the
/// "pull tags for the files I have" path.
///
/// The naiad lock is held only in short bursts: once during sha256 backfill
/// (list query + write bursts), once at startup to load the file list and
/// drain staged rows, and once per batch to commit — Hydrus is queried with
/// no naiad lock held.
///
/// `on_progress(files_done, files_total, tags_applied)` fires after each batch.
///
/// # Errors
/// Returns an error if the import or any db step fails.
pub fn run_library_import_with_progress(
    db: &Arc<Mutex<Db>>,
    cfg: &HydrusConfig,
    mut on_progress: impl FnMut(u64, u64, u64),
) -> Result<ImportOutcome, PluginError> {
    let plugin = hydrus_plugin(cfg)?;

    // Backfill burst(s): snapshot work list, hash off-lock, write in chunks.
    // A fresh skip set per import call: the library import is user-triggered
    // and not called in a rapid loop, so cross-call persistence is not needed.
    let backfilled = backfill_sha256(db, &mut HashSet::new())
        .map_err(|e| PluginError(format!("backfill: {e}")))?;

    // Lock burst A: prepare service, drain staged rows, load files.
    let (service_id, mut applied, files) = {
        let guard = db.lock_recover();
        let service_id = ensure_service(&guard)?;
        let applied = guard
            .resolve_staged_mappings(service_id, "sha256")
            .map_err(|e| PluginError(format!("resolve staged: {e}")))?;
        let files = guard
            .library_files_with_sha256()
            .map_err(|e| PluginError(format!("library files: {e}")))?;
        (service_id, applied, files)
    };
    let total = files.len() as u64;

    let reader = plugin.reader()?;
    let mut done = 0u64;

    for chunk in files.chunks(LIBRARY_IMPORT_BATCH) {
        // No naiad lock held while querying Hydrus.
        let shas: Vec<&str> = chunk.iter().map(|(_, sha)| sha.as_str()).collect();
        let tags_by_sha = reader.batch_tags(&shas)?;

        let mut batch: Vec<(i64, Tag)> = Vec::new();
        for (file_id, sha) in chunk {
            if let Some(tags) = tags_by_sha.get(&sha.to_lowercase()) {
                for tag in tags {
                    batch.push((*file_id, tag.clone()));
                }
            }
        }
        done += chunk.len() as u64;

        // Lock burst B: apply just this batch, then release.
        if !batch.is_empty() {
            let guard = db.lock_recover();
            applied += guard
                .apply_hydrus_mappings(service_id, &batch)
                .map_err(|e| PluginError(format!("apply: {e}")))?;
        }
        on_progress(done, total, applied);
    }
    on_progress(done, total, applied);

    Ok(ImportOutcome {
        mappings_staged: applied,
        mappings_resolved: applied,
        siblings: 0,
        parents: 0,
        sha256_backfilled: backfilled,
    })
}

/// Per-file Hydrus tag lookup. When `apply`, also writes the tags into the service.
///
/// # Errors
/// Returns an error if the lookup or any db step fails.
pub fn lookup(
    db: &Db,
    cfg: &HydrusConfig,
    blake3_hex: &str,
    apply: bool,
) -> Result<Vec<Tag>, PluginError> {
    let plugin = hydrus_plugin(cfg)?;
    let hash: Hash = blake3_hex
        .parse()
        .map_err(|_| PluginError("bad blake3 hex".into()))?;
    let sha256 = db
        .sha256_of(&hash)
        .map_err(|e| PluginError(format!("db: {e}")))?;
    let file = FileRef {
        blake3: blake3_hex.to_string(),
        sha256,
    };
    let tags = plugin.tags_for(&file)?;

    if apply && !tags.is_empty() {
        let service_id = ensure_service(db)?;
        let file_id = db
            .file_id_by_hash(&hash)
            .map_err(|e| PluginError(format!("db: {e}")))?
            .ok_or_else(|| PluginError("unknown file".into()))?;
        for tag in &tags {
            let tag_id = db
                .intern_tag(tag)
                .map_err(|e| PluginError(format!("db: {e}")))?;
            db.add_mapping(file_id, tag_id, service_id)
                .map_err(|e| PluginError(format!("db: {e}")))?;
        }
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use naiad_core::FileRecord;
    use naiad_db::Db;
    use rusqlite::Connection;
    use std::sync::Mutex;

    const SHA_HEX: &str = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";

    fn build_fixture(dir: &std::path::Path) {
        let master = Connection::open(dir.join("client.master.db")).unwrap();
        master
            .execute_batch(
                "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
                 CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
                 CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
                 CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);",
            )
            .unwrap();
        master
            .execute(
                "INSERT INTO hashes (hash_id, hash) VALUES (1, ?1)",
                [hex::decode(SHA_HEX).unwrap()],
            )
            .unwrap();
        master
            .execute_batch(
                "INSERT INTO namespaces VALUES (1, ''), (2, 'character');
                 INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, 'samus_aran');
                 INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 2, 3);",
            )
            .unwrap();

        let client = Connection::open(dir.join("client.db")).unwrap();
        client
            .execute_batch(
                "CREATE TABLE current_files_4 (hash_id INTEGER, timestamp_ms INTEGER);
                 INSERT INTO current_files_4 VALUES (1, 0);
                 CREATE TABLE current_tag_siblings_9 (bad_tag_id INTEGER, good_tag_id INTEGER);
                 INSERT INTO current_tag_siblings_9 VALUES (3, 2);
                 CREATE TABLE current_tag_parents_9 (child_tag_id INTEGER, parent_tag_id INTEGER);
                 INSERT INTO current_tag_parents_9 VALUES (2, 1);",
            )
            .unwrap();

        let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_9 VALUES (1, 1), (2, 1);",
            )
            .unwrap();
    }

    #[test]
    fn run_import_stages_and_resolves_mappings() {
        let dir = tempfile::tempdir().unwrap();
        build_fixture(dir.path());
        let db = Db::open_in_memory().unwrap();

        // We need to find bytes whose SHA-256 == SHA_HEX. We can't easily
        // manufacture those, so we test staging independently:
        // seed the db with any file, then manually set its sha256.
        let (blake, _sha) = naiad_core::hash_reader_dual(&b"dummy"[..]).unwrap();
        let rec = FileRecord::new(blake, std::path::PathBuf::from("dummy.jpg"), 5, None);
        db.insert_file(&rec, 1).unwrap();
        let file_id = db.file_id_by_hash(&blake).unwrap().unwrap();
        db.set_sha256(file_id, SHA_HEX).unwrap();

        let cfg = HydrusConfig {
            dir: Some(dir.path().to_path_buf()),
            tag_services: vec![9],
        };
        let db = Mutex::new(db);
        let outcome = run_import(&db, &cfg).unwrap();
        assert!(outcome.siblings >= 1, "expected at least 1 sibling");
        assert!(outcome.parents >= 1, "expected at least 1 parent");
        assert!(
            outcome.mappings_resolved >= 1,
            "expected at least 1 resolved mapping"
        );
    }

    #[test]
    fn library_import_stages_tags_for_my_files_only() {
        let dir = tempfile::tempdir().unwrap();
        build_fixture(dir.path());
        let db = Db::open_in_memory().unwrap();

        // One library file whose sha256 matches the Hydrus fixture.
        let (blake, _sha) = naiad_core::hash_reader_dual(&b"mine"[..]).unwrap();
        let rec = FileRecord::new(blake, std::path::PathBuf::from("mine.jpg"), 4, None);
        db.insert_file(&rec, 1).unwrap();
        let file_id = db.file_id_by_hash(&blake).unwrap().unwrap();
        db.set_sha256(file_id, SHA_HEX).unwrap();

        let db = std::sync::Arc::new(std::sync::Mutex::new(db));
        let cfg = HydrusConfig {
            dir: Some(dir.path().to_path_buf()),
            tag_services: vec![9],
        };
        let outcome = run_library_import(&db, &cfg).unwrap();
        assert!(outcome.mappings_resolved >= 1, "tags applied to my file");
        assert_eq!(outcome.siblings, 0, "library scope pulls no relations");
        assert_eq!(outcome.parents, 0);
        let tags = db.lock_recover().tags_of(file_id).unwrap();
        assert!(!tags.is_empty(), "my file now carries Hydrus tags");
    }

    #[test]
    fn lookup_returns_tags_for_file_with_sha256() {
        let dir = tempfile::tempdir().unwrap();
        build_fixture(dir.path());
        let db = Db::open_in_memory().unwrap();

        let (blake, _) = naiad_core::hash_reader_dual(&b"dummy2"[..]).unwrap();
        let rec = FileRecord::new(blake, std::path::PathBuf::from("dummy2.jpg"), 6, None);
        db.insert_file(&rec, 1).unwrap();
        let file_id = db.file_id_by_hash(&blake).unwrap().unwrap();
        db.set_sha256(file_id, SHA_HEX).unwrap();

        let cfg = HydrusConfig {
            dir: Some(dir.path().to_path_buf()),
            tag_services: vec![9],
        };
        let tags = lookup(&db, &cfg, &blake.to_hex(), false).unwrap();
        assert!(!tags.is_empty(), "expected tags from Hydrus lookup");
    }

    #[test]
    fn relations_import_pulls_graph_without_mappings() {
        let dir = tempfile::tempdir().unwrap();
        build_fixture(dir.path());
        let db = Db::open_in_memory().unwrap();

        // A file mapped with the bad tag; the pulled sibling must canonicalize it.
        let (blake, _) = naiad_core::hash_reader_dual(&b"rel"[..]).unwrap();
        let rec = FileRecord::new(blake, std::path::PathBuf::from("rel.jpg"), 3, None);
        db.insert_file(&rec, 1).unwrap();
        let file_id = db.file_id_by_hash(&blake).unwrap().unwrap();
        let bad = db
            .intern_tag(&naiad_core::Tag::parse("character:samus_aran").unwrap())
            .unwrap();
        db.add_mapping(file_id, bad, 1).unwrap();

        let cfg = HydrusConfig {
            dir: Some(dir.path().to_path_buf()),
            tag_services: vec![9],
        };
        let db = Mutex::new(db);
        let mut ticks: Vec<(u64, u64)> = Vec::new();
        let outcome = run_relations_import_with_progress(&db, &cfg, |done, total, _s, _p| {
            ticks.push((done, total));
        })
        .unwrap();

        assert_eq!(outcome.siblings, 1);
        assert_eq!(outcome.parents, 1);
        // Fixture: 1 current sibling + 1 current parent = total 2; the final
        // tick always reports (total, total).
        assert_eq!(ticks.last().copied(), Some((2, 2)));
        // No mappings written: the file still carries exactly its one raw tag.
        assert_eq!(db.lock_recover().tags_of(file_id).unwrap().len(), 1);
        // The pulled sibling canonicalizes at display time.
        let display: Vec<String> = db
            .lock_recover()
            .display_tags_of(file_id, naiad_db::ReadScope::Merged)
            .unwrap()
            .into_iter()
            .map(|t| t.tag.to_string())
            .collect();
        assert!(
            display.iter().any(|t| t == "character:samus"),
            "canonical tag shown, got {display:?}"
        );
        assert!(
            display.iter().all(|t| t != "character:samus_aran"),
            "bad tag replaced, got {display:?}"
        );
    }

    #[test]
    fn relations_import_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        build_fixture(dir.path());
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let cfg = HydrusConfig {
            dir: Some(dir.path().to_path_buf()),
            tag_services: vec![9],
        };

        let first = run_relations_import(&db, &cfg).unwrap();
        let second = run_relations_import(&db, &cfg).unwrap();
        assert_eq!((first.siblings, first.parents), (1, 1));
        assert_eq!(
            (second.siblings, second.parents),
            (first.siblings, first.parents),
            "re-run converges: same counts, no duplicates"
        );
    }

    // ── BatchSink unit tests ─────────────────────────────────────────────────

    /// Helper: make a MappingRecord with a given sha256, tag, and status.
    fn mapping_rec(sha256: &str, tag: &str, status: RecordStatus) -> MappingRecord {
        MappingRecord {
            sha256: sha256.to_string(),
            tag: naiad_core::Tag::parse(tag).unwrap(),
            status,
        }
    }

    /// Helper: make a RelationRecord (sibling, current).
    fn sibling_rec(from: &str, to: &str) -> RelationRecord {
        RelationRecord {
            kind: RelationKind::Sibling,
            from: naiad_core::Tag::parse(from).unwrap(),
            to: naiad_core::Tag::parse(to).unwrap(),
            status: RecordStatus::Current,
        }
    }

    /// Helper: make a deleted RelationRecord (sibling, deleted).
    fn deleted_sibling_rec(from: &str, to: &str) -> RelationRecord {
        RelationRecord {
            kind: RelationKind::Sibling,
            from: naiad_core::Tag::parse(from).unwrap(),
            to: naiad_core::Tag::parse(to).unwrap(),
            status: RecordStatus::Deleted,
        }
    }

    /// Seed a naiad file with the given sha256, returning its file_id.
    fn seed_file_with_sha(db: &Db, sha256: &str) -> i64 {
        let seed = sha256.as_bytes();
        let (blake, _) = naiad_core::hash_reader_dual(seed).unwrap();
        let rec = FileRecord::new(blake, std::path::PathBuf::from("test.jpg"), 1, None);
        db.insert_file(&rec, 1).unwrap();
        let file_id = db.file_id_by_hash(&blake).unwrap().unwrap();
        db.set_sha256(file_id, sha256).unwrap();
        file_id
    }

    #[test]
    fn batch_sink_flush_fires_at_boundary() {
        // Verify that pushing exactly IMPORT_WRITE_BATCH records triggers a flush
        // (observable: the buffer is cleared and the count is correct).
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let svc = db
            .lock_recover()
            .add_local_service("bsink-svc", None)
            .unwrap();
        let sha = "aa".repeat(32);

        let mut sink = BatchSink::new(&db, svc);

        // Push IMPORT_WRITE_BATCH - 1 records: buffer not full, no flush yet.
        for i in 0..IMPORT_WRITE_BATCH - 1 {
            let rec = mapping_rec(&sha, &format!("tag{i}"), RecordStatus::Current);
            sink.mapping(rec).unwrap();
        }
        assert_eq!(
            sink.mappings.len(),
            IMPORT_WRITE_BATCH - 1,
            "buffer not yet full"
        );
        assert!(sink.err.is_none(), "no error before flush");

        // One more record crosses the threshold and triggers an auto-flush.
        let rec = mapping_rec(
            &sha,
            &format!("tag{}", IMPORT_WRITE_BATCH - 1),
            RecordStatus::Current,
        );
        sink.mapping(rec).unwrap();

        assert_eq!(sink.mappings.len(), 0, "buffer cleared after auto-flush");
        assert!(sink.err.is_none(), "flush succeeded");
        assert_eq!(
            sink.mappings_ct, IMPORT_WRITE_BATCH as u64,
            "count equals records pushed"
        );
    }

    #[test]
    fn batch_sink_partial_buffers_flush_on_finish() {
        // Verify that finish() flushes fewer-than-IMPORT_WRITE_BATCH records.
        // Observable: buffer cleared + no error.  Correctness of the data in
        // the DB is proved by the later_status_wins test (which uses resolve).
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let svc = db
            .lock_recover()
            .add_local_service("bsink-svc2", None)
            .unwrap();
        let sha = "bb".repeat(32);

        let mut sink = BatchSink::new(&db, svc);

        // Push fewer than IMPORT_WRITE_BATCH records — no auto-flush.
        for i in 0..10 {
            let rec = mapping_rec(&sha, &format!("partial{i}"), RecordStatus::Current);
            sink.mapping(rec).unwrap();
        }
        assert_eq!(sink.mappings.len(), 10, "buffer holds all 10 records");
        assert!(sink.err.is_none());

        // finish() must flush the partial buffer.
        sink.finish();
        assert_eq!(sink.mappings.len(), 0, "buffer cleared after finish()");
        assert!(sink.err.is_none(), "flush succeeded");
        assert_eq!(sink.mappings_ct, 10, "count correct");
    }

    #[test]
    fn batch_sink_later_status_wins_across_flush_boundary() {
        // Prove that a "deleted" record written after a flush boundary overwrites
        // the earlier "current" record via the ON CONFLICT DO UPDATE upsert.
        // Observable: resolve_staged_mappings returns 0 (deleted wins → no
        // mapping applied), and tags_of returns an empty set.
        let db_inner = Db::open_in_memory().unwrap();
        let sha = "cc".repeat(32);
        let file_id = seed_file_with_sha(&db_inner, &sha);
        let db = Mutex::new(db_inner);
        let svc = db
            .lock_recover()
            .add_local_service("bsink-svc3", None)
            .unwrap();

        let mut sink = BatchSink::new(&db, svc);

        // Fill a full batch for the same (sha, tag) with status "current",
        // triggering one auto-flush.
        let tag_name = "winner:tag";
        for _i in 0..IMPORT_WRITE_BATCH {
            sink.mapping(mapping_rec(&sha, tag_name, RecordStatus::Current))
                .unwrap();
        }
        // After the auto-flush, staged_mappings has the row as "current".
        // Push one "deleted" record for the same key in the next partial batch.
        sink.mapping(mapping_rec(&sha, tag_name, RecordStatus::Deleted))
            .unwrap();
        sink.finish();
        assert!(sink.err.is_none(), "no flush error");

        // Resolve drains the staged row.  The return value is the drain count
        // (rows removed from staged_mappings), not the insertion count.
        // To verify "deleted wins", we check tags_of: a "current" final status
        // would have inserted a mapping; "deleted" leaves no mapping.
        db.lock_recover()
            .resolve_staged_mappings(svc, "sha256")
            .unwrap();
        let tags = db.lock_recover().tags_of(file_id).unwrap();
        assert!(tags.is_empty(), "deleted status wins: file carries no tags");
    }

    #[test]
    fn batch_sink_deleted_relation_skipped() {
        // Deleted relations must not enter the buffer at all (mirroring DbSink).
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let svc = db
            .lock_recover()
            .add_local_service("bsink-svc4", None)
            .unwrap();

        let mut sink = BatchSink::new(&db, svc);
        sink.relation(deleted_sibling_rec("bad:tag", "good:tag"))
            .unwrap();

        // Must not enter the buffer.
        assert_eq!(sink.siblings.len(), 0, "Deleted relation not buffered");
        // After finish, count stays 0.
        sink.finish();
        assert!(sink.err.is_none());
        assert_eq!(sink.siblings_ct, 0, "count stays 0 for deleted");
    }

    #[test]
    fn batch_sink_counts_parity() {
        // mappings_ct = per-record count (all records, regardless of sql rows changed).
        // siblings_ct = returned applied count from add_siblings_batch (excludes self).
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let svc = db
            .lock_recover()
            .add_local_service("bsink-svc5", None)
            .unwrap();
        let sha = "dd".repeat(32);

        let mut sink = BatchSink::new(&db, svc);

        // 3 mapping records.
        for i in 0..3 {
            sink.mapping(mapping_rec(
                &sha,
                &format!("count{i}"),
                RecordStatus::Current,
            ))
            .unwrap();
        }
        // 1 valid sibling + 1 self-sibling (same from/to → excluded by batch method).
        sink.relation(sibling_rec("bad:one", "good:one")).unwrap();
        sink.relation(sibling_rec("self:tag", "self:tag")).unwrap();

        sink.finish();
        assert!(sink.err.is_none());

        assert_eq!(sink.mappings_ct, 3, "mapping count = 3 records buffered");
        // add_siblings_batch returns 1 (the non-self sibling only).
        assert_eq!(
            sink.siblings_ct, 1,
            "self-sibling excluded from siblings_ct"
        );
        assert_eq!(sink.parents_ct, 0, "no parents");
    }

    // ── Build scaled fixture + benchmark ────────────────────────────────────

    /// Build a scaled Hydrus fixture for the import benchmark.
    ///
    /// Creates `client.master.db`, `client.db`, `client.mappings.db` in `dir`
    /// with deterministic, counter-derived data at benchmark scale:
    ///  - ~50 k hashes (hash_id 1..=50_000)
    ///  - ~30 k tags   (100 namespaces × 300 subtags)
    ///  - ~200 k current_mappings (4 per file)
    ///  - ~10 k current_tag_siblings + ~10 k current_tag_parents
    fn build_bench_fixture(dir: &std::path::Path) {
        const N_HASHES: u64 = 50_000;
        const N_NAMESPACES: u64 = 100;
        const N_SUBTAGS: u64 = 300;
        const N_TAGS: u64 = N_NAMESPACES * N_SUBTAGS; // 30 000
        const N_MAPPINGS_PER_FILE: u64 = 4;
        const N_SIBLINGS: u64 = 10_000;
        const N_PARENTS: u64 = 10_000;

        /// Convert counter `i` to a 32-byte deterministic hash blob.
        /// Bytes 0..8 = `i` as little-endian u64; bytes 8..32 = zero.
        fn hash_blob(i: u64) -> Vec<u8> {
            let mut b = [0u8; 32];
            b[..8].copy_from_slice(&i.to_le_bytes());
            b.to_vec()
        }

        // ── client.master.db ────────────────────────────────────────────────
        {
            let m = Connection::open(dir.join("client.master.db")).unwrap();
            m.execute_batch(
                "PRAGMA synchronous=OFF; PRAGMA journal_mode=OFF;
                 CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
                 CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
                 CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
                 CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);",
            )
            .unwrap();

            // hashes (one transaction)
            m.execute_batch("BEGIN;").unwrap();
            {
                let mut stmt = m
                    .prepare("INSERT INTO hashes (hash_id, hash) VALUES (?1, ?2)")
                    .unwrap();
                for i in 1..=N_HASHES {
                    stmt.execute(rusqlite::params![i as i64, hash_blob(i)])
                        .unwrap();
                }
            }
            m.execute_batch("COMMIT;").unwrap();

            // namespaces: id 1 = "" (unnamespaced), id 2..=N_NAMESPACES = "ns{j}"
            m.execute_batch("BEGIN;").unwrap();
            {
                let mut stmt = m
                    .prepare("INSERT INTO namespaces (namespace_id, namespace) VALUES (?1, ?2)")
                    .unwrap();
                stmt.execute(rusqlite::params![1i64, ""]).unwrap();
                for j in 2..=N_NAMESPACES {
                    stmt.execute(rusqlite::params![j as i64, format!("ns{j}")])
                        .unwrap();
                }
            }
            m.execute_batch("COMMIT;").unwrap();

            // subtags: "tag{k}" for k in 1..=N_SUBTAGS
            m.execute_batch("BEGIN;").unwrap();
            {
                let mut stmt = m
                    .prepare("INSERT INTO subtags (subtag_id, subtag) VALUES (?1, ?2)")
                    .unwrap();
                for k in 1..=N_SUBTAGS {
                    stmt.execute(rusqlite::params![k as i64, format!("tag{k}")])
                        .unwrap();
                }
            }
            m.execute_batch("COMMIT;").unwrap();

            // tags: tag_id = (j-1)*N_SUBTAGS + k, namespace_id=j, subtag_id=k
            m.execute_batch("BEGIN;").unwrap();
            {
                let mut stmt = m
                    .prepare(
                        "INSERT INTO tags (tag_id, namespace_id, subtag_id) VALUES (?1, ?2, ?3)",
                    )
                    .unwrap();
                for j in 1..=N_NAMESPACES {
                    for k in 1..=N_SUBTAGS {
                        let tag_id = (j - 1) * N_SUBTAGS + k;
                        stmt.execute(rusqlite::params![tag_id as i64, j as i64, k as i64])
                            .unwrap();
                    }
                }
            }
            m.execute_batch("COMMIT;").unwrap();
        }

        // ── client.db ───────────────────────────────────────────────────────
        // current_files_4 (file service = DEFAULT_FILE_SERVICE = 4)
        // current_tag_siblings_9 / current_tag_parents_9 (tag service 9)
        {
            let c = Connection::open(dir.join("client.db")).unwrap();
            c.execute_batch(
                "PRAGMA synchronous=OFF; PRAGMA journal_mode=OFF;
                 CREATE TABLE current_files_4 (hash_id INTEGER, timestamp_ms INTEGER);
                 CREATE TABLE current_tag_siblings_9 (bad_tag_id INTEGER, good_tag_id INTEGER);
                 CREATE TABLE current_tag_parents_9 (child_tag_id INTEGER, parent_tag_id INTEGER);",
            )
            .unwrap();

            // current_files_4: all N_HASHES files are in the file service
            c.execute_batch("BEGIN;").unwrap();
            {
                let mut stmt = c
                    .prepare("INSERT INTO current_files_4 (hash_id, timestamp_ms) VALUES (?1, 0)")
                    .unwrap();
                for i in 1..=N_HASHES {
                    stmt.execute(rusqlite::params![i as i64]).unwrap();
                }
            }
            c.execute_batch("COMMIT;").unwrap();

            // siblings: bad=i (1..=10_000), good=i+15_000 (15_001..=25_000), no self-relations
            c.execute_batch("BEGIN;").unwrap();
            {
                let mut stmt = c
                    .prepare(
                        "INSERT INTO current_tag_siblings_9 (bad_tag_id, good_tag_id) VALUES (?1, ?2)",
                    )
                    .unwrap();
                for i in 1..=N_SIBLINGS {
                    stmt.execute(rusqlite::params![i as i64, (i + 15_000) as i64])
                        .unwrap();
                }
            }
            c.execute_batch("COMMIT;").unwrap();

            // parents: child=i+20_000 (20_001..=30_000), parent=i (1..=10_000)
            c.execute_batch("BEGIN;").unwrap();
            {
                let mut stmt = c
                    .prepare(
                        "INSERT INTO current_tag_parents_9 (child_tag_id, parent_tag_id) VALUES (?1, ?2)",
                    )
                    .unwrap();
                for i in 1..=N_PARENTS {
                    stmt.execute(rusqlite::params![(i + 20_000) as i64, i as i64])
                        .unwrap();
                }
            }
            c.execute_batch("COMMIT;").unwrap();
        }

        // ── client.mappings.db ──────────────────────────────────────────────
        // 4 mappings per file → N_HASHES * 4 = 200 000 rows
        {
            let mp = Connection::open(dir.join("client.mappings.db")).unwrap();
            mp.execute_batch(
                "PRAGMA synchronous=OFF; PRAGMA journal_mode=OFF;
                 CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);",
            )
            .unwrap();

            mp.execute_batch("BEGIN;").unwrap();
            {
                let mut stmt = mp
                    .prepare("INSERT INTO current_mappings_9 (tag_id, hash_id) VALUES (?1, ?2)")
                    .unwrap();
                for h in 1..=N_HASHES {
                    for j in 0..N_MAPPINGS_PER_FILE {
                        let tag_id = (h * 7 + j) % N_TAGS + 1;
                        stmt.execute(rusqlite::params![tag_id as i64, h as i64])
                            .unwrap();
                    }
                }
            }
            mp.execute_batch("COMMIT;").unwrap();
        }
    }

    /// Benchmark harness measuring `run_import` performance after the
    /// batch-and-release refactor.
    ///
    /// Run with:
    /// ```
    /// cargo test -p naiad-daemon --release full_import_benchmark -- --ignored --nocapture
    /// ```
    ///
    /// Prints wall time, records/sec, and competing-writer lock-wait (max / p99).
    /// After the refactor (batch-per-flush locking) the max lock-wait should
    /// collapse to a single flush duration (single-digit ms), proving lock
    /// fairness.
    #[test]
    #[ignore]
    fn full_import_benchmark() {
        use crate::lock::LockRecover;
        use naiad_core::{FileRecord, Hash, Tag};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        const N_HASHES: u64 = 50_000;
        const N_MAPPINGS_PER_FILE: u64 = 4;

        // ── Build Hydrus fixture ────────────────────────────────────────────
        let fixture_dir = tempfile::tempdir().unwrap();
        eprintln!(
            "[bench] building Hydrus fixture ({N_HASHES} hashes, 30k tags, {} mappings, \
             10k siblings, 10k parents)...",
            N_HASHES * N_MAPPINGS_PER_FILE
        );
        build_bench_fixture(fixture_dir.path());
        eprintln!("[bench] Hydrus fixture built.");

        // ── Seed naiad Db (on-disk, WAL, as in production) ─────────────────
        let naiad_dir = tempfile::tempdir().unwrap();
        let db_path = naiad_dir.path().join("naiad.db");
        let db = Db::open(&db_path).unwrap();

        eprintln!("[bench] seeding {N_HASHES} files into naiad Db (one transaction)...");
        db.with_tx(|db| {
            for i in 1u64..=N_HASHES {
                // Deterministic Blake3-shaped hash: bytes 0..8 = i (LE), rest = 0.
                let mut bytes = [0u8; 32];
                bytes[..8].copy_from_slice(&i.to_le_bytes());
                let blake = Hash::from_bytes(bytes);

                // SHA-256 hex = lowercase hex of the same 32-byte blob.
                let sha256_hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

                let path = std::path::PathBuf::from(format!("/bench/{i}.jpg"));
                let rec = FileRecord::new(blake, path, 1024, None);
                db.insert_file(&rec, 1)?;
                let file_id = db
                    .file_id_by_hash(&blake)?
                    .ok_or_else(|| naiad_db::Error::NotFound(format!("bench file {i}")))?;
                db.set_sha256(file_id, &sha256_hex)?;
            }
            Ok(())
        })
        .unwrap();
        eprintln!("[bench] naiad Db seeded.");

        // ── Config ─────────────────────────────────────────────────────────
        let cfg = HydrusConfig {
            dir: Some(fixture_dir.path().to_path_buf()),
            tag_services: vec![9],
        };

        let db_arc: Arc<Mutex<Db>> = Arc::new(Mutex::new(db));

        // ── Competing-writer thread ─────────────────────────────────────────
        // Simulates interactive writes during a long import (lock-fairness probe).
        // Each iteration: time how long it takes to acquire the DB lock, then do
        // a tiny write (intern a throwaway tag) and release.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let db_clone = Arc::clone(&db_arc);

        let writer_handle = std::thread::spawn(move || {
            let writer_tag = Tag::parse("bench:writer").unwrap();
            let mut waits: Vec<Duration> = Vec::new();
            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                let t = Instant::now();
                let g = db_clone.lock_recover();
                waits.push(t.elapsed());
                g.intern_tag(&writer_tag).ok();
                drop(g);
                std::thread::sleep(Duration::from_millis(1));
            }
            waits
        });

        // ── Import (batch-per-flush, no pre-acquired lock) ──────────────────
        //
        // Give the competing-writer thread ~50 ms to start before timing begins.
        // run_import takes &Mutex<Db> and locks only in short bursts, so the
        // writer thread can interleave freely — max lock-wait should be O(ms).
        eprintln!("[bench] starting run_import (AFTER BATCHING)...");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let wall_start = Instant::now();
        run_import(&db_arc, &cfg).expect("run_import failed");
        let wall_time = wall_start.elapsed();

        // Signal the competing writer to stop.
        stop.store(true, Ordering::Relaxed);
        let waits = writer_handle.join().unwrap();

        // ── Compute stats ──────────────────────────────────────────────────
        let total_records = N_HASHES * N_MAPPINGS_PER_FILE; // mappings only (relations separate)
        let records_per_sec = if wall_time.as_secs_f64() > 0.0 {
            total_records as f64 / wall_time.as_secs_f64()
        } else {
            f64::INFINITY
        };

        let (max_wait, p99_wait, n_samples) = if waits.is_empty() {
            (Duration::ZERO, Duration::ZERO, 0usize)
        } else {
            let mut sorted = waits.clone();
            sorted.sort();
            let max = *sorted.last().unwrap();
            let p99_idx = ((sorted.len() as f64 * 0.99) as usize)
                .saturating_sub(1)
                .min(sorted.len() - 1);
            let p99 = sorted[p99_idx];
            (max, p99, sorted.len())
        };

        eprintln!();
        eprintln!("=== AFTER BATCHING ===");
        eprintln!("  wall time:          {wall_time:.2?}");
        eprintln!("  throughput:         {records_per_sec:.0} records/sec  (mapping records only)");
        eprintln!("  lock-wait samples:  {n_samples}");
        eprintln!("  max lock-wait:      {max_wait:.2?}");
        eprintln!("  p99 lock-wait:      {p99_wait:.2?}");
        eprintln!("======================");

        // ── Coarse regression guards ────────────────────────────────────────
        // Measured on 2026-07-05 (Windows 10, release build):
        //   wall 11.54 s, 17 337 rec/s, max-wait 3.33 s, p99 126 ms, 1 217 samples
        // Baseline (pre-batching, same fixture, same machine):
        //   wall 199.89 s, ~1 001 rec/s, max-wait 199.94 s, p99 199.94 s, 1 sample
        //
        // Bounds are set to ~5× measured wall time and ~10× measured lock-wait
        // so that normal run-to-run variance and slower CI machines won't
        // false-fire, while still catching a revert to the pre-batching
        // behaviour (which would breach every bound below by ~20×).

        // Throughput must beat 3 000 rec/s (≈ ⅙ of measured; pre-batching: ~1 001).
        assert!(
            records_per_sec > 3_000.0,
            "throughput {records_per_sec:.0} rec/s below 3 000 floor \
             (pre-batching was ~1 001 rec/s)"
        );

        // Wall time must stay under 60 s (≈ 5× measured 11.54 s;
        // pre-batching was 199.89 s).
        assert!(
            wall_time < Duration::from_secs(60),
            "wall time {wall_time:.2?} exceeded 60 s ceiling \
             (pre-batching was ~200 s)"
        );

        // p99 lock-wait must stay under 2 000 ms (≈ 16× measured 126 ms;
        // pre-batching p99 was 199 940 ms).
        assert!(
            p99_wait < Duration::from_millis(2_000),
            "p99 lock-wait {p99_wait:.2?} exceeded 2 000 ms ceiling \
             (pre-batching was ~200 s)"
        );

        // max lock-wait must stay under 30 s (≈ 9× measured 3.33 s;
        // pre-batching max was 199.94 s — a 6× margin still proves batching).
        assert!(
            max_wait < Duration::from_secs(30),
            "max lock-wait {max_wait:.2?} exceeded 30 s ceiling \
             (pre-batching was ~200 s)"
        );

        // Competing writer must have collected enough samples to be a valid signal.
        assert!(
            n_samples > 50,
            "only {n_samples} lock-wait samples — competing writer may not have run"
        );
    }

    #[test]
    fn backfill_sets_sha256_from_real_bytes_on_disk() {
        let bytes = b"backfill-me-real-bytes";
        let (blake, expected_sha) = naiad_core::hash_reader_dual(&bytes[..]).unwrap();

        // Write the bytes to a real path the file's location will point at.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("real.jpg");
        std::fs::write(&path, bytes).unwrap();

        // Insert WITHOUT sha256, with the location pointing at the on-disk file.
        let db = Db::open_in_memory().unwrap();
        let rec = FileRecord::new(blake, path.clone(), bytes.len() as u64, None);
        db.insert_file(&rec, 1).unwrap();

        // Precondition: the file is missing its sha256.
        assert_eq!(db.sha256_of(&blake).unwrap(), None);
        assert_eq!(db.files_missing_sha256().unwrap().len(), 1);

        // Backfill reads the real bytes and stores the computed sha256.
        let db = Mutex::new(db);
        let n = backfill_sha256(&db, &mut HashSet::new()).unwrap();
        assert_eq!(n, 1, "exactly one file backfilled");
        let guard = db.lock_recover();
        assert_eq!(
            guard.sha256_of(&blake).unwrap(),
            Some(expected_sha),
            "stored sha256 must equal hash_reader_dual over the same bytes"
        );
        assert!(
            guard.files_missing_sha256().unwrap().is_empty(),
            "no files should remain missing sha256"
        );
    }
}
