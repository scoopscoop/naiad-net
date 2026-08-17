//! Seed the bridge repo from an offline Hydrus snapshot.
//!
//! Three resumable phases, each gated by a flag in `sync_state`:
//! 1. **mappings** — stream all current + deleted PTR mappings into the repo.
//! 2. **defs** — import the service-local id -> sha256 / id -> tag maps.
//! 3. **cursor** — recover and store the Hydrus update watermark.
//!
//! Re-running after an interruption skips any phase already marked `"done"`.
//! A crash **within** the mappings phase is handled per source format (#182):
//! Format-A (hash_id-ordered) passes checkpoint every chunk flush and resume
//! from the last fully-ingested hash; the Format-B/indexed path re-runs the
//! phase from scratch (upsert-safe, but a full PTR ingest is multi-hour).

use std::path::Path;
use std::time::Instant;

use anyhow::Context as _;
use naiad_netproto::{Account, Op, RelKind, RelationSubmission};
use naiad_plugin::{MappingRecord, PluginError, RecordStatus, RelationKind, RelationRecord, Sink};
use naiad_plugin_hydrus::{HydrusDb, HydrusPlugin};

use crate::InternCaches;
use crate::RepoStore;
use crate::bridge::state::StateDb;
use crate::store::{SeedCheckpoint, SeedPass};

/// Chunk size for the relations backfill (#225). Relation volume is tiny versus
/// mappings (ADR 0002), so a modest buffer amortises the transaction overhead
/// without holding a large working set.
const RELATIONS_CHUNK: usize = 10_000;

/// A [`Sink`] that turns each streamed Hydrus [`RelationRecord`] into a signed
/// bridge [`RelationSubmission`] and applies them to the serving `RepoStore` in
/// chunks via `apply_bridge_relations` (LWW + no-op guard, so re-running a seed
/// restates existing edges as no-ops and does not churn `seq`, #225 §6).
///
/// `import_relations_only` streams current rows before deleted rows, so an edge
/// Hydrus lists as both nets to its final state under LWW.
struct BridgeRelationSink<'a> {
    repo: &'a RepoStore,
    bridge: &'a Account,
    buf: Vec<RelationSubmission>,
    /// Rows signed and buffered (i.e. streamed from the snapshot).
    streamed: u64,
    /// Rows whose stored status actually changed (returned by the store).
    applied: u64,
}

impl BridgeRelationSink<'_> {
    /// Flush the buffered submissions to the store, accumulating the changed-row
    /// count. A no-op on an empty buffer.
    fn flush(&mut self) -> anyhow::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.applied += self.repo.apply_bridge_relations(&self.buf)?;
        self.buf.clear();
        Ok(())
    }
}

impl Sink for BridgeRelationSink<'_> {
    // `import_relations_only` never feeds mappings; this arm is unreachable but
    // required by the trait.
    fn mapping(&mut self, _rec: MappingRecord) -> naiad_plugin::Result<()> {
        Ok(())
    }

    fn relation(&mut self, rec: RelationRecord) -> naiad_plugin::Result<()> {
        let op = match rec.status {
            RecordStatus::Current => Op::Add,
            RecordStatus::Deleted => Op::Remove,
        };
        let kind = match rec.kind {
            RelationKind::Sibling => RelKind::Sibling,
            RelationKind::Parent => RelKind::Parent,
        };
        self.buf
            .push(self.bridge.sign_relation(op, kind, &rec.from, &rec.to));
        self.streamed += 1;
        if self.buf.len() >= RELATIONS_CHUNK {
            self.flush()
                .map_err(|e| PluginError(format!("applying bridge relations: {e:#}")))?;
        }
        Ok(())
    }
}

/// Chunk size for the seed phase: larger than the sync default (50 k) to
/// amortise transaction overhead during the trusted bulk load.  Sync keeps its
/// own 50 k constant; this one is seed-only (spec §7.2, OQ-3).
const CHUNK_SIZE: usize = 250_000;
const LOG_EVERY: u64 = 100_000;

