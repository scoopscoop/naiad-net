//! Regression test for #137: a poisoned `RepoStore` mutex must not permanently
//! brick subsequent HTTP requests.
//!
//! Before the fix every handler called `.expect("repo store mutex poisoned")`.
//! A single panic while holding the lock poisoned the mutex forever; every
//! subsequent request then hit the same `.expect()` and returned 500 for the
//! remaining life of the process — a silent outage in an unattended container.
//!
//! The fix replaces every `.expect()` with `.lock_recover()` (from
//! [`naiad_core::LockRecover`]), which calls `into_inner` on the `PoisonError`
//! to take the guard regardless.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use naiad_netproto::{HashDomain, REPO_CAPS, REPO_SNAPSHOT};
use naiad_server::{RepoStore, app_split};
use tower::ServiceExt;

fn with_mock_addr(router: Router) -> Router {
    router.layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

/// Poison both the writer and reader mutexes, then verify that read endpoints
/// still return 200.
///
/// The timeout converts a hypothetical deadlock (which the old `expect` path
/// would never hit, but any regression might) into a clean test failure rather
/// than a hung suite.
#[tokio::test(flavor = "multi_thread")]
async fn poisoned_store_mutex_does_not_brick_subsequent_requests() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));

    // Poison the mutex: spawn a thread that panics while holding the lock.
    let store2 = store.clone();
    let _ = std::thread::spawn(move || {
        let _guard = store2.lock().unwrap();
        panic!("deliberate panic to poison the RepoStore mutex (#137 regression)");
    })
    .join(); // join returns Err on panic; we discard it intentionally

    // Precondition: the mutex is actually poisoned now.
    assert!(
        store.lock().is_err(),
        "precondition: RepoStore mutex must be poisoned before testing recovery"
    );

    let router = with_mock_addr(app_split(store, None, 1000, None, None, HashDomain::Blake3));

    // GET /repo/caps should return 200 — it acquires the store mutex inside
    // spawn_blocking.  Before the fix this would panic on .expect() and axum
    // would catch the panic and return 500.
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        router.clone().oneshot(
            Request::builder()
                .uri(REPO_CAPS)
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("GET /repo/caps timed out — possible deadlock")
    .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /repo/caps must return 200 after mutex recovery, not {}",
        resp.status()
    );

    // Also verify GET /repo/snapshot for belt-and-suspenders coverage.
    let resp2 = tokio::time::timeout(
        Duration::from_secs(5),
        router.oneshot(
            Request::builder()
                .uri(REPO_SNAPSHOT)
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("GET /repo/snapshot timed out — possible deadlock")
    .unwrap();

    assert_eq!(
        resp2.status(),
        StatusCode::OK,
        "GET /repo/snapshot must return 200 after mutex recovery, not {}",
        resp2.status()
    );
}

/// Recovering the mutex is not enough on its own: the panic that poisoned it
/// may also have left an open transaction on the shared `Connection`.
///
/// `read_snapshot` opens `BEGIN DEFERRED` and issues COMMIT/ROLLBACK on the
/// normal return path only, so a panic inside the closure unwinds straight past
/// the cleanup. Recovering the poisoned lock then hands the next handler a
/// connection that is still mid-transaction, and its own `BEGIN DEFERRED` fails
/// with "cannot start a transaction within a transaction" — restoring the exact
/// permanent-outage behaviour #137 set out to remove, just with a different
/// error. The three handlers that call `read_snapshot` (snapshot, buckets,
/// relations) are the realistic place for such a panic to originate.
#[tokio::test(flavor = "multi_thread")]
async fn panic_inside_read_snapshot_does_not_strand_an_open_transaction() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));

    // Poison the mutex the way production actually would: panic *inside* a
    // read transaction rather than while merely holding the lock.
    let store2 = store.clone();
    let _ = std::thread::spawn(move || {
        let guard = store2.lock().unwrap();
        let _out: naiad_server::Result<()> = guard.read_snapshot(|_| {
            panic!("deliberate panic inside read_snapshot (#137 follow-up)");
        });
    })
    .join();

    assert!(
        store.lock().is_err(),
        "precondition: the mutex must be poisoned by the panic"
    );

    let router = with_mock_addr(app_split(store, None, 1000, None, None, HashDomain::Blake3));

    // /repo/snapshot goes through read_snapshot, so it is the endpoint that
    // inherits any stranded transaction.
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        router.oneshot(
            Request::builder()
                .uri(REPO_SNAPSHOT)
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("GET /repo/snapshot timed out — possible deadlock")
    .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /repo/snapshot must still serve after a panic inside read_snapshot, not {}",
        resp.status()
    );
}
