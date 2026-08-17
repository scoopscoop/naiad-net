//! PTR bridge (ADR 0024): mirror the Hydrus Public Tag Repository into a
//! SHA-256-keyed `RepoStore` and serve it as an ordinary naiad repo. Absorbed
//! from the former `naiad-bridge` crate (issue #128) — modules are unchanged
//! apart from module-path fixes.

pub(crate) mod hydrus_wire;
pub mod lock;
pub mod ptr_client;
pub mod seed;
pub mod sidecar;
pub mod sidecar_seed;
pub mod sidecar_sync;
pub mod state;
pub mod sync;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use naiad_netproto::Account;
use naiad_plugin_hydrus::HydrusDb;

/// Default filename of the persisted synthetic **bridge author** Ed25519 key
/// (#225 §2), placed beside the bridge's state/sidecar DB.
pub const BRIDGE_AUTHOR_KEY_FILE: &str = "bridge-author.key";

/// Resolve the bridge author key path beside a given state/sidecar DB path.
///
/// The bridge author is the stable, persisted identity every bridged PTR
/// sibling/parent relation is signed by, so a client can trust-weight or
/// suppress the PTR relation source (ADR 0009) without touching the host repo's
/// own `--repo-key` identity. It lives next to `bridge-state.db` (mirror) or the
/// sidecar DB (sidecar), NOT beside `repo.db`, so it travels with the bridge's
/// private state.
pub fn bridge_author_key_path(state_db: &Path) -> PathBuf {
    state_db.with_file_name(BRIDGE_AUTHOR_KEY_FILE)
}

/// Load (or first-time create) the persisted bridge author account.
///
/// Delegates to [`Account::load_or_create`], which writes the fresh 32-byte seed
/// with best-effort `0600` permissions on Unix — the key is a signing secret and
/// must not be world-readable. The key is single-writer-guarded by the existing
/// #193 bridge lock, so no concurrent signer races it.
///
/// # Errors
/// Returns an error if an existing key cannot be read or a new one written.
pub fn load_bridge_author(state_db: &Path) -> anyhow::Result<Account> {
    let path = bridge_author_key_path(state_db);
    Account::load_or_create(&path)
        .with_context(|| format!("opening bridge author key {}", path.display()))
}

/// Spawn the PTR sync follow-loop on a background thread. Each DB handle is
/// opened *inside* the thread so the serving store keeps its own connection.
/// Errors are logged under `target: "bridge"`; the loop's internal retry
/// (`sync::MIN_POLL_SECS`) governs transient failures and it never terminates
/// the caller. Relocated from the former `bridge/serve.rs::run` spawn block.
/// Acquires the single-writer bridge lock (#193) inside the thread; on contention
/// the follow-loop is skipped with an error log and serve continues unaffected.
pub fn spawn_follow(
    db: PathBuf,
    state_db: PathBuf,
    ptr_url: String,
    ptr_key: String,
    freshness: Option<std::sync::Arc<crate::stats::freshness::SyncFreshness>>,
) {
    std::thread::Builder::new()
        .name("bridge-follow".into())
        .spawn(move || {
            // Single-writer guard (#193). Contention is not fatal to serve:
            // the follow-loop simply does not start.
            let _lock = match lock::BridgeLock::acquire(&lock::lock_path(&state_db)) {
                Ok(l) => l,
                Err(e) if e.is::<lock::Contended>() => {
                    tracing::error!(
                        target: "bridge",
                        "another bridge process appears to be running; \
                         follow-loop not started (this serve will keep serving)"
                    );
                    return;
                }
                Err(e) => {
                    tracing::error!(
                        target: "bridge",
                        error = %format!("{e:#}"),
                        "sync thread: cannot acquire bridge lock"
                    );
                    return;
                }
            };
            let state = match state::StateDb::open(&state_db) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        target: "bridge",
                        error = %format!("{e:#}"),
                        "sync thread: cannot open state db"
                    );
                    return;
                }
            };
            let repo = match crate::RepoStore::open(&db) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(
                        target: "bridge",
                        error = %format!("{e:#}"),
                        "sync thread: cannot open repo db"
                    );
                    return;
                }
            };
            // Persisted synthetic bridge author (#225): signs every bridged PTR
            // sibling/parent relation. Failure to open the key is fatal to the
            // follow-loop (but not to serve) — without it we cannot apply relations.
            let bridge_author = match load_bridge_author(&state_db) {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!(
                        target: "bridge",
                        error = %format!("{e:#}"),
                        "sync thread: cannot open bridge author key"
                    );
                    return;
                }
            };
            let mut client = ptr_client::PtrClient::new(&ptr_url, &ptr_key);
            if let Err(e) = sync::follow(
                &state,
                &repo,
                &bridge_author,
                &mut client,
                freshness.as_deref(),
            ) {
                tracing::error!(
                    target: "bridge",
                    error = %format!("{e:#}"),
                    "sync loop exited with error"
                );
            }
        })
        .expect("spawn bridge-follow thread");
}

