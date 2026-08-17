//! HTTP tests for the repository's snapshot read endpoint, both via oneshot
//! and via the real `RepoClient` over a bound socket. v8 shape:
//! `hash → [OriginTag]` — tag string plus optional generation-source origin.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use naiad_netproto::{PROTOCOL_VERSION, REPO_SNAPSHOT, RepoClient};
use naiad_server::{RepoStore, app};
use tower::ServiceExt;

/// Wrap a router with a `MockConnectInfo` layer so that handlers that extract
/// `ConnectInfo<SocketAddr>` work in oneshot tests (no real TCP socket).
fn with_mock_addr(router: Router) -> Router {
    router.layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

fn seeded_store() -> RepoStore {
    use naiad_core::{Tag, hash_bytes};
    use naiad_netproto::{Account, Op};
    let store = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    let h = hash_bytes(b"file");
    for tag in ["character:samus", "series:metroid"] {
        store
            .apply_submission(&acct.sign(Op::Add, &h, &Tag::parse(tag).unwrap()))
            .unwrap();
    }
    store
}

#[tokio::test]
async fn snapshot_endpoint_serves_the_store_via_oneshot() {
    let store = Arc::new(Mutex::new(seeded_store()));
    let resp = with_mock_addr(app(store, 1000))
        .oneshot(
            Request::builder()
                .uri(REPO_SNAPSHOT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let snap: naiad_netproto::Snapshot = serde_json::from_slice(&body).unwrap();
    assert_eq!(snap.version, PROTOCOL_VERSION);
    let h = naiad_core::hash_bytes(b"file").to_hex();
    // v8 shape: Vec<OriginTag>; tag strings accessible via .tag field.
    let tags = snap.tags.get(&h).unwrap();
    assert!(
        tags.iter().any(|t| t.tag == "character:samus"),
        "character:samus must appear in snapshot"
    );
    assert!(
        tags.iter().any(|t| t.tag == "series:metroid"),
        "series:metroid must appear in snapshot"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_client_fetches_a_bound_servers_snapshot() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Arc::new(Mutex::new(seeded_store()));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app(store, 1000).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let base = format!("http://{addr}");
    let snap = tokio::task::spawn_blocking(move || RepoClient::new(&base).fetch_snapshot())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(snap.version, PROTOCOL_VERSION);
    let h = naiad_core::hash_bytes(b"file").to_hex();
    // v8 shape: Vec<OriginTag>.
    let tags = snap.tags.get(&h).unwrap();
    assert!(tags.iter().any(|t| t.tag == "character:samus"));
    assert!(tags.iter().any(|t| t.tag == "series:metroid"));
}

#[tokio::test]
async fn snapshot_add_then_remove_updates_the_view() {
    use naiad_core::{Tag, hash_bytes};
    use naiad_netproto::{Account, Op};

    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let acct = Account::generate();
    let h = hash_bytes(b"file");

    // Add two tags.
    store
        .lock()
        .unwrap()
        .apply_submission(&acct.sign(Op::Add, &h, &Tag::parse("character:samus").unwrap()))
        .unwrap();
    store
        .lock()
        .unwrap()
        .apply_submission(&acct.sign(Op::Add, &h, &Tag::parse("series:metroid").unwrap()))
        .unwrap();

    let snap1: naiad_netproto::Snapshot = serde_json::from_slice(
        &to_bytes(
            with_mock_addr(app(store.clone(), 1000))
                .oneshot(
                    Request::builder()
                        .uri(REPO_SNAPSHOT)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(snap1.tags.get(&h.to_hex()).unwrap().len(), 2);

    // Remove one tag.
    store
        .lock()
        .unwrap()
        .apply_submission(&acct.sign(Op::Remove, &h, &Tag::parse("series:metroid").unwrap()))
        .unwrap();

    let snap2: naiad_netproto::Snapshot = serde_json::from_slice(
        &to_bytes(
            with_mock_addr(app(store.clone(), 1000))
                .oneshot(
                    Request::builder()
                        .uri(REPO_SNAPSHOT)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let tags2 = snap2.tags.get(&h.to_hex()).unwrap();
    assert_eq!(tags2.len(), 1);
    assert_eq!(tags2[0].tag, "character:samus");
}
