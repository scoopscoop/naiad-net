//! `SyncFreshness` — shared handle updated by the in-process follow-loop
//! once per completed pass; the gauge sampler reads it every 10 min (#235, Piece D).
//!
//! Atomics only — never blocks sync. Default state is "never polled"
//! (all fields zero).
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// A point-in-time snapshot of sync freshness read from [`SyncFreshness`].
///
/// Each field is loaded with `Relaxed` ordering independently, so a concurrent
/// [`SyncFreshness::record_pass`] may produce a torn read (one field from the
/// old pass, another from the new). This is acceptable — the gauge sampler
/// treats freshness as best-effort and the values are only advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FreshnessSnapshot {
    /// Unix timestamp of the last successful poll; `0` = never polled.
    pub last_poll_unix: i64,
    /// The `next_update_index` cursor after the last pass.
    pub last_applied_update: u64,
    /// Rows applied in the most recent pass.
    pub rows_last_cycle: u64,
}

/// Shared handle the in-process follow-loop updates once per completed pass;
/// the gauge sampler reads it every 10 min. Atomics — never blocks sync.
#[derive(Default)]
pub struct SyncFreshness {
    /// Unix timestamp of the last *successful* poll (zero = never polled).
    last_poll_unix: AtomicI64,
    /// The `next_update_index` cursor after the last pass.
    last_applied_update: AtomicU64,
    /// Rows applied in the most recent pass.
    rows_last_cycle: AtomicU64,
}

impl SyncFreshness {
    /// Record one completed sync pass (even a zero-row pass updates the poll ts).
    ///
    /// Parameters: `cursor` = `next_update_index` after the pass,
    /// `rows` = rows applied this cycle, `now_unix` = current UTC seconds.
    pub(crate) fn record_pass(&self, cursor: u64, rows: u64, now_unix: i64) {
        self.last_poll_unix.store(now_unix, Ordering::Relaxed);
        self.last_applied_update.store(cursor, Ordering::Relaxed);
        self.rows_last_cycle.store(rows, Ordering::Relaxed);
    }

    /// Read a point-in-time snapshot.
    ///
    /// Each atomic is loaded independently with `Relaxed` ordering; a
    /// concurrent `record_pass` may produce a torn read — this is acceptable
    /// because freshness data is best-effort and only used for dashboard display.
    /// `last_poll_unix == 0` means never polled.
    pub(crate) fn snapshot(&self) -> FreshnessSnapshot {
        FreshnessSnapshot {
            last_poll_unix: self.last_poll_unix.load(Ordering::Relaxed),
            last_applied_update: self.last_applied_update.load(Ordering::Relaxed),
            rows_last_cycle: self.rows_last_cycle.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_polled_reads_zero_then_records() {
        let f = SyncFreshness::default();
        // Initial state: everything zero (never polled sentinel).
        let snap = f.snapshot();
        assert_eq!(
            snap.last_poll_unix, 0,
            "default last_poll_unix must be zero"
        );
        assert_eq!(
            snap.last_applied_update, 0,
            "default last_applied_update must be zero"
        );
        assert_eq!(
            snap.rows_last_cycle, 0,
            "default rows_last_cycle must be zero"
        );
        // Record one completed pass.
        f.record_pass(20481, 1204, 1_755_213_600);
        let snap2 = f.snapshot();
        assert_eq!(snap2.last_poll_unix, 1_755_213_600);
        assert_eq!(snap2.last_applied_update, 20481);
        assert_eq!(snap2.rows_last_cycle, 1204);
    }

    #[test]
    fn zero_row_pass_still_updates_poll_timestamp() {
        let f = SyncFreshness::default();
        f.record_pass(0, 0, 1_755_213_600);
        let snap = f.snapshot();
        assert_eq!(
            snap.last_poll_unix, 1_755_213_600,
            "zero-row pass must update last_poll_unix"
        );
        assert_eq!(snap.last_applied_update, 0);
        assert_eq!(snap.rows_last_cycle, 0);
    }

    #[test]
    fn successive_passes_overwrite_previous() {
        let f = SyncFreshness::default();
        f.record_pass(100, 50, 1_000_000);
        f.record_pass(200, 0, 1_001_000);
        let snap = f.snapshot();
        assert_eq!(snap.last_poll_unix, 1_001_000);
        assert_eq!(snap.last_applied_update, 200);
        assert_eq!(snap.rows_last_cycle, 0);
    }
}
