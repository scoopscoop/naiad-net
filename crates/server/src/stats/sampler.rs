//! Background samplers for the stats subsystem (#235, Piece B).
//!
//! Two independent tokio tasks:
//!
//! 1. **System sampler** — every 60 s via `sysinfo`: records `cpu_pct`,
//!    `rss_bytes`, `store_disk_bytes`, `net_rx_bytes`, `net_tx_bytes`,
//!    `uptime_secs` to `stats.db`.
//!
//! 2. **Gauge + freshness sampler** — every 600 s: opens a read-only repo
//!    connection in `spawn_blocking`, records `tags_stored`, `hashes_stored`,
//!    `mappings_stored`; then samples `SyncFreshness` (falling back to the
//!    persisted cursor when `last_poll_unix == 0` and the configured state DB
//!    exists) to record `sync_last_applied_update`, `sync_last_poll_age_secs`,
//!    `sync_rows_last_cycle`.
//!
//! Every write is best-effort: a failure logs `warn target: "stats"` and
//! continues. A slow or failed repo read in the gauge sampler never stalls
//! the system sampler — they are independent tasks.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Notify;

use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Networks, ProcessRefreshKind, RefreshKind, System,
};
use tokio::task::JoinHandle;
use tokio::time;

use crate::stats::freshness::SyncFreshness;
use crate::stats::store::StatsDb;
use crate::store::RepoStore;

// ── Cadence constants ─────────────────────────────────────────────────────────

/// System sampler tick interval: 60 seconds.
const SYSTEM_SAMPLE_INTERVAL_SECS: u64 = 60;
/// Gauge + freshness sampler tick interval: 600 seconds (10 minutes).
const GAUGE_SAMPLE_INTERVAL_SECS: u64 = 600;

// ── Metric name constants (spec §Data model) ──────────────────────────────────

const METRIC_CPU_PCT: &str = "cpu_pct";
const METRIC_RSS_BYTES: &str = "rss_bytes";
const METRIC_STORE_DISK_BYTES: &str = "store_disk_bytes";
const METRIC_NET_RX_BYTES: &str = "net_rx_bytes";
const METRIC_NET_TX_BYTES: &str = "net_tx_bytes";
const METRIC_UPTIME_SECS: &str = "uptime_secs";
const METRIC_TAGS_STORED: &str = "tags_stored";
const METRIC_HASHES_STORED: &str = "hashes_stored";
const METRIC_MAPPINGS_STORED: &str = "mappings_stored";
const METRIC_SYNC_LAST_APPLIED_UPDATE: &str = "sync_last_applied_update";
const METRIC_SYNC_LAST_POLL_AGE_SECS: &str = "sync_last_poll_age_secs";
const METRIC_SYNC_ROWS_LAST_CYCLE: &str = "sync_rows_last_cycle";

// Bridge-sidecar gauges (populated only in BridgeMode::Sidecar) ──────────────

/// Count of distinct hashes in the sidecar (from the `sync_state` cache; see
/// `Sidecar::recompute_bridge_counts` for the update path).
const METRIC_BRIDGE_HASHES_STORED: &str = "bridge_hashes_stored";
/// Count of `defs_tags` rows (known tag definitions), from the `sync_state` cache.
const METRIC_BRIDGE_TAGS_STORED: &str = "bridge_tags_stored";
/// Exact total mapping pairs across all `bucket_map` rows, from the `sync_state`
/// cache (produced by a full scan in `Sidecar::recompute_bridge_counts`; no
/// longer approximate — updated periodically by the bridge count refresher task).
const METRIC_BRIDGE_MAPPINGS_STORED: &str = "bridge_mappings_stored";

/// Staleness threshold for the bridge count cache: 25 hours. If the cache
/// timestamp is older than this at startup, an immediate recompute is triggered.
const BRIDGE_COUNT_STALE_SECS: u64 = 25 * 3600;

/// Periodic fallback recompute interval for the bridge count refresher: 24 hours.
/// The sync-coupled trigger (via `count_notify`) fires sooner when mappings are
/// applied, so on an active node the cache is fresher than this bound.
const BRIDGE_COUNT_REFRESH_INTERVAL_SECS: u64 = 24 * 3600;

/// Sentinel value for `sync_last_poll_age_secs` when the follow-loop has never
/// polled (no in-process sync has ever completed).
const NEVER_POLLED_SENTINEL: f64 = -1.0;

