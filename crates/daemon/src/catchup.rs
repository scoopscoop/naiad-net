//! Startup catch-up rescan progress, surfaced through `/api/health` so the UI
//! can show a "Library scan" activity job without a second endpoint — the same
//! health-poll piggyback used for background watch-registration (#110, #119).

use std::sync::{Arc, Mutex};

/// Progress of the startup catch-up rescan (`naiad-catchup-scan` thread).
/// Serialized into `/api/health`.
///
/// # Post-startup invariant
/// `complete` latches to `true` once the rescan returns (success *or* error) and
/// never resets; `running` is `false` from that point on. A default value
/// (all-zero, `running=false`, `complete=false`) means "no catch-up has run" —
/// the state an in-process test or a daemon started without a watcher reports.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct CatchupStatus {
    /// Whether the catch-up scan thread is currently running.
    pub running: bool,
    /// Files imported across all roots so far.
    pub imported: u64,
    /// Per-file indexing errors across all roots so far.
    pub errors: u64,
    /// Registered roots to scan at startup.
    pub roots_total: usize,
    /// Registered roots fully scanned so far.
    pub roots_done: usize,
    /// Basename/path of the root currently being scanned, or `None`.
    pub current: Option<String>,
    /// Latches true when the rescan returns; never resets.
    pub complete: bool,
}

/// Shared handle written by the catch-up scan thread, read by `health_handler`.
pub(crate) type CatchupShared = Arc<Mutex<CatchupStatus>>;

/// A fresh idle status (`running=false`, `complete=false`, all counts zero).
pub(crate) fn new_shared() -> CatchupShared {
    Arc::new(Mutex::new(CatchupStatus::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn populated_status_serializes_with_expected_fields() {
        let s = CatchupStatus {
            running: true,
            imported: 12_000,
            errors: 3,
            roots_total: 2,
            roots_done: 1,
            current: Some("newstuff".into()),
            complete: false,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["running"], true);
        assert_eq!(v["imported"], 12_000);
        assert_eq!(v["errors"], 3);
        assert_eq!(v["roots_total"], 2);
        assert_eq!(v["roots_done"], 1);
        assert_eq!(v["current"], "newstuff");
        assert_eq!(v["complete"], false);
    }

    #[test]
    fn default_is_idle_and_incomplete() {
        let v = serde_json::to_value(CatchupStatus::default()).unwrap();
        assert_eq!(v["running"], false);
        assert_eq!(v["complete"], false);
        assert_eq!(v["imported"], 0);
        assert_eq!(v["roots_total"], 0);
        assert_eq!(v["current"], serde_json::Value::Null);
    }
}
