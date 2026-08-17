//! Build the PTR sidecar from an offline Hydrus `client.db` set (#207 Task 4).
//!
//! ## Phase overview
//!
//! **Phase D** streams service-space defs into `defs_hashes` and `defs_tags` in
//! bounded-batch transactions (peak memory O(DEFS_BATCH × row size)).
//!
//! **Phase M** is split into two sub-phases:
//!
//! - **M1 (stage):** reads `hash_id` bands sequentially off the
//!   `(hash_id, tag_id)` covering index and appends each band's packed rows into
//!   a sequential, unindexed `staging.db` beside `sidecar.db`.  Random inserts
//!   into the B-tree-keyed `bucket_map` are avoided entirely.  Per-band progress
//!   is tracked in `staging.band_done`; bands already flagged in either
//!   `staging.band_done` (staged this run) or the legacy sidecar `sync_state`
//!   `seed_idband_done_{IDBAND}_{i}` (written by an old direct-write binary) are
//!   skipped.  Both tables live in `staging.db` so each band's commit is a
//!   single-file transaction — atomic under any journal mode.
//!
//! - **M2 (merge):** ATTACHes `staging.db` to the sidecar connection and does
//!   one hash-ordered `INSERT OR REPLACE INTO bucket_map SELECT hash, ids FROM
//!   stg.buckets ORDER BY hash` in a single transaction.  Hash-ordered input
//!   visits the `bucket_map` clustered leaves ~once, eliminating the per-insert
//!   random-RMW cost that made direct cold inserts decay to 25k rows/s.  After
//!   commit, sets `seed_merge_done`, DETACHes, deletes `staging.db` (and its
//!   `-journal`/`-wal`/`-shm` sidecars), then writes the watermark.
//!
//! ## Flag stores
//!
//! - `staging.band_done(i)` — per-band M1 progress in `staging.db`.
//! - `sidecar.sync_state seed_idband_done_{IDBAND}_{i}` — **legacy** flags from
//!   an old direct-write binary; honored as done in M1 but never written by this
//!   code.
//! - `sidecar.sync_state seed_merge_done` — set after a successful M2; guards
//!   the whole Phase M block so a resume after a complete run goes straight to
//!   the watermark step.
//!
//! ## Single-writer assumption
//!
//! At most one seeder process may write `sidecar.db`/`staging.db` at a time.
//! An old direct-write binary and the new staged binary must **not** run
//! concurrently — they would race on `bucket_map` and the two flag stores.
//! Stop the old binary before starting the new one; each binary's per-band
//! transaction atomicity makes a mid-band kill safe.
//!
//! ## SQLITE_TMPDIR
//!
//! M2 sorts `stg.buckets` by hash with an external merge sort that spills to
//! temp files.  The default temp directory is often `/tmp` on **tmpfs (RAM)** —
//! a 40-90 GB sort there would OOM a 15 GB box.  Set `SQLITE_TMPDIR` (before
//! starting the process) to a directory on a real disk with adequate free space
//! (e.g. the NVMe holding the snapshot).  The seeder also applies
//! `PRAGMA temp_store_directory` on the sidecar connection as a defensive
//! duplicate of the env var.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, bail};
use naiad_plugin_hydrus::HydrusDb;
use rusqlite::{Connection, OptionalExtension as _};

use crate::bridge::sidecar::Sidecar;
use crate::bridge::sidecar::pack_tag_ids;

const DEFS_BATCH: usize = 50_000;
/// Fixed hash_id band size for Phase M: 65_536 hash_ids per band.
const IDBAND: u64 = 1 << 16;
/// Sentinel value in the translation array meaning absent/unparseable tag_id.
const SENTINEL: u32 = u32::MAX;

/// Decode a 64-char sha256 hex into 32 bytes.
fn hex32(s: &str) -> Option<[u8; 32]> {
    let b = hex::decode(s).ok()?;
    <[u8; 32]>::try_from(b.as_slice()).ok()
}

/// Flush a non-empty hash-def batch into `sidecar` inside one transaction and
/// clear it.  No-op when `batch` is empty.
fn flush_h(sidecar: &Sidecar, batch: &mut Vec<(u64, [u8; 32])>) -> anyhow::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let tx = sidecar.conn().unchecked_transaction()?;
    sidecar.insert_defs_hashes(batch)?;
    tx.commit()?;
    batch.clear();
    Ok(())
}

/// Flush a non-empty tag-def batch into `sidecar` inside one transaction and
/// clear it.  No-op when `batch` is empty.
fn flush_t(sidecar: &Sidecar, batch: &mut Vec<(u64, String)>) -> anyhow::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let tx = sidecar.conn().unchecked_transaction()?;
    sidecar.insert_defs_tags(batch)?;
    tx.commit()?;
    batch.clear();
    Ok(())
}

/// Derive the directory that holds `sidecar.db` via `PRAGMA database_list`.
fn sidecar_dir(sidecar: &Sidecar) -> anyhow::Result<PathBuf> {
    let mut file_path: Option<PathBuf> = None;
    sidecar
        .conn()
        .pragma_query(None, "database_list", |row| {
            let name: String = row.get(1)?;
            if name == "main" {
                let file: String = row.get(2)?;
                file_path = Some(PathBuf::from(file));
            }
            Ok(())
        })
        .context("PRAGMA database_list")?;
    let p = file_path.context("PRAGMA database_list: no main database row")?;
    p.parent()
        .map(|d| d.to_path_buf())
        .context("sidecar path has no parent directory")
}

