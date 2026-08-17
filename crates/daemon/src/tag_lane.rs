//! Dedicated, cancellable SQLite lane for tag-oriented reads.
//!
//! Dropping a `spawn_blocking` join handle does not stop its closure. This lane
//! bridges request cancellation into SQLite's interrupt handle while ensuring a
//! queued request can never interrupt the different request currently holding
//! the connection. Completion, namespace listing, and tag-detail handlers share
//! the lane so they do not consume the general read pool (#50, #70, #76).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use naiad_db::{Db, DbInterruptHandle};

use crate::lock::LockRecover;

const QUEUED: u8 = 0;
const RUNNING: u8 = 1;
const FINISHED: u8 = 2;
const CANCELLED: u8 = 3;

trait QueryInterrupt: Send + Sync {
    fn interrupt(&self);
}

impl QueryInterrupt for DbInterruptHandle {
    fn interrupt(&self) {
        DbInterruptHandle::interrupt(self);
    }
}

/// One read-only connection reserved for completion, namespace, and tag-detail
/// reads, with request-drop cancellation tied to the current lane operation.
#[derive(Clone)]
pub(crate) struct TagLane {
    pub(crate) db: Arc<Mutex<Db>>,
    interrupt: Arc<dyn QueryInterrupt>,
}

impl TagLane {
    pub(crate) fn new(db: Db) -> Self {
        let interrupt = Arc::new(db.interrupt_handle());
        Self {
            db: Arc::new(Mutex::new(db)),
            interrupt,
        }
    }

    #[cfg(test)]
    fn with_interrupt(db: Db, interrupt: Arc<dyn QueryInterrupt>) -> Self {
        Self {
            db: Arc::new(Mutex::new(db)),
            interrupt,
        }
    }

    /// Run one blocking DB operation. If the awaiting handler is dropped, an
    /// active statement is interrupted; a request still queued on the mutex is
    /// merely marked cancelled and skips its closure once it reaches the front.
    /// Completion is published before unlocking, and interrupt delivery is
    /// acknowledged before handoff, so late drops cannot affect the next owner.
    pub(crate) async fn run<T, F>(&self, f: F) -> Result<anyhow::Result<T>, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce(&Db) -> anyhow::Result<T> + Send + 'static,
    {
        let state = Arc::new(OperationState::new());
        let cancel = CancelOnDrop {
            state: Arc::clone(&state),
            interrupt: Arc::clone(&self.interrupt),
        };
        let db = Arc::clone(&self.db);
        let op = std::any::type_name::<F>();

        let result = tokio::task::spawn_blocking(move || {
            let wait_start = Instant::now();
            let db = db.lock_recover();
            let lock_wait = wait_start.elapsed();
            let work_start = Instant::now();

            let out = if state
                .phase
                .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _completion = OperationCompletion(&state);
                let cancellation = Arc::clone(&state);
                db.with_query_cancellation(
                    move || cancellation.is_cancelled(),
                    |db| {
                        if state.is_cancelled() {
                            Err(anyhow::anyhow!(
                                "tag DB operation cancelled before execution"
                            ))
                        } else {
                            f(db)
                        }
                    },
                )
            } else {
                Err(anyhow::anyhow!(
                    "tag DB operation cancelled before execution"
                ))
            };

            crate::server::log_db_op(op, lock_wait, work_start.elapsed());
            out
        })
        .await;

        // On the ordinary completion path the worker has already published
        // FINISHED, so this is a no-op. If this future itself is dropped while
        // awaiting the worker, Drop runs immediately and signals SQLite.
        drop(cancel);
        result
    }
}

struct OperationState {
    phase: AtomicU8,
    interrupt_delivered: Mutex<bool>,
    interrupt_delivery: Condvar,
}