/// Compute the sidecar follow-loop sleep duration given `next_due` (an absolute
/// Unix timestamp) and the current Unix time `now`. Floored at
/// [`sync::MIN_POLL_SECS`], matching the mirror path's sleep math in
/// `sync::follow`. Pure function so the floor logic is unit-testable.
fn sidecar_follow_sleep_secs(next_due: u64, now: u64) -> u64 {
    next_due.saturating_sub(now).max(sync::MIN_POLL_SECS)
}

/// Spawn the sidecar PTR sync follow-loop on a background thread (X2 freshness
/// path). Mirrors [`spawn_follow`] but opens the `Sidecar` and applies mappings
/// via `sidecar_sync::sync_once`, sleeping [`sync::MIN_POLL_SECS`] (1 h floor)
/// between passes and holding the single-writer bridge lock (#193).
///
/// Since #225 it *also* opens the serving `RepoStore` (a second connection on
/// `db`, exactly as [`spawn_follow`] does) and the persisted bridge author key,
/// because bridged PTR sibling/parent relations live only in the `RepoStore`
/// (§5) — a dual-domain PTR repo always has a native store beside the sidecar
/// (ADR 0024). The sidecar's own mapping+cursor transaction is untouched; the
/// relation apply is a separate small transaction on the `RepoStore`.
pub fn spawn_sidecar_follow(
    db: PathBuf,
    state_db: PathBuf,
    ptr_url: String,
    ptr_key: String,
    freshness: Option<std::sync::Arc<crate::stats::freshness::SyncFreshness>>,
    count_refresh_notify: Option<std::sync::Arc<tokio::sync::Notify>>,
) {
    std::thread::Builder::new()
        .name("bridge-sidecar-follow".into())
        .spawn(move || {
            let _lock = match lock::BridgeLock::acquire(&lock::lock_path(&state_db)) {
                Ok(l) => l,
                Err(e) if e.is::<lock::Contended>() => {
                    tracing::error!(
                        target: "bridge",
                        "another bridge process is running; sidecar follow-loop not started"
                    );
                    return;
                }
                Err(e) => {
                    tracing::error!(
                        target: "bridge",
                        error = %format!("{e:#}"),
                        "sidecar follow: cannot acquire lock"
                    );
                    return;
                }
            };
            let sidecar = match sidecar::Sidecar::open(&state_db) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        target: "bridge",
                        error = %format!("{e:#}"),
                        "sidecar follow: cannot open sidecar"
                    );
                    return;
                }
            };
            // #225: the serving RepoStore (relations destination) and the bridge
            // author key. Both are required to apply relations; failure to open
            // either aborts the follow-loop (serve keeps running).
            let repo = match crate::RepoStore::open(&db) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(
                        target: "bridge",
                        error = %format!("{e:#}"),
                        "sidecar follow: cannot open repo db for relations"
                    );
                    return;
                }
            };
            let bridge_author = match load_bridge_author(&state_db) {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!(
                        target: "bridge",
                        error = %format!("{e:#}"),
                        "sidecar follow: cannot open bridge author key"
                    );
                    return;
                }
            };
            let mut client = ptr_client::PtrClient::new(&ptr_url, &ptr_key);
            // De-noise a stuck upstream (mirrors sync::follow). First failure logs
            // at WARN; identical repeats log at DEBUG; a periodic reminder re-WARNs
            // every SYNC_FAIL_REWARN_EVERY passes; recovery logs at INFO.
            let mut consecutive_failures: u64 = 0;
            let mut last_error = String::new();
            loop {
                match sidecar_sync::sync_once(&sidecar, &mut client, Some((&repo, &bridge_author)))
                {
                    Ok(r) => {
                        if consecutive_failures > 0 {
                            tracing::info!(
                                target: "bridge",
                                recovered_after = consecutive_failures,
                                "sync recovered after {consecutive_failures} consecutive failed pass(es)"
                            );
                            consecutive_failures = 0;
                            last_error.clear();
                        }
                        tracing::info!(
                            target: "bridge",
                            indexes_applied = r.indexes_applied,
                            mappings_applied = r.mappings_applied,
                            "sidecar sync pass complete"
                        );
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        // Update freshness handle (best-effort; never blocks sync).
                        if let Some(f) = &freshness {
                            let cursor = sidecar.next_update_index().unwrap_or(0);
                            f.record_pass(cursor, r.mappings_applied, now as i64);
                        }
                        // Trigger the bridge count refresher if mappings landed (#236).
                        // The refresher task wakes and recomputes the sync_state cache
                        // in the background — the follow-loop does not wait for it.
                        if r.mappings_applied > 0 {
                            if let Some(notify) = &count_refresh_notify {
                                notify.notify_one();
                            }
                        }
                        let sleep = sidecar_follow_sleep_secs(r.next_due, now);
                        tracing::info!(target: "bridge", sleep_secs = sleep, "sync idle; sleeping");
                        std::thread::sleep(std::time::Duration::from_secs(sleep));
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        consecutive_failures += 1;
                        let error_changed = msg != last_error;
                        if sync::should_warn_sync_failure(consecutive_failures, error_changed) {
                            tracing::warn!(
                                target: "bridge",
                                error = %msg,
                                consecutive_failures,
                                "sync pass failed; retrying every {}s (identical repeats \
                                 logged at DEBUG until the error changes, recovers, or the next reminder)",
                                sync::MIN_POLL_SECS
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
                        std::thread::sleep(std::time::Duration::from_secs(sync::MIN_POLL_SECS));
                    }
                }
            }
        })
        .expect("spawn bridge-sidecar-follow thread");
}