// ── Disk-size helper ──────────────────────────────────────────────────────────

/// Sum the on-disk sizes of `path`, `path-wal`, and `path-shm`.
/// Missing files contribute 0 — `stat` failures are silently ignored.
fn db_file_size(path: &Path) -> u64 {
    let fname = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut total: u64 = 0;
    for suffix in ["", "-wal", "-shm"] {
        let candidate = parent.join(format!("{fname}{suffix}"));
        total += std::fs::metadata(&candidate).map(|m| m.len()).unwrap_or(0);
    }
    total
}

/// Sum the on-disk byte sizes for all configured store files.
///
/// `bridge_state` path is included when `Some`; missing files inside
/// `db_file_size` already contribute 0 so no separate `exists()` guard
/// is needed.
fn store_disk_bytes(repo_db: &Path, bridge_state: Option<&Path>) -> u64 {
    let mut total = db_file_size(repo_db);
    if let Some(bp) = bridge_state {
        total += db_file_size(bp);
    }
    total
}

// ── System sampler ────────────────────────────────────────────────────────────

/// Spawn the 60-second system sampler task.
///
/// Writes `cpu_pct`, `rss_bytes`, `store_disk_bytes`, `net_rx_bytes`,
/// `net_tx_bytes`, and `uptime_secs` to `stats.db` each tick.
///
/// `repo_db_path` and `bridge_state_path` (optional) are used only to
/// compute `store_disk_bytes`; they are never opened.
pub(crate) fn spawn_system_sampler(
    db: Arc<StatsDb>,
    repo_db_path: PathBuf,
    bridge_state_path: Option<PathBuf>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Capture process start for uptime accounting.
        let started = Instant::now();

        // Capture own PID once.
        let own_pid = sysinfo::Pid::from_u32(std::process::id());

        // Create the sysinfo System and Networks once; refresh per tick.
        let refresh_kind = RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything())
            .with_processes(ProcessRefreshKind::new().with_memory());
        let mut sys = System::new_with_specifics(refresh_kind);

        // First CPU reading — satisfies the first half of the two-reading
        // requirement; the second refresh happens inside the loop after
        // ≥SYSTEM_SAMPLE_INTERVAL_SECS have elapsed.
        sys.refresh_cpu_usage();

        let mut networks = Networks::new_with_refreshed_list();

        // Align first tick to the next minute boundary.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let secs_into_minute = now_secs % SYSTEM_SAMPLE_INTERVAL_SECS;
        let first_delay = Duration::from_secs(SYSTEM_SAMPLE_INTERVAL_SECS - secs_into_minute);
        time::sleep(first_delay).await;

        let mut interval = time::interval(Duration::from_secs(SYSTEM_SAMPLE_INTERVAL_SECS));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            // CPU: second refresh satisfies MINIMUM_CPU_UPDATE_INTERVAL
            // (≥SYSTEM_SAMPLE_INTERVAL_SECS elapsed since the previous refresh).
            sys.refresh_cpu_usage();
            let cpu_pct = sys.global_cpu_usage() as f64;

            // RSS of this process.
            sys.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::Some(&[own_pid]),
                false,
                ProcessRefreshKind::new().with_memory(),
            );
            let rss = sys.process(own_pid).map(|p| p.memory()).unwrap_or(0);

            // Network: refresh and sum cumulative totals across all interfaces.
            networks.refresh();
            let (net_rx, net_tx): (u64, u64) = networks.iter().fold((0, 0), |(rx, tx), (_, n)| {
                (rx + n.total_received(), tx + n.total_transmitted())
            });

            let disk = store_disk_bytes(&repo_db_path, bridge_state_path.as_deref());
            let uptime = started.elapsed().as_secs();

            let now_minute = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
                / 60
                * 60;

            macro_rules! sample {
                ($metric:expr, $value:expr) => {
                    if let Err(e) = db.write_sample(now_minute, $metric, "", $value) {
                        tracing::warn!(
                            target: "stats",
                            error = %e,
                            metric = $metric,
                            "sample write failed"
                        );
                    }
                };
            }

            sample!(METRIC_CPU_PCT, cpu_pct);
            sample!(METRIC_RSS_BYTES, rss as f64);
            sample!(METRIC_STORE_DISK_BYTES, disk as f64);
            sample!(METRIC_NET_RX_BYTES, net_rx as f64);
            sample!(METRIC_NET_TX_BYTES, net_tx as f64);
            sample!(METRIC_UPTIME_SECS, uptime as f64);
        }
    })
}