/// Open (or create) the staging database, applying the schema and bulk-write
/// pragmas.  The connection is tuned for fast sequential appends; durability is
/// traded for speed because staging is fully reconstructible.
fn open_staging(staging_path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(staging_path)
        .with_context(|| format!("opening staging db {}", staging_path.display()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS buckets (hash BLOB NOT NULL, ids BLOB NOT NULL);
         CREATE TABLE IF NOT EXISTS band_done (i INTEGER PRIMARY KEY);",
    )
    .context("creating staging schema")?;
    // journal_mode=PERSIST: keeps a real on-disk rollback journal so a kill-9
    // mid-band rolls the partial band back cleanly (not possible with OFF/MEMORY).
    // synchronous=OFF: skip per-commit fsync — staging is reconstructible.
    // temp_store=FILE: sort spill goes to disk, not RAM.
    conn.pragma_update(None, "journal_mode", "PERSIST")
        .context("staging journal_mode")?;
    conn.pragma_update(None, "synchronous", "OFF")
        .context("staging synchronous")?;
    conn.pragma_update(None, "temp_store", "FILE")
        .context("staging temp_store")?;
    conn.pragma_update(None, "cache_size", -2_000_000i64) // 2 GB
        .context("staging cache_size")?;
    Ok(conn)
}

/// Delete `staging_path` and any `-journal`, `-wal`, `-shm` sidecars.
/// Missing files are silently ignored.
fn delete_staging_files(staging_path: &Path) -> anyhow::Result<()> {
    let suffixes = ["", "-journal", "-wal", "-shm"];
    for suffix in suffixes {
        let p = if suffix.is_empty() {
            staging_path.to_path_buf()
        } else {
            let mut s = staging_path.as_os_str().to_os_string();
            s.push(suffix);
            PathBuf::from(s)
        };
        match fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("deleting staging file {}", p.display()));
            }
        }
    }
    Ok(())
}

/// M1 — staging pass.
///
/// Reads every `hash_id` band from `[0, num_bands)`, skipping those already
/// done in either the legacy sidecar `sync_state` flags or `staging.band_done`.
/// Each band's packed `(hash, ids)` rows are appended to `staging.buckets`, and
/// the band flag is set in `staging.band_done`, both in one single-file
/// transaction on `staging`.
fn stage_bands(
    sidecar: &Sidecar,
    staging: &Connection,
    hydrus: &HydrusDb,
    svc: i64,
    translation: &[u32],
    num_bands: u64,
) -> anyhow::Result<()> {
    let log_every = (num_bands / 256).max(1);
    let mut bands_done: u64 = 0;
    let mut bands_skipped: u64 = 0;
    let mut interval_rows: u64 = 0;
    let mut log_inst = Instant::now();

    // Prepare staging statements once outside the loop.
    let mut ins_bucket = staging
        .prepare("INSERT INTO buckets (hash, ids) VALUES (?1, ?2)")
        .context("preparing staging INSERT buckets")?;
    let mut ins_band = staging
        .prepare("INSERT OR IGNORE INTO band_done (i) VALUES (?1)")
        .context("preparing staging INSERT band_done")?;
    let mut chk_band = staging
        .prepare("SELECT 1 FROM band_done WHERE i = ?1")
        .context("preparing staging SELECT band_done")?;

    for i in 0..num_bands {
        // Skip if already legacy-done (old binary wrote these rows into bucket_map).
        let legacy_key = format!("seed_idband_done_{IDBAND}_{i}");
        if sidecar.get_flag(&legacy_key)?.as_deref() == Some("1") {
            bands_skipped += 1;
            continue;
        }

        // Skip if already staged this run.
        let staged_done: bool = chk_band
            .query_row([i as i64], |_| Ok(true))
            .optional()
            .context("checking staging band_done")?
            .unwrap_or(false);
        if staged_done {
            bands_skipped += 1;
            continue;
        }

        let lo = i * IDBAND;
        let hi = lo + IDBAND;

        // Collect groups for the band (ORDER BY hash_id, tag_id → consecutive
        // equal hash_id rows group together).
        let mut groups: Vec<([u8; 32], Vec<u64>)> = Vec::new();
        let mut band_mapping_rows: u64 = 0;
        hydrus
            .stream_ptr_idband_mappings(svc, lo, hi, &mut |hash, tag_id| {
                band_mapping_rows += 1;
                let sid = translation
                    .get(tag_id as usize)
                    .copied()
                    .unwrap_or(SENTINEL);
                if sid == SENTINEL {
                    match groups.last() {
                        Some((h, _)) if *h == hash => {}
                        _ => groups.push((hash, Vec::new())),
                    }
                } else {
                    match groups.last_mut() {
                        Some((h, ids)) if *h == hash => ids.push(sid as u64),
                        _ => groups.push((hash, vec![sid as u64])),
                    }
                }
                true
            })
            .with_context(|| format!("staging band {i}/{num_bands} lo={lo}"))?;
        interval_rows += band_mapping_rows;

        // One transaction per band on the staging connection: both buckets rows
        // and the band_done flag are in the same file — single-file atomic commit.
        let tx = staging
            .unchecked_transaction()
            .with_context(|| format!("staging band {i}: opening transaction"))?;
        for (h, ids) in &groups {
            if ids.is_empty() {
                continue; // all tags were unparseable → no row
            }
            let packed = pack_tag_ids(ids);
            ins_bucket
                .execute(rusqlite::params![&h[..], packed])
                .with_context(|| format!("staging band {i}: INSERT buckets"))?;
        }
        ins_band
            .execute([i as i64])
            .with_context(|| format!("staging band {i}: INSERT band_done"))?;
        tx.commit()
            .with_context(|| format!("staging band {i}: committing"))?;
        bands_done += 1;

        if bands_done % log_every == 0 {
            let secs = log_inst.elapsed().as_secs_f64().max(1e-9);
            let rows_per_sec = (interval_rows as f64 / secs) as u64;
            tracing::info!(
                target: "bridge",
                band = i,
                num_bands,
                rows = band_mapping_rows,
                bands_done,
                bands_skipped,
                rows_per_sec,
                "sidecar seed M1: band staged"
            );
            log_inst = Instant::now();
            interval_rows = 0;
        }
    }

    tracing::info!(
        target: "bridge",
        bands_done,
        bands_skipped,
        num_bands,
        "sidecar seed M1: staging complete"
    );
    Ok(())
}