/// Run all three seed phases.
///
/// When `rebuild` is `true`, clears `repo_mappings` and `repo_hashes` in one
/// transaction, resets the `seed_phase_mappings` flag so phase 1 re-runs,
/// then at the end replays the `submissions` log on top and mints a new
/// store-generation id. This is the rebuild-in-place path (`--rebuild`).
///
/// When `rebuild` is `false` (normal seed), the generation is minted at
/// successful completion only if none exists yet — every seeded store should
/// carry one from birth. A plain resume (all phases already done) does NOT
/// replace an existing generation.
///
/// # Errors
/// Returns an error if any phase fails (DB errors, snapshot missing, etc.).
pub fn run(
    snapshot_dir: &Path,
    service_id: Option<i64>,
    repo: &RepoStore,
    state: &StateDb,
    bridge: &Account,
    rebuild: bool,
) -> anyhow::Result<()> {
    let hydrus = HydrusDb::open(snapshot_dir)?;

    // Resolve service id: explicit override, or auto-discover from snapshot.
    let svc = match service_id {
        Some(id) => id,
        None => {
            let ids = hydrus.repository_service_ids()?;
            match ids.as_slice() {
                [] => anyhow::bail!("no repository_updates_* tables in snapshot"),
                [id] => *id,
                ids => anyhow::bail!("multiple repository services {ids:?}; pass --service-id"),
            }
        }
    };
    tracing::info!(svc, "seed: resolved service id");

    // ── Guard against an interrupted rebuild ─────────────────────────────────
    // A crash mid-rebuild leaves `rebuild_in_progress = 1` in repo_meta. A
    // plain `bridge seed` on a half-rebuilt store would resume from phase-done
    // flags, skip phase 1, and never replay submissions — silent corruption.
    // Bail early with an actionable error so the operator must finish with
    // `--rebuild` before resuming normal operation.
    // If --rebuild is set we fall through and redo the full rebuild, which
    // re-sets the marker, completes, and clears it at the end.
    if repo
        .rebuild_in_progress()
        .context("checking rebuild_in_progress marker")?
        && !rebuild
    {
        anyhow::bail!(
            "a previous `bridge seed --rebuild` was interrupted; \
             re-run with `--rebuild` to finish the rebuild and replay local submissions"
        );
    }

    // ── Snapshot fingerprint + checkpoint read/validate (§7.1) ───────────────
    let fp = snapshot_fingerprint(snapshot_dir, &hydrus, svc)
        .context("computing snapshot fingerprint")?;
    let raw_ckpt = repo
        .read_seed_checkpoint()
        .context("reading seed checkpoint")?;
    // Consume a checkpoint only when BOTH service_id AND fingerprint match (I5).
    let mut valid_ckpt = raw_ckpt
        .clone()
        .filter(|c| c.service_id == svc && c.fp == fp);

    // ── Rebuild-in-place pre-processing (§7.2) ───────────────────────────────
    if rebuild {
        if repo
            .rebuild_in_progress()
            .context("--rebuild: checking rebuild_in_progress marker")?
            && valid_ckpt.is_some()
        {
            // I8 / X8: an interrupted --rebuild whose checkpoint still matches this
            // snapshot. SKIP the destructive clear; keep the marker set; fall
            // through to phase 1, which resumes from the checkpoint. MOTIVATING SCENARIO.
            tracing::info!(
                "seed: --rebuild: valid checkpoint present — resuming interrupted rebuild"
            );
        } else {
            // Full clear (as today) + clear the checkpoint so a fresh rebuild
            // against a NEW snapshot is not blocked by a stale one (decision 5).
            repo.set_rebuild_in_progress()
                .context("--rebuild: setting rebuild_in_progress marker")?;
            tracing::info!("seed: --rebuild: clearing repo_mappings and repo_hashes");
            repo.clear_mirrored_mappings()
                .context("--rebuild: clearing mirrored mappings")?;
            state
                .reset_seed_phase_mappings()
                .context("--rebuild: resetting seed_phase_mappings flag")?;
            repo.clear_seed_checkpoint()
                .context("--rebuild: clearing stale seed checkpoint")?;
            valid_ckpt = None; // store cleared — nothing to resume
            tracing::info!("seed: --rebuild: mapping state cleared — re-running phase 1");
        }
    }

    // ── Phase 1: mappings ────────────────────────────────────────────────────
    if state.get_flag("seed_phase_mappings")?.as_deref() == Some("done") {
        tracing::info!("seed: phase mappings already done, skipping");
    } else {
        tracing::info!(svc, "seed: phase mappings starting");

        // Shared state for all paths — caches and seq are threaded across all
        // chunks whether we run the deferred path or the indexed path.
        let mut caches = InternCaches::default();
        let mut seq: i64 = repo.mapping_cursor()? as i64;
        let mut total: u64 = 0;

        // §7.3: a plain seed against a mismatched checkpoint is a hard error.
        // (The rebuild path can never reach here with a stale checkpoint — §7.2
        // either resumed it or cleared it.)
        if !rebuild && raw_ckpt.is_some() && valid_ckpt.is_none() {
            anyhow::bail!(
                "bridge seed: the phase-1 checkpoint was written against a different \
                 Hydrus snapshot (fingerprint mismatch). Point --snapshot at the original \
                 snapshot to resume, or delete the repo store and re-seed from scratch, or \
                 run with --rebuild to discard progress and rebuild against this snapshot."
            );
        }

        // §7.4: choose the phase-1 plan.
        enum Plan {
            ResumeCurrent { start_after: u64 },
            RunDeleted { start_after: Option<u64> },
            FreshDeferred,
            SelfHealThenIndexed,
            Indexed,
        }
        let index_present = repo.has_hash_unique_index()?;
        let plan = match &valid_ckpt {
            Some(ck) if ck.pass == SeedPass::Current && !index_present => {
                Plan::ResumeCurrent {
                    start_after: ck.high_water,
                } // I4: append resume
            }
            Some(ck) if ck.pass == SeedPass::Current => {
                // index present => current pass complete + indexed: post-build crash (X3)
                Plan::RunDeleted { start_after: None }
            }
            Some(ck) => {
                // ck.pass == Deleted
                Plan::RunDeleted {
                    start_after: Some(ck.high_water),
                }
            }
            None => {
                if !index_present {
                    Plan::SelfHealThenIndexed // §4.5 legacy self-heal, UNCHANGED
                } else {
                    let hash_count: i64 =
                        repo.conn
                            .query_row("SELECT COUNT(*) FROM repo_hashes", [], |r| r.get(0))?;
                    if hash_count == 0 && hydrus.mappings_hash_ordered(svc)? {
                        Plan::FreshDeferred
                    } else {
                        Plan::Indexed // Format-B / C, UNCHANGED (I10)
                    }
                }
            }
        };

        // The Format-B WARN still fires whenever the chosen plan is Indexed under --rebuild.
        if rebuild && matches!(plan, Plan::Indexed) {
            tracing::warn!(
                "seed: --rebuild source is NOT hash_id-indexed (Format-B); mappings will be \
                 ingested in source order: seed throughput becomes write-random and seq-ordered \
                 delta reads lose locality (#203). A hash_id-indexed (Format-A) snapshot is \
                 required for ordered ingest."
            );
        }

        // §7.5: execute the plan.
        match plan {
            Plan::FreshDeferred => {
                tracing::info!(
                    "seed: fresh hash_id-ordered (Format-A) source — using deferred index path"
                );
                repo.drop_hash_unique_index()
                    .context("drop_hash_unique_index")?;
                tracing::info!("seed: hash uniqueness index dropped — streaming current pass");
                run_pass(
                    repo,
                    &hydrus,
                    svc,
                    SeedPass::Current,
                    true,
                    None,
                    &fp,
                    &mut caches,
                    &mut seq,
                    &mut total,
                )?;
                tracing::info!(
                    total,
                    "seed: current pass complete — building hash uniqueness index"
                );
                repo.build_hash_unique_index()
                    .context("build_hash_unique_index after deferred current pass")?;
                tracing::info!("seed: hash uniqueness index built — streaming deleted pass");
                run_pass(
                    repo,
                    &hydrus,
                    svc,
                    SeedPass::Deleted,
                    false,
                    None,
                    &fp,
                    &mut caches,
                    &mut seq,
                    &mut total,
                )?;
            }
            Plan::ResumeCurrent { start_after } => {
                // Index already absent — do NOT drop again (I4).
                tracing::info!(start_after, "seed: resuming current pass from checkpoint");
                run_pass(
                    repo,
                    &hydrus,
                    svc,
                    SeedPass::Current,
                    true,
                    Some(start_after),
                    &fp,
                    &mut caches,
                    &mut seq,
                    &mut total,
                )?;
                tracing::info!(
                    total,
                    "seed: resumed current pass complete — building hash uniqueness index"
                );
                repo.build_hash_unique_index()
                    .context("build_hash_unique_index after resumed current pass")?;
                run_pass(
                    repo,
                    &hydrus,
                    svc,
                    SeedPass::Deleted,
                    false,
                    None,
                    &fp,
                    &mut caches,
                    &mut seq,
                    &mut total,
                )?;
            }
            Plan::RunDeleted { start_after } => {
                // Current pass already complete + index built; do NOT touch the index.
                tracing::info!(
                    ?start_after,
                    "seed: current pass done — running deleted pass"
                );
                run_pass(
                    repo,
                    &hydrus,
                    svc,
                    SeedPass::Deleted,
                    false,
                    start_after,
                    &fp,
                    &mut caches,
                    &mut seq,
                    &mut total,
                )?;
            }
            Plan::SelfHealThenIndexed => {
                tracing::info!(
                    "seed: repo_hashes uniqueness index absent — self-healing before indexed path"
                );
                repo.build_hash_unique_index().context(
                    "self-heal: failed to rebuild repo_hashes uniqueness index — \
                     this usually means duplicate hashes are present, which can \
                     happen if --unsafe-fast was used during a prior seed; \
                     delete repo.db and re-seed from scratch",
                )?;
                run_indexed_all(repo, &hydrus, svc, &mut caches, &mut seq, &mut total)?;
            }
            Plan::Indexed => {
                run_indexed_all(repo, &hydrus, svc, &mut caches, &mut seq, &mut total)?;
            }
        }

        // I6: clear the checkpoint (repo.db) BEFORE marking the phase done (state.db).
        // A crash in this two-statement window leaves no checkpoint and an un-done
        // flag => the next run finds valid_ckpt=None, index present, hash_count>0 =>
        // Indexed => a safe idempotent upsert re-ingest (crash matrix X6).
        repo.clear_seed_checkpoint()
            .context("clearing seed checkpoint on phase-1 completion")?;
        tracing::info!(total, "seed: phase mappings complete");
        state.set_flag("seed_phase_mappings", "done")?;
    }

    // ── Phase 2: defs ────────────────────────────────────────────────────────
    if state.get_flag("seed_phase_defs")?.as_deref() == Some("done") {
        tracing::info!("seed: phase defs already done, skipping");
    } else {
        tracing::info!(svc, "seed: phase defs starting");

        // Hash map: stream in CHUNK_SIZE batches to avoid loading ~54M rows into RAM.
        let mut hash_buf: Vec<(u64, String)> = Vec::with_capacity(CHUNK_SIZE);
        let mut hash_flush_err: Option<anyhow::Error> = None;
        let mut hash_total: u64 = 0;

        let hash_stream = hydrus.stream_ptr_hash_id_map(svc, &mut |id, sha| {
            if hash_flush_err.is_some() {
                return false;
            }
            hash_buf.push((id, sha.to_string()));
            hash_total += 1;
            if hash_buf.len() >= CHUNK_SIZE {
                if let Err(e) = state.insert_defs_hashes(&std::mem::take(&mut hash_buf)) {
                    hash_flush_err = Some(e);
                    return false;
                }
            }
            true
        });
        if let Some(e) = hash_flush_err {
            return Err(e.context("inserting defs_hashes chunk"));
        }
        hash_stream?;
        if !hash_buf.is_empty() {
            state
                .insert_defs_hashes(&hash_buf)
                .context("inserting final defs_hashes chunk")?;
        }
        tracing::info!(hash_total, "seed: defs hashes imported");

        // Tag map: ~53M rows at full PTR scale (measured 52,996,539 on the
        // 2026-07 snapshot; 2-3+ GB collected). Tolerated for the retiring
        // mirror seed; the #207 sidecar seed must stream instead (spec F5).
        let tags = hydrus.repository_tag_id_map(svc)?;
        tracing::info!(tag_count = tags.len(), "seed: importing tag id map");
        state.insert_defs_tags(&tags)?;

        tracing::info!("seed: phase defs complete");
        state.set_flag("seed_phase_defs", "done")?;
    }

    // ── Phase 3: cursor ──────────────────────────────────────────────────────
    if state.get_flag("seed_phase_cursor")?.as_deref() == Some("done") {
        tracing::info!("seed: phase cursor already done, skipping");
    } else {
        tracing::info!(svc, "seed: phase cursor starting");
        let watermark = hydrus.recover_watermark(svc)?;
        let next_idx = watermark.map(|w| w + 1).unwrap_or(0);
        tracing::info!(watermark = ?watermark, next_idx, "seed: storing update cursor");
        state.set_next_update_index(next_idx)?;
        state.set_flag("seed_phase_cursor", "done")?;
    }

    // ── Phase 4: relations (#225) ────────────────────────────────────────────
    // Backfill the full sibling/parent graph from the snapshot into the serving
    // RepoStore's relations table, signed by the bridge author. Reuses
    // plugin-hydrus's relation reader (which resolves ids to parsed Tags itself)
    // rather than duplicating the SQL. Gated by its own flag so a resumed seed
    // skips a completed relations phase; the per-row no-op guard makes even a
    // re-run without the flag correct (just slower).
    if state.get_flag("seed_phase_relations")?.as_deref() == Some("done") {
        tracing::info!("seed: phase relations already done, skipping");
    } else {
        tracing::info!(svc, "seed: phase relations starting");
        let plugin = HydrusPlugin::new(snapshot_dir.to_path_buf(), vec![svc]);
        let mut sink = BridgeRelationSink {
            repo,
            bridge,
            buf: Vec::new(),
            streamed: 0,
            applied: 0,
        };
        let stats = plugin
            .import_relations_only(&mut sink)
            .context("streaming snapshot relations")?;
        sink.flush().context("flushing final relation chunk")?;
        tracing::info!(
            target: "bridge",
            streamed = sink.streamed,
            applied = sink.applied,
            siblings = stats.siblings,
            parents = stats.parents,
            "seed: phase relations complete"
        );
        state.set_flag("seed_phase_relations", "done")?;
    }

    // #203: refresh planner stats after the mapping phases, before the meta
    // writes, so a freshly (re-)seeded store serves buckets with a good plan.
    repo.analyze()
        .context("running ANALYZE after seed phases")?;

    // ── Post-seed: replay submissions + mint/refresh store generation ────────
    if rebuild {
        // Replay the submissions log on top of the freshly re-seeded mappings.
        // This restores local signed contributions (origin=local) that were in
        // the store before the rebuild — their effect is not in the PTR data.
        let replayed = repo
            .replay_submissions()
            .context("--rebuild: replaying submissions")?;
        tracing::info!(replayed, "seed: --rebuild: submissions replayed");

        // Mint a NEW generation id. The in-progress marker is cleared
        // immediately after as the very last step — a crash before this leaves
        // the marker set, and the next `bridge seed` will refuse to proceed
        // without `--rebuild`, preventing silent cursor corruption.
        let new_gen = repo
            .mint_store_generation()
            .context("--rebuild: minting store generation")?;
        tracing::info!(generation = %new_gen, "seed: --rebuild: new store generation minted");

        // Persist the distinct-hash count (#202). One scan at the end of an
        // offline rebuild, never on the request path. Written after the mint and
        // before the marker clear so a crash here leaves rebuild_in_progress set.
        let count = repo
            .distinct_hash_count()
            .context("--rebuild: computing distinct hash count")?;
        repo.write_distinct_hash_count(count)
            .context("--rebuild: persisting distinct hash count")?;
        tracing::info!(count, "seed: --rebuild: distinct hash count persisted");

        repo.clear_rebuild_in_progress()
            .context("--rebuild: clearing rebuild_in_progress marker")?;
        tracing::info!("seed: --rebuild: complete (in-progress marker cleared)");
    } else {
        // Normal seed (not rebuild): every freshly seeded store should carry
        // a generation from birth so clients that connect for the first time
        // get one. Only mint when absent; a plain resume keeps the existing id.
        if repo
            .store_generation()
            .context("checking store generation")?
            .is_none()
        {
            let new_gen = repo
                .mint_store_generation()
                .context("minting initial store generation")?;
            tracing::info!(generation = %new_gen, "seed: minted initial store generation");
        }

        // Backfill the distinct-hash count if absent (#202). Runs on every
        // completed non-rebuild run (fresh seed and plain resume alike) so an
        // upgraded store self-heals on the next invocation.
        if repo
            .read_distinct_hash_count()
            .context("checking distinct hash count")?
            .is_none()
        {
            let count = repo
                .distinct_hash_count()
                .context("computing distinct hash count")?;
            repo.write_distinct_hash_count(count)
                .context("persisting distinct hash count")?;
            tracing::info!(count, "seed: persisted distinct hash count");
        }
    }

    tracing::info!("seed: all phases complete");
    Ok(())
}