impl OperationState {
    fn new() -> Self {
        Self {
            phase: AtomicU8::new(QUEUED),
            interrupt_delivered: Mutex::new(false),
            interrupt_delivery: Condvar::new(),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.phase.load(Ordering::Acquire) == CANCELLED
    }

    fn finish(&self) {
        if self
            .phase
            .compare_exchange(RUNNING, FINISHED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }

        let mut delivered = self.interrupt_delivered.lock_recover();
        while !*delivered {
            delivered = self
                .interrupt_delivery
                .wait(delivered)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    fn acknowledge_interrupt(&self) {
        let mut delivered = self.interrupt_delivered.lock_recover();
        *delivered = true;
        self.interrupt_delivery.notify_all();
    }
}

struct OperationCompletion<'a>(&'a OperationState);

impl Drop for OperationCompletion<'_> {
    fn drop(&mut self) {
        self.0.finish();
    }
}

struct CancelOnDrop {
    state: Arc<OperationState>,
    interrupt: Arc<dyn QueryInterrupt>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        loop {
            match self.state.phase.load(Ordering::Acquire) {
                FINISHED | CANCELLED => return,
                QUEUED => {
                    if self
                        .state
                        .phase
                        .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return;
                    }
                }
                RUNNING => {
                    if self
                        .state
                        .phase
                        .compare_exchange(RUNNING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        self.interrupt.interrupt();
                        self.state.acknowledge_interrupt();
                        return;
                    }
                }
                unexpected => unreachable!("invalid tag-lane state {unexpected}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    struct FakeInterrupt {
        calls: AtomicUsize,
        release: Arc<AtomicBool>,
    }

    impl QueryInterrupt for FakeInterrupt {
        fn interrupt(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.release.store(true, Ordering::Release);
        }
    }

    struct BlockingInterrupt {
        calls: AtomicUsize,
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }

    impl QueryInterrupt for BlockingInterrupt {
        fn interrupt(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }

    async fn wait_until(flag: &AtomicBool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !flag.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker did not start");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_active_request_interrupts_and_releases_lane() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(FakeInterrupt {
            calls: AtomicUsize::new(0),
            release: Arc::clone(&release),
        });
        let lane = TagLane::with_interrupt(
            Db::open_in_memory().unwrap(),
            interrupt.clone() as Arc<dyn QueryInterrupt>,
        );

        let task_lane = lane.clone();
        let task_started = Arc::clone(&started);
        let task_release = Arc::clone(&release);
        let task = tokio::spawn(async move {
            task_lane
                .run(move |_| {
                    task_started.store(true, Ordering::Release);
                    while !task_release.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    Err::<(), _>(anyhow::anyhow!("interrupted"))
                })
                .await
        });

        wait_until(&started).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(interrupt.calls.load(Ordering::SeqCst), 1);

        let second = tokio::time::timeout(
            Duration::from_secs(2),
            lane.run(|db| Ok(db.list_files()?.len())),
        )
        .await
        .expect("cancelled owner did not release tag lane")
        .expect("second worker panicked")
        .expect("second query failed");
        assert_eq!(second, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn winning_canceller_acknowledges_interrupt_before_handoff() {
        let started = Arc::new(AtomicBool::new(false));
        let finish = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let interrupt_entered = Arc::new(AtomicBool::new(false));
        let interrupt_release = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(BlockingInterrupt {
            calls: AtomicUsize::new(0),
            entered: Arc::clone(&interrupt_entered),
            release: Arc::clone(&interrupt_release),
        });
        let lane = TagLane::with_interrupt(
            Db::open_in_memory().unwrap(),
            interrupt.clone() as Arc<dyn QueryInterrupt>,
        );

        let owner_lane = lane.clone();
        let owner_started = Arc::clone(&started);
        let owner_finish = Arc::clone(&finish);
        let owner_finished = Arc::clone(&finished);
        let owner = tokio::spawn(async move {
            owner_lane
                .run(move |_| {
                    owner_started.store(true, Ordering::Release);
                    while !owner_finish.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    owner_finished.store(true, Ordering::Release);
                    Ok(())
                })
                .await
        });

        wait_until(&started).await;
        owner.abort();
        wait_until(&interrupt_entered).await;
        finish.store(true, Ordering::Release);
        wait_until(&finished).await;
        assert!(
            lane.db.try_lock().is_err(),
            "lane released before interrupt delivery was acknowledged"
        );

        interrupt_release.store(true, Ordering::Release);
        assert!(owner.await.unwrap_err().is_cancelled());
        assert_eq!(interrupt.calls.load(Ordering::SeqCst), 1);

        let second = tokio::time::timeout(Duration::from_secs(2), lane.run(|_| Ok(())))
            .await
            .expect("next owner did not acquire tag lane")
            .expect("second worker panicked");
        second.expect("second query failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn panicking_owner_finishes_before_releasing_lane() {
        let next_started = Arc::new(AtomicBool::new(false));
        let next_release = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(FakeInterrupt {
            calls: AtomicUsize::new(0),
            release: Arc::clone(&next_release),
        });
        let lane = TagLane::with_interrupt(
            Db::open_in_memory().unwrap(),
            interrupt.clone() as Arc<dyn QueryInterrupt>,
        );

        let mut panicking =
            Box::pin(lane.run(|_| -> anyhow::Result<()> { panic!("tag operation panicked") }));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            Future::poll(panicking.as_mut(), &mut context),
            Poll::Pending
        ));

        let worker_started = Arc::clone(&next_started);
        let worker_release = Arc::clone(&next_release);
        let mut next = Box::pin(lane.run(move |_| {
            worker_started.store(true, Ordering::Release);
            while !worker_release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(())
        }));
        assert!(matches!(
            Future::poll(next.as_mut(), &mut context),
            Poll::Pending
        ));

        wait_until(&next_started).await;
        let panic = panicking
            .await
            .expect_err("panicking worker returned successfully");
        assert!(panic.is_panic());
        assert_eq!(
            interrupt.calls.load(Ordering::SeqCst),
            0,
            "panicking owner interrupted the next lane owner"
        );

        next_release.store(true, Ordering::Release);
        next.await
            .expect("next worker panicked")
            .expect("next query failed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_queued_request_does_not_interrupt_active_owner() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(FakeInterrupt {
            calls: AtomicUsize::new(0),
            release: Arc::clone(&release),
        });
        let lane = TagLane::with_interrupt(
            Db::open_in_memory().unwrap(),
            interrupt.clone() as Arc<dyn QueryInterrupt>,
        );

        let owner_lane = lane.clone();
        let owner_started = Arc::clone(&started);
        let owner_release = Arc::clone(&release);
        let owner = tokio::spawn(async move {
            owner_lane
                .run(move |_| {
                    owner_started.store(true, Ordering::Release);
                    while !owner_release.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    Ok(())
                })
                .await
        });
        wait_until(&started).await;

        let queued_lane = lane.clone();
        let queued = tokio::spawn(async move { queued_lane.run(|_| Ok(())).await });
        tokio::task::yield_now().await;
        queued.abort();
        assert!(queued.await.unwrap_err().is_cancelled());
        assert_eq!(
            interrupt.calls.load(Ordering::SeqCst),
            0,
            "queued cancellation interrupted a different request"
        );

        release.store(true, Ordering::Release);
        owner
            .await
            .expect("owner task panicked")
            .expect("owner worker panicked")
            .expect("owner query failed");
    }
}