/// M2 — merge pass.
///
/// ATTACHes `staging_path` to the sidecar connection and runs one hash-ordered
/// `INSERT OR REPLACE INTO bucket_map SELECT hash, ids FROM stg.buckets ORDER BY
/// hash` in a single transaction.  After commit, runs a WAL checkpoint, sets
/// `seed_merge_done`, DETACHes, and deletes `staging_path` (and its sidecars).
///
/// This function is idempotent: if `staging_path` no longer exists (a prior
/// successful call deleted it), SQLite creates a fresh empty database and the
/// SELECT returns zero rows — a safe no-op that leaves `bucket_map` intact.
fn merge_staging(sidecar: &Sidecar, staging_path: &Path) -> anyhow::Result<()> {
    // Force on-disk temp files for the ORDER BY hash external merge sort. The
    // bundled SQLite compiles with SQLITE_TEMP_STORE=1 (files by default), but a
    // memory temp_store would spill a multi-tens-of-GB sort into the 15 GB box's
    // RAM and OOM it — pin FILE so the spill can never land in RAM regardless of
    // build flags or a stray earlier pragma.
    sidecar
        .conn()
        .pragma_update(None, "temp_store", "FILE")
        .context("forcing temp_store=FILE for M2 sort")?;
    // Honor SQLITE_TMPDIR for the external merge sort that ORDER BY hash triggers.
    // Also set PRAGMA temp_store_directory defensively on the connection.
    if let Ok(tmpdir) = std::env::var("SQLITE_TMPDIR") {
        if !tmpdir.is_empty() {
            sidecar
                .conn()
                .pragma_update(None, "temp_store_directory", &tmpdir)
                .context("setting temp_store_directory for M2 sort")?;
        }
    }

    let staging_str = staging_path
        .to_str()
        .context("staging_path is not valid UTF-8")?;

    sidecar
        .conn()
        .execute("ATTACH DATABASE ?1 AS stg", [staging_str])
        .context("attaching staging.db for M2 merge")?;

    // Ensure the table exists even if staging.db was recreated empty (idempotency).
    sidecar
        .conn()
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS stg.buckets (hash BLOB NOT NULL, ids BLOB NOT NULL);",
        )
        .context("M2 merge: ensuring stg.buckets exists")?;

    let tx = sidecar
        .conn()
        .unchecked_transaction()
        .context("M2 merge: opening transaction")?;
    sidecar
        .conn()
        .execute(
            "INSERT OR REPLACE INTO bucket_map (hash, tag_ids) \
             SELECT hash, ids FROM stg.buckets ORDER BY hash",
            [],
        )
        .context("M2 merge: INSERT OR REPLACE")?;
    tx.commit().context("M2 merge: committing transaction")?;

    // Checkpoint before setting the point-of-no-return flag.
    sidecar
        .conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .context("M2 merge: WAL checkpoint")?;

    sidecar.set_flag("seed_merge_done", "1")?;

    sidecar
        .conn()
        .execute_batch("DETACH DATABASE stg;")
        .context("M2 merge: detaching staging")?;

    delete_staging_files(staging_path).context("M2 merge: deleting staging files")?;

    tracing::info!(target: "bridge", "sidecar seed M2: merge complete");
    Ok(())
}