/// Standalone sibling/parent relation backfill into a serving `RepoStore` (#225
/// §6). This is the sidecar deployment's `bridge seed-relations` step: a sidecar
/// seed (`sidecar_seed`) builds only the sidecar mapping file, so the relations
/// backfill for that deployment targets the native `RepoStore` beside it as a
/// distinct step. Mirror deployments get the same backfill as phase 4 of
/// [`run`]; this entry point is unflagged (no `sync_state` phase gate — the
/// sidecar's state store is not a [`StateDb`]) and relies entirely on
/// `apply_bridge_relations`'s per-row no-op guard for idempotency, so re-running
/// it restates existing edges without churning `seq`.
///
/// # Errors
/// Returns an error if the snapshot cannot be opened, the service id is ambiguous,
/// or a store write fails.
pub fn seed_relations(
    snapshot_dir: &Path,
    service_id: Option<i64>,
    repo: &RepoStore,
    bridge: &Account,
) -> anyhow::Result<()> {
    let hydrus = HydrusDb::open(snapshot_dir)?;
    let svc = match service_id {
        Some(id) => id,
        None => {
            let ids = hydrus.repository_service_ids()?;
            match ids.as_slice() {
                [] => anyhow::bail!("no repository_updates_* tables in snapshot"),
                [id] => *id,
                ids => anyhow::bail!("multiple repository services {ids:?}; pass --service-id"),
            }
        }
    };
    tracing::info!(svc, "seed-relations: starting standalone relation backfill");
    let plugin = HydrusPlugin::new(snapshot_dir.to_path_buf(), vec![svc]);
    let mut sink = BridgeRelationSink {
        repo,
        bridge,
        buf: Vec::new(),
        streamed: 0,
        applied: 0,
    };
    let stats = plugin
        .import_relations_only(&mut sink)
        .context("streaming snapshot relations")?;
    sink.flush().context("flushing final relation chunk")?;
    tracing::info!(
        target: "bridge",
        streamed = sink.streamed,
        applied = sink.applied,
        siblings = stats.siblings,
        parents = stats.parents,
        "seed-relations: complete"
    );
    Ok(())
}

/// A cheap, read-only fingerprint of the Hydrus snapshot, used at resume time to
/// confirm the snapshot on disk is the same one a checkpoint was written against
/// (seed spec §6). Composition is versioned and compared byte-for-byte:
///
/// `v1:svc=<service_id>:maxhash=<MAX(master.hashes.hash_id)>:mapsize=<len(client.mappings.db)>`
///
/// - `service_id` — a resume against a different service is meaningless.
/// - `MAX(hash_id)` — Hydrus assigns `hash_id` monotonically; a refreshed export has a larger max. O(1) via the PK.
/// - `client.mappings.db` file size — catches mapping changes that leave MAX(hash_id) unchanged. O(1) stat.
///
/// `pub(crate)` so tests can build a matching checkpoint.
///
/// # Errors
/// Returns an error if the max query or the file stat fails.
pub(crate) fn snapshot_fingerprint(
    snapshot_dir: &Path,
    hydrus: &HydrusDb,
    svc: i64,
) -> anyhow::Result<String> {
    let max = hydrus.master_hash_max()?.unwrap_or(0);
    let mapsize = std::fs::metadata(snapshot_dir.join("client.mappings.db"))?.len();
    Ok(format!("v1:svc={svc}:maxhash={max}:mapsize={mapsize}"))
}

