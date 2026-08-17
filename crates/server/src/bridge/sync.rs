//! Sync loop (issue #124): pull metadata since the stored cursor, apply each
//! new update index in order (definitions before content), advance the Hydrus
//! cursor only after the repo commit. Crash-safe: the repo apply is an
//! idempotent upsert and the cursor advances forward only.
//!
//! **Write-lock window**: `apply_index` commits 50k-row chunks over the
//! bridge's RW connection; on large PTR indices this can delay concurrent HTTP
//! submission writes up to the 10-second busy_timeout — see the comment in
//! `apply_index` for details.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, anyhow};
use naiad_core::Tag;
use naiad_netproto::{Account, Op, RelKind, RelationSubmission};

use crate::RepoStore;
use crate::bridge::hydrus_wire::{Action, RelationRow, Update, decode_update};
use crate::bridge::ptr_client::{Metadata, MetadataEntry, PtrClient};
use crate::bridge::state::StateDb;

/// Minimum poll interval when following the PTR. Steady-state sleeps follow
/// the PTR's advertised `next_update_due`; this floor only governs the
/// overdue re-check and failure-retry cadence. Hourly is plenty for a
/// roughly-daily update stream and politer than the PTR's own 240 s minimum.
pub const MIN_POLL_SECS: u64 = 3600;

/// Re-emit the "sync pass failed" WARN once every this many consecutive
/// failures. A persistently unreachable or misconfigured upstream would
/// otherwise log an identical WARN every poll (`MIN_POLL_SECS`) forever; at 15
/// the reminder lands roughly every 15 h (15 × 3600 s). Identical failures in
/// between are logged at DEBUG, so the default INFO console stays quiet.
const SYNC_FAIL_REWARN_EVERY: u64 = 15;

/// Whether a sync failure should surface at WARN (vs DEBUG). True for the first
/// failure of a streak, whenever the error text changes, or on the periodic
/// reminder — so a stuck upstream stays visible without spamming every poll.
pub(crate) fn should_warn_sync_failure(consecutive_failures: u64, error_changed: bool) -> bool {
    consecutive_failures == 1 || error_changed || consecutive_failures % SYNC_FAIL_REWARN_EVERY == 0
}

/// The subset of `PtrClient` that `sync_once` needs — abstracted so tests can
/// inject a fake without HTTP.
pub trait UpdateSource {
    fn metadata(&mut self, since: u64) -> anyhow::Result<Metadata>;
    fn fetch_update(&mut self, hash_hex: &str) -> anyhow::Result<Vec<u8>>;
}

impl UpdateSource for PtrClient {
    fn metadata(&mut self, since: u64) -> anyhow::Result<Metadata> {
        PtrClient::metadata(self, since)
    }
    fn fetch_update(&mut self, hash_hex: &str) -> anyhow::Result<Vec<u8>> {
        PtrClient::fetch_update(self, hash_hex)
    }
}

/// Result of one sync pass: the fetched metadata plus what was applied (#197).
#[derive(Debug)]
pub struct SyncReport {
    pub meta: Metadata,
    pub indexes_applied: u64,
    pub mappings_applied: u64,
}

/// One-line success summary for a one-shot sync (#197). Pure — caller passes
/// the measured duration; `after` is the state DB cursor after the pass.
pub fn summary_line(
    before: u64,
    after: u64,
    indexes_applied: u64,
    mappings_applied: u64,
    elapsed: std::time::Duration,
) -> String {
    let secs = elapsed.as_secs();
    if indexes_applied == 0 {
        format!("sync ok: cursor {before} (no new updates) in {secs}s")
    } else {
        format!(
            "sync ok: cursor {before}\u{2192}{after} ({indexes_applied} updates, {mappings_applied} mappings) in {secs}s"
        )
    }
}