/// Print the sync cursor and store statistics. Read-only with respect to both
/// the repo store and the state DB — opens neither with write intent, so this
/// command cannot hit `SQLITE_BUSY` against a bridge mid-transaction.
/// Extracted from the former `bridge/serve.rs::status`.
///
/// If the state DB file does not yet exist (bridge has never run), the state
/// fields are reported as zero/absent without creating the file.
///
/// # Errors
/// Returns an error if any DB read fails.
pub fn status(db: &Path, state_db: &Path) -> anyhow::Result<()> {
    let store = crate::RepoStore::open_readonly(db)
        .with_context(|| format!("opening repo db read-only {}", db.display()))?;

    let hashes = store.distinct_hash_count()?;
    let mappings = store.current_mapping_count()?;
    let seq = store.mapping_cursor()?;

    // Open the state db read-only only when it already exists — a read-only
    // open errors on a missing file and we don't want to create it here.
    let (cursor, next_due) = if state_db.exists() {
        let state = state::StateDb::open_readonly(state_db)
            .with_context(|| format!("opening state db read-only {}", state_db.display()))?;
        let c = state.next_update_index()?;
        let d = state
            .get_flag("next_update_due")?
            .unwrap_or_else(|| "0".into());
        (c, d)
    } else {
        (0, "0".to_string())
    };

    println!("hydrus cursor (next_update_index): {cursor}");
    println!("distinct hashes:                   {hashes}");
    println!("current mappings:                  {mappings}");
    println!("mapping seq high-watermark:        {seq}");
    println!("last next_update_due:              {next_due}");

    Ok(())
}

/// Outcome of a parity audit; maps to a process exit code in `main.rs`.
pub enum AuditOutcome {
    Pass,
    Fail,
    Refused,
}

/// Parse a `--band` hex prefix into `(lo_hex_64, prefix_bits)`. Empty/None -> full range.
fn parse_band(band: Option<&str>) -> anyhow::Result<(String, u32)> {
    match band {
        None | Some("") => Ok(("0".repeat(64), 0)),
        Some(hex) => {
            if hex.len() > 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                anyhow::bail!("--band must be 0..64 hex chars, got {hex:?}");
            }
            let bits = (hex.len() as u32) * 4;
            let lo_hex = format!("{:0<64}", hex.to_lowercase()); // right-pad with '0'
            Ok((lo_hex, bits))
        }
    }
}

