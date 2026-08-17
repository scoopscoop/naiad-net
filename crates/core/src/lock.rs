//! Poison-recovery for mutexes.
//!
//! A panic while a request holds a mutex would otherwise poison it permanently
//! and brick every subsequent request with `PoisonError` until the process
//! restarts (#29 on the daemon side; #137 on the server side). The
//! `LockRecover` extension trait calls `into_inner` on a `PoisonError` to take
//! the guard regardless, allowing the next caller to proceed.
//!
//! ## Safety note for callers
//!
//! If the panicking thread was mid-write, the guarded value *may* be in a
//! partially-mutated state. Callers must document why recovery is safe for
//! their particular guarded type:
//!
//! - **`naiad-daemon`** (issue #29): panics happen before any SQLite statement
//!   is executed, so the guarded `Db` connection is never left half-written.
//!   Taking the poisoned guard via `into_inner` is unambiguously safe.
//!
//! - **`naiad-server`** (issue #137): panics may happen during SQLite
//!   operations, but every transaction site goes through a `rusqlite` RAII
//!   guard (`unchecked_transaction`), whose `Drop` rolls back during unwind.
//!   The `Connection` is therefore back to a clean, committable state when the
//!   next caller takes the guard. Any in-progress write is simply lost, which
//!   is the correct failure behaviour — far better than a permanently wedged
//!   process in an unattended container.
//!
//! ## The invariant this depends on
//!
//! Recovery is only safe while *every* transaction on the guarded connection
//! uses an RAII guard. Raw `BEGIN`/`COMMIT` issued through `execute_batch` does
//! **not** qualify: an unwinding panic skips the cleanup and strands an open
//! transaction, and the recovered connection then fails every later `BEGIN`
//! with "cannot start a transaction within a transaction" — reproducing the
//! permanent outage this trait exists to prevent, under a different error.
//!
//! `RepoStore::read_snapshot` was exactly that shape and was converted in the
//! #137 follow-up; `crates/server/tests/lock_recovery.rs` pins the behaviour.
//! Anything introducing a new raw `BEGIN`, or mutating state across statements
//! in ways rollback would not reverse, must be reviewed before relying on this.

use std::sync::{Mutex, MutexGuard};

/// Locking that recovers a poisoned mutex instead of panicking.
///
/// A blanket impl covers all [`Mutex<T>`] so callers can write
/// `my_mutex.lock_recover()` in place of `my_mutex.lock().unwrap()`.
pub trait LockRecover<T> {
    /// Lock the mutex, taking the guard back even if a previous holder panicked.
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockRecover<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn recovers_after_a_panicked_holder_poisoned_the_lock() {
        let m = Arc::new(Mutex::new(0_i32));

        // Poison the lock: panic while holding the guard.
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let mut g = m2.lock().unwrap();
            *g = 1;
            panic!("boom");
        })
        .join();

        // A plain lock would now fail; lock_recover takes the guard anyway and
        // sees the value the panicking thread had written.
        assert!(m.lock().is_err(), "precondition: lock is poisoned");
        assert_eq!(*m.lock_recover(), 1);
    }
}
