//! A small pool of read-only connections to the library DB. WAL allows any
//! number of concurrent readers; the pool bounds how many the daemon uses and
//! hands them out via an async semaphore so a slow query delays only its own
//! lane, not every read endpoint (#50).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use naiad_db::{Db, SharedRelationCache};
use tokio::sync::Semaphore;

use crate::lock::LockRecover;

pub(crate) struct ReadPool {
    conns: Mutex<Vec<Db>>,
    sem: Arc<Semaphore>,
}

/// Returns the checked-out connection to the pool on drop — including when the
/// borrowing closure panics, so a panicking query cannot shrink the pool.
struct ConnReturn {
    conn: Option<Db>,
    pool: Arc<ReadPool>,
}

impl Drop for ConnReturn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.conns.lock_recover().push(conn);
        }
    }
}

impl ReadPool {
    /// Open `size` read-only connections on `path`, all sharing `cache` so the
    /// merged relation graph is built once across the pool rather than once per
    /// connection (#70).
    pub(crate) fn open(
        path: &Path,
        size: usize,
        cache: &SharedRelationCache,
    ) -> naiad_db::Result<Arc<Self>> {
        assert!(size > 0, "read pool size must be at least 1");
        let mut conns = Vec::with_capacity(size);
        for _ in 0..size {
            conns.push(Db::open_readonly_with_cache(path, cache.clone())?);
        }
        Ok(Arc::new(Self {
            conns: Mutex::new(conns),
            sem: Arc::new(Semaphore::new(size)),
        }))
    }

    /// Check out a connection (waiting if all are busy), run `f` on it inside
    /// `spawn_blocking`, and return it. The `Err` is the join error (panic).
    /// Callers convert the error leg with `.map_err(internal)` like every other
    /// `spawn_blocking` site in `server.rs`.
    pub(crate) async fn run<T, F>(self: &Arc<Self>, f: F) -> Result<T, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce(&Db) -> T + Send + 'static,
    {
        let op = std::any::type_name::<F>();
        let wait_start = Instant::now();
        let permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("read pool semaphore closed");
        let pool_wait = wait_start.elapsed();
        let conn = self
            .conns
            .lock_recover()
            .pop()
            .expect("permit held but pool empty");
        let pool = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let guard = ConnReturn {
                conn: Some(conn),
                pool,
            };
            let work_start = Instant::now();
            let out = f(guard.conn.as_ref().expect("guard holds conn"));
            crate::server::log_db_op(op, pool_wait, work_start.elapsed());
            out
        })
        .await
    }

    /// Test hook: grab all permits so the next `run` must wait — lets tests
    /// prove another lane stays responsive while this pool is saturated.
    #[cfg(test)]
    pub(crate) async fn exhaust(self: &Arc<Self>) -> tokio::sync::OwnedSemaphorePermit {
        let n = u32::try_from(self.sem.available_permits()).expect("pool size fits u32");
        assert!(n > 0, "exhaust called on an already-saturated pool");
        self.sem
            .clone()
            .acquire_many_owned(n)
            .await
            .expect("read pool semaphore closed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a fresh DB file that `Db::open_readonly` can open.
    /// We open it with `Db::open` first (to run migrations), then drop the
    /// writer so the file is fully committed before the read pool opens it.
    fn make_db_file() -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        // Open with the writer to create and migrate the schema.
        let _writer = naiad_db::Db::open(&path).expect("open writer");
        // Drop the writer before opening read-only connections.
        (dir, path)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_returns_value_and_reuses_conns_beyond_pool_size() {
        let (_dir, path) = make_db_file();
        let pool = ReadPool::open(&path, 2, &Db::new_relation_cache()).expect("open pool");
        for _ in 0..5 {
            let result = pool.run(|db| db.list_files().map(|v| v.len())).await;
            let len = result.expect("no join error").expect("no db error");
            assert_eq!(len, 0);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn panic_in_closure_does_not_lose_the_connection() {
        let (_dir, path) = make_db_file();
        let pool = ReadPool::open(&path, 1, &Db::new_relation_cache()).expect("open pool");
        // First call: panic inside the closure.
        let result = pool.run::<(), _>(|_| panic!("boom")).await;
        assert!(
            result.is_err(),
            "panicking closure must return Err(JoinError)"
        );
        // Second call: must succeed — the connection must have been returned.
        let result = pool.run(|db| db.list_files().map(|v| v.len())).await;
        let len = result.expect("no join error").expect("no db error");
        assert_eq!(len, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_runs_all_complete() {
        let (_dir, path) = make_db_file();
        let pool = ReadPool::open(&path, 2, &Db::new_relation_cache()).expect("open pool");
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = Arc::clone(&pool);
            handles.push(tokio::spawn(async move {
                p.run(|db| db.list_files().map(|v| v.len())).await
            }));
        }
        for handle in handles {
            let result = handle
                .await
                .expect("task join")
                .expect("no join error")
                .expect("no db error");
            assert_eq!(result, 0);
        }
    }
}
