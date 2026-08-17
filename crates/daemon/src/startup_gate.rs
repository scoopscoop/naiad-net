//! One-shot gate that holds heavy startup background work — the relation-graph
//! and tag-completion cache warmup — until the first interactive gallery query
//! has been served, or a fallback timeout elapses. On a cold OS file cache the
//! warmup's multi-hundred-megabyte sequential reads otherwise run concurrently
//! with the UI's first `/api/search` and starve it, leaving the gallery blank
//! for tens of seconds (#121). Letting the query go first means it faults in the
//! pages it needs off an uncontended disk; the warmup then runs behind it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// Shared signal: the first gallery query releases the gate via [`fire`]; the
/// background warmup task waits on it via [`wait`], bounded by a timeout so a
/// headless daemon (no UI, no first query) still warms its caches.
///
/// [`fire`]: StartupGate::fire
/// [`wait`]: StartupGate::wait
#[derive(Debug)]
pub(crate) struct StartupGate {
    fired: AtomicBool,
    notify: Notify,
}

impl StartupGate {
    pub(crate) fn new() -> Self {
        Self {
            fired: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// A gate that is already fired at construction. Used for gates whose arming
    /// path may never run: a consumer that waits on an unarmed gate falls through
    /// instantly instead of eating its backstop timeout (#132).
    pub(crate) fn fired() -> Self {
        Self {
            fired: AtomicBool::new(true),
            notify: Notify::new(),
        }
    }

    /// Release the gate. Idempotent and cheap: only the first caller wakes the
    /// waiter, so handlers can call this on every query without coordination.
    pub(crate) fn fire(&self) {
        if !self.fired.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
        }
    }

    /// Whether the gate has already fired. Cheap, non-blocking; used to skip
    /// startup-window-only work (e.g. forcing a relation-graph warm) once the
    /// background warmup has completed.
    pub(crate) fn is_fired(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }

    /// Wait until [`fire`](Self::fire) is called or `timeout` elapses, whichever
    /// comes first. Returns immediately if the gate has already fired.
    pub(crate) async fn wait(&self, timeout: Duration) {
        if self.fired.load(Ordering::Acquire) {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        // Register interest before the re-check below: `notify_waiters` only
        // wakes waiters already registered at the time it is called, so without
        // enabling first a `fire()` racing this call could be missed. With the
        // enable + re-check, either we observe `fired` set or our registered
        // waiter is guaranteed to be woken.
        notified.as_mut().enable();
        if self.fired.load(Ordering::Acquire) {
            return;
        }
        tokio::select! {
            () = notified => {}
            () = tokio::time::sleep(timeout) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_returns_after_fire() {
        let gate = StartupGate::new();
        gate.fire();
        // Already fired: returns immediately, well within the generous timeout.
        gate.wait(Duration::from_secs(30)).await;
    }

    #[tokio::test]
    async fn wait_wakes_on_concurrent_fire() {
        let gate = std::sync::Arc::new(StartupGate::new());
        let g2 = gate.clone();
        let waiter = tokio::spawn(async move { g2.wait(Duration::from_secs(30)).await });
        // Give the waiter a chance to register, then release it.
        tokio::task::yield_now().await;
        gate.fire();
        waiter.await.unwrap();
    }

    #[tokio::test]
    async fn wait_falls_back_to_timeout() {
        let gate = StartupGate::new();
        // Never fired: `wait` returns once the (short) timeout elapses.
        gate.wait(Duration::from_millis(50)).await;
        assert!(!gate.fired.load(Ordering::Acquire));
    }

    /// A gate constructed with [`StartupGate::fired`] is already fired: `is_fired`
    /// returns `true` immediately, and `wait` with a generous timeout returns
    /// without blocking. Used so an unarmed gate falls through instantly instead
    /// of eating its backstop timeout (#132).
    #[tokio::test]
    async fn fired_constructor_reports_fired_and_wait_returns_immediately() {
        let gate = StartupGate::fired();
        assert!(
            gate.is_fired(),
            "fired() gate must report is_fired() == true"
        );
        // wait should return immediately without consuming the timeout.
        tokio::time::timeout(
            Duration::from_millis(200),
            gate.wait(Duration::from_secs(30)),
        )
        .await
        .expect("wait on a fired() gate must return immediately");
    }
}
