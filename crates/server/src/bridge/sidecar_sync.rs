//! Apply PTR update files to the sidecar (#207 Task 5, the X2 freshness path).
//!
//! Per update index: definitions (Tag::parse-normalized) commit in their own
//! transaction, then content (read-modify-write in wire order) commits in
//! bounded sub-batches, and the cursor advances only in the FINAL sub-batch
//! (#231, the F14 design). Crash safety is cursor-driven rather than
//! single-transaction: an interrupted index leaves the cursor untouched, so
//! `sync_once` replays it wholesale — every write in the pipeline is
//! idempotent (defs are upserts; per-hash mutation replay converges because
//! set membership is decided by the last wire op per `(hash, tag)`; the
//! relation apply has its own no-op guard).
//!
//! Bounded sub-batches exist so the WAL can checkpoint DURING an apply: a
//! single 1M-mapping transaction pins multi-GB of WAL that no checkpoint can
//! reclaim mid-flight (#231's 3.5 GB `sidecar.db-wal`). A
//! `wal_checkpoint(TRUNCATE)` between sub-batches keeps the file at the
//! `journal_size_limit` ceiling instead.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context as _, anyhow};
use naiad_core::Tag;
use naiad_netproto::{Account, RelKind, RelationSubmission};

use crate::RepoStore;
use crate::bridge::hydrus_wire::{Action, SkipCounts, Update, decode_update};
use crate::bridge::ptr_client::MetadataEntry;
use crate::bridge::sidecar::Sidecar;
use crate::bridge::sync::UpdateSource;

/// Log an apply heartbeat roughly every N mutations (#226): crossing-detector
/// so chunk sizes need not divide the interval.
const HEARTBEAT_INTERVAL: u64 = 1_000_000;

/// Returns `true` when `now` crosses an `interval` boundary that `prev` had not
/// yet crossed — the heartbeat-crossing predicate extracted as a pure function
/// so it can be unit-tested independently of the log plumbing (#226).
fn heartbeat_crossed(prev: u64, now: u64, interval: u64) -> bool {
    now / interval != prev / interval
}

/// The optional relations destination for a sidecar sync pass (#225): the
/// serving `RepoStore` and the persisted bridge author. `None` on paths that do
/// not apply relations (e.g. relation-agnostic tests); `Some` on the follow-loop
/// and CLI sync, where bridged PTR siblings/parents are written to the store.
pub type RelationsTarget<'a> = Option<(&'a RepoStore, &'a Account)>;

fn hex32(s: &str) -> Option<[u8; 32]> {
    let b = hex::decode(s).ok()?;
    <[u8; 32]>::try_from(b.as_slice()).ok()
}

/// Mapping-RMW hashes per sub-batch transaction (#231).
///
/// Each touched hash dirties ~1–2 B-tree leaf pages of `bucket_map` (scattered
/// keys on a multi-GB tree rarely share leaves), so 10 000 hashes commit
/// roughly 40–80 MB of WAL at the default 4 KB page size — the same order as
/// [`crate::bridge::sidecar::SIDECAR_WAL_SIZE_LIMIT`], so the between-batch checkpoint keeps
/// the file near that ceiling.
const SYNC_APPLY_BATCH_HASHES: usize = 10_000;

/// Best-effort `wal_checkpoint(TRUNCATE)` after a sub-batch commit (#231).
///
/// Advisory: a failed or blocked checkpoint must not fail the sync pass — the
/// next boundary retries. `busy` is logged at WARN because a *persistently*
/// busy checkpoint is exactly the reader-starvation signature this exists to
/// surface.
fn checkpoint_after_batch(sidecar: &Sidecar, index: u64) {
    match sidecar.checkpoint_wal() {
        Ok(cp) if cp.busy => tracing::warn!(
            target: "bridge",
            index,
            log_frames = cp.log_frames,
            checkpointed_frames = cp.checkpointed_frames,
            "sidecar sync: WAL checkpoint blocked by a reader; starvation if persistent"
        ),
        Ok(cp) => tracing::debug!(
            target: "bridge",
            index,
            log_frames = cp.log_frames,
            checkpointed_frames = cp.checkpointed_frames,
            "sidecar sync: WAL checkpointed"
        ),
        Err(e) => tracing::warn!(
            target: "bridge",
            index,
            error = %format!("{e:#}"),
            "sidecar sync: WAL checkpoint failed"
        ),
    }
}

/// Result of one sidecar sync pass (#207 cron contract).
///
/// The caller holds the before/after cursors via [`Sidecar::next_update_index`];
/// `next_due` lets the CLI schedule the next run without a second PTR round-trip.
#[derive(Debug)]
pub struct SidecarSyncReport {
    /// PTR-advertised Unix timestamp for the next expected update.
    pub next_due: u64,
    /// Number of update indexes applied this pass (0 when already up to date).
    pub indexes_applied: u64,
    /// Total `(hash, tag)` mutations applied across all indexes.
    pub mappings_applied: u64,
}