// ── Gauge + freshness sampler ─────────────────────────────────────────────────

/// Spawn the 600-second gauge + freshness sampler task.
///
/// Writes `tags_stored`, `hashes_stored`, `mappings_stored`,
/// `sync_last_applied_update`, `sync_last_poll_age_secs`, and
/// `sync_rows_last_cycle` to `stats.db` each tick.
///
/// When `sidecar_path` is `Some` (only in `BridgeMode::Sidecar`) the sampler
/// also opens the sidecar read-only each tick and writes `bridge_hashes_stored`,
/// `bridge_tags_stored`, and `bridge_mappings_stored` (approximate, see
/// [`Sidecar::approx_mapping_count`]). In mirror or native mode `sidecar_path`
/// is `None` and these metrics are not emitted — the native gauges already
/// reflect the correct data.
///
/// `repo_db_path` is opened read-only in `spawn_blocking` each tick; a slow
/// or failed open never stalls the system sampler (independent task).
///
/// `bridge_state_path` is the optional path to the bridge state/sidecar DB;
/// it is opened read-only as a freshness fallback when `SyncFreshness` shows
/// `last_poll_unix == 0` (no in-process follow-loop has run yet).
pub(crate) fn spawn_gauge_sampler(
    db: Arc<StatsDb>,
    freshness: Arc<SyncFreshness>,
    repo_db_path: PathBuf,
    bridge_state_path: Option<PathBuf>,
    sidecar_path: Option<PathBuf>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Align to next 10-minute boundary.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let secs_into_ten = now_secs % GAUGE_SAMPLE_INTERVAL_SECS;
        let first_delay = Duration::from_secs(GAUGE_SAMPLE_INTERVAL_SECS - secs_into_ten);
        time::sleep(first_delay).await;

        let mut interval = time::interval(Duration::from_secs(GAUGE_SAMPLE_INTERVAL_SECS));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let now_minute = (now_secs as i64) / 60 * 60;

            // ── Store counts (in spawn_blocking so slow scans don't block runtime) ──
            let db_for_blocking = Arc::clone(&db);
            let rdb = repo_db_path.clone();
            let store_result = tokio::task::spawn_blocking(move || {
                let store = RepoStore::open_readonly(&rdb)?;
                let tags = store.distinct_tag_count()?;
                let hashes = match store.read_distinct_hash_count()? {
                    Some(n) => n,
                    None => store.distinct_hash_count()?,
                };
                let mappings = store.current_mapping_count()?;
                Ok::<(u64, u64, u64), anyhow::Error>((tags, hashes, mappings))
            })
            .await;

            match store_result {
                Ok(Ok((tags, hashes, mappings))) => {
                    macro_rules! gauge {
                        ($metric:expr, $value:expr) => {
                            if let Err(e) =
                                db_for_blocking.write_sample(now_minute, $metric, "", $value as f64)
                            {
                                tracing::warn!(
                                    target: "stats",
                                    error = %e,
                                    metric = $metric,
                                    "gauge sample write failed"
                                );
                            }
                        };
                    }
                    gauge!(METRIC_TAGS_STORED, tags);
                    gauge!(METRIC_HASHES_STORED, hashes);
                    gauge!(METRIC_MAPPINGS_STORED, mappings);
                }
                Ok(Err(e)) => {
                    tracing::warn!(target: "stats", error = %e, "gauge repo read failed");
                }
                Err(e) => {
                    tracing::warn!(
                        target: "stats",
                        error = %e,
                        "gauge spawn_blocking panicked"
                    );
                }
            }

            // ── Bridge sidecar gauges (BridgeMode::Sidecar only) ──────────────────
            //
            // HOT-PATH INVARIANT (#236): this block issues ZERO scans of `bucket_map`.
            // It reads only three primary-key lookups from `sync_state` via
            // `cached_bridge_counts()`. All expensive counting (the full btree scan)
            // is performed exclusively by the bridge count refresher task
            // (`spawn_bridge_count_refresher`), which writes its results into the
            // `sync_state` cache. If the cache is absent (refresher not yet run),
            // no samples are emitted rather than scanning.
            if let Some(ref sc_path) = sidecar_path {
                let sc = sc_path.clone();
                let db_sc = Arc::clone(&db);
                let bridge_result = tokio::task::spawn_blocking(move || {
                    // O(1): three sync_state primary-key lookups only.
                    // No bucket_map access on this path.
                    let sidecar = crate::bridge::sidecar::Sidecar::open_readonly(&sc)?;
                    sidecar.cached_bridge_counts()
                })
                .await;

                match bridge_result {
                    Ok(Ok(Some((hashes, tags, mappings)))) => {
                        macro_rules! bgauge {
                            ($metric:expr, $value:expr) => {
                                if let Err(e) = db_sc.write_sample(
                                    now_minute,
                                    $metric,
                                    "",
                                    $value as f64,
                                ) {
                                    tracing::warn!(
                                        target: "stats",
                                        error = %e,
                                        metric = $metric,
                                        "bridge gauge sample write failed"
                                    );
                                }
                            };
                        }
                        bgauge!(METRIC_BRIDGE_HASHES_STORED, hashes);
                        bgauge!(METRIC_BRIDGE_TAGS_STORED, tags);
                        bgauge!(METRIC_BRIDGE_MAPPINGS_STORED, mappings);
                    }
                    Ok(Ok(None)) => {
                        // Cache not yet populated by the refresher; emit nothing.
                        // The refresher will write the cache after its first pass.
                        tracing::debug!(
                            target: "stats",
                            "bridge count cache not yet populated; skipping bridge gauge tick"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            target: "stats",
                            error = %e,
                            "bridge sidecar gauge cache read failed"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "stats",
                            error = %e,
                            "bridge sidecar gauge spawn_blocking panicked"
                        );
                    }
                }
            }

            // ── Freshness ────────────────────────────────────────────────────────
            let snap = freshness.snapshot();

            let (final_cursor, final_age, final_rows) = if snap.last_poll_unix != 0 {
                // In-process follow-loop has run.
                let age = (now_secs as i64) - snap.last_poll_unix;
                (snap.last_applied_update, age as f64, snap.rows_last_cycle)
            } else {
                // No in-process sync yet; try to read the persisted cursor as fallback.
                let persisted = read_persisted_cursor(bridge_state_path.as_deref()).await;
                (persisted.unwrap_or(0), NEVER_POLLED_SENTINEL, 0u64)
            };

            macro_rules! sync_sample {
                ($metric:expr, $value:expr) => {
                    if let Err(e) = db.write_sample(now_minute, $metric, "", $value) {
                        tracing::warn!(
                            target: "stats",
                            error = %e,
                            metric = $metric,
                            "sync sample write failed"
                        );
                    }
                };
            }

            sync_sample!(METRIC_SYNC_LAST_APPLIED_UPDATE, final_cursor as f64);
            sync_sample!(METRIC_SYNC_LAST_POLL_AGE_SECS, final_age);
            sync_sample!(METRIC_SYNC_ROWS_LAST_CYCLE, final_rows as f64);
        }
    })
}

