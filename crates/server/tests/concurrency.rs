//! The point of the read/write connection split (#45): a read endpoint must be
//! served even while the writer connection is locked by a long write. With the
//! old single-mutex store this deadlocks; with the dedicated read-only
//! connection the read goes through.

use std::net::SocketAddr;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use naiad_netproto::REPO_SNAPSHOT;
use naiad_server::{RepoStore, app_split};
use tower::ServiceExt;

fn with_mock_addr(router: Router) -> Router {
    router.layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

fn seed(store: &RepoStore) {
    use naiad_core::{Tag, hash_bytes};
    use naiad_netproto::{Account, Op};
    let acct = Account::generate();
    let h = hash_bytes(b"file");
    store
        .apply_submission(&acct.sign(Op::Add, &h, &Tag::parse("character:samus").unwrap()))
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn read_endpoint_is_served_while_the_writer_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repo.db");

    let writer = Arc::new(Mutex::new(RepoStore::open(&path).unwrap()));
    seed(&writer.lock().unwrap());
    let reader = Arc::new(Mutex::new(RepoStore::open_readonly(&path).unwrap()));

    // A thread grabs the writer mutex and holds it until told to release —
    // simulating a long mirror-apply or bulk submit.
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let hold = {
        let writer = writer.clone();
        std::thread::spawn(move || {
            let _guard = writer.lock().expect("writer mutex poisoned");
            locked_tx.send(()).unwrap();
            // Hold the writer lock across the whole read below.
            release_rx.recv().unwrap();
        })
    };

    // Wait until the writer is definitely held.
    locked_rx.recv().unwrap();

    // The read must complete without the writer lock. A timeout converts the
    // pre-split deadlock into a clean failure instead of hanging the suite.
    let router = with_mock_addr(app_split(
        writer.clone(),
        Some(reader),
        1000,
        None,
        None,
        naiad_netproto::HashDomain::Blake3,
    ));
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
    .expect("read endpoint stalled behind the writer lock — split is not working")
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Let the writer thread finish now that the read has returned.
    release_tx.send(()).unwrap();
    hold.join().unwrap();
}

#[tokio::test]
async fn reads_fall_back_to_the_writer_when_no_read_store() {
    // app_split(store, None, …) — the in-memory / fallback path still serves reads.
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    seed(&store.lock().unwrap());
    let resp = with_mock_addr(app_split(
        store,
        None,
        1000,
        None,
        None,
        naiad_netproto::HashDomain::Blake3,
    ))
    .oneshot(
        Request::builder()
            .uri(REPO_SNAPSHOT)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