/// Read-only sidecar ↔ Hydrus-snapshot parity audit (#184 retarget for #207 sidecar).
///
/// Opens the sidecar read-only and the Hydrus snapshot, gates on watermark
/// equality (sidecar `next_update_index - 1` vs `HydrusDb::recover_watermark`),
/// then compares per-band digests produced by [`sidecar::Sidecar::audit_band_digest`]
/// and [`HydrusDb::audit_band_digest`]. Returns [`AuditOutcome`].
///
/// # Errors
/// Returns an error if either store cannot be opened or a query fails.
pub fn parity_audit_sidecar(
    sidecar_path: &Path,
    snapshot_dir: &Path,
    service_id: Option<i64>,
    band: Option<&str>,
) -> anyhow::Result<AuditOutcome> {
    let sc = sidecar::Sidecar::open_readonly(sidecar_path)
        .with_context(|| format!("opening sidecar read-only {}", sidecar_path.display()))?;
    let hydrus = HydrusDb::open(snapshot_dir)
        .with_context(|| format!("opening hydrus snapshot {}", snapshot_dir.display()))?;
    let svc = match service_id {
        Some(id) => id,
        None => match hydrus.repository_service_ids()?.as_slice() {
            [one] => *one,
            [] => anyhow::bail!("no repository service in snapshot; pass --service-id"),
            ids => anyhow::bail!("multiple repository services {ids:?}; pass --service-id"),
        },
    };

    let w_s: i64 = sc.next_update_index()? as i64 - 1;
    let w_h = match hydrus.recover_watermark(svc)? {
        Some(w) => w as i64,
        None => {
            println!("REFUSED: snapshot has no fully-processed update (watermark = none)");
            return Ok(AuditOutcome::Refused);
        }
    };
    if w_s != w_h {
        println!("REFUSED: watermark mismatch (sidecar W_s={w_s}, snapshot W_h={w_h})");
        if w_s < w_h {
            println!(
                "  sidecar is {} update(s) behind the snapshot; \
                 advance sidecar seed/sync to {w_h} and rerun.",
                w_h - w_s
            );
        } else {
            println!(
                "  snapshot is {} update(s) behind the sidecar; \
                 take a newer snapshot and rerun.",
                w_s - w_h
            );
        }
        return Ok(AuditOutcome::Refused);
    }

    let (lo_hex, bits) = parse_band(band)?;
    let (s_count, s_dig) = sc.audit_band_digest(&lo_hex, bits)?;
    let (h_count, h_dig) = hydrus.audit_band_digest(&lo_hex, bits, svc)?;
    let label = band.filter(|b| !b.is_empty()).unwrap_or("<full range>");
    if s_count == h_count && s_dig == h_dig {
        println!(
            "PASS  band={label} watermark={w_s} count={s_count} digest={}",
            hex::encode(s_dig)
        );
        Ok(AuditOutcome::Pass)
    } else {
        println!("FAIL  band={label} watermark={w_s}");
        println!("  sidecar: count={s_count} digest={}", hex::encode(s_dig));
        println!("  hydrus : count={h_count} digest={}", hex::encode(h_dig));
        Ok(AuditOutcome::Fail)
    }
}