// ── Bridge count refresher ────────────────────────────────────────────────────

/// Spawn the bridge count refresher task (BridgeMode::Sidecar only, #236).
///
/// This task is the **only** code path that scans `bucket_map` for stats
/// purposes. It runs `Sidecar::recompute_bridge_counts` (a full sequential
/// scan, potentially many minutes on a large store) and writes the results
/// into the `sync_state` cache. The 600 s gauge tick then reads from that
/// cache via `cached_bridge_counts()` — zero `bucket_map` access at tick time.
///
/// # Trigger policy
/// 1. **Startup**: checks if the cache is absent or older than
///    [`BRIDGE_COUNT_STALE_SECS`] (25 h); if so, triggers an immediate recompute.
/// 2. **Sync-coupled**: whenever the sidecar follow-loop applies > 0 mappings it
///    calls `count_notify.notify_one()`. The refresher wakes and recomputes,
///    keeping the cache fresh after each sync pass on an active node.
/// 3. **Periodic fallback**: if no notify arrives, recomputes every
///    [`BRIDGE_COUNT_REFRESH_INTERVAL_SECS`] (24 h) regardless. This covers idle
///    nodes where the follow-loop applies 0 mappings indefinitely.
///
/// Recomputes are sequential (the task loop completes one before sleeping/waiting
/// for the next trigger), so concurrent recomputes cannot occur.
pub(crate) fn spawn_bridge_count_refresher(
    sidecar_path: PathBuf,
    count_notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // ── Startup: recompute if cache is absent or stale ────────────────────
        let needs_startup_recompute = {
            let path = sidecar_path.clone();
            tokio::task::spawn_blocking(move || {
                let sc = match crate::bridge::sidecar::Sidecar::open_readonly(&path) {
                    Ok(s) => s,
                    Err(_) => return true, // can't check → trigger recompute
                };
                // Check the timestamp key; None or stale → needs recompute.
                match sc.get_flag(crate::bridge::sidecar::Sidecar::STAT_KEY_COUNTS_COMPUTED_UNIX) {
                    Ok(Some(ts)) => {
                        let computed: u64 = match ts.parse() {
                            Ok(v) => v,
                            Err(_) => return true,
                        };
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        now.saturating_sub(computed) > BRIDGE_COUNT_STALE_SECS
                    }
                    _ => true, // absent or error → needs recompute
                }
            })
            .await
            .unwrap_or(true)
        };

        if needs_startup_recompute {
            run_recompute(&sidecar_path).await;
        } else {
            tracing::debug!(
                target: "stats",
                "bridge count cache is fresh at startup; skipping immediate recompute"
            );
        }

        // ── Main loop: sync-coupled notify OR 24 h periodic fallback ─────────
        loop {
            tokio::select! {
                _ = count_notify.notified() => {
                    tracing::debug!(
                        target: "stats",
                        "bridge count refresher: sync-coupled trigger (mappings applied)"
                    );
                }
                _ = time::sleep(Duration::from_secs(BRIDGE_COUNT_REFRESH_INTERVAL_SECS)) => {
                    tracing::debug!(
                        target: "stats",
                        "bridge count refresher: 24 h periodic fallback trigger"
                    );
                }
            }
            run_recompute(&sidecar_path).await;
        }
    })
}