/// One sync pass: fetch metadata since the stored cursor and apply every
/// update index at or after it, in ascending order.
///
/// Within each index, all `DefinitionsUpdate` files are applied before any
/// `ContentUpdate` so that hash/tag id resolution is always available.
/// The Hydrus cursor (`next_update_index`) is advanced to `index + 1` only
/// after the repo commit, so a crash mid-index is safely replayed.
///
/// Returns a [`SyncReport`] with the fetched metadata and applied counts,
/// so the caller can read `next_update_due` without a second PTR round-trip
/// and can report how many indexes and mappings were applied this pass.
pub fn sync_once(
    state: &StateDb,
    repo: &RepoStore,
    bridge: &Account,
    src: &mut dyn UpdateSource,
) -> anyhow::Result<SyncReport> {
    let since = state.next_update_index()?;
    let meta = src.metadata(since).context("fetching metadata")?;
    let mut entries: Vec<&MetadataEntry> = meta
        .entries
        .iter()
        .filter(|e| e.update_index >= since)
        .collect();
    entries.sort_by_key(|e| e.update_index);
    let mut indexes_applied: u64 = 0;
    let mut mappings_applied: u64 = 0;
    for entry in entries {
        mappings_applied += apply_index(state, repo, bridge, src, entry)?;
        indexes_applied += 1;
    }
    // #202: refresh the persisted distinct-hash count once per pass (end-of-pass
    // recompute escape hatch). No-op when no count row exists or no mapping rows
    // were applied (guards against a wasted full-table scan on a definitions-only
    // pass where repo_mappings is untouched).
    if mappings_applied > 0 {
        if let Err(e) = repo.refresh_distinct_hash_count() {
            tracing::warn!(target: "bridge", error = %format!("{e:#}"),
                "failed to refresh distinct_hash_count after sync pass");
        }
    }
    Ok(SyncReport {
        meta,
        indexes_applied,
        mappings_applied,
    })
}