/// Build (or resume, or `--rebuild`) the sidecar at `sidecar` from the Hydrus
/// snapshot at `snapshot_dir`.
///
/// ## Phase M — stage + merge
///
/// Phase M now runs in two sub-phases (M1 and M2) to avoid random B-tree
/// inserts into `bucket_map`:
///
/// - **M1** appends each band's packed blobs to a sequential `staging.db` beside
///   `sidecar.db`.  Bands already done in the legacy sidecar `sync_state` flags
///   (written by an old direct-write binary) or in `staging.band_done` are
///   skipped.
/// - **M2** does one hash-ordered `INSERT OR REPLACE INTO bucket_map SELECT …
///   FROM stg.buckets`, then sets `seed_merge_done`, deletes `staging.db`, and
///   writes the watermark.
///
/// `staging.db` appears beside `sidecar.db` during a seed and is deleted on
/// completion.  `SQLITE_TMPDIR` must point at a real disk (not tmpfs) — M2's
/// external sort can spill 40-90 GB.  The old and new seed binaries must not run
/// concurrently.
///
/// Rebuild clears `bucket_map`, all seed markers including `seed_merge_done`, and
/// deletes any stale `staging.db`, then rebuilds from scratch.  The defs tables
/// (`defs_hashes`, `defs_tags`) are NOT cleared.
///
/// # Errors
/// Returns an error on a snapshot open/scan failure or a sidecar write failure.
pub fn seed(
    snapshot_dir: &Path,
    service_id: Option<i64>,
    sidecar: &Sidecar,
    rebuild: bool,
) -> anyhow::Result<()> {
    // Seed-time connection tuning.
    sidecar
        .conn()
        .pragma_update(None, "cache_size", -4_000_000i64) // KiB → 4 GB
        .context("setting seed cache_size")?;
    sidecar
        .conn()
        .pragma_update(None, "synchronous", "NORMAL")
        .context("setting seed synchronous")?;

    let hydrus = HydrusDb::open(snapshot_dir)
        .with_context(|| format!("opening snapshot {}", snapshot_dir.display()))?;
    let svc = match service_id {
        Some(id) => id,
        None => match hydrus.repository_service_ids()?.as_slice() {
            [one] => *one,
            [] => bail!("no repository service in snapshot; pass --service-id"),
            ids => bail!("multiple repository services {ids:?}; pass --service-id"),
        },
    };

    // Derive staging.db path from sidecar path (via PRAGMA database_list).
    let sc_dir = sidecar_dir(sidecar).context("locating sidecar directory")?;
    let staging_path = sc_dir.join("staging.db");

    if rebuild {
        sidecar
            .conn()
            .execute("DELETE FROM bucket_map", [])
            .context("rebuild: clearing bucket_map")?;
        sidecar
            .conn()
            .execute(
                "DELETE FROM sync_state \
                 WHERE key = 'seed_defs_done' \
                    OR key LIKE 'seed_band_done_%' \
                    OR key LIKE 'seed_idband_done_%' \
                    OR key = 'seed_merge_done'",
                [],
            )
            .context("rebuild: clearing seed markers")?;
        delete_staging_files(&staging_path).context("rebuild: deleting staging.db")?;
        tracing::info!(
            target: "bridge",
            "sidecar seed --rebuild: cleared bucket_map + markers + staging"
        );
    }

    // ── Phase D — definitions (bounded-batch streaming, per-chunk txns) ──────
    if sidecar.get_flag("seed_defs_done")?.as_deref() != Some("1") {
        let mut hbatch: Vec<(u64, [u8; 32])> = Vec::new();
        let mut herr: Option<anyhow::Error> = None;
        let mut h_total: u64 = 0;
        let mut h_log_at: u64 = 0;
        let mut h_inst = Instant::now();
        hydrus.stream_ptr_hash_id_map(svc, &mut |id, sha_hex| {
            if let Some(arr) = hex32(sha_hex) {
                hbatch.push((id, arr));
            }
            h_total += 1;
            if hbatch.len() >= DEFS_BATCH {
                if let Err(e) = flush_h(sidecar, &mut hbatch) {
                    herr = Some(e);
                    return false;
                }
                let secs = h_inst.elapsed().as_secs_f64().max(1e-9);
                let rows_since = h_total - h_log_at;
                let rows_per_sec = (rows_since as f64 / secs) as u64;
                tracing::info!(
                    target: "bridge",
                    total = h_total,
                    rows_per_sec,
                    "sidecar seed: hash defs streamed"
                );
                h_inst = Instant::now();
                h_log_at = h_total;
            }
            true
        })?;
        if let Some(e) = herr {
            return Err(e);
        }
        flush_h(sidecar, &mut hbatch)?;
        tracing::info!(target: "bridge", total = h_total, "sidecar seed: hash defs done");

        let mut tbatch: Vec<(u64, String)> = Vec::new();
        let mut terr: Option<anyhow::Error> = None;
        let mut t_total: u64 = 0;
        let mut t_log_at: u64 = 0;
        let mut t_inst = Instant::now();
        hydrus.stream_ptr_tag_id_map(svc, &mut |id, tag| {
            tbatch.push((id, tag.to_string()));
            t_total += 1;
            if tbatch.len() >= DEFS_BATCH {
                if let Err(e) = flush_t(sidecar, &mut tbatch) {
                    terr = Some(e);
                    return false;
                }
                let secs = t_inst.elapsed().as_secs_f64().max(1e-9);
                let rows_since = t_total - t_log_at;
                let rows_per_sec = (rows_since as f64 / secs) as u64;
                tracing::info!(
                    target: "bridge",
                    total = t_total,
                    rows_per_sec,
                    "sidecar seed: tag defs streamed"
                );
                t_inst = Instant::now();
                t_log_at = t_total;
            }
            true
        })?;
        if let Some(e) = terr {
            return Err(e);
        }
        flush_t(sidecar, &mut tbatch)?;
        tracing::info!(target: "bridge", total = t_total, "sidecar seed: tag defs done");

        sidecar.set_flag("seed_defs_done", "1")?;
        tracing::info!(target: "bridge", "sidecar seed: Phase D (defs) done");
    }

    // ── Phase M — M1 (stage) + M2 (merge) ───────────────────────────────────

    // Guard: if seed_merge_done is set, Phase M is fully complete.
    if sidecar.get_flag("seed_merge_done")?.as_deref() == Some("1") {
        tracing::info!(target: "bridge", "sidecar seed: Phase M already complete (seed_merge_done)");
        // seed_merge_done is the point of no return; any staging.db still on disk
        // is a stranded remnant of a crash between setting the flag and deleting
        // the file (M2 sets the flag first). It is authoritatively garbage now —
        // reap it so a multi-GB file is not leaked past a "complete" seed.
        delete_staging_files(&staging_path)
            .context("reaping stranded staging.db after complete seed")?;
    } else {
        let Some(max_hash_id) = hydrus
            .max_current_hash_id(svc)
            .context("querying max current hash_id")?
        else {
            tracing::info!(target: "bridge", "sidecar seed: Phase M skipped (empty mappings)");
            let next = hydrus
                .recover_watermark(svc)
                .context("recovering update watermark")?
                .map_or(0, |w| w + 1);
            sidecar.set_next_update_index(next)?;
            tracing::info!(target: "bridge", next, "sidecar seed: complete");
            return Ok(());
        };

        let max_tag_id = hydrus
            .max_rtm_tag_id(svc)
            .context("querying max rtm tag_id")?
            .unwrap_or(0);
        let mut translation: Vec<u32> = vec![SENTINEL; max_tag_id as usize + 1];
        let mut trans_err: Option<anyhow::Error> = None;
        hydrus.stream_ptr_tag_translation(svc, &mut |tag_id, service_tag_id| {
            if service_tag_id >= SENTINEL as u64 {
                trans_err = Some(anyhow::anyhow!(
                    "service_tag_id {service_tag_id} (for tag_id {tag_id}) is >= u32::MAX \
                     ({SENTINEL}); widen `translation` to Vec<u64> and remove the SENTINEL \
                     overflow guard in sidecar_seed.rs"
                ));
                return false;
            }
            if (tag_id as usize) < translation.len() {
                translation[tag_id as usize] = service_tag_id as u32;
            }
            true
        })?;
        if let Some(e) = trans_err {
            return Err(e);
        }
        tracing::info!(
            target: "bridge",
            max_tag_id,
            "sidecar seed: translation map built"
        );

        // Clean up stale legacy hash-band flags.
        sidecar
            .conn()
            .execute(
                "DELETE FROM sync_state WHERE key LIKE 'seed_band_done_%'",
                [],
            )
            .context("clearing legacy seed_band_done_* flags")?;

        let num_bands = max_hash_id / IDBAND + 1;

        // M1: open (or reopen) staging.db and stage all non-done bands.
        let staging_conn = open_staging(&staging_path).context("opening staging.db for M1")?;
        stage_bands(
            sidecar,
            &staging_conn,
            &hydrus,
            svc,
            &translation,
            num_bands,
        )?;
        // Drop staging connection before M2 opens it via ATTACH.
        drop(staging_conn);

        // M2: merge staging into bucket_map, set flag, delete staging.
        merge_staging(sidecar, &staging_path)?;
    }

    // ── Watermark ────────────────────────────────────────────────────────────
    let next = hydrus
        .recover_watermark(svc)
        .context("recovering update watermark")?
        .map_or(0, |w| w + 1);
    sidecar.set_next_update_index(next)?;
    tracing::info!(target: "bridge", next, "sidecar seed: complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_builds_bucket_map_defs_and_watermark() {
        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc_path = dir.path().join("sidecar.db");
        let sidecar = Sidecar::create(&sc_path).unwrap();
        seed(dir.path(), Some(9), &sidecar, false).unwrap();

        // defs
        assert_eq!(sidecar.sha256_for(500).unwrap().unwrap()[0], 0x11);
        let tags = sidecar.defs_tags_for(&[800, 801]).unwrap();
        assert_eq!(tags.get(&800).map(String::as_str), Some("maid"));
        assert_eq!(tags.get(&801).map(String::as_str), Some("character:samus"));

        // bucket_map: h1 → {800,801}, h2 → {800}, h3 → no row (no mappings),
        //             h4 → no row (only unparseable tag → F10 filter).
        let h1: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x11;
            b
        };
        let h2: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x33;
            b
        };
        let h3: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0xaa;
            b
        };
        let h4: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0xbb;
            b
        };
        assert_eq!(sidecar.read_tag_set(&h1).unwrap(), vec![800, 801]);
        assert_eq!(sidecar.read_tag_set(&h2).unwrap(), vec![800]);
        assert_eq!(
            sidecar.read_tag_set(&h3).unwrap(),
            Vec::<u64>::new(),
            "h3 has no mappings — no bucket_map row"
        );
        assert_eq!(
            sidecar.read_tag_set(&h4).unwrap(),
            Vec::<u64>::new(),
            "h4's only tag is unparseable (F10 filter) — no bucket_map row"
        );

        // watermark: recover_watermark = 0 → next = 1.
        assert_eq!(sidecar.next_update_index().unwrap(), 1);

        // A full-range serve renders h1's tags.
        let (hits, _) = sidecar.bucket(&hex::encode(h1), 256, usize::MAX).unwrap();
        assert_eq!(
            hits.get(&hex::encode(h1)),
            Some(&vec!["character:samus".to_string(), "maid".to_string()])
        );

        // staging.db must be absent after a successful seed.
        assert!(
            !dir.path().join("staging.db").exists(),
            "staging.db must be deleted after successful seed"
        );

        // seed_merge_done must be set.
        assert_eq!(
            sidecar.get_flag("seed_merge_done").unwrap().as_deref(),
            Some("1"),
            "seed_merge_done must be set after complete seed"
        );
    }

    #[test]
    fn seed_resume_skips_done_bands_and_rebuild_clears() {
        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sidecar = Sidecar::create(dir.path().join("sidecar.db")).unwrap();
        seed(dir.path(), Some(9), &sidecar, false).unwrap();

        let h1: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x11;
            b
        };

        // ── Resume skip is discriminating ────────────────────────────────────
        // Corrupt h1's row WITHOUT clearing seed_merge_done.  A plain re-seed must
        // skip Phase M entirely (seed_merge_done guard), leaving the corruption intact.
        sidecar.write_tag_set(&h1, &[999]).unwrap();
        seed(dir.path(), Some(9), &sidecar, false).unwrap();
        assert_eq!(
            sidecar.read_tag_set(&h1).unwrap(),
            vec![999],
            "plain re-seed must skip Phase M (seed_merge_done set), leaving corruption at [999]"
        );

        // ── Rebuild removes stale rows absent from fixture ────────────────────
        let ghost: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x77;
            b
        };
        sidecar.write_tag_set(&ghost, &[1]).unwrap();
        assert_eq!(
            sidecar.read_tag_set(&ghost).unwrap(),
            vec![1],
            "ghost row must exist before rebuild"
        );

        // rebuild=true DELETEs all of bucket_map + seed_merge_done, then rebuilds.
        seed(dir.path(), Some(9), &sidecar, true).unwrap();
        assert_eq!(
            sidecar.read_tag_set(&h1).unwrap(),
            vec![800, 801],
            "h1 must be restored by rebuild"
        );
        assert_eq!(
            sidecar.read_tag_set(&ghost).unwrap(),
            Vec::<u64>::new(),
            "ghost row absent from fixture must be gone after rebuild"
        );

        // staging.db must be absent after rebuild+seed completes.
        assert!(
            !dir.path().join("staging.db").exists(),
            "staging.db must be deleted after rebuild + seed"
        );
    }

    /// A stale legacy `seed_band_done_*` flag (from an aborted hash-band run)
    /// must never cause an id-band to be skipped; only `seed_idband_done_*`
    /// flags (legacy direct-write) or `staging.band_done` (this run) govern M1.
    #[test]
    fn seed_idband_resume_ignores_legacy_band_flags() {
        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sidecar = Sidecar::create(dir.path().join("sidecar.db")).unwrap();

        // Insert a stale legacy flag pretending band 0 is done (old banding).
        sidecar.set_flag("seed_band_done_8_0", "1").unwrap();
        seed(dir.path(), Some(9), &sidecar, false).unwrap();

        // h1 must be correctly seeded — the legacy seed_band_done_* flag must
        // have been ignored by M1 (only seed_idband_done_* and staging.band_done
        // are consulted as skip guards).
        let h1: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x11;
            b
        };
        assert_eq!(
            sidecar.read_tag_set(&h1).unwrap(),
            vec![800, 801],
            "h1 must be seeded even if a stale legacy seed_band_done_* flag exists"
        );

        // Verify that seed_merge_done was set and staging.db is gone.
        assert_eq!(
            sidecar.get_flag("seed_merge_done").unwrap().as_deref(),
            Some("1")
        );
        assert!(!dir.path().join("staging.db").exists());

        // A rebuild clears seed_merge_done and restores h1 correctly even when
        // another stale legacy flag is present.
        sidecar.write_tag_set(&h1, &[999]).unwrap();
        sidecar.set_flag("seed_band_done_12_0", "1").unwrap();
        seed(dir.path(), Some(9), &sidecar, true).unwrap();
        assert_eq!(
            sidecar.read_tag_set(&h1).unwrap(),
            vec![800, 801],
            "h1 must be restored by rebuild regardless of legacy band flags"
        );
    }

    /// Phase M must (a) iterate more than one hash_id band and seed a hash whose
    /// hash_id lands in a band > 0, and (b) correctly handle a hash whose rows
    /// interleave an *unparseable* tag with a lower tag_id ahead of a parseable
    /// tag (the empty-group-then-join path in the sink): the parseable tag must
    /// still be recorded, and the unparseable one dropped, with a real
    /// `bucket_map` row written.
    #[test]
    fn seed_spans_multiple_bands_and_empty_group_first() {
        use rusqlite::Connection;

        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();

        let hid5: i64 = IDBAND as i64 + 4; // 65_540 → band 1
        let h5_blob = hex::decode(format!("cc{}", "00".repeat(31))).unwrap();
        {
            let master = Connection::open(dir.path().join("client.master.db")).unwrap();
            master
                .execute_batch(
                    "INSERT INTO subtags VALUES (4, 'foo');
                     INSERT INTO tags VALUES (4, 1, 3), (5, 1, 4);
                     INSERT INTO repository_tag_id_map_9 VALUES (803, 4), (804, 5);",
                )
                .unwrap();
            master
                .execute(
                    "INSERT INTO hashes (hash_id, hash) VALUES (?1, ?2)",
                    rusqlite::params![hid5, h5_blob],
                )
                .unwrap();
            let mappings = Connection::open(dir.path().join("client.mappings.db")).unwrap();
            mappings
                .execute(
                    "INSERT INTO current_mappings_9 (tag_id, hash_id) VALUES (4, ?1), (5, ?1)",
                    rusqlite::params![hid5],
                )
                .unwrap();
        }

        let sidecar = Sidecar::create(dir.path().join("sidecar.db")).unwrap();
        seed(dir.path(), Some(9), &sidecar, false).unwrap();

        // Band 0 hashes still correct.
        let h1: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x11;
            b
        };
        assert_eq!(sidecar.read_tag_set(&h1).unwrap(), vec![800, 801]);

        // Band 1 hash: only the parseable tag (804) survives.
        let h5: [u8; 32] = <[u8; 32]>::try_from(h5_blob.as_slice()).unwrap();
        assert_eq!(
            sidecar.read_tag_set(&h5).unwrap(),
            vec![804],
            "band-1 hash must keep the parseable tag despite the leading unparseable one"
        );

        // staging.db is gone and seed_merge_done is set.
        assert!(!dir.path().join("staging.db").exists());
        assert_eq!(
            sidecar.get_flag("seed_merge_done").unwrap().as_deref(),
            Some("1")
        );
    }

    /// Mixed legacy + staged resume — the real deployment scenario.
    ///
    /// Band 0 is already done by the old direct-write binary (legacy
    /// `seed_idband_done_*` flag + rows already in `bucket_map`).  Band 1 is a
    /// fresh hash the old binary never reached.  This drives `stage_bands`
    /// directly and inspects `staging.db` to *prove* the discriminating claim:
    /// M1 skips the legacy band (its hashes are never re-staged) yet stages the
    /// remainder, and M2 merges only the staged remainder onto the untouched
    /// legacy rows.  The all-legacy variant (staging empty) cannot prove this
    /// because an idempotent `INSERT OR REPLACE` of the same rows is
    /// indistinguishable from skipping them.
    #[test]
    fn seed_mixed_legacy_staged_resume() {
        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();

        // Augment a band-1 hash h5 (hash_id 65_540) with a parseable tag 804
        // (and a leading unparseable 803), exactly as the multi-band test does.
        let hid5: i64 = IDBAND as i64 + 4; // band 1
        let h5_blob = hex::decode(format!("cc{}", "00".repeat(31))).unwrap();
        {
            let master = Connection::open(dir.path().join("client.master.db")).unwrap();
            master
                .execute_batch(
                    "INSERT INTO subtags VALUES (4, 'foo');
                     INSERT INTO tags VALUES (4, 1, 3), (5, 1, 4);
                     INSERT INTO repository_tag_id_map_9 VALUES (803, 4), (804, 5);",
                )
                .unwrap();
            master
                .execute(
                    "INSERT INTO hashes (hash_id, hash) VALUES (?1, ?2)",
                    rusqlite::params![hid5, h5_blob],
                )
                .unwrap();
            let mappings = Connection::open(dir.path().join("client.mappings.db")).unwrap();
            mappings
                .execute(
                    "INSERT INTO current_mappings_9 (tag_id, hash_id) VALUES (4, ?1), (5, ?1)",
                    rusqlite::params![hid5],
                )
                .unwrap();
        }

        let sc_path = dir.path().join("sidecar.db");
        let sidecar = Sidecar::create(&sc_path).unwrap();

        // Simulate the old binary having written band 0 into bucket_map and set
        // the legacy flag.  h1 (hash_id 500) and h2 (hash_id 501) are in band 0.
        let h1: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x11;
            b
        };
        let h2: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x33;
            b
        };
        let h5: [u8; 32] = <[u8; 32]>::try_from(h5_blob.as_slice()).unwrap();
        sidecar.write_tag_set(&h1, &[800, 801]).unwrap();
        sidecar.write_tag_set(&h2, &[800]).unwrap();
        sidecar
            .set_flag(&format!("seed_idband_done_{IDBAND}_0"), "1")
            .unwrap();

        // Build the translation map and drive stage_bands directly so we can
        // inspect staging.db before it is consumed and deleted by the merge.
        let hydrus = HydrusDb::open(dir.path()).unwrap();
        let svc = 9i64;
        let max_hash_id = hydrus.max_current_hash_id(svc).unwrap().unwrap();
        let num_bands = max_hash_id / IDBAND + 1;
        assert_eq!(num_bands, 2, "fixture must span band 0 and band 1");
        let max_tag_id = hydrus.max_rtm_tag_id(svc).unwrap().unwrap_or(0);
        let mut translation: Vec<u32> = vec![SENTINEL; max_tag_id as usize + 1];
        hydrus
            .stream_ptr_tag_translation(svc, &mut |tag_id, service_tag_id| {
                if (tag_id as usize) < translation.len() {
                    translation[tag_id as usize] = service_tag_id as u32;
                }
                true
            })
            .unwrap();

        let staging_path = dir.path().join("staging.db");
        {
            let staging_conn = open_staging(&staging_path).unwrap();
            stage_bands(
                &sidecar,
                &staging_conn,
                &hydrus,
                svc,
                &translation,
                num_bands,
            )
            .unwrap();
        }

        // ── Discriminating assertions on staging.db ──────────────────────────
        {
            let stg = Connection::open(&staging_path).unwrap();
            // Band 0 (legacy) must NOT be staged; band 1 (fresh) must be.
            let band0_staged: bool = stg
                .query_row("SELECT 1 FROM band_done WHERE i = 0", [], |_| Ok(true))
                .optional()
                .unwrap()
                .unwrap_or(false);
            let band1_staged: bool = stg
                .query_row("SELECT 1 FROM band_done WHERE i = 1", [], |_| Ok(true))
                .optional()
                .unwrap()
                .unwrap_or(false);
            assert!(!band0_staged, "legacy band 0 must never be staged by M1");
            assert!(band1_staged, "fresh band 1 must be staged by M1");

            // Band-0 hashes must not appear in staging.buckets; h5 must.
            let count_of = |h: &[u8; 32]| -> i64 {
                stg.query_row(
                    "SELECT COUNT(*) FROM buckets WHERE hash = ?1",
                    [&h[..]],
                    |r| r.get(0),
                )
                .unwrap()
            };
            assert_eq!(count_of(&h1), 0, "legacy h1 must not be re-staged");
            assert_eq!(count_of(&h2), 0, "legacy h2 must not be re-staged");
            assert_eq!(count_of(&h5), 1, "fresh h5 must be staged exactly once");
        }

        // ── M2 merges the staged remainder onto the untouched legacy rows ────
        merge_staging(&sidecar, &staging_path).unwrap();

        assert_eq!(
            sidecar.read_tag_set(&h1).unwrap(),
            vec![800, 801],
            "legacy h1 must survive the staged merge untouched"
        );
        assert_eq!(
            sidecar.read_tag_set(&h2).unwrap(),
            vec![800],
            "legacy h2 must survive the staged merge untouched"
        );
        assert_eq!(
            sidecar.read_tag_set(&h5).unwrap(),
            vec![804],
            "staged band-1 hash must be merged in (parseable tag only)"
        );

        assert_eq!(
            sidecar.get_flag("seed_merge_done").unwrap().as_deref(),
            Some("1"),
            "seed_merge_done must be set"
        );
        assert!(
            !staging_path.exists(),
            "staging.db must be deleted after merge"
        );

        // A full rebuild starts fresh (clears the legacy flag + bucket_map) and
        // must still reproduce the correct union from the snapshot alone.
        seed(dir.path(), Some(9), &sidecar, true).unwrap();
        assert_eq!(sidecar.read_tag_set(&h1).unwrap(), vec![800, 801]);
        assert_eq!(sidecar.read_tag_set(&h2).unwrap(), vec![800]);
        assert_eq!(sidecar.read_tag_set(&h5).unwrap(), vec![804]);
    }

    /// Merge idempotency after a simulated crash between M1 and M2.
    ///
    /// Calls `stage_bands` to fully populate `staging.db` (with `seed_merge_done`
    /// unset), then calls `merge_staging` twice.  The second call must be safe:
    /// staging.db is already gone (deleted by the first call), SQLite reopens an
    /// empty file, the SELECT returns zero rows, and `bucket_map` remains correct.
    #[test]
    fn seed_merge_idempotent_after_simulated_crash() {
        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc_path = dir.path().join("sidecar.db");
        let sidecar = Sidecar::create(&sc_path).unwrap();

        // Run Phase D so defs exist.
        seed(dir.path(), Some(9), &sidecar, false).unwrap();

        // Reset seed_merge_done to simulate a crash after M1 but before M2.
        // Also wipe bucket_map so we can verify M2 fills it.
        sidecar.set_flag("seed_merge_done", "0").unwrap();
        sidecar
            .conn()
            .execute("DELETE FROM bucket_map", [])
            .unwrap();

        // Open snapshot + build translation to call stage_bands directly.
        let hydrus = HydrusDb::open(dir.path()).unwrap();
        let svc = 9i64;
        let max_hash_id = hydrus.max_current_hash_id(svc).unwrap().unwrap();
        let max_tag_id = hydrus.max_rtm_tag_id(svc).unwrap().unwrap_or(0);
        let mut translation: Vec<u32> = vec![SENTINEL; max_tag_id as usize + 1];
        hydrus
            .stream_ptr_tag_translation(svc, &mut |tag_id, service_tag_id| {
                if (tag_id as usize) < translation.len() {
                    translation[tag_id as usize] = service_tag_id as u32;
                }
                true
            })
            .unwrap();
        let num_bands = max_hash_id / IDBAND + 1;

        let staging_path = dir.path().join("staging.db");
        {
            let staging_conn = open_staging(&staging_path).unwrap();
            stage_bands(
                &sidecar,
                &staging_conn,
                &hydrus,
                svc,
                &translation,
                num_bands,
            )
            .unwrap();
        }

        // staging.db must exist and band_done must have entries.
        assert!(staging_path.exists(), "staging.db must exist after M1");

        // First merge_staging call.
        merge_staging(&sidecar, &staging_path).unwrap();

        let h1: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x11;
            b
        };
        assert_eq!(
            sidecar.read_tag_set(&h1).unwrap(),
            vec![800, 801],
            "bucket_map must be correct after first merge"
        );
        assert!(
            !staging_path.exists(),
            "staging.db must be deleted after first merge"
        );
        assert_eq!(
            sidecar.get_flag("seed_merge_done").unwrap().as_deref(),
            Some("1")
        );

        // Second merge_staging call — staging.db is gone; SQLite creates empty DB.
        // INSERT OR REPLACE SELECT ... FROM empty stg.buckets → 0 rows inserted.
        // bucket_map must remain correct.
        merge_staging(&sidecar, &staging_path).unwrap();
        assert_eq!(
            sidecar.read_tag_set(&h1).unwrap(),
            vec![800, 801],
            "bucket_map must still be correct after second (idempotent) merge"
        );
        // staging.db (the newly-created empty one) must be deleted again.
        assert!(!staging_path.exists());
    }
}