/// Stream one phase-1 pass with hash-aligned, checkpointed flushing.
///
/// `pass` tags the checkpoint (Current/Deleted) and selects the source table.
/// `deferred` picks the no-SELECT append resolver (current pass, index dropped)
/// vs the indexed resolver (deleted pass). `start_after` resumes at `hash_id > h`.
///
/// The buffer is flushed only when it has reached `CHUNK_SIZE` **and** the incoming
/// row begins a new `hash_id` — so the stamped `high_water` (the previous row's
/// `hash_id`) is a fully-ingested hash (I2). The checkpoint is written inside the
/// chunk transaction by `apply_*` (I1).
#[allow(clippy::too_many_arguments)]
fn run_pass(
    repo: &RepoStore,
    hydrus: &HydrusDb,
    svc: i64,
    pass: SeedPass,
    deferred: bool,
    start_after: Option<u64>,
    fp: &str,
    caches: &mut InternCaches,
    seq: &mut i64,
    total: &mut u64,
) -> anyhow::Result<()> {
    let is_delete = matches!(pass, SeedPass::Deleted);
    let mut buf: Vec<(String, String, bool)> = Vec::with_capacity(CHUNK_SIZE);
    let mut prev_hash_id: Option<u64> = None;
    let mut flush_err: Option<anyhow::Error> = None;
    let mut log_instant = Instant::now();
    let mut log_rows_at_last: u64 = *total;

    let apply = |repo: &RepoStore,
                 flush: Vec<(String, String, bool)>,
                 caches: &mut InternCaches,
                 seq: &mut i64,
                 hw: u64|
     -> anyhow::Result<()> {
        let ck = SeedCheckpoint {
            v: 1,
            pass,
            high_water: hw,
            service_id: svc,
            fp: fp.to_string(),
        };
        if deferred {
            repo.apply_current_mappings_deferred(flush, caches, seq, Some(&ck))?;
        } else {
            repo.apply_mappings_bulk_cached(flush, caches, seq, Some(&ck))?;
        }
        Ok(())
    };

    let stream_result = hydrus.stream_ptr_mappings_pass(
        svc,
        is_delete,
        start_after,
        &mut |hash_id, sha, tag, _del| {
            if flush_err.is_some() {
                return false;
            }
            // Hash-aligned flush (§4.2): only at a hash boundary, once the buffer
            // is full. high_water = prev_hash_id — every one of its rows is in buf.
            if buf.len() >= CHUNK_SIZE && Some(hash_id) != prev_hash_id {
                let hw = prev_hash_id.expect("non-empty buffer => prev_hash_id set");
                let flush = std::mem::replace(&mut buf, Vec::with_capacity(CHUNK_SIZE));
                if let Err(e) = apply(repo, flush, caches, seq, hw) {
                    flush_err = Some(e);
                    return false;
                }
            }
            buf.push((sha.to_string(), tag.to_string(), is_delete));
            prev_hash_id = Some(hash_id);
            *total += 1;
            if *total % LOG_EVERY == 0 {
                let elapsed_secs = log_instant.elapsed().as_secs_f64();
                let rows_since = *total - log_rows_at_last;
                let rows_per_sec = if elapsed_secs > 0.0 {
                    (rows_since as f64 / elapsed_secs) as u64
                } else {
                    0
                };
                tracing::info!(total = *total, rows_per_sec, "seed: mappings streamed");
                log_instant = Instant::now();
                log_rows_at_last = *total;
            }
            true
        },
    );
    if let Some(e) = flush_err {
        return Err(e.context(format!("applying checkpointed {pass:?}-pass mapping chunk")));
    }
    stream_result?;
    // Final flush — high_water is the last hash seen (all its rows are buffered).
    if !buf.is_empty() {
        let hw = prev_hash_id.expect("non-empty buffer => prev_hash_id set");
        apply(repo, buf, caches, seq, hw)
            .with_context(|| format!("applying final checkpointed {pass:?}-pass mapping chunk"))?;
    }
    Ok(())
}