fn apply_index(
    state: &StateDb,
    repo: &RepoStore,
    bridge: &Account,
    src: &mut dyn UpdateSource,
    entry: &MetadataEntry,
) -> anyhow::Result<u64> {
    // Download and decode all files for this index.
    let mut decoded = Vec::new();
    for h in &entry.update_hashes {
        let bytes = src.fetch_update(h).with_context(|| format!("update {h}"))?;
        decoded.push(decode_update(&bytes).with_context(|| format!("decoding update {h}"))?);
    }

    // Definitions first so content can resolve ids.
    for u in &decoded {
        if let Update::Definitions(d) = u {
            state.insert_defs_hashes(&d.hashes)?;
            state.insert_defs_tags(&d.tags)?;
        }
    }

    // Collect unique tag_ids and hash_ids referenced by content updates, then
    // resolve them in two batch queries rather than one query per row/hash_id.
    let mut need_tag_ids: HashSet<u64> = HashSet::new();
    let mut need_hash_ids: HashSet<u64> = HashSet::new();
    for u in &decoded {
        if let Update::Content(c) = u {
            for row in &c.mappings {
                need_tag_ids.insert(row.tag_id);
                for hid in &row.hash_ids {
                    need_hash_ids.insert(*hid);
                }
            }
            // Relation endpoints resolve through the same defs_tags map (#225).
            for rel in c.siblings.iter().chain(c.parents.iter()) {
                need_tag_ids.insert(rel.from_id);
                need_tag_ids.insert(rel.to_id);
            }
        }
    }
    let tag_id_vec: Vec<u64> = need_tag_ids.into_iter().collect();
    let hash_id_vec: Vec<u64> = need_hash_ids.into_iter().collect();
    let tags_map: HashMap<u64, String> = state.defs_tags_for(&tag_id_vec)?;
    let hashes_map: HashMap<u64, String> = state.defs_hashes_for(&hash_id_vec)?;

    // Assemble (sha256, tag, is_delete) batch from the resolved maps.
    // Zero SQL in this inner loop — all resolution was done in the batch above.
    let mut batch: Vec<(String, String, bool)> = Vec::new();
    for u in &decoded {
        if let Update::Content(c) = u {
            for row in &c.mappings {
                let tag = tags_map
                    .get(&row.tag_id)
                    .ok_or_else(|| anyhow!("unresolved service_tag_id {}", row.tag_id))?;
                let is_delete = row.action == Action::Delete;
                for hid in &row.hash_ids {
                    let sha = hashes_map
                        .get(hid)
                        .ok_or_else(|| anyhow!("unresolved service_hash_id {hid}"))?;
                    batch.push((sha.clone(), tag.clone(), is_delete));
                }
            }
        }
    }

    let applied = batch.len() as u64;
    if !batch.is_empty() {
        // Each `apply_mappings_bulk` call commits its own transaction over the bridge's RW
        // connection; the WAL write lock is released between chunks. On a large PTR index,
        // back-to-back chunk commits can still starve a concurrent HTTP submission write (on a
        // separate connection): it waits for the lock and, if it cannot acquire within the
        // 10-second busy_timeout, ERRORS with SQLITE_BUSY rather than merely blocking. Operators
        // tuning for low write latency should know this starvation window grows with the size of
        // the index being applied — it is inherent to the chunk-commit approach.
        for chunk in batch.chunks(50_000) {
            repo.apply_mappings_bulk(chunk.iter().cloned())?;
        }
    }

    // #225: build and apply bridged sibling/parent relations in the SAME pass,
    // in wire order (siblings then parents, file by file), before the cursor
    // advances. Endpoint ids resolve through the batch tags_map above; each
    // string is Tag::parse-normalized (as the mirror stores raw defs). An
    // unparseable endpoint drops the ROW and is counted, never a hard error —
    // one bad relation must not abort an index.
    let mut rel_subs: Vec<RelationSubmission> = Vec::new();
    let mut siblings_built = 0u64;
    let mut parents_built = 0u64;
    let mut dropped_relations = 0u64;
    for u in &decoded {
        if let Update::Content(c) = u {
            for rel in &c.siblings {
                match build_relation_sub(bridge, RelKind::Sibling, rel, &tags_map)? {
                    Some(sub) => {
                        rel_subs.push(sub);
                        siblings_built += 1;
                    }
                    None => dropped_relations += 1,
                }
            }
            for rel in &c.parents {
                match build_relation_sub(bridge, RelKind::Parent, rel, &tags_map)? {
                    Some(sub) => {
                        rel_subs.push(sub);
                        parents_built += 1;
                    }
                    None => dropped_relations += 1,
                }
            }
        }
    }
    repo.apply_bridge_relations(&rel_subs)?;

    // Aggregate structured skip/unknown counts across all decoded files for this index.
    let mut skips = crate::bridge::hydrus_wire::SkipCounts::default();
    for u in &decoded {
        match u {
            Update::Content(c) => skips.merge(c.skips),
            Update::Definitions(d) => skips.unknown_def_kind += d.unknown_def_kind,
        }
    }

    // Cursor advances only after the repo commit — crash before here replays this index.
    state.set_next_update_index(entry.update_index + 1)?;
    let unknown = skips.unknown_content_type + skips.unknown_def_kind;
    if unknown > 0 {
        tracing::warn!(
            target: "bridge",
            index = entry.update_index,
            mappings = batch.len(),
            siblings = siblings_built,
            parents = parents_built,
            dropped_relations,
            unknown_content_type = skips.unknown_content_type,
            unknown_def_kind = skips.unknown_def_kind,
            "applied update index with UNKNOWN content types - possible PTR format drift"
        );
    } else {
        tracing::info!(
            target: "bridge",
            index = entry.update_index,
            mappings = batch.len(),
            siblings = siblings_built,
            parents = parents_built,
            dropped_relations,
            "applied update index"
        );
    }
    Ok(applied)
}

/// Resolve a decoded relation row's endpoints through `tags_map`, normalize each
/// with [`Tag::parse`], and sign a bridge [`RelationSubmission`]. Returns `None`
/// (drop the row, count it) when an endpoint id is unknown or an endpoint string
/// is unparseable — a single malformed relation must never abort an index (#225).
pub(crate) fn build_relation_sub(
    bridge: &Account,
    kind: RelKind,
    rel: &RelationRow,
    tags_map: &HashMap<u64, String>,
) -> anyhow::Result<Option<RelationSubmission>> {
    // Deliberate asymmetry with the mappings path (which hard-errors on an
    // unresolved id): an unresolved/unparseable relation endpoint drops just this
    // ROW (counted by the caller as `dropped_relations`), because one malformed
    // relation must never wedge the whole follow-loop on that index.
    let (Some(from_raw), Some(to_raw)) = (tags_map.get(&rel.from_id), tags_map.get(&rel.to_id))
    else {
        return Ok(None);
    };
    let (Ok(from), Ok(to)) = (Tag::parse(from_raw), Tag::parse(to_raw)) else {
        return Ok(None);
    };
    let op = match rel.action {
        Action::Add => Op::Add,
        Action::Delete => Op::Remove,
    };
    Ok(Some(bridge.sign_relation(op, kind, &from, &to)))
}