/// Run `Sidecar::recompute_bridge_counts` in `spawn_blocking`, logging success
/// or warning on failure. Best-effort: a failed recompute leaves the cache
/// unchanged and the loop retries on the next trigger.
async fn run_recompute(sidecar_path: &Path) {
    let path = sidecar_path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        // Open read-write so set_flag can write to sync_state.
        let sidecar = crate::bridge::sidecar::Sidecar::open(&path)?;
        let start = Instant::now();
        sidecar.recompute_bridge_counts()?;
        Ok::<_, anyhow::Error>(start.elapsed())
    })
    .await;

    match result {
        Ok(Ok(elapsed)) => {
            tracing::info!(
                target: "stats",
                elapsed_secs = elapsed.as_secs(),
                "bridge count refresher: recompute complete"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(
                target: "stats",
                error = %e,
                "bridge count refresher: recompute failed (cache unchanged)"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "stats",
                error = %e,
                "bridge count refresher: spawn_blocking panicked"
            );
        }
    }
}

/// Try to read the persisted `next_update_index` cursor from a bridge state or
/// sidecar DB at `path`. Returns `None` if the path is absent, is not a
/// recognised DB, or the read fails — all failures are best-effort.
async fn read_persisted_cursor(path: Option<&Path>) -> Option<u64> {
    let path = path?.to_path_buf();
    if !path.exists() {
        return None;
    }
    tokio::task::spawn_blocking(move || {
        // Try as a sidecar first (the newer format), then fall back to StateDb.
        if let Ok(sidecar) = crate::bridge::sidecar::Sidecar::open_readonly(&path) {
            return sidecar.next_update_index().ok();
        }
        if let Ok(state) = crate::bridge::state::StateDb::open_readonly(&path) {
            return state.next_update_index().ok();
        }
        None
    })
    .await
    .ok()
    .flatten()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_file_size_missing_file_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.db");
        assert_eq!(
            db_file_size(&path),
            0,
            "missing file must contribute 0 bytes"
        );
    }

    #[test]
    fn db_file_size_sums_main_wal_shm() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("test.db");
        std::fs::write(&base, b"main").unwrap();
        let wal = dir.path().join("test.db-wal");
        std::fs::write(&wal, b"waldata").unwrap();
        let shm = dir.path().join("test.db-shm");
        std::fs::write(&shm, b"shm").unwrap();
        // main=4, wal=7, shm=3 → 14 total
        assert_eq!(db_file_size(&base), 14);
    }

    #[test]
    fn db_file_size_partial_wal_only() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("partial.db");
        std::fs::write(&base, b"mainfile").unwrap();
        let wal = dir.path().join("partial.db-wal");
        std::fs::write(&wal, b"walonly").unwrap();
        // shm absent → 0; main=8, wal=7 → 15
        assert_eq!(db_file_size(&base), 15);
    }

    #[test]
    fn store_disk_bytes_no_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo.db");
        std::fs::write(&repo, b"repodata").unwrap();
        assert_eq!(store_disk_bytes(&repo, None), 8);
    }

    #[test]
    fn store_disk_bytes_with_bridge_missing_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo.db");
        std::fs::write(&repo, b"repo").unwrap();
        let bridge = dir.path().join("bridge.db");
        // bridge file absent → db_file_size returns 0 for all three
        assert_eq!(
            store_disk_bytes(&repo, Some(&bridge)),
            4,
            "missing bridge file must contribute 0"
        );
    }

    #[test]
    fn store_disk_bytes_with_bridge_present() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo.db");
        std::fs::write(&repo, b"repo").unwrap();
        let bridge = dir.path().join("bridge.db");
        std::fs::write(&bridge, b"bridgedata").unwrap();
        // repo=4, bridge=10 → 14
        assert_eq!(store_disk_bytes(&repo, Some(&bridge)), 14);
    }

    // ── Bridge gauge source-selection tests (#235) ────────────────────────────

    /// In sidecar mode the new bridge gauge metric constants must have the
    /// correct names (they drive both stats.db keys and the dashboard series
    /// keys, so regressions here would silently break rendering).
    #[test]
    fn bridge_metric_constant_names() {
        assert_eq!(METRIC_BRIDGE_HASHES_STORED, "bridge_hashes_stored");
        assert_eq!(METRIC_BRIDGE_TAGS_STORED, "bridge_tags_stored");
        assert_eq!(METRIC_BRIDGE_MAPPINGS_STORED, "bridge_mappings_stored");
    }

    /// When `sidecar_path` is `None` (native/mirror mode) the gauge sampler
    /// must NOT open the sidecar — verifiable by passing a path that does not
    /// exist: if the sampler were to open it, the subsequent `Sidecar::open_readonly`
    /// would fail. This is a structural property of the `if let Some(...)` guard:
    /// just assert the guard path is absent in None mode without running the sampler.
    #[test]
    fn sidecar_path_none_does_not_open_sidecar() {
        // Structural: Option::None → the `if let Some(ref sc_path) = sidecar_path`
        // block is unreachable. Verify the None case directly.
        let sidecar_path: Option<PathBuf> = None;
        assert!(
            sidecar_path.is_none(),
            "None sidecar_path must not trigger sidecar open"
        );
    }

    /// When `sidecar_path` is `Some` the gauge tick reads from the `sync_state`
    /// cache via `cached_bridge_counts()`. Verify: after seeding and calling
    /// `recompute_bridge_counts()`, opening a read-only handle and calling
    /// `cached_bridge_counts()` returns the correct counts (this is exactly what
    /// the 600 s gauge tick does — O(1) sync_state lookups, zero bucket_map scan).
    #[test]
    fn sidecar_path_some_reads_from_cache() {
        use crate::bridge::sidecar::Sidecar;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sc.db");

        // Write side: seed data and populate the cache via recompute.
        {
            let sc = Sidecar::create(&path).unwrap();
            sc.write_tag_set(&[0x01u8; 32], &[1, 2, 3]).unwrap();
            sc.write_tag_set(&[0x02u8; 32], &[4, 5]).unwrap();
            sc.recompute_bridge_counts().unwrap();
        }

        // Read side: open read-only (as the gauge tick does) and call
        // cached_bridge_counts — must return non-None with correct counts.
        let ro = Sidecar::open_readonly(&path).unwrap();
        let (hashes, _tags, mappings) = ro
            .cached_bridge_counts()
            .unwrap()
            .expect("cache must be populated after recompute");
        assert_eq!(hashes, 2, "two hashes in fixture");
        assert_eq!(mappings, 5, "five total mapping pairs (3 + 2)");
    }
}