/// Fetch and apply every PTR update index newer than the sidecar cursor.
///
/// Returns a [`SidecarSyncReport`] so the caller can schedule the next run and
/// print a summary without a second PTR round-trip.
///
/// # Errors
/// Returns an error on a fetch/decode failure, an unresolved `service_hash_id`,
/// a malformed hash def, or a sidecar write failure. The failing index number is
/// included in the error chain.
pub fn sync_once(
    sidecar: &Sidecar,
    src: &mut dyn UpdateSource,
    relations: RelationsTarget<'_>,
) -> anyhow::Result<SidecarSyncReport> {
    let since = sidecar.next_update_index()?;
    let meta = src.metadata(since).context("fetching metadata")?;
    let mut entries: Vec<&MetadataEntry> = meta
        .entries
        .iter()
        .filter(|e| e.update_index >= since)
        .collect();
    entries.sort_by_key(|e| e.update_index);
    let pending = entries.len();
    if pending > 0 {
        tracing::info!(
            target: "bridge",
            cursor = since,
            pending,
            "sidecar sync: pass start"
        );
    } else {
        tracing::debug!(
            target: "bridge",
            cursor = since,
            pending,
            "sidecar sync: pass start"
        );
    }
    let mut indexes_applied: u64 = 0;
    let mut mappings_applied: u64 = 0;
    for entry in entries {
        mappings_applied += apply_index(sidecar, src, relations, entry)
            .with_context(|| format!("applying update index {}", entry.update_index))?;
        indexes_applied += 1;
    }
    Ok(SidecarSyncReport {
        next_due: meta.next_update_due,
        indexes_applied,
        mappings_applied,
    })
}

/// Apply one update index (defs tx → content RMW in bounded sub-batches →
/// cursor in the final sub-batch), checkpointing the WAL between commits.
///
/// This is the F14 sub-batch design, implemented for #231: a whole-index
/// transaction pins WAL frames for its entire (possibly multi-hour) run.
/// Cursor atomicity is preserved — `next_update_index` advances only in the
/// last transaction, so an interruption anywhere replays the full index and
/// every write path is idempotent (see the module doc).
fn apply_index(
    sidecar: &Sidecar,
    src: &mut dyn UpdateSource,
    relations: RelationsTarget<'_>,
    entry: &MetadataEntry,
) -> anyhow::Result<u64> {
    apply_index_batched(
        sidecar,
        src,
        relations,
        entry,
        SYNC_APPLY_BATCH_HASHES,
        HEARTBEAT_INTERVAL,
    )
}