/// Follow the PTR: sync, sleep until the next update is due (min 1 h), repeat.
///
/// Transient errors (network timeouts, 5xx responses, state-db read/write
/// failures) are logged as warnings and retried after `MIN_POLL_SECS`; they
/// never terminate the loop.
///
/// # Sleep math
/// `next_update_due` is treated as an absolute Unix timestamp, consistent with
/// the `begin_ts`/`end_ts` epoch fields on the same metadata entry.
/// Task-13 live-PTR smoke test will confirm; `saturating_sub` is safe either
/// way: a small relative value saturates to 0 and the MIN_POLL_SECS floor kicks in.
pub fn follow(
    state: &StateDb,
    repo: &RepoStore,
    bridge: &Account,
    src: &mut dyn UpdateSource,
    freshness: Option<&crate::stats::freshness::SyncFreshness>,
) -> anyhow::Result<()> {
    // De-noise a stuck upstream. A failure that repeats every poll interval
    // (e.g. a mirror served with no PTR key, or an unreachable PTR) used to log
    // an identical WARN forever. Now: WARN on the first failure of a streak and
    // whenever the error text changes, a periodic WARN reminder every
    // `SYNC_FAIL_REWARN_EVERY` failures (carrying the streak length), DEBUG on
    // the identical repeats in between, and a single INFO on recovery.
    let mut consecutive_failures: u64 = 0;
    let mut last_error = String::new();
    loop {
        // sync_once returns a SyncReport with the metadata it already fetched — no second PTR round-trip needed.
        let due = match sync_once(state, repo, bridge, src) {
            Ok(report) => {
                if consecutive_failures > 0 {
                    tracing::info!(
                        target: "bridge",
                        recovered_after = consecutive_failures,
                        "sync recovered after {consecutive_failures} consecutive failed pass(es)"
                    );
                    consecutive_failures = 0;
                    last_error.clear();
                }
                // Update freshness handle (best-effort; never blocks sync).
                if let Some(f) = freshness {
                    let cursor = state.next_update_index().unwrap_or(0);
                    let now_unix = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    f.record_pass(cursor, report.mappings_applied, now_unix);
                }
                report.meta.next_update_due
            }
            Err(e) => {
                let msg = format!("{e:#}");
                consecutive_failures += 1;
                if should_warn_sync_failure(consecutive_failures, msg != last_error) {
                    tracing::warn!(
                        target: "bridge",
                        error = %msg,
                        consecutive_failures,
                        "sync pass failed; retrying every {MIN_POLL_SECS}s (identical repeats \
                         logged at DEBUG until the error changes, recovers, or the next reminder)"
                    );
                } else {
                    tracing::debug!(
                        target: "bridge",
                        error = %msg,
                        consecutive_failures,
                        "sync pass still failing; retrying after poll interval"
                    );
                }
                last_error = msg;
                std::thread::sleep(std::time::Duration::from_secs(MIN_POLL_SECS));
                continue;
            }
        };

        // next_update_due is an absolute Unix timestamp (sits beside begin_ts/end_ts epochs).
        // Task-13 live-PTR smoke test will confirm; saturating_sub is safe under either
        // interpretation (small relative value saturates to 0 and the MIN_POLL_SECS floor kicks in).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let sleep = due.saturating_sub(now).max(MIN_POLL_SECS);

        // Record for `status` (Task 10). Best-effort: a transient state-db failure
        // must not kill the loop (serve would keep running with a silently dead
        // mirror), so warn-and-retry like every other fallible step above.
        if let Err(e) = state.set_flag("next_update_due", &due.to_string()) {
            tracing::warn!(
                target: "bridge",
                error = %format!("{e:#}"),
                "state flag write failed; retrying after poll interval"
            );
            std::thread::sleep(std::time::Duration::from_secs(MIN_POLL_SECS));
            continue;
        }
        tracing::info!(target: "bridge", sleep_secs = sleep, "sync idle; sleeping");
        std::thread::sleep(std::time::Duration::from_secs(sleep));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The WARN/DEBUG decision table for a failing sync streak: the first
    /// failure warns, identical repeats stay quiet, a changed error re-warns,
    /// and a periodic reminder re-warns even on an unchanged error.
    #[test]
    fn sync_failure_warns_on_first_change_and_reminder_only() {
        // First failure of a streak always warns (error_changed is true here too).
        assert!(should_warn_sync_failure(1, true));

        // Identical repeats before the reminder stay at DEBUG.
        assert!(!should_warn_sync_failure(2, false));
        assert!(!should_warn_sync_failure(SYNC_FAIL_REWARN_EVERY - 1, false));

        // A changed error re-warns immediately, mid-streak.
        assert!(should_warn_sync_failure(2, true));

        // The periodic reminder re-warns even when the error is unchanged.
        assert!(should_warn_sync_failure(SYNC_FAIL_REWARN_EVERY, false));
        assert!(should_warn_sync_failure(2 * SYNC_FAIL_REWARN_EVERY, false));
    }

    struct Fake {
        meta: Metadata,
        files: std::collections::HashMap<String, Vec<u8>>,
    }

    impl UpdateSource for Fake {
        fn metadata(&mut self, _since: u64) -> anyhow::Result<Metadata> {
            Ok(self.meta.clone())
        }
        fn fetch_update(&mut self, h: &str) -> anyhow::Result<Vec<u8>> {
            self.files
                .get(h)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no update {h}"))
        }
    }

    /// A persisted bridge author for tests, keyed beside the given dir's state
    /// db — exercises the real `load_bridge_author` path (#225).
    fn bridge_account(dir: &std::path::Path) -> Account {
        crate::bridge::load_bridge_author(&dir.join("state.db")).unwrap()
    }

    fn fixture() -> (Fake, String, String) {
        let sha = "ab".repeat(32);
        let def = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            36,
            1,
            [[0, [[500, sha]]], [1, [[800, "character:samus"]]]]
        ]));
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [500]]]]]]]
        ]));
        let dh = "11".repeat(32);
        let ch = "22".repeat(32);
        let meta = Metadata {
            entries: vec![
                MetadataEntry {
                    update_index: 0,
                    update_hashes: vec![dh.clone()],
                    begin_ts: 0,
                    end_ts: 0,
                },
                MetadataEntry {
                    update_index: 1,
                    update_hashes: vec![ch.clone()],
                    begin_ts: 0,
                    end_ts: 0,
                },
            ],
            next_update_due: 2,
        };
        let files = std::collections::HashMap::from([(dh, def), (ch, content)]);
        (
            Fake { meta, files },
            "ab".repeat(32),
            "character:samus".to_string(),
        )
    }

    #[test]
    fn summary_line_formats_updates_and_idle() {
        use std::time::Duration;
        assert_eq!(
            summary_line(1234, 1240, 6, 152340, Duration::from_secs(84)),
            "sync ok: cursor 1234\u{2192}1240 (6 updates, 152340 mappings) in 84s"
        );
        assert_eq!(
            summary_line(1240, 1240, 0, 0, Duration::from_secs(2)),
            "sync ok: cursor 1240 (no new updates) in 2s"
        );
    }

    #[test]
    fn unresolved_service_id_is_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::RepoStore::open(dir.path().join("repo.db")).unwrap();
        let state = crate::bridge::state::StateDb::open(dir.path().join("state.db")).unwrap();
        let ch = "33".repeat(32);
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[0, [[0, [[800, [500]]]]]]]
        ]));
        let mut src = Fake {
            meta: Metadata {
                entries: vec![MetadataEntry {
                    update_index: 0,
                    update_hashes: vec![ch.clone()],
                    begin_ts: 0,
                    end_ts: 0,
                }],
                next_update_due: 1,
            },
            files: std::collections::HashMap::from([(ch, content)]),
        };
        let bridge = bridge_account(dir.path());
        let err = sync_once(&state, &repo, &bridge, &mut src).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("800") && msg.contains("service_tag_id"),
            "names the id: {msg}"
        );
    }

    /// Build a two-index source: index 0 defs (tag 800 → `from`, 801 → `to`),
    /// index 1 a single sibling group of `action` carrying the id pair (800,801).
    fn sibling_source(from: &str, to: &str, action: i64) -> Fake {
        let def = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            36,
            1,
            [[0, []], [1, [[800, from], [801, to]]]]
        ]));
        let content = crate::bridge::ptr_client::zlib_json(&serde_json::json!([
            34,
            1,
            [[1, [[action, [[800, 801]]]]]]
        ]));
        let dh = "11".repeat(32);
        let ch = "22".repeat(32);
        let meta = Metadata {
            entries: vec![
                MetadataEntry {
                    update_index: 0,
                    update_hashes: vec![dh.clone()],
                    begin_ts: 0,
                    end_ts: 0,
                },
                MetadataEntry {
                    update_index: 1,
                    update_hashes: vec![ch.clone()],
                    begin_ts: 0,
                    end_ts: 0,
                },
            ],
            next_update_due: 2,
        };
        Fake {
            meta,
            files: std::collections::HashMap::from([(dh, def), (ch, content)]),
        }
    }

    /// A sibling ADD flows end-to-end into the RepoStore's relations table,
    /// authored by the bridge key, and advances the relation cursor by one.
    #[test]
    fn sync_once_applies_bridge_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::RepoStore::open(dir.path().join("repo.db")).unwrap();
        let state = crate::bridge::state::StateDb::open(dir.path().join("state.db")).unwrap();
        let bridge = bridge_account(dir.path());
        let mut src = sibling_source("character:samus", "character:samus aran", 0);

        assert_eq!(repo.relation_cursor().unwrap(), 0);
        sync_once(&state, &repo, &bridge, &mut src).unwrap();

        let g = repo.relations().unwrap();
        assert_eq!(g.siblings.len(), 1, "one bridged sibling landed");
        assert_eq!(g.siblings[0].from, "character:samus");
        assert_eq!(g.siblings[0].to, "character:samus aran");
        assert_eq!(
            g.siblings[0].author,
            bridge.public_hex(),
            "authored by the bridge key"
        );
        assert_eq!(
            repo.relation_cursor().unwrap(),
            1,
            "relation cursor advanced by one"
        );

        // Second pass over the same metadata (Hydrus cursor reset): a replay.
        // The relation is restated as a no-op, so the relation cursor is unchanged.
        state.set_next_update_index(0).unwrap();
        sync_once(&state, &repo, &bridge, &mut src).unwrap();
        assert_eq!(
            repo.relation_cursor().unwrap(),
            1,
            "idempotent replay does not churn the relation seq"
        );
    }

    /// An unparseable relation endpoint drops the row and is NOT a hard error:
    /// the index still applies, and no relation is stored.
    #[test]
    fn sync_once_drops_unparseable_relation_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::RepoStore::open(dir.path().join("repo.db")).unwrap();
        let state = crate::bridge::state::StateDb::open(dir.path().join("state.db")).unwrap();
        let bridge = bridge_account(dir.path());
        // `to` endpoint (id 801) is "character:" — empty subtag, unparseable.
        let mut src = sibling_source("character:samus", "character:", 0);

        sync_once(&state, &repo, &bridge, &mut src).expect("bad relation must not abort the index");
        assert!(
            repo.relations().unwrap().siblings.is_empty(),
            "unparseable-endpoint row was dropped, not stored"
        );
        assert_eq!(
            state.next_update_index().unwrap(),
            2,
            "the index still advanced the Hydrus cursor"
        );
    }

    /// An idle pass (cursor already at the end, nothing to apply) does not scan
    /// or rewrite the count row.
    #[test]
    fn sync_once_idle_pass_does_not_rewrite_count_row() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::RepoStore::open(dir.path().join("repo.db")).unwrap();
        let state = crate::bridge::state::StateDb::open(dir.path().join("state.db")).unwrap();
        let (mut src, _sha, _tag) = fixture();
        let bridge = bridge_account(dir.path());
        sync_once(&state, &repo, &bridge, &mut src).unwrap();
        // Write a sentinel after the first pass (simulates external edit).
        repo.write_distinct_hash_count(42).unwrap();
        // Second pass: cursor is already at the end, indexes_applied == 0.
        let idle = sync_once(&state, &repo, &bridge, &mut src).unwrap();
        assert_eq!(idle.indexes_applied, 0, "idle pass applied nothing");
        // Sentinel must be untouched — no refresh when nothing applied.
        assert_eq!(repo.read_distinct_hash_count().unwrap(), Some(42));
    }
}
