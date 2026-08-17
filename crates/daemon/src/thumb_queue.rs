//! Newest-first (LIFO) permit queue for thumbnail generation (#54).
//!
//! Tokio's `Semaphore` admits waiters FIFO, so after a deep gallery fling the
//! tiles currently on screen wait behind the whole stale backlog. This queue
//! keeps the semaphore as the concurrency bound but parks waiters on an
//! explicit stack: the most recently arrived request — approximately what the
//! user is looking at — is admitted first.

use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

/// A bounded permit pool whose waiters are admitted newest-first.
pub struct LifoPermits {
    /// The concurrency bound. Only touched via `try_acquire`; blocked callers
    /// park on `waiters` instead, so the semaphore's own FIFO queue stays empty.
    sem: Arc<Semaphore>,
    /// Parked callers, top of the stack = most recent arrival. Entries whose
    /// receiver was dropped are skipped at hand-off time.
    waiters: Mutex<Vec<oneshot::Sender<Permit>>>,
}

/// An admitted slot. Dropping it hands the slot to the newest live waiter, or
/// returns it to the pool when nobody is parked.
pub struct Permit {
    queue: Arc<LifoPermits>,
    /// `None` only on the corpse left behind after a failed hand-off (the
    /// permit was already moved out), which makes its own drop a no-op.
    inner: Option<OwnedSemaphorePermit>,
}

impl LifoPermits {
    #[must_use]
    pub fn new(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            sem: Arc::new(Semaphore::new(permits)),
            waiters: Mutex::new(Vec::new()),
        })
    }

    /// Wait for a slot. Under contention the *newest* caller is admitted
    /// first. Cancellation-safe: dropping the returned future abandons the
    /// parked entry, which is skipped when its turn comes.
    pub async fn acquire(self: &Arc<Self>) -> Permit {
        if let Ok(inner) = self.sem.clone().try_acquire_owned() {
            return Permit {
                queue: self.clone(),
                inner: Some(inner),
            };
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut waiters = self.waiters.lock().expect("thumb queue poisoned");
            // Re-check under the lock: releases hand off under this same lock,
            // so a permit freed since the try above cannot be missed here.
            if let Ok(inner) = self.sem.clone().try_acquire_owned() {
                return Permit {
                    queue: self.clone(),
                    inner: Some(inner),
                };
            }
            waiters.push(tx);
        }
        rx.await
            .expect("queue dropped while waiting — impossible, waiter holds an Arc to it")
    }

    /// Number of parked waiters, including abandoned ones not yet skipped.
    /// Test-visibility hook for synchronizing on queue state.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.waiters.lock().expect("thumb queue poisoned").len()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let Some(mut inner) = self.inner.take() else {
            return; // failed hand-off corpse; the permit already moved on
        };
        let mut waiters = self.queue.waiters.lock().expect("thumb queue poisoned");
        while let Some(tx) = waiters.pop() {
            match tx.send(Permit {
                queue: self.queue.clone(),
                inner: Some(inner),
            }) {
                Ok(()) => return,
                // Waiter gave up (request future dropped): reclaim the permit
                // and try the next one down.
                Err(mut corpse) => {
                    inner = corpse.inner.take().expect("permit vanished in hand-off");
                }
            }
        }
        // Nobody is parked: release to the semaphore while still holding the
        // waiters lock, pairing with the locked re-check in `acquire`.
        drop(inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio::time::{sleep, timeout};

    const WAIT: Duration = Duration::from_secs(2);

    /// Block (bounded) until `queue` has at least `n` parked waiters.
    async fn settled_waiters(queue: &std::sync::Arc<LifoPermits>, n: usize) {
        timeout(WAIT, async {
            while queue.waiting() < n {
                sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("waiter never reached the queue");
    }

    #[tokio::test]
    async fn acquire_is_immediate_when_a_permit_is_free() {
        let queue = LifoPermits::new(1);
        timeout(WAIT, queue.acquire())
            .await
            .expect("free permit should be granted immediately");
    }

    #[tokio::test]
    async fn released_permit_can_be_reacquired() {
        let queue = LifoPermits::new(1);
        let permit = queue.acquire().await;
        drop(permit);
        timeout(WAIT, queue.acquire())
            .await
            .expect("released permit should return to the pool");
    }

    #[tokio::test]
    async fn newest_waiter_is_admitted_first() {
        let queue = LifoPermits::new(1);
        let held = queue.acquire().await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        // Queue two waiters in a known order: "first" arrives, then "second".
        for label in ["first", "second"] {
            let queue_for_task = queue.clone();
            let tx = tx.clone();
            let parked = queue.waiting();
            tokio::spawn(async move {
                let permit = queue_for_task.acquire().await;
                tx.send(label).unwrap();
                drop(permit);
            });
            settled_waiters(&queue, parked + 1).await;
        }

        drop(held);
        // LIFO: the later arrival is admitted first.
        assert_eq!(timeout(WAIT, rx.recv()).await.unwrap(), Some("second"));
        assert_eq!(timeout(WAIT, rx.recv()).await.unwrap(), Some("first"));
    }

    #[tokio::test]
    async fn dropped_waiter_is_skipped_without_losing_the_permit() {
        let queue = LifoPermits::new(1);
        let held = queue.acquire().await;

        // A waiter whose future is dropped mid-wait (client disconnected).
        let queue_for_task = queue.clone();
        let doomed = tokio::spawn(async move {
            let _permit = queue_for_task.acquire().await;
        });
        settled_waiters(&queue, 1).await;
        doomed.abort();
        let _ = doomed.await;

        // A live waiter queued on top of the dead one.
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        let queue_for_task = queue.clone();
        tokio::spawn(async move {
            let permit = queue_for_task.acquire().await;
            tx.send(()).unwrap();
            drop(permit);
        });
        settled_waiters(&queue, 2).await;

        drop(held);
        // The live waiter is admitted despite the dead entry...
        timeout(WAIT, rx.recv())
            .await
            .expect("live waiter starved behind a dropped one")
            .unwrap();
        // ...and the dead waiter's hand-off does not swallow the permit.
        timeout(WAIT, queue.acquire())
            .await
            .expect("permit leaked through the dropped waiter");
    }
}