/// [`apply_index`] with explicit sub-batch size and heartbeat interval (tests
/// shrink both to force multi-batch execution and crossing without large
/// fixtures; production paths pass the module-level constants).
fn apply_index_batched(
    sidecar: &Sidecar,
    src: &mut dyn UpdateSource,
    relations: RelationsTarget<'_>,
    entry: &MetadataEntry,
    batch_hashes: usize,
    heartbeat_interval: u64,
) -> anyhow::Result<u64> {
    let batch_hashes = batch_hashes.max(1);
    let mut decoded = Vec::new();
    let fetch_start = Instant::now();
    let mut total_bytes: u64 = 0;
    for h in &entry.update_hashes {
        let bytes = src.fetch_update(h).with_context(|| format!("update {h}"))?;
        total_bytes += bytes.len() as u64;
        decoded.push(decode_update(&bytes).with_context(|| format!("decoding update {h}"))?);
    }
    tracing::info!(
        target: "bridge",
        index = entry.update_index,
        update_files = entry.update_hashes.len(),
        bytes = total_bytes,
        elapsed_ms = fetch_start.elapsed().as_millis() as u64,
        "sidecar sync: fetched + decoded update files"
    );

    let tx = sidecar.conn().unchecked_transaction()?;

    // Definitions first (own transaction), so content can resolve ids from
    // this index. Committed before the mapping sub-batches: on a mid-index
    // failure these rows persist, which is safe — they are upserts and the
    // unmoved cursor replays them identically.
    let mut defs_count: u64 = 0;
    let mut dropped_tags: u64 = 0;
    for u in &decoded {
        if let Update::Definitions(d) = u {
            // Hard error on malformed hex — spec §Error handling anti-silence.
            let hrows: Vec<(u64, [u8; 32])> = d
                .hashes
                .iter()
                .map(|(id, hx)| {
                    hex32(hx).map(|a| (*id, a)).ok_or_else(|| {
                        anyhow!("defs_hashes: service_hash_id {id} has malformed hex {hx:?}")
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            defs_count += hrows.len() as u64;
            sidecar.insert_defs_hashes(&hrows)?;
            // F11: normalize to the seed's Tag::parse form; drop unparseable.
            let trows: Vec<(u64, String)> = d
                .tags
                .iter()
                .filter_map(|(id, raw)| Tag::parse(raw).ok().map(|t| (*id, t.to_string())))
                .collect();
            dropped_tags += (d.tags.len() - trows.len()) as u64;
            defs_count += trows.len() as u64;
            sidecar.insert_defs_tags(&trows)?;
        }
    }
    tx.commit()
        .with_context(|| format!("committing defs for index {}", entry.update_index))?;
    checkpoint_after_batch(sidecar, entry.update_index);

    // Content: gather per-hash mutations in wire order (F1-gap), then apply once
    // per hash (each apply_mutations does one RMW over the pre-index set + this
    // index's ordered mutations). Resolution reads see the defs committed above.
    let mut order: Vec<[u8; 32]> = Vec::new();
    let mut muts: HashMap<[u8; 32], Vec<(u64, bool)>> = HashMap::new();
    let mut count = 0u64;
    for u in &decoded {
        if let Update::Content(c) = u {
            for row in &c.mappings {
                let is_delete = row.action == Action::Delete;
                for hid in &row.hash_ids {
                    let sha = sidecar.sha256_for(*hid)?.ok_or_else(|| {
                        anyhow!("content references service_hash_id {hid} absent from defs_hashes")
                    })?;
                    muts.entry(sha)
                        .or_insert_with(|| {
                            order.push(sha);
                            Vec::new()
                        })
                        .push((row.tag_id, is_delete));
                    count += 1;
                }
            }
        }
    }

    // Apply the RMWs in bounded sub-batches; the LAST batch is left to the
    // final transaction below so it commits atomically with the relation apply
    // and the cursor. `chunks` is empty for a defs-only index.
    let chunks: Vec<&[[u8; 32]]> = order.chunks(batch_hashes).collect();
    let batches = chunks.len().max(1);
    // Heartbeat (#226): crossing-detector over HEARTBEAT_INTERVAL mutations so
    // chunk sizes need not divide the interval. `heartbeat_prev` is the
    // `rows_applied` value at the last emit; rate resets at each emit so a
    // stalled chunk doesn't amortise a fast one.
    let mut rows_applied: u64 = 0;
    let mut heartbeat_prev: u64 = 0;
    let mut heartbeat_inst = Instant::now();
    for chunk in chunks.iter().take(chunks.len().saturating_sub(1)) {
        let tx = sidecar.conn().unchecked_transaction()?;
        for sha in *chunk {
            sidecar.apply_mutations(sha, &muts[sha])?;
            rows_applied += muts[sha].len() as u64;
        }
        tx.commit().with_context(|| {
            format!(
                "committing mapping sub-batch of index {}",
                entry.update_index
            )
        })?;
        checkpoint_after_batch(sidecar, entry.update_index);
        if heartbeat_crossed(heartbeat_prev, rows_applied, heartbeat_interval) {
            let secs = heartbeat_inst.elapsed().as_secs_f64().max(1e-9);
            let rows_per_sec = ((rows_applied - heartbeat_prev) as f64 / secs) as u64;
            tracing::info!(
                target: "bridge",
                index = entry.update_index,
                rows_applied,
                rows_total = count,
                rows_per_sec,
                "sidecar sync: apply heartbeat"
            );
            heartbeat_prev = rows_applied;
            heartbeat_inst = Instant::now();
        }
    }

    // Final transaction: last mapping sub-batch + cursor (relations, on their
    // own RepoStore connection, are applied just before the commit — the #225
    // crash-safety ordering is preserved).
    let tx = sidecar.conn().unchecked_transaction()?;
    if let Some(last) = chunks.last() {
        for sha in *last {
            sidecar.apply_mutations(sha, &muts[sha])?;
            rows_applied += muts[sha].len() as u64;
        }
        // Emit any final heartbeat crossing from the last chunk.
        if heartbeat_crossed(heartbeat_prev, rows_applied, heartbeat_interval) {
            let secs = heartbeat_inst.elapsed().as_secs_f64().max(1e-9);
            let rows_per_sec = ((rows_applied - heartbeat_prev) as f64 / secs) as u64;
            tracing::info!(
                target: "bridge",
                index = entry.update_index,
                rows_applied,
                rows_total = count,
                rows_per_sec,
                "sidecar sync: apply heartbeat"
            );
        }
    }

    let mut skips = SkipCounts::default();
    for u in &decoded {
        match u {
            Update::Content(c) => skips.merge(c.skips),
            Update::Definitions(d) => skips.unknown_def_kind += d.unknown_def_kind,
        }
    }

    // #225: apply bridged sibling/parent relations to the serving RepoStore
    // BEFORE the sidecar cursor advances and commits. This ordering is the
    // crash-safety invariant (spec §7): the cursor advances only inside the
    // final sidecar tx below, so if the relation apply fails (or the process
    // crashes) here — before `set_next_update_index`/`tx.commit` — the final tx
    // rolls back, the cursor stays put, and `sync_once` replays this whole
    // index (earlier sub-batches persist but replay idempotently, see the
    // module doc). On replay the mapping RMW and the relation apply are both
    // idempotent (the latter by `apply_bridge_relations`'s no-op guard), so no
    // rows are lost and no `seq` churns. The relation apply is still a SEPARATE
    // transaction on the RepoStore connection (the sidecar mapping+cursor tx is
    // untouched — ADR 0028 stays as designed); endpoints resolve through the
    // sidecar's own (already-normalized) defs_tags map, committed in the defs
    // transaction above.
    let (mut siblings_built, mut parents_built, mut dropped_relations) = (0u64, 0u64, 0u64);
    if let Some((repo, bridge)) = relations {
        let mut need_tag_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for u in &decoded {
            if let Update::Content(c) = u {
                for rel in c.siblings.iter().chain(c.parents.iter()) {
                    need_tag_ids.insert(rel.from_id);
                    need_tag_ids.insert(rel.to_id);
                }
            }
        }
        let tag_id_vec: Vec<u64> = need_tag_ids.into_iter().collect();
        let tags_map = sidecar.defs_tags_for(&tag_id_vec)?;
        let mut rel_subs: Vec<RelationSubmission> = Vec::new();
        for u in &decoded {
            if let Update::Content(c) = u {
                for (rows, kind, built) in [
                    (&c.siblings, RelKind::Sibling, &mut siblings_built),
                    (&c.parents, RelKind::Parent, &mut parents_built),
                ] {
                    for rel in rows {
                        match crate::bridge::sync::build_relation_sub(bridge, kind, rel, &tags_map)?
                        {
                            Some(sub) => {
                                rel_subs.push(sub);
                                *built += 1;
                            }
                            None => dropped_relations += 1,
                        }
                    }
                }
            }
        }
        repo.apply_bridge_relations(&rel_subs).with_context(|| {
            format!("applying bridge relations for index {}", entry.update_index)
        })?;
    }

    // Cursor in the same transaction as the FINAL mapping sub-batch (the ADR
    // 0028 correctness win, sub-batched per F14/#231), committed only AFTER
    // the relation apply above succeeds.
    sidecar.set_next_update_index(entry.update_index + 1)?;
    tx.commit()
        .with_context(|| format!("committing index {}", entry.update_index))?;
    checkpoint_after_batch(sidecar, entry.update_index);

    let unknown = skips.unknown_content_type + skips.unknown_def_kind;
    if unknown > 0 {
        tracing::warn!(
            target: "bridge",
            index = entry.update_index,
            defs = defs_count,
            mappings = count,
            batches,
            dropped_tags,
            siblings = siblings_built,
            parents = parents_built,
            dropped_relations,
            unknown_content_type = skips.unknown_content_type,
            unknown_def_kind = skips.unknown_def_kind,
            "sidecar sync: applied index with UNKNOWN content types - possible PTR format drift"
        );
    } else {
        tracing::info!(
            target: "bridge",
            index = entry.update_index,
            defs = defs_count,
            mappings = count,
            batches,
            dropped_tags,
            siblings = siblings_built,
            parents = parents_built,
            dropped_relations,
            "sidecar sync: applied index"
        );
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::ptr_client::{Metadata, MetadataEntry};
    use crate::bridge::sync::UpdateSource;

    struct Fake {
        meta: Metadata,
        files: HashMap<String, Vec<u8>>,
    }
    impl UpdateSource for Fake {
        fn metadata(&mut self, _since: u64) -> anyhow::Result<Metadata> {
            Ok(self.meta.clone())
        }
        fn fetch_update(&mut self, h: &str) -> anyhow::Result<Vec<u8>> {
            self.files
                .get(h)
                .cloned()
                .ok_or_else(|| anyhow!("no update {h}"))
        }
    }

    /// Build a single-index `Fake` with the given update hash → bytes mapping.
    fn one_index(files: HashMap<String, Vec<u8>>) -> Fake {
        let update_hashes: Vec<String> = files.keys().cloned().collect();
        Fake {
            meta: Metadata {
                entries: vec![MetadataEntry {
                    update_index: 0,
                    update_hashes,
                    begin_ts: 0,
                    end_ts: 0,
                }],
                next_update_due: 0,
            },
            files,
        }
    }

    /// Defs update: hash 500 → sha_hex, tag 800 → tag_str.
    fn defs_file(sha_hex: &str, tag_str: &str) -> Vec<u8> {
        crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            36,
            1,
            [[0, [[500, sha_hex]]], [1, [[800, tag_str]]]]
        ]))
    }

    // Build a one-index source: defs (service_hash_id 500 → sha, service_tag_id
    // 800 → "character:samus") then a content ADD of tag 800 to hash 500.
    fn add_source() -> (Fake, [u8; 32]) {
        let sha = "ab".repeat(32);
        let def = defs_file(&sha, "character:samus");
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [500]]]]]]]
        ]));
        let dh = "11".repeat(32);
        let ch = "22".repeat(32);
        let mut files = HashMap::new();
        files.insert(dh, def);
        files.insert(ch, content);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&hex::decode("ab".repeat(32)).unwrap());
        (one_index(files), arr)
    }

    #[test]
    fn sync_once_applies_and_advances_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let (mut src, sha) = add_source();
        let report = sync_once(&sidecar, &mut src, None).unwrap();
        assert_eq!(report.mappings_applied, 1, "one mapping applied");
        assert_eq!(report.indexes_applied, 1);
        assert_eq!(sidecar.read_tag_set(&sha).unwrap(), vec![800]);
        assert_eq!(sidecar.next_update_index().unwrap(), 1);
    }

    #[test]
    fn sync_once_is_idempotent_on_replay() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let (mut src, sha) = add_source();
        sync_once(&sidecar, &mut src, None).unwrap();
        // Reset the cursor and replay: the ADD is a no-op (already present).
        sidecar.set_next_update_index(0).unwrap();
        sync_once(&sidecar, &mut src, None).unwrap();
        assert_eq!(sidecar.read_tag_set(&sha).unwrap(), vec![800]);
        assert_eq!(sidecar.next_update_index().unwrap(), 1);
    }

    #[test]
    fn content_with_unresolved_hash_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        // Content-only index referencing hash id 999 with no defs → hard error.
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [999]]]]]]]
        ]));
        let ch = "33".repeat(32);
        let mut files = HashMap::new();
        files.insert(ch, content);
        let mut src = one_index(files);
        let err = sync_once(&sidecar, &mut src, None).unwrap_err();
        assert!(format!("{err:#}").contains("999"), "names the id: {err:#}");
    }

    // --- AC: wire order (F1-gap) ---

    /// Content update carrying ADD then DELETE in wire order → tag_set empty.
    #[test]
    fn wire_order_add_then_delete_nets_absent() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let sha_hex = "ab".repeat(32);
        let def = defs_file(&sha_hex, "character:samus");
        // ADD tag 800 to hash 500, then DELETE tag 800 from hash 500 — both in
        // same content block, wire order preserved. Net result: absent.
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [500]]]], [1, [[800, [500]]]]]]]
        ]));
        let mut files = HashMap::new();
        files.insert("11".repeat(32), def);
        files.insert("22".repeat(32), content);
        let mut src = one_index(files);
        sync_once(&sidecar, &mut src, None).unwrap();
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&hex::decode("ab".repeat(32)).unwrap());
        assert!(
            sidecar.read_tag_set(&sha).unwrap().is_empty(),
            "ADD then DELETE should net absent"
        );
    }

    /// Content update carrying DELETE then ADD in wire order → tag_set present.
    #[test]
    fn wire_order_delete_then_add_nets_present() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let sha_hex = "ab".repeat(32);
        let def = defs_file(&sha_hex, "character:samus");
        // DELETE tag 800 from hash 500 (no-op on empty set), then ADD → present.
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[1, [[800, [500]]]], [0, [[800, [500]]]]]]]
        ]));
        let mut files = HashMap::new();
        files.insert("11".repeat(32), def);
        files.insert("22".repeat(32), content);
        let mut src = one_index(files);
        sync_once(&sidecar, &mut src, None).unwrap();
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&hex::decode("ab".repeat(32)).unwrap());
        assert_eq!(
            sidecar.read_tag_set(&sha).unwrap(),
            vec![800],
            "DELETE then ADD should net present"
        );
    }

    // --- AC: F11 normalization ---

    /// Tag::parse normalizes: trims whitespace, collapses internal runs,
    /// lowercases. Denormalized input is stored in canonical form; unparseable
    /// (empty subtag) is dropped.
    ///
    /// Tag::parse fact established for this test:
    ///   `"Character:  Samus   Aran "` → Tag { ns="character", sub="samus aran" }
    ///   → Display "character:samus aran"   (rewrite, not a no-op)
    ///   `"character:"` → Err(EmptyTag)     (dropped)
    #[test]
    fn f11_normalization_rewrites_and_drops() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let sha_hex = "cd".repeat(32);
        // Defs: two tags — one denormalized (id 801), one unparseable (id 802).
        let def = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            36,
            1,
            [
                [0, [[500, sha_hex]]],
                [1, [[801, "Character:  Samus   Aran "], [802, "character:"]]]
            ]
        ]));
        let mut files = HashMap::new();
        files.insert("aa".repeat(32), def);
        let mut src = one_index(files);
        sync_once(&sidecar, &mut src, None).unwrap();

        let tags = sidecar.defs_tags_for(&[801, 802]).unwrap();
        assert_eq!(
            tags.get(&801).map(String::as_str),
            Some("character:samus aran"),
            "denormalized tag must be stored in Tag::parse canonical form"
        );
        assert!(
            !tags.contains_key(&802),
            "unparseable tag (empty subtag) must be dropped"
        );
    }

    // --- AC: crash safety is cursor-driven (#231 sub-batch contract) ---

    /// An index whose content references an unresolvable service_hash_id fails
    /// before any mapping sub-batch commits: cursor stays 0 and bucket_map is
    /// empty. The defs, committed in their own transaction (#231), PERSIST —
    /// that is safe because they are upserts and the unmoved cursor replays
    /// this index identically.
    #[test]
    fn failed_index_leaves_cursor_and_mappings_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let sha_hex = "ef".repeat(32);
        // Defs introduce hash 500 and tag 800.
        let def = defs_file(&sha_hex, "character:samus");
        // Content: first ADD tag 800 to hash 500 (resolvable), then ADD tag 800
        // to hash 999 (unresolvable — never in defs).
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [500, 999]]]]]]]
        ]));
        let mut files = HashMap::new();
        files.insert("bb".repeat(32), def);
        files.insert("cc".repeat(32), content);
        let mut src = one_index(files);
        let err = sync_once(&sidecar, &mut src, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("999"),
            "error must name the bad id: {err:#}"
        );

        // The replay guarantees: cursor unmoved, no mappings visible.
        assert_eq!(
            sidecar.next_update_index().unwrap(),
            0,
            "cursor must stay 0 after a failed index"
        );
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&hex::decode("ef".repeat(32)).unwrap());
        assert!(
            sidecar.read_tag_set(&sha).unwrap().is_empty(),
            "bucket_map must be empty after a failed index"
        );
        // Defs from the committed defs transaction persist (#231 contract).
        let tags = sidecar.defs_tags_for(&[800]).unwrap();
        assert_eq!(
            tags.get(&800).map(String::as_str),
            Some("character:samus"),
            "defs commit in their own transaction and persist across the failure"
        );
    }

    // --- AC: #231 sub-batched apply ---

    /// Two hashes: 500 → "ab"*32, 501 → "cd"*32, both mapped to tag 800.
    fn two_hash_source_entry() -> (Fake, MetadataEntry, [u8; 32], [u8; 32]) {
        let sha_a = "ab".repeat(32);
        let sha_b = "cd".repeat(32);
        let def = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            36,
            1,
            [
                [0, [[500, sha_a], [501, sha_b]]],
                [1, [[800, "character:samus"]]]
            ]
        ]));
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [500, 501]]]]]]]
        ]));
        let mut files = HashMap::new();
        files.insert("11".repeat(32), def);
        files.insert("22".repeat(32), content);
        let src = one_index(files);
        let entry = src.meta.entries[0].clone();
        let mut a = [0u8; 32];
        a.copy_from_slice(&hex::decode("ab".repeat(32)).unwrap());
        let mut b = [0u8; 32];
        b.copy_from_slice(&hex::decode("cd".repeat(32)).unwrap());
        (src, entry, a, b)
    }

    /// With `batch_hashes = 1`, a two-hash index runs as defs tx + one
    /// committed sub-batch + the final (mapping + cursor) tx — and must land in
    /// exactly the same end state as the single-transaction path did.
    #[test]
    fn multi_batch_apply_reaches_the_same_end_state() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let (mut src, entry, sha_a, sha_b) = two_hash_source_entry();
        let n =
            apply_index_batched(&sidecar, &mut src, None, &entry, 1, HEARTBEAT_INTERVAL).unwrap();
        assert_eq!(n, 2, "both mappings applied");
        assert_eq!(sidecar.read_tag_set(&sha_a).unwrap(), vec![800]);
        assert_eq!(sidecar.read_tag_set(&sha_b).unwrap(), vec![800]);
        assert_eq!(sidecar.next_update_index().unwrap(), 1, "cursor advanced");
    }

    /// Mid-index failure with committed sub-batches: force the final tx to fail
    /// (read-only RepoStore sabotages the relation apply) after the first
    /// sub-batch committed. The cursor must stay 0; a replay against a healthy
    /// target must converge to the full end state (the module-doc idempotency
    /// argument, exercised end to end).
    #[test]
    fn committed_sub_batches_replay_idempotently_after_failure() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let bridge = crate::bridge::load_bridge_author(&dir.path().join("state.db")).unwrap();

        // Content carries a sibling so the relation apply actually runs.
        let sha_a = "ab".repeat(32);
        let sha_b = "cd".repeat(32);
        let def = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            36,
            1,
            [
                [0, [[500, sha_a], [501, sha_b]]],
                [1, [[800, "character:samus"], [801, "series:metroid"]]]
            ]
        ]));
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [500, 501]]]]]], [1, [[0, [[800, 801]]]]]]
        ]));
        let mut files = HashMap::new();
        files.insert("11".repeat(32), def);
        files.insert("22".repeat(32), content);
        let mut src = one_index(files);
        let entry = src.meta.entries[0].clone();

        // Sabotaged first attempt: relation writes error in the final tx.
        let broken = RepoStore::open(dir.path().join("repo.db")).unwrap();
        broken.apply_read_only_serve_pragmas().unwrap();
        let err = apply_index_batched(
            &sidecar,
            &mut src,
            Some((&broken, &bridge)),
            &entry,
            1,
            HEARTBEAT_INTERVAL,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("bridge relations"),
            "expected the relation apply to fail: {err:#}"
        );
        assert_eq!(
            sidecar.next_update_index().unwrap(),
            0,
            "cursor must stay 0 — the index will replay"
        );
        drop(broken);

        // Replay against a healthy store: converges to the full end state.
        let repo = RepoStore::open(dir.path().join("repo2.db")).unwrap();
        let n = apply_index_batched(
            &sidecar,
            &mut src,
            Some((&repo, &bridge)),
            &entry,
            1,
            HEARTBEAT_INTERVAL,
        )
        .unwrap();
        assert_eq!(n, 2);
        let mut a = [0u8; 32];
        a.copy_from_slice(&hex::decode("ab".repeat(32)).unwrap());
        let mut b = [0u8; 32];
        b.copy_from_slice(&hex::decode("cd".repeat(32)).unwrap());
        assert_eq!(sidecar.read_tag_set(&a).unwrap(), vec![800]);
        assert_eq!(sidecar.read_tag_set(&b).unwrap(), vec![800]);
        assert_eq!(sidecar.next_update_index().unwrap(), 1);
        assert_eq!(repo.relations().unwrap().siblings.len(), 1);
    }

    /// After a sync pass with no concurrent readers, the between-batch
    /// `wal_checkpoint(TRUNCATE)` calls must leave `*.db-wal` at zero bytes —
    /// the #231 acceptance criterion that the WAL stays bounded.
    #[test]
    fn sync_pass_truncates_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        let sidecar = Sidecar::create(&db).unwrap();
        let (mut src, _sha) = add_source();
        sync_once(&sidecar, &mut src, None).unwrap();
        let wal_len = std::fs::metadata(dir.path().join("s.db-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(
            wal_len, 0,
            "WAL must be truncated after the final checkpoint (got {wal_len} bytes)"
        );
    }

    // --- AC: hard error on malformed hash def ---

    /// A defs update with a non-64-hex string for a hash must produce a hard
    /// error naming the service_hash_id, and the cursor must remain at 0.
    #[test]
    fn malformed_hash_def_errors_naming_id() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        // Hash id 777 carries "not-valid-hex" — 13 chars, not 64.
        let def = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            36,
            1,
            [[0, [[777, "not-valid-hex"]]], [1, []]]
        ]));
        let mut files = HashMap::new();
        files.insert("dd".repeat(32), def);
        let mut src = one_index(files);
        let err = sync_once(&sidecar, &mut src, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("777"), "error must name the id: {msg}");
        assert_eq!(
            sidecar.next_update_index().unwrap(),
            0,
            "cursor must stay 0 after malformed-hex error"
        );
    }

    // --- AC: idle path (report struct) ---

    /// Running sync_once twice without touching the cursor: the second pass sees
    /// no new update indexes and returns zeros (mirrors sync.rs idle-path test).
    #[test]
    fn second_pass_when_up_to_date_is_idle() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let (mut src, _sha) = add_source();
        sync_once(&sidecar, &mut src, None).unwrap();
        // Second pass: cursor is already at 1, metadata entry is index 0 → filtered out.
        let report = sync_once(&sidecar, &mut src, None).unwrap();
        assert_eq!(report.indexes_applied, 0, "no new indexes on second pass");
        assert_eq!(report.mappings_applied, 0, "no new mappings on second pass");
    }

    // --- AC: #225 relations land in the RepoStore, mapping stays in the sidecar ---

    /// A content index carrying both a mapping and a sibling: the mapping lands in
    /// the sidecar `bucket_map`, and the sibling lands in the serving RepoStore's
    /// relations table authored by the bridge key. A replay is a relation no-op.
    #[test]
    fn sync_once_applies_relations_to_repo_store() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let repo = RepoStore::open(dir.path().join("repo.db")).unwrap();
        let bridge = crate::bridge::load_bridge_author(&dir.path().join("state.db")).unwrap();

        let sha_hex = "ab".repeat(32);
        let def = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            36,
            1,
            [
                [0, [[500, sha_hex]]],
                [1, [[800, "character:samus"], [801, "series:metroid"]]]
            ]
        ]));
        // mapping: tag 800 → hash 500 ; sibling ADD: 800 → 801.
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [500]]]]]], [1, [[0, [[800, 801]]]]]]
        ]));
        let mut files = HashMap::new();
        files.insert("11".repeat(32), def);
        files.insert("22".repeat(32), content);
        let mut src = one_index(files);

        sync_once(&sidecar, &mut src, Some((&repo, &bridge))).unwrap();

        // Mapping stayed in the sidecar.
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&hex::decode("ab".repeat(32)).unwrap());
        assert_eq!(sidecar.read_tag_set(&sha).unwrap(), vec![800]);

        // Sibling landed in the RepoStore, authored by the bridge key.
        let g = repo.relations().unwrap();
        assert_eq!(g.siblings.len(), 1);
        assert_eq!(g.siblings[0].from, "character:samus");
        assert_eq!(g.siblings[0].to, "series:metroid");
        assert_eq!(g.siblings[0].author, bridge.public_hex());
        let cursor = repo.relation_cursor().unwrap();

        // Replay (reset sidecar cursor): relation apply is a no-op.
        sidecar.set_next_update_index(0).unwrap();
        sync_once(&sidecar, &mut src, Some((&repo, &bridge))).unwrap();
        assert_eq!(
            repo.relation_cursor().unwrap(),
            cursor,
            "replay must not churn the relation seq"
        );
    }

    // --- AC: #226 heartbeat crossing predicate ---

    /// `heartbeat_crossed` fires when `now` crosses a boundary `prev` had not yet
    /// reached. Three cases: exact boundary, non-chunk-aligned mid-interval
    /// crossing, and the no-double-fire guarantee when both values are inside the
    /// same interval bucket.
    #[test]
    fn heartbeat_crossed_fires_on_interval_boundary() {
        // Exact boundary: prev=0, now=interval → prev/I=0, now/I=1 → crossed.
        assert!(heartbeat_crossed(0, 5, 5), "exact boundary must fire");
        // Non-aligned: prev=3, now=7, interval=5 → prev/5=0, now/5=1 → crossed.
        assert!(
            heartbeat_crossed(3, 7, 5),
            "non-chunk-aligned crossing must fire"
        );
        // No double-fire: prev=5, now=9, interval=5 → both in bucket 1 → no cross.
        assert!(
            !heartbeat_crossed(5, 9, 5),
            "within same bucket must not fire (no double-fire)"
        );
        // Still within first bucket: prev=0, now=4 → no cross.
        assert!(!heartbeat_crossed(0, 4, 5), "below interval must not fire");
        // Two-bucket jump: prev=0, now=10, interval=5 → crosses at least once.
        assert!(heartbeat_crossed(0, 10, 5), "two-bucket jump must fire");
    }

    /// `apply_index_batched` with batch_hashes=1 and heartbeat_interval=1 on a
    /// two-hash index: every hash-apply crosses the interval boundary, so the
    /// crossing predicate is exercised at a non-chunk-aligned point (after the
    /// first hash, rows_applied=1 crosses interval=1 from prev=0). The end-state
    /// must match the single-batch path — the test verifies the interval parameter
    /// is plumbed through without corrupting the apply logic.
    #[test]
    fn apply_index_batched_with_small_heartbeat_interval_completes_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let (mut src, entry, sha_a, sha_b) = two_hash_source_entry();
        // batch_hashes=1 forces a mid-index sub-batch; heartbeat_interval=1
        // ensures the crossing predicate fires after the very first hash-apply
        // (non-chunk-aligned: rows_applied=1 with prev=0, interval=1).
        let n = apply_index_batched(&sidecar, &mut src, None, &entry, 1, 1).unwrap();
        assert_eq!(n, 2, "both mappings must be applied");
        assert_eq!(
            sidecar.read_tag_set(&sha_a).unwrap(),
            vec![800],
            "sha_a must carry tag 800"
        );
        assert_eq!(
            sidecar.read_tag_set(&sha_b).unwrap(),
            vec![800],
            "sha_b must carry tag 800"
        );
        assert_eq!(
            sidecar.next_update_index().unwrap(),
            1,
            "cursor must advance"
        );
    }

    /// Crash-safety ordering (#225 §7): if the relation apply fails, the sidecar
    /// cursor must NOT advance, so `sync_once` retries the whole index. We force
    /// the failure by marking the RepoStore `query_only` — its relation writes
    /// error — and assert the cursor stays 0 and the sidecar mappings rolled back.
    #[test]
    fn relation_apply_failure_leaves_cursor_unadvanced() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Sidecar::create(dir.path().join("s.db")).unwrap();
        let repo = RepoStore::open(dir.path().join("repo.db")).unwrap();
        // Sabotage: writes to this store now error (attempt to write readonly db).
        repo.apply_read_only_serve_pragmas().unwrap();
        let bridge = crate::bridge::load_bridge_author(&dir.path().join("state.db")).unwrap();

        let sha_hex = "ab".repeat(32);
        let def = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            36,
            1,
            [
                [0, [[500, sha_hex]]],
                [1, [[800, "character:samus"], [801, "series:metroid"]]]
            ]
        ]));
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [500]]]]]], [1, [[0, [[800, 801]]]]]]
        ]));
        let mut files = HashMap::new();
        files.insert("11".repeat(32), def);
        files.insert("22".repeat(32), content);
        let mut src = one_index(files);

        let err = sync_once(&sidecar, &mut src, Some((&repo, &bridge))).unwrap_err();
        assert!(
            format!("{err:#}").contains("bridge relations"),
            "error should name the failed relation apply: {err:#}"
        );
        // Cursor stayed 0 → the index will be replayed on the next pass.
        assert_eq!(
            sidecar.next_update_index().unwrap(),
            0,
            "cursor must not advance when the relation apply fails"
        );
        // The sidecar mapping tx rolled back too — the whole index is atomic.
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&hex::decode("ab".repeat(32)).unwrap());
        assert!(
            sidecar.read_tag_set(&sha).unwrap().is_empty(),
            "sidecar mappings must roll back when the index fails"
        );
    }
}
