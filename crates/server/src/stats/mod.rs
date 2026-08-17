//! Built-in, loopback-only statistics subsystem (#235). Records request,
//! system, store, and PTR-sync metrics into a separate `stats.db`, rolls them
//! up RRD-style, and serves a self-contained dashboard on a second loopback
//! port. Strictly subordinate to serving: every failure logs one
//! `warn target: "stats"` and the repo keeps serving.

pub(crate) mod freshness;
pub(crate) mod http;
pub(crate) mod middleware;
pub(crate) mod sampler;
pub(crate) mod store;
pub(crate) mod users;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Notify;

use crate::settings::StatsConfig;
use crate::stats::freshness::SyncFreshness;
use crate::stats::middleware::{MinuteAccumulator, StatsLayer};
use crate::stats::store::StatsDb;
use crate::stats::users::UserCounter;

/// All shared handles for the stats subsystem.
///
/// Cloneable via `Arc` internals; passed into the main router and the
/// follow-loops. Fields are `pub(crate)` — external callers use the
/// [`make_layer`][StatsHandle::make_layer] and
/// [`freshness`][StatsHandle::freshness] accessors.
pub struct StatsHandle {
    #[allow(dead_code)] // keep-alive Arc for flush/sampler tasks spawned in spawn_stats
    pub(crate) db: Arc<StatsDb>,
    pub(crate) accum: Arc<MinuteAccumulator>,
    pub(crate) users: Arc<UserCounter>,
    pub(crate) freshness: Arc<SyncFreshness>,
    /// Notify handle for the bridge count refresher task (#236).
    ///
    /// `Some` only in `BridgeMode::Sidecar`. The sidecar follow-loop calls
    /// `notify_one()` after each sync pass that applied > 0 mappings, triggering
    /// a background recompute of the `bucket_map` count cache. `None` in all
    /// other modes so the follow-loop and the refresher are both absent.
    pub(crate) count_refresh_notify: Option<Arc<Notify>>,
}

impl StatsHandle {
    /// Build a [`StatsLayer`] for the main router (wraps `accum` + `users`).
    pub fn make_layer(&self) -> StatsLayer {
        StatsLayer::new(Arc::clone(&self.accum), Arc::clone(&self.users))
    }

    /// A shared freshness handle to hand to the follow-loop.
    pub fn freshness(&self) -> Arc<SyncFreshness> {
        Arc::clone(&self.freshness)
    }

    /// The bridge count refresh notifier for the sidecar follow-loop (#236).
    ///
    /// Returns `Some` only in `BridgeMode::Sidecar` (i.e. when `spawn_stats`
    /// received a `sidecar_path`). The caller should call `notify_one()` after
    /// each sync pass that applied > 0 mappings.
    pub fn count_refresh_notify(&self) -> Option<Arc<Notify>> {
        self.count_refresh_notify.as_ref().map(Arc::clone)
    }
}