/// Read-only mirror<->snapshot parity audit (issue #184, part 1).
///
/// Opens both databases read-only, gates on watermark equality, then compares
/// per-band digests produced by [`crate::RepoStore::audit_band_digest`] and
/// [`HydrusDb::audit_band_digest`]. Returns [`AuditOutcome`] which the caller
/// maps to a process exit code.
pub fn parity_audit(
    db: &Path,
    state_db: &Path,
    snapshot_dir: &Path,
    service_id: Option<i64>,
    band: Option<&str>,
) -> anyhow::Result<AuditOutcome> {
    let store = crate::RepoStore::open_readonly(db)
        .with_context(|| format!("opening repo db read-only {}", db.display()))?;
    if !state_db.exists() {
        anyhow::bail!(
            "state db {} does not exist; has the bridge ever seeded/synced?",
            state_db.display()
        );
    }
    let state = state::StateDb::open_readonly(state_db)
        .with_context(|| format!("opening state db read-only {}", state_db.display()))?;
    let hydrus = HydrusDb::open(snapshot_dir)
        .with_context(|| format!("opening hydrus snapshot {}", snapshot_dir.display()))?;

    // Resolve service id exactly like seed::run.
    let svc = match service_id {
        Some(id) => id,
        None => match hydrus.repository_service_ids()?.as_slice() {
            [one] => *one,
            [] => anyhow::bail!("no repository service in snapshot; pass --service-id"),
            ids => anyhow::bail!("multiple repository services {ids:?}; pass --service-id"),
        },
    };

    let next = state.next_update_index()?;
    let w_m: i64 = next as i64 - 1; // last fully-applied update index
    let w_h = match hydrus.recover_watermark(svc)? {
        Some(w) => w as i64,
        None => {
            println!("REFUSED: snapshot has no fully-processed update (watermark = none)");
            return Ok(AuditOutcome::Refused);
        }
    };
    if w_m != w_h {
        println!("REFUSED: watermark mismatch (mirror W_m={w_m}, snapshot W_h={w_h})");
        if w_m < w_h {
            println!(
                "  mirror is {} update(s) behind the snapshot; advance the follow-loop to {w_h} and rerun.",
                w_h - w_m
            );
        } else {
            println!(
                "  snapshot is {} update(s) behind the mirror; take a newer snapshot and rerun.",
                w_m - w_h
            );
        }
        return Ok(AuditOutcome::Refused);
    }

    let (lo_hex, bits) = parse_band(band)?;
    let (m_count, m_dig) = store.audit_band_digest(&lo_hex, bits)?;
    let (h_count, h_dig) = hydrus.audit_band_digest(&lo_hex, bits, svc)?;

    let band_label = band.filter(|b| !b.is_empty()).unwrap_or("<full range>");
    if m_count == h_count && m_dig == h_dig {
        println!(
            "PASS  band={band_label} watermark={w_m} count={m_count} digest={}",
            hex::encode(m_dig)
        );
        println!("(local-origin rows (origin != NULL) are excluded from the comparison; #198.)");
        Ok(AuditOutcome::Pass)
    } else {
        println!("FAIL  band={band_label} watermark={w_m}");
        println!("  mirror : count={m_count} digest={}", hex::encode(m_dig));
        println!("  hydrus : count={h_count} digest={}", hex::encode(h_dig));
        Ok(AuditOutcome::Fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sidecar_follow_sleep_secs` floors at `MIN_POLL_SECS` and respects
    /// `next_due` when it is far enough in the future.
    #[test]
    fn sidecar_follow_sleep_secs_floors_and_respects_next_due() {
        // When next_due is in the past, saturating_sub → 0, floor kicks in.
        assert_eq!(
            sidecar_follow_sleep_secs(0, 1000),
            sync::MIN_POLL_SECS,
            "past next_due is floored at MIN_POLL_SECS"
        );

        // When next_due is exactly MIN_POLL_SECS ahead, result equals the floor.
        assert_eq!(
            sidecar_follow_sleep_secs(1000 + sync::MIN_POLL_SECS, 1000),
            sync::MIN_POLL_SECS,
            "next_due exactly at floor boundary returns MIN_POLL_SECS"
        );

        // When next_due is well beyond the floor, the actual delta is respected.
        let future = 1000 + sync::MIN_POLL_SECS + 60;
        assert_eq!(
            sidecar_follow_sleep_secs(future, 1000),
            sync::MIN_POLL_SECS + 60,
            "next_due beyond floor respects the actual delta"
        );
    }

    /// `parity_audit_sidecar` must return `AuditOutcome::Refused` when the
    /// sidecar's watermark does not match the snapshot's watermark.
    #[test]
    fn parity_audit_sidecar_refuses_on_watermark_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc_path = dir.path().join("sidecar.db");
        {
            let sc = sidecar::Sidecar::create(&sc_path).unwrap();
            sidecar_seed::seed(dir.path(), Some(9), &sc, false).unwrap();
            // After seed: next_update_index = 1, so w_s = 0. snapshot w_h = 0. Match.
            // Bump past the snapshot watermark so they mismatch.
            sc.set_next_update_index(5).unwrap();
            // w_s = 5 - 1 = 4, w_h = 0 → Refused.
        }
        let outcome = parity_audit_sidecar(&sc_path, dir.path(), Some(9), None).unwrap();
        assert!(
            matches!(outcome, AuditOutcome::Refused),
            "watermark mismatch must return Refused"
        );
    }
}
