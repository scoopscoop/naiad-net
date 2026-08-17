//! Startup cache-warmup phase, surfaced through `/api/health` so the UI can
//! show a "Preparing library" activity job while the warmup runs — the same
//! health-poll piggyback already used for background watch-registration (#110)
//! and the catch-up rescan (#119).
//!
//! Before this, the ~96s warmup on a cold 95k-file library was invisible: the
//! catch-up scan defers behind it (#126), so its counters stay at zero and the
//! activity panel reported "Nothing running." for the whole window (#130).
//!
//! The phase is **written by the warmup task itself**, not inferred from the
//! `graph_ready` / `warmup_done` gates. Inference cannot distinguish "parked on
//! the startup gate" from "building": `StartupGate::wait` also returns on a
//! timeout without firing, so a gate-derived phase reports a parked warmup long
//! after it has actually started reading pages.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Which step of the background cache warmup is in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WarmupPhase {
    /// No warmup has been spawned — the daemon was built without a read pool to
    /// warm. Reported with `complete = true`: there is nothing to wait for.
    Idle,
    /// Spawned, but parked on the startup gate waiting for the first gallery
    /// query (#121). Nothing is being read yet, so the UI must not claim work
    /// has begun.
    Queued,
    /// Building the merged relation graph (the ~34s cold step on a 95k library).
    Graph,
    /// Graph is built; walking the tag-completion index/table/count pages.
    Completion,
    /// Both steps have finished (or failed — the warmup advances either way).
    Done,
}

impl WarmupPhase {
    const IDLE: u8 = 0;
    const QUEUED: u8 = 1;
    const GRAPH: u8 = 2;
    const COMPLETION: u8 = 3;
    const DONE: u8 = 4;

    fn code(self) -> u8 {
        match self {
            Self::Idle => Self::IDLE,
            Self::Queued => Self::QUEUED,
            Self::Graph => Self::GRAPH,
            Self::Completion => Self::COMPLETION,
            Self::Done => Self::DONE,
        }
    }

    fn from_code(c: u8) -> Self {
        match c {
            Self::QUEUED => Self::Queued,
            Self::GRAPH => Self::Graph,
            Self::COMPLETION => Self::Completion,
            Self::DONE => Self::Done,
            // Only `code()` ever writes the cell, so this is unreachable in
            // practice; treating anything unknown as `Idle` keeps the invariant
            // "not `Idle` means a warmup exists that will fire `graph_ready`"
            // conservative rather than inventing phantom work.
            _ => Self::Idle,
        }
    }

    /// Whether nothing is being warmed: finished, or never started.
    fn is_settled(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }
}

/// Progress of the startup cache warmup. Serialized into `/api/health`.
///
/// # Invariant
/// `complete` is true exactly when nothing is being warmed: either the warmup
/// finished (`Done`) or it was never spawned (`Idle`). The UI shows its
/// "Preparing library" job only while `complete` is false, so a daemon with no
/// read pool never grows a job that can't finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct WarmupStatus {
    pub phase: WarmupPhase,
    pub complete: bool,
}

/// Phase cell written by `spawn_cache_warmup` as it advances, read by
/// `health_handler` and by `await_relation_graph`.
#[derive(Debug)]
pub(crate) struct WarmupProgress {
    phase: AtomicU8,
}

impl WarmupProgress {
    pub(crate) fn new() -> Self {
        Self {
            phase: AtomicU8::new(WarmupPhase::IDLE),
        }
    }

    /// Advance to `phase`. Called only from the warmup task, in order.
    pub(crate) fn set(&self, phase: WarmupPhase) {
        self.phase.store(phase.code(), Ordering::Release);
    }

    pub(crate) fn phase(&self) -> WarmupPhase {
        WarmupPhase::from_code(self.phase.load(Ordering::Acquire))
    }

    /// Whether a warmup exists that will eventually fire `graph_ready`. When it
    /// does not, an interactive tag read must not wait on that gate — nothing
    /// will ever fire it and the read stalls for the full backstop (#131).
    /// Production code no longer calls this directly: gates are pre-fired by
    /// construction so the guard is not needed (#132). Kept for tests.
    #[allow(dead_code)]
    pub(crate) fn is_spawned(&self) -> bool {
        self.phase() != WarmupPhase::Idle
    }

    pub(crate) fn status(&self) -> WarmupStatus {
        let phase = self.phase();
        WarmupStatus {
            phase,
            complete: phase.is_settled(),
        }
    }
}

impl Default for WarmupProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared handle written by the warmup task, read by `health_handler`.
pub(crate) type WarmupShared = Arc<WarmupProgress>;

/// A fresh, not-yet-started progress cell.
pub(crate) fn new_shared() -> WarmupShared {
    Arc::new(WarmupProgress::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_cell_is_idle_settled_and_unspawned() {
        let p = WarmupProgress::new();
        let s = p.status();
        assert_eq!(s.phase, WarmupPhase::Idle);
        assert!(s.complete, "nothing is warming, so nothing is pending");
        assert!(!p.is_spawned());
    }

    /// The parked state is the one a gate-derived phase got wrong: spawned, but
    /// waiting on the startup gate with nothing read yet.
    #[test]
    fn queued_is_spawned_but_not_complete() {
        let p = WarmupProgress::new();
        p.set(WarmupPhase::Queued);
        let s = p.status();
        assert_eq!(s.phase, WarmupPhase::Queued);
        assert!(!s.complete);
        assert!(
            p.is_spawned(),
            "a parked warmup will still fire graph_ready"
        );
    }

    #[test]
    fn working_phases_are_spawned_and_incomplete() {
        for phase in [WarmupPhase::Graph, WarmupPhase::Completion] {
            let p = WarmupProgress::new();
            p.set(phase);
            let s = p.status();
            assert_eq!(s.phase, phase);
            assert!(!s.complete);
            assert!(p.is_spawned());
        }
    }

    #[test]
    fn done_is_complete_and_still_counts_as_spawned() {
        let p = WarmupProgress::new();
        p.set(WarmupPhase::Done);
        let s = p.status();
        assert_eq!(s.phase, WarmupPhase::Done);
        assert!(s.complete);
        assert!(p.is_spawned());
    }

    #[test]
    fn phases_serialize_as_snake_case_strings() {
        let p = WarmupProgress::new();
        let v = serde_json::to_value(p.status()).unwrap();
        assert_eq!(v["phase"], "idle");
        assert_eq!(v["complete"], true);

        p.set(WarmupPhase::Queued);
        assert_eq!(serde_json::to_value(p.status()).unwrap()["phase"], "queued");

        p.set(WarmupPhase::Graph);
        assert_eq!(serde_json::to_value(p.status()).unwrap()["phase"], "graph");

        p.set(WarmupPhase::Completion);
        assert_eq!(
            serde_json::to_value(p.status()).unwrap()["phase"],
            "completion"
        );

        p.set(WarmupPhase::Done);
        let v = serde_json::to_value(p.status()).unwrap();
        assert_eq!(v["phase"], "done");
        assert_eq!(v["complete"], true);
    }

    /// Every variant must round-trip through the atomic cell, or a phase would
    /// silently read back as `Idle` and the UI would drop the job.
    #[test]
    fn every_phase_round_trips_through_the_cell() {
        for phase in [
            WarmupPhase::Idle,
            WarmupPhase::Queued,
            WarmupPhase::Graph,
            WarmupPhase::Completion,
            WarmupPhase::Done,
        ] {
            let p = WarmupProgress::new();
            p.set(phase);
            assert_eq!(p.phase(), phase, "{phase:?} must round-trip");
        }
    }
}