/// The indexed/Format-B phase-1 path (both passes via `stream_all_ptr_mappings`),
/// byte-identical to the pre-#182 behaviour. Writes NO checkpoint (I7).
fn run_indexed_all(
    repo: &RepoStore,
    hydrus: &HydrusDb,
    svc: i64,
    caches: &mut InternCaches,
    seq: &mut i64,
    total: &mut u64,
) -> anyhow::Result<()> {
    let mut buf: Vec<(String, String, bool)> = Vec::with_capacity(CHUNK_SIZE);
    let mut flush_err: Option<anyhow::Error> = None;
    let mut log_instant = Instant::now();
    let mut log_rows_at_last: u64 = *total;

    let stream_result = hydrus.stream_all_ptr_mappings(svc, &mut |sha, tag, del| {
        if flush_err.is_some() {
            return false;
        }
        buf.push((sha.to_string(), tag.to_string(), del));
        *total += 1;
        if *total % LOG_EVERY == 0 {
            let elapsed_secs = log_instant.elapsed().as_secs_f64();
            let rows_since = *total - log_rows_at_last;
            let rows_per_sec = if elapsed_secs > 0.0 {
                (rows_since as f64 / elapsed_secs) as u64
            } else {
                0
            };
            tracing::info!(total = *total, rows_per_sec, "seed: mappings streamed");
            log_instant = Instant::now();
            log_rows_at_last = *total;
        }
        if buf.len() >= CHUNK_SIZE {
            if let Err(e) =
                repo.apply_mappings_bulk_cached(std::mem::take(&mut buf), caches, seq, None)
            {
                flush_err = Some(e);
                return false;
            }
        }
        true
    });
    if let Some(e) = flush_err {
        return Err(e.context("applying mappings bulk chunk"));
    }
    stream_result?;
    if !buf.is_empty() {
        repo.apply_mappings_bulk_cached(std::mem::take(&mut buf), caches, seq, None)
            .context("applying final mappings chunk")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::tempdir;

    const SHA_A: &str = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";
    /// SHA_B is a deleted-only hash — present only in `deleted_mappings_9`, not
    /// in `current_mappings_9`.  Used to exercise the deleted-pass resolver on a
    /// hash that was never interned by the current pass, covering both the indexed
    /// and deferred code paths.
    const SHA_B: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";

    /// Minimal three-file Hydrus snapshot sufficient for seed phase testing.
    /// Service id 9.  One current mapping ("character:samus" on SHA_A), two
    /// deleted mappings ("meta:badtag" on SHA_A; "maid" on SHA_B — deleted-only).
    /// One fully-processed update (index 0), one partial (index 1) → watermark =
    /// Some(0).
    fn build_seed_fixture(dir: &std::path::Path) {
        // client.master.db
        let master = Connection::open(dir.join("client.master.db")).unwrap();
        master
            .execute_batch(
                "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
                 CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
                 CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
                 CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);
                 CREATE TABLE repository_hash_id_map_9 (service_hash_id INTEGER PRIMARY KEY, hash_id INTEGER);
                 CREATE TABLE repository_tag_id_map_9 (service_tag_id INTEGER PRIMARY KEY, tag_id INTEGER);",
            )
            .unwrap();
        master
            .execute(
                "INSERT INTO hashes (hash_id, hash) VALUES (1, ?1), (2, ?2)",
                rusqlite::params![hex::decode(SHA_A).unwrap(), hex::decode(SHA_B).unwrap(),],
            )
            .unwrap();
        master
            .execute_batch(
                "INSERT INTO namespaces VALUES (1, ''), (2, 'character'), (3, 'meta');
                 INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, 'badtag');
                 INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3);
                 INSERT INTO repository_hash_id_map_9 VALUES (500, 1);
                 INSERT INTO repository_tag_id_map_9 VALUES (800, 2);",
            )
            .unwrap();

        // client.db (main): update watermark tables
        let client = Connection::open(dir.join("client.db")).unwrap();
        client
            .execute_batch(
                "CREATE TABLE repository_updates_9 (update_index INTEGER, hash_id INTEGER);
                 INSERT INTO repository_updates_9 VALUES (0, 100), (1, 101);
                 CREATE TABLE repository_updates_processed_9
                     (hash_id INTEGER, content_type INTEGER, processed INTEGER);
                 INSERT INTO repository_updates_processed_9
                     VALUES (100, 1, 1), (101, 1, 1), (101, 2, 0);",
            )
            .unwrap();

        // client.mappings.db: current + deleted mappings
        // current_mappings_9: tag_id=2 (character:samus), hash_id=1 (SHA_A)
        // deleted_mappings_9: tag_id=3 (meta:badtag), hash_id=1 (SHA_A)
        //                     tag_id=1 (maid),         hash_id=2 (SHA_B — deleted-only)
        let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_9 VALUES (2, 1);
                 CREATE TABLE deleted_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO deleted_mappings_9 VALUES (3, 1), (1, 2);",
            )
            .unwrap();
    }

    #[test]
    fn seed_run_imports_mappings_defs_and_cursor() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture(snapshot_dir.path());

        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open(repo_dir.path().join("repo.db")).unwrap();

        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();

        // (a) Snapshot contains the current mapping; deleted tag is NOT present.
        let snap = repo.snapshot().unwrap();
        assert!(
            snap.values()
                .any(|tags| tags.iter().any(|t| t.tag.contains("samus"))),
            "current mapping (character:samus) must appear in snapshot"
        );
        assert!(
            !snap
                .values()
                .any(|tags| tags.iter().any(|t| t.tag.contains("badtag"))),
            "deleted mapping (meta:badtag) must NOT appear in snapshot"
        );

        // (b) defs_hashes and defs_tags populated.
        let h_count: i64 = state
            .conn()
            .query_row("SELECT COUNT(*) FROM defs_hashes", [], |r| r.get(0))
            .unwrap();
        assert!(h_count > 0, "defs_hashes must be populated after seed");

        let t_count: i64 = state
            .conn()
            .query_row("SELECT COUNT(*) FROM defs_tags", [], |r| r.get(0))
            .unwrap();
        assert!(t_count > 0, "defs_tags must be populated after seed");

        // (c) Cursor = watermark + 1 = 0 + 1 = 1.
        assert_eq!(
            state.next_update_index().unwrap(),
            1,
            "next_update_index must equal watermark+1"
        );

        // (d) Second run: flags already "done", no duplication, cursor unchanged.
        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();
        assert_eq!(
            state.get_flag("seed_phase_mappings").unwrap().as_deref(),
            Some("done")
        );
        assert_eq!(
            state.get_flag("seed_phase_defs").unwrap().as_deref(),
            Some("done")
        );
        assert_eq!(
            state.get_flag("seed_phase_cursor").unwrap().as_deref(),
            Some("done")
        );
        assert_eq!(
            state.next_update_index().unwrap(),
            1,
            "cursor must not change on re-run"
        );
    }

    #[test]
    fn seed_run_discovers_service_id_automatically() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture(snapshot_dir.path());

        let repo = RepoStore::open_in_memory().unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        // Pass None → auto-discover service id 9 from repository_updates_9 table.
        run(
            snapshot_dir.path(),
            None,
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();
        assert_eq!(
            state.get_flag("seed_phase_mappings").unwrap().as_deref(),
            Some("done"),
            "seed must complete when service id is auto-discovered"
        );
    }

    /// Same as `build_seed_fixture` but adds hash-led covering indexes to both
    /// `current_mappings_9` and `deleted_mappings_9`, making this a Format-A
    /// fixture.  This makes `mappings_hash_ordered(9)` return `true`, which
    /// activates the deferred seed path on a fresh store.
    fn build_seed_fixture_indexed(dir: &std::path::Path) {
        build_seed_fixture(dir);
        let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE UNIQUE INDEX current_mappings_9_hash_id_tag_id_index \
                     ON current_mappings_9 (hash_id, tag_id); \
                 CREATE UNIQUE INDEX deleted_mappings_9_hash_id_tag_id_index \
                     ON deleted_mappings_9 (hash_id, tag_id);",
            )
            .unwrap();
    }

    /// Adversarial Format-A fixture: hash_id order DIFFERS from hash byte order.
    ///
    ///   hash_id=1 → "ff…"  (largest bytes, lowest source id)
    ///   hash_id=2 → "aa…"  (smallest bytes, middle source id)
    ///   hash_id=3 → "bb…"  (middle bytes, highest source id)
    ///
    /// Stream order via the hash-led index (ORDER BY hash_id): ff, aa, bb.
    /// Hash byte order (ascending):                            aa, bb, ff.
    /// These differ — the deferred path writes `repo_hashes` in hash_id stream
    /// order (ff, aa, bb), NOT in byte order, because `stream_ptr_mappings_pass`
    /// orders by `m.hash_id` (Hydrus intern key), which has no byte correlation on
    /// real snapshots.  SHA_FF gets two tags (alpha + delta) so per-hash
    /// contiguity is non-trivial.
    const SHA_FF: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    const SHA_AA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_BB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn build_seed_fixture_indexed_multi(dir: &std::path::Path) {
        // client.master.db — adversarial: hash_id=1 → ff (largest bytes), so
        // stream order (hash_id asc) ≠ byte order.
        let master = Connection::open(dir.join("client.master.db")).unwrap();
        master
            .execute_batch(
                "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
                 CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
                 CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
                 CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);
                 CREATE TABLE repository_hash_id_map_9 (service_hash_id INTEGER PRIMARY KEY, hash_id INTEGER);
                 CREATE TABLE repository_tag_id_map_9 (service_tag_id INTEGER PRIMARY KEY, tag_id INTEGER);",
            )
            .unwrap();
        // hash_id 1→ff (largest), 2→aa (smallest), 3→bb (middle)
        master
            .execute(
                "INSERT INTO hashes (hash_id, hash) VALUES (1, ?1), (2, ?2), (3, ?3)",
                rusqlite::params![
                    hex::decode(SHA_FF).unwrap(),
                    hex::decode(SHA_AA).unwrap(),
                    hex::decode(SHA_BB).unwrap(),
                ],
            )
            .unwrap();
        // 4 tags: alpha(1), beta(2), gamma(3), delta(4) — alpha+delta go on hash_id=1(ff)
        master
            .execute_batch(
                "INSERT INTO namespaces VALUES (1, '');
                 INSERT INTO subtags VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma'), (4, 'delta');
                 INSERT INTO tags VALUES (1, 1, 1), (2, 1, 2), (3, 1, 3), (4, 1, 4);
                 INSERT INTO repository_hash_id_map_9 VALUES (1, 1), (2, 2), (3, 3);
                 INSERT INTO repository_tag_id_map_9 VALUES (800, 1);",
            )
            .unwrap();

        // client.db — watermark
        let client = Connection::open(dir.join("client.db")).unwrap();
        client
            .execute_batch(
                "CREATE TABLE repository_updates_9 (update_index INTEGER, hash_id INTEGER);
                 INSERT INTO repository_updates_9 VALUES (0, 100);
                 CREATE TABLE repository_updates_processed_9
                     (hash_id INTEGER, content_type INTEGER, processed INTEGER);
                 INSERT INTO repository_updates_processed_9 VALUES (100, 1, 1);",
            )
            .unwrap();

        // client.mappings.db — current mappings with hash-led indexes (Format-A).
        // hash_id=1(ff): tag 1(alpha) + tag 4(delta)  — two tags → non-trivial contiguity
        // hash_id=2(aa): tag 2(beta)
        // hash_id=3(bb): tag 3(gamma)
        // Rows are inserted in REVERSE hash_id order so that physical table order
        // (before the index) differs from hash_id order, confirming the index drives
        // the stream ordering.
        let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_9 VALUES (3, 3), (2, 2), (4, 1), (1, 1);
                 CREATE UNIQUE INDEX current_mappings_9_hash_id_tag_id_index
                     ON current_mappings_9 (hash_id, tag_id);
                 CREATE TABLE deleted_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 CREATE UNIQUE INDEX deleted_mappings_9_hash_id_tag_id_index
                     ON deleted_mappings_9 (hash_id, tag_id);",
            )
            .unwrap();
    }

    /// Deferred seed on a hash_id-ordered (Format-A) fixture produces the same
    /// snapshot content as the normal indexed seed on the same data.
    ///
    /// Strategy: seed store A via the indexed path (Format-B fixture, no
    /// hash-led index → indexed path) and store B via the deferred path
    /// (Format-A fixture → deferred path on fresh store).  Assert snapshots
    /// match (same current mappings, same cursor).
    #[test]
    fn deferred_seed_produces_same_output_as_indexed_path() {
        // ── Store A: indexed path (Format-B, unordered fallback) ─────────────
        let snap_b = tempdir().unwrap();
        build_seed_fixture(snap_b.path()); // no hash-led indexes → indexed path

        let repo_a_dir = tempdir().unwrap();
        let repo_a = RepoStore::open(repo_a_dir.path().join("repo.db")).unwrap();
        let state_a_dir = tempdir().unwrap();
        let state_a = StateDb::open(state_a_dir.path().join("state.db")).unwrap();
        run(
            snap_b.path(),
            Some(9),
            &repo_a,
            &state_a,
            &Account::generate(),
            false,
        )
        .unwrap();

        // ── Store B: deferred path (Format-A, hash_id-ordered) ───────────────
        let snap_a = tempdir().unwrap();
        build_seed_fixture_indexed(snap_a.path()); // hash-led indexes → deferred

        let repo_b_dir = tempdir().unwrap();
        let repo_b = RepoStore::open_bulk_ingest(repo_b_dir.path().join("repo.db"), false).unwrap();
        let state_b_dir = tempdir().unwrap();
        let state_b = StateDb::open(state_b_dir.path().join("state.db")).unwrap();
        run(
            snap_a.path(),
            Some(9),
            &repo_b,
            &state_b,
            &Account::generate(),
            false,
        )
        .unwrap();

        // ── Compare snapshots ─────────────────────────────────────────────────
        let snap_a_out = repo_a.snapshot().unwrap();
        let snap_b_out = repo_b.snapshot().unwrap();
        assert_eq!(
            snap_a_out.len(),
            snap_b_out.len(),
            "both stores must have the same number of hashes in snapshot"
        );
        for (hash_hex, tags_a) in &snap_a_out {
            let tags_b = snap_b_out
                .get(hash_hex)
                .expect("deferred store must contain same hashes as indexed store");
            let mut ta: Vec<_> = tags_a.iter().map(|t| t.tag.as_str()).collect();
            let mut tb: Vec<_> = tags_b.iter().map(|t| t.tag.as_str()).collect();
            ta.sort_unstable();
            tb.sort_unstable();
            assert_eq!(ta, tb, "tags for {hash_hex} must match between paths");
        }

        // ── Verify deferred store is fully indexed and serveable ──────────────
        // Opening with open() asserts the index is present (I7).
        let repo_b_serving = RepoStore::open(repo_b_dir.path().join("repo.db"))
            .expect("deferred-seeded store must be openable (index must be present)");

        // ── bucket() equality (full-range scan, current-only status=0) ────────
        // SHA_B is deleted-only → does NOT appear in bucket output.
        // SHA_A has character:samus (current) → appears in both stores.
        let lo = "00".repeat(32);
        let hi = "gg"; // sentinel: non-hex, degrades to all-0xFF upper bound (see hi_bound)
        let bucket_a = repo_a.bucket(&lo, hi, usize::MAX).unwrap().0;
        let bucket_b = repo_b_serving.bucket(&lo, hi, usize::MAX).unwrap().0;
        assert_eq!(
            bucket_a.len(),
            bucket_b.len(),
            "bucket() must return the same number of hashes for both paths"
        );
        for (hash_hex, tags_a) in &bucket_a {
            let tags_b = bucket_b
                .get(hash_hex)
                .expect("deferred bucket must contain same hashes as indexed bucket");
            let mut ta: Vec<_> = tags_a.iter().map(|t| t.tag.as_str()).collect();
            let mut tb: Vec<_> = tags_b.iter().map(|t| t.tag.as_str()).collect();
            ta.sort_unstable();
            tb.sort_unstable();
            assert_eq!(
                ta, tb,
                "bucket tags for {hash_hex} must match between paths"
            );
        }
        // SHA_B (deleted-only) must be absent from bucket output.
        assert!(
            !bucket_a.contains_key(SHA_B),
            "deleted-only SHA_B must not appear in bucket output"
        );

        // ── mapping_cursor() equality ─────────────────────────────────────────
        // Both stores ingested the same rows (3 total: SHA_A/samus current,
        // SHA_A/badtag deleted, SHA_B/maid deleted) → MAX(seq) must be equal.
        let cursor_a = repo_a.mapping_cursor().unwrap();
        let cursor_b = repo_b_serving.mapping_cursor().unwrap();
        assert_eq!(
            cursor_a, cursor_b,
            "mapping_cursor() must be equal for both seed paths"
        );
        assert!(
            cursor_a > 0,
            "mapping_cursor must be positive after seeding"
        );

        // ── Both flags done ───────────────────────────────────────────────────
        assert_eq!(
            state_b.get_flag("seed_phase_mappings").unwrap().as_deref(),
            Some("done"),
            "deferred seed must mark phase mappings done"
        );
    }

    /// Crash-resume: drop the index + apply some rows via the deferred path,
    /// then re-invoke `run`.  The self-heal branch (§4.5) must rebuild the
    /// index, complete the seed, and leave the store consistent.
    #[test]
    fn crash_resume_self_heals_and_completes() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture_indexed(snapshot_dir.path());

        let repo_dir = tempdir().unwrap();
        let state_dir = tempdir().unwrap();

        // Open for bulk ingest (does not assert index) so we can manipulate it.
        let repo = RepoStore::open_bulk_ingest(repo_dir.path().join("repo.db"), false).unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        // Simulate a crash mid-current-pass:
        // 1. Drop the unique index (as the deferred path would do).
        repo.drop_hash_unique_index().unwrap();

        // 2. Apply one row via the deferred resolver (partial progress).
        let sha_a = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";
        let mut partial_caches = InternCaches::default();
        let mut partial_seq: i64 = 0;
        repo.apply_current_mappings_deferred(
            vec![(sha_a.to_string(), "character:samus".to_string(), false)],
            &mut partial_caches,
            &mut partial_seq,
            None,
        )
        .unwrap();

        // 3. Crash: index is absent, one partial row exists, flag not "done".
        assert!(
            !repo.has_hash_unique_index().unwrap(),
            "index must be absent after drop"
        );

        // Re-invoke seed on a fresh open_bulk_ingest handle (simulates restart).
        let repo2 = RepoStore::open_bulk_ingest(repo_dir.path().join("repo.db"), false).unwrap();
        run(
            snapshot_dir.path(),
            Some(9),
            &repo2,
            &state,
            &Account::generate(),
            false,
        )
        .expect("seed::run must complete after self-heal");

        // Self-heal rebuilt the index and the seed finished.
        assert!(
            repo2.has_hash_unique_index().unwrap(),
            "index must be present after self-heal + seed completion"
        );
        assert_eq!(
            state.get_flag("seed_phase_mappings").unwrap().as_deref(),
            Some("done"),
            "phase must be marked done after self-heal seed"
        );

        // The snapshot must contain the current mapping from the fixture.
        let snap = repo2.snapshot().unwrap();
        assert!(
            snap.values()
                .any(|tags| tags.iter().any(|t| t.tag.contains("samus"))),
            "current mapping (character:samus) must appear after self-heal seed"
        );

        // Verify index is in place: open() asserts it (I7).
        let _ = RepoStore::open(repo_dir.path().join("repo.db"))
            .expect("store must be openable (index must be present after self-heal)");
    }

    /// A store with a duplicate hash entry causes `build_hash_unique_index`
    /// to fail loudly — the I8 correctness backstop.
    #[test]
    fn build_hash_unique_index_fails_on_duplicate_hash() {
        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open_bulk_ingest(repo_dir.path().join("repo.db"), false).unwrap();

        // Drop the named index so we can insert duplicates.
        repo.drop_hash_unique_index().unwrap();

        // Insert the same 32-byte hash blob twice.
        let hash_bytes = hex::decode("ab".repeat(32)).unwrap();
        repo.conn
            .execute(
                "INSERT INTO repo_hashes(hash) VALUES(?1)",
                [hash_bytes.as_slice()],
            )
            .unwrap();
        repo.conn
            .execute(
                "INSERT INTO repo_hashes(hash) VALUES(?1)",
                [hash_bytes.as_slice()],
            )
            .unwrap();

        // build_hash_unique_index must fail (UNIQUE constraint violated — I8).
        assert!(
            repo.build_hash_unique_index().is_err(),
            "build_hash_unique_index must fail loudly when duplicate hashes are present"
        );
    }

    // ── store-generation minting on normal seed (#194) ────────────────────────

    #[test]
    fn normal_seed_mints_generation_on_first_run() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture(snapshot_dir.path());

        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open(repo_dir.path().join("repo.db")).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        // No generation before seed.
        assert!(
            repo.store_generation().unwrap().is_none(),
            "fresh store must have no generation"
        );

        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();

        // Generation minted after successful seed.
        let minted = repo.store_generation().unwrap();
        assert!(
            minted.is_some(),
            "normal seed must mint a store generation on first run"
        );
        assert_eq!(
            minted.as_ref().unwrap().len(),
            32,
            "generation must be 32 hex chars"
        );

        assert_eq!(
            repo.read_distinct_hash_count().unwrap(),
            Some(repo.distinct_hash_count().unwrap()),
            "normal seed must persist the distinct-hash count"
        );
    }

    #[test]
    fn normal_seed_resume_keeps_generation() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture(snapshot_dir.path());

        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open(repo_dir.path().join("repo.db")).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();
        let gen1 = repo.store_generation().unwrap();

        // A second run (resume, all phases done) must NOT change the generation.
        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();
        let gen2 = repo.store_generation().unwrap();

        assert_eq!(
            gen1, gen2,
            "a plain resume must not change the existing store generation"
        );
    }

    // ── rebuild-in-place (#194) ───────────────────────────────────────────────

    #[test]
    fn resume_seed_backfills_absent_count_row() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture(snapshot_dir.path());
        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open(repo_dir.path().join("repo.db")).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();
        // Simulate an upgraded-but-not-rebuilt store: generation present, count absent.
        repo.conn
            .execute(
                "DELETE FROM repo_meta WHERE key = 'distinct_hash_count'",
                [],
            )
            .unwrap();
        assert_eq!(repo.read_distinct_hash_count().unwrap(), None);

        // A plain resume (all phases done) must backfill the count.
        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();
        assert_eq!(repo.read_distinct_hash_count().unwrap(), Some(1));
    }

    // ── rebuild_in_progress guard (#194 S1) ───────────────────────────────────

    #[test]
    fn interrupted_rebuild_blocks_plain_seed() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture(snapshot_dir.path());

        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open(repo_dir.path().join("repo.db")).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        // Simulate a crash after the marker was set but before the rebuild finished.
        repo.set_rebuild_in_progress().unwrap();

        // A plain seed (rebuild=false) must fail with the actionable error.
        let err = run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--rebuild"),
            "error must tell the operator to re-run with --rebuild: {msg}"
        );
    }

    // ── #182 resumable-seed tests (spec §10) ─────────────────────────────────

    /// §10.1 — a crash mid current pass resumes and completes; content and cursor
    /// equal a clean full seed (no duplicate rows).
    #[test]
    fn resume_mid_current_pass_completes() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture_indexed(snapshot_dir.path());
        let hydrus = HydrusDb::open(snapshot_dir.path()).unwrap();
        let fp = snapshot_fingerprint(snapshot_dir.path(), &hydrus, 9).unwrap();

        // Crashed store: index dropped, SHA_A (source hash_id=1) applied + checkpoint.
        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open_bulk_ingest(repo_dir.path().join("repo.db"), false).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        repo.drop_hash_unique_index().unwrap();
        let mut caches = InternCaches::default();
        let mut seq: i64 = 0;
        let ckpt = SeedCheckpoint {
            v: 1,
            pass: SeedPass::Current,
            high_water: 1,
            service_id: 9,
            fp: fp.clone(),
        };
        repo.apply_current_mappings_deferred(
            vec![(SHA_A.to_string(), "character:samus".to_string(), false)],
            &mut caches,
            &mut seq,
            Some(&ckpt),
        )
        .unwrap();
        assert!(repo.read_seed_checkpoint().unwrap().is_some());

        // Resume.
        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();
        assert!(
            repo.has_hash_unique_index().unwrap(),
            "index present after completion"
        );
        assert_eq!(
            repo.read_seed_checkpoint().unwrap(),
            None,
            "checkpoint cleared"
        );
        assert_eq!(
            state.get_flag("seed_phase_mappings").unwrap().as_deref(),
            Some("done")
        );

        // Clean full seed for comparison.
        let clean_snap = tempdir().unwrap();
        build_seed_fixture_indexed(clean_snap.path());
        let clean_dir = tempdir().unwrap();
        let clean_repo =
            RepoStore::open_bulk_ingest(clean_dir.path().join("repo.db"), false).unwrap();
        let clean_state_dir = tempdir().unwrap();
        let clean_state = StateDb::open(clean_state_dir.path().join("state.db")).unwrap();
        run(
            clean_snap.path(),
            Some(9),
            &clean_repo,
            &clean_state,
            &Account::generate(),
            false,
        )
        .unwrap();

        assert_eq!(
            repo.snapshot().unwrap(),
            clean_repo.snapshot().unwrap(),
            "resumed content must equal a clean full seed"
        );
        assert_eq!(
            repo.mapping_cursor().unwrap(),
            clean_repo.mapping_cursor().unwrap(),
            "resumed cursor must equal clean seed (no duplicate rows)"
        );
    }

    /// §10.2 — a crash mid deleted pass resumes and completes.
    #[test]
    fn resume_mid_deleted_pass_completes() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture_indexed(snapshot_dir.path());
        let hydrus = HydrusDb::open(snapshot_dir.path()).unwrap();
        let fp = snapshot_fingerprint(snapshot_dir.path(), &hydrus, 9).unwrap();

        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open_bulk_ingest(repo_dir.path().join("repo.db"), false).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        // Current pass fully applied + index built.
        repo.drop_hash_unique_index().unwrap();
        let mut caches = InternCaches::default();
        let mut seq: i64 = 0;
        repo.apply_current_mappings_deferred(
            vec![(SHA_A.to_string(), "character:samus".to_string(), false)],
            &mut caches,
            &mut seq,
            None,
        )
        .unwrap();
        repo.build_hash_unique_index().unwrap();
        // Partial deleted pass: first deleted hash (SHA_A/meta:badtag, source hash_id=1).
        let ckpt = SeedCheckpoint {
            v: 1,
            pass: SeedPass::Deleted,
            high_water: 1,
            service_id: 9,
            fp: fp.clone(),
        };
        repo.apply_mappings_bulk_cached(
            vec![(SHA_A.to_string(), "meta:badtag".to_string(), true)],
            &mut caches,
            &mut seq,
            Some(&ckpt),
        )
        .unwrap();

        // Resume → deleted pass continues at hash_id > 1 (SHA_B/maid).
        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();

        // Current mapping intact.
        let snap = repo.snapshot().unwrap();
        assert!(
            snap.values()
                .any(|ts| ts.iter().any(|t| t.tag.contains("samus"))),
            "current mapping must remain intact"
        );
        // SHA_B (deleted-only) resolved with a status=1 row.
        let sha_b_deleted: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM repo_mappings m \
                 JOIN repo_hashes h ON h.id = m.hash_id \
                 WHERE m.status = 1 AND hex(h.hash) = upper(?1)",
                [SHA_B],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sha_b_deleted, 1,
            "SHA_B deleted mapping (maid) must resolve on resume"
        );
        assert_eq!(
            repo.read_seed_checkpoint().unwrap(),
            None,
            "checkpoint cleared"
        );
        assert_eq!(
            state.get_flag("seed_phase_mappings").unwrap().as_deref(),
            Some("done")
        );
    }

    /// §10.3 (X3) — checkpoint pass=Current with the index PRESENT routes to a
    /// from-scratch deleted pass, without a second (erroring) index build.
    #[test]
    fn resume_current_pass_index_present_runs_deleted() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture_indexed(snapshot_dir.path());
        let hydrus = HydrusDb::open(snapshot_dir.path()).unwrap();
        let fp = snapshot_fingerprint(snapshot_dir.path(), &hydrus, 9).unwrap();

        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open_bulk_ingest(repo_dir.path().join("repo.db"), false).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        // Current pass complete + index built; deleted not started.
        repo.drop_hash_unique_index().unwrap();
        let mut caches = InternCaches::default();
        let mut seq: i64 = 0;
        repo.apply_current_mappings_deferred(
            vec![(SHA_A.to_string(), "character:samus".to_string(), false)],
            &mut caches,
            &mut seq,
            None,
        )
        .unwrap();
        repo.build_hash_unique_index().unwrap();
        // Checkpoint says Current, but the index is already present (X3 window).
        let ckpt = SeedCheckpoint {
            v: 1,
            pass: SeedPass::Current,
            high_water: 1,
            service_id: 9,
            fp,
        };
        repo.write_seed_checkpoint(&ckpt).unwrap();

        // Must NOT attempt to rebuild the (present) index — that would Err.
        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .expect("RunDeleted must not rebuild the existing index");

        let deleted_rows: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM repo_mappings WHERE status = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(deleted_rows >= 1, "deleted pass must have run from scratch");
        assert_eq!(
            repo.read_seed_checkpoint().unwrap(),
            None,
            "checkpoint cleared"
        );
    }

    /// §10.4 — a crash exactly at a two-tag hash boundary (SHA_FF: alpha+delta)
    /// must not split or duplicate the hash; later hashes append after; per-hash
    /// seq blocks stay gapless (I2/I3).
    #[test]
    fn resume_boundary_hash_not_split() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture_indexed_multi(snapshot_dir.path());
        let hydrus = HydrusDb::open(snapshot_dir.path()).unwrap();
        let fp = snapshot_fingerprint(snapshot_dir.path(), &hydrus, 9).unwrap();

        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open_bulk_ingest(repo_dir.path().join("repo.db"), false).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        // Crash exactly after SHA_FF (source hash_id=1) with BOTH its tags applied.
        repo.drop_hash_unique_index().unwrap();
        let mut caches = InternCaches::default();
        let mut seq: i64 = 0;
        let ckpt = SeedCheckpoint {
            v: 1,
            pass: SeedPass::Current,
            high_water: 1,
            service_id: 9,
            fp,
        };
        repo.apply_current_mappings_deferred(
            vec![
                (SHA_FF.to_string(), "alpha".to_string(), false),
                (SHA_FF.to_string(), "delta".to_string(), false),
            ],
            &mut caches,
            &mut seq,
            Some(&ckpt),
        )
        .unwrap();

        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();

        let snap = repo.snapshot().unwrap();
        // SHA_FF has BOTH alpha and delta (not split, not duplicated).
        let ff = snap.get(SHA_FF).expect("SHA_FF must be present");
        let mut ff_tags: Vec<&str> = ff.iter().map(|t| t.tag.as_str()).collect();
        ff_tags.sort_unstable();
        assert_eq!(
            ff_tags,
            vec!["alpha", "delta"],
            "SHA_FF must carry exactly alpha+delta"
        );
        // aa/bb appended after.
        assert!(
            snap.get(SHA_AA)
                .map(|v| v.iter().any(|t| t.tag == "beta"))
                .unwrap_or(false),
            "SHA_AA (beta) must be appended after the resume boundary"
        );
        assert!(
            snap.get(SHA_BB)
                .map(|v| v.iter().any(|t| t.tag == "gamma"))
                .unwrap_or(false),
            "SHA_BB (gamma) must be appended after the resume boundary"
        );

        // Gapless per-hash seq blocks (as in rebuild_clusters_mappings_per_hash_and_analyzes).
        let mut cstmt = repo
            .conn
            .prepare(
                "SELECT hash_id, MIN(seq), MAX(seq), COUNT(*) \
                 FROM repo_mappings WHERE status = 0 GROUP BY hash_id",
            )
            .unwrap();
        let groups: Vec<(i64, i64, i64, i64)> = cstmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(groups.len(), 3, "three distinct current-mapping hashes");
        for (hash_id, lo, hi, cnt) in &groups {
            assert_eq!(
                hi - lo + 1,
                *cnt,
                "hash_id={hash_id}: seq block must be gapless"
            );
        }
    }

    /// §10.5 — a plain seed against a mismatched-fingerprint checkpoint is a hard,
    /// actionable error and does not complete.
    #[test]
    fn resume_fingerprint_mismatch_hard_errors() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture_indexed(snapshot_dir.path());

        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open_bulk_ingest(repo_dir.path().join("repo.db"), false).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        repo.drop_hash_unique_index().unwrap();
        let mut caches = InternCaches::default();
        let mut seq: i64 = 0;
        // Deliberately WRONG fingerprint (maxhash/mapsize will never match).
        let bad = SeedCheckpoint {
            v: 1,
            pass: SeedPass::Current,
            high_water: 1,
            service_id: 9,
            fp: "v1:svc=9:maxhash=999999999:mapsize=1".to_string(),
        };
        repo.apply_current_mappings_deferred(
            vec![(SHA_A.to_string(), "character:samus".to_string(), false)],
            &mut caches,
            &mut seq,
            Some(&bad),
        )
        .unwrap();

        let err = run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--rebuild"),
            "message must mention --rebuild: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("snapshot"),
            "message must mention the snapshot: {msg}"
        );
        assert_ne!(
            state.get_flag("seed_phase_mappings").unwrap().as_deref(),
            Some("done"),
            "a mismatched plain seed must not complete the phase"
        );
    }

    /// §10.7 — a clean deferred seed clears the checkpoint and flags the phase done.
    #[test]
    fn ckpt_cleared_on_completion() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture_indexed(snapshot_dir.path());
        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open_bulk_ingest(repo_dir.path().join("repo.db"), false).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();
        assert_eq!(
            repo.read_seed_checkpoint().unwrap(),
            None,
            "checkpoint cleared on completion"
        );
        assert_eq!(
            state.get_flag("seed_phase_mappings").unwrap().as_deref(),
            Some("done")
        );
    }

    /// §10.8 (I7) — the Format-B/indexed path never writes a seed_ckpt row.
    #[test]
    fn format_b_writes_no_checkpoint() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture(snapshot_dir.path()); // Format-B (no hash-led index)
        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open(repo_dir.path().join("repo.db")).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();

        run(
            snapshot_dir.path(),
            Some(9),
            &repo,
            &state,
            &Account::generate(),
            false,
        )
        .unwrap();

        assert_eq!(repo.read_seed_checkpoint().unwrap(), None);
        let n: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM repo_meta WHERE key = 'seed_ckpt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "Format-B must never create a seed_ckpt row (I7)");
        // Byte-identical Format-B content: current mapping present.
        let snap = repo.snapshot().unwrap();
        assert!(
            snap.values()
                .any(|ts| ts.iter().any(|t| t.tag.contains("samus")))
        );
    }

    /// #225 seed backfill: sibling/parent relations stream from a snapshot into
    /// the RepoStore authored by the bridge key; current edges are `current`,
    /// deleted edges are `deleted`, and a second run is a `seq` no-op.
    #[test]
    fn seed_relations_backfills_signed_bridge_edges() {
        let dir = tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_relations_fixture(dir.path(), 9).unwrap();
        let repo = RepoStore::open_in_memory().unwrap();
        let bridge = Account::generate();

        seed_relations(dir.path(), Some(9), &repo, &bridge).unwrap();

        // Current graph: one sibling (samus_aran → samus), one parent (samus → maid).
        let g = repo.relations().unwrap();
        assert_eq!(g.siblings.len(), 1, "one current sibling");
        assert_eq!(g.siblings[0].from, "character:samus_aran");
        assert_eq!(g.siblings[0].to, "character:samus");
        assert_eq!(g.siblings[0].author, bridge.public_hex());
        assert_eq!(g.parents.len(), 1, "one current parent");
        assert_eq!(g.parents[0].from, "character:samus");
        assert_eq!(g.parents[0].to, "maid");
        assert_eq!(g.parents[0].author, bridge.public_hex());

        // The deleted sibling (maid → character:samus) is a tombstone: absent from
        // the current graph but visible as a `deleted` delta from seq 0.
        let deltas = repo.edges_since(0).unwrap();
        let tomb = deltas
            .iter()
            .find(|e| e.status == naiad_netproto::EdgeStatus::Deleted)
            .expect("the deleted sibling is present as a tombstone");
        assert_eq!(tomb.from, "maid");
        assert_eq!(tomb.to, "character:samus");
        assert_eq!(tomb.author, bridge.public_hex());

        // Second run is a relation-seq no-op (LWW + no-op guard).
        let cursor = repo.relation_cursor().unwrap();
        seed_relations(dir.path(), Some(9), &repo, &bridge).unwrap();
        assert_eq!(
            repo.relation_cursor().unwrap(),
            cursor,
            "re-running the seed must not churn the relation seq"
        );
    }

    /// Full `run` with a snapshot that HAS sibling/parent tables: Phase 4 imports
    /// them into the RepoStore authored by the bridge key, marks
    /// `seed_phase_relations` done, and a second run resume-skips Phase 4 (relation
    /// seq no-op). Exercises the phase inside `run`, not just `seed_relations`.
    #[test]
    fn seed_run_phase_relations_imports_and_resume_skips() {
        let snapshot_dir = tempdir().unwrap();
        build_seed_fixture(snapshot_dir.path());
        // Add sibling/parent tables to the same snapshot (master tag ids from
        // build_seed_fixture: 1=maid, 2=character:samus, 3=meta:badtag).
        {
            let client = Connection::open(snapshot_dir.path().join("client.db")).unwrap();
            client
                .execute_batch(
                    "CREATE TABLE current_tag_siblings_9 (bad_tag_id INTEGER, good_tag_id INTEGER);
                     INSERT INTO current_tag_siblings_9 VALUES (3, 2);
                     CREATE TABLE current_tag_parents_9 (child_tag_id INTEGER, parent_tag_id INTEGER);
                     INSERT INTO current_tag_parents_9 VALUES (2, 1);
                     CREATE TABLE deleted_tag_siblings_9 (bad_tag_id INTEGER, good_tag_id INTEGER);
                     INSERT INTO deleted_tag_siblings_9 VALUES (1, 2);",
                )
                .unwrap();
        }

        let repo_dir = tempdir().unwrap();
        let repo = RepoStore::open(repo_dir.path().join("repo.db")).unwrap();
        let state_dir = tempdir().unwrap();
        let state = StateDb::open(state_dir.path().join("state.db")).unwrap();
        let bridge = Account::generate();

        run(snapshot_dir.path(), Some(9), &repo, &state, &bridge, false).unwrap();

        // Phase 4 populated the relations, authored by the bridge key.
        let g = repo.relations().unwrap();
        assert_eq!(g.siblings.len(), 1, "one current sibling from the snapshot");
        assert_eq!(g.siblings[0].from, "meta:badtag");
        assert_eq!(g.siblings[0].to, "character:samus");
        assert_eq!(g.siblings[0].author, bridge.public_hex());
        assert_eq!(g.parents.len(), 1);
        assert_eq!(g.parents[0].from, "character:samus");
        assert_eq!(g.parents[0].to, "maid");
        // The phase flag is marked done so a resume skips the bulk rescan.
        assert_eq!(
            state.get_flag("seed_phase_relations").unwrap().as_deref(),
            Some("done"),
            "seed_phase_relations must be marked done"
        );

        // Second run: all phases (incl. relations) done → resume-skip, seq no-op.
        let cursor = repo.relation_cursor().unwrap();
        run(snapshot_dir.path(), Some(9), &repo, &state, &bridge, false).unwrap();
        assert_eq!(
            repo.relation_cursor().unwrap(),
            cursor,
            "resumed seed must not re-run Phase 4 or churn the relation seq"
        );
    }
}