/// Spawn the stats subsystem: open `stats.db`, wire tasks, bind the listener.
///
/// Called from the `Serve` arm **inside** `rt.block_on` after the read pool is
/// built. Returns `None` (one `warn` emitted) when:
/// - `cfg.enabled == false`, or
/// - `stats.db` cannot be opened (best-effort; the main server keeps serving).
///
/// The listener bind failure is also tolerant: a `warn` is logged and the
/// dashboard is simply unavailable — samplers and middleware keep running.
///
/// `sidecar_path` must be `Some` **only** when the serve node is in
/// `BridgeMode::Sidecar`. When `Some`:
/// - The gauge sampler reads bridge counts from the `sync_state` cache via
///   `Sidecar::cached_bridge_counts()` (O(1); no `bucket_map` scan at tick time).
/// - A bridge count refresher task (`spawn_bridge_count_refresher`) is spawned to
///   run the expensive `Sidecar::recompute_bridge_counts()` scan in the background
///   (at startup if the cache is stale, then on each sync-coupled notify or every
///   24 h as a fallback). The returned [`StatsHandle`] carries a `count_refresh_notify`
///   handle for the sidecar follow-loop to trigger post-sync recomputes (#236).
///
/// In mirror or native mode this must be `None` so the bridge gauges are not
/// emitted (the native gauges already reflect the correct data for those modes).
pub async fn spawn_stats(
    cfg: &StatsConfig,
    repo_db: &Path,
    bridge_state: Option<PathBuf>,
    sidecar_path: Option<PathBuf>,
) -> Option<StatsHandle> {
    if !cfg.enabled {
        tracing::warn!(target: "stats", "stats disabled in config; stats subsystem not started");
        return None;
    }

    let db = match StatsDb::open(&cfg.db_path) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            tracing::warn!(
                target: "stats",
                error = %e,
                path = %cfg.db_path.display(),
                "stats.db open failed; stats disabled (main server unaffected)"
            );
            return None;
        }
    };

    let now_epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let accum = Arc::new(MinuteAccumulator::new(now_epoch_secs / 60 * 60));
    let users = Arc::new(UserCounter::production());
    let freshness = Arc::new(SyncFreshness::default());

    // Flush task: every 60 s swap the minute bucket and write it to stats.db,
    // also driving user-window roll-ups.
    crate::stats::middleware::spawn_flush(Arc::clone(&accum), Arc::clone(&db), Arc::clone(&users));

    // Hourly rollup task: minute→hour→day roll-up + prune.
    {
        let db_r = Arc::clone(&db);
        tokio::spawn(async move {
            // Align to the next hour boundary.
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let secs_into_hour = now_secs % 3600;
            let first_delay = if secs_into_hour == 0 {
                3600
            } else {
                3600 - secs_into_hour
            };
            tokio::time::sleep(tokio::time::Duration::from_secs(first_delay)).await;

            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                if let Err(e) = db_r.roll_and_prune(now) {
                    tracing::warn!(
                        target: "stats",
                        error = %e,
                        "stats roll_and_prune failed; skipping this hour"
                    );
                }
            }
        });
    }

    // System sampler: 60 s.
    crate::stats::sampler::spawn_system_sampler(
        Arc::clone(&db),
        repo_db.to_owned(),
        bridge_state.clone(),
    );

    // Bridge count refresher + gauge sampler: 600 s (#236).
    //
    // In sidecar mode the refresher task owns the expensive `bucket_map` scan
    // (runs at startup + on sync-coupled notify + 24 h periodic fallback). The
    // gauge sampler reads only from the `sync_state` cache — zero live scans.
    let count_refresh_notify: Option<Arc<Notify>> = if let Some(ref sc_path) = sidecar_path {
        let notify = Arc::new(Notify::new());
        crate::stats::sampler::spawn_bridge_count_refresher(sc_path.clone(), Arc::clone(&notify));
        Some(notify)
    } else {
        None
    };

    crate::stats::sampler::spawn_gauge_sampler(
        Arc::clone(&db),
        Arc::clone(&freshness),
        repo_db.to_owned(),
        bridge_state,
        sidecar_path,
    );

    // Bind the stats HTTP listener (best-effort: bind failure = dashboard
    // unavailable, samplers + middleware keep running).
    let started = Instant::now();
    match crate::stats::http::StatsHttpState::open(&cfg.db_path, started) {
        Ok(state) => {
            let state = Arc::new(state);
            match tokio::net::TcpListener::bind(cfg.listen).await {
                Ok(listener) => {
                    let router = crate::stats::http::app(state);
                    tokio::spawn(async move {
                        if let Err(e) = axum::serve(listener, router).await {
                            tracing::warn!(
                                target: "stats",
                                error = %e,
                                "stats listener exited with error"
                            );
                        }
                    });
                    tracing::info!(
                        target: "stats",
                        listen = %cfg.listen,
                        "stats dashboard available"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "stats",
                        listen = %cfg.listen,
                        error = %e,
                        "stats listener bind failed; dashboard unavailable (samplers still running)"
                    );
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "stats",
                error = %e,
                "stats HTTP state open failed; dashboard unavailable (samplers still running)"
            );
        }
    }

    Some(StatsHandle {
        db,
        accum,
        users,
        freshness,
        count_refresh_notify,
    })
}
