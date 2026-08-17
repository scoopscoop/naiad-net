//! HTTP tests for the relation submit + bulk-read endpoints, over a real
//! `RepoClient` on a bound socket. Mirrors the harness in `buckets.rs`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use naiad_core::Tag;
use naiad_netproto::{Account, Op, RelKind, RepoClient};
use naiad_server::{RepoStore, app};

/// Bind an axum repo on `127.0.0.1:0` and return its base URL.
async fn serve(store: RepoStore) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Arc::new(Mutex::new(store));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app(store, 1000).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread")]
async fn valid_relation_is_accepted_then_served() {
    let store = RepoStore::open_in_memory().unwrap();
    let base = serve(store).await;
    let acct = Account::generate();
    let sub = acct.sign_relation(
        Op::Add,
        RelKind::Sibling,
        &Tag::parse("character:samus_aran").unwrap(),
        &Tag::parse("character:samus").unwrap(),
    );

    let graph = tokio::task::spawn_blocking(move || {
        let client = RepoClient::new(&base);
        client.submit_relation(&sub).unwrap();
        client.fetch_relations().unwrap()
    })
    .await
    .unwrap();

    assert_eq!(graph.siblings.len(), 1);
    assert_eq!(graph.siblings[0].from, "character:samus_aran");
    assert_eq!(graph.siblings[0].to, "character:samus");
    assert_eq!(graph.siblings[0].author, acct.public_hex());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_relation_is_rejected_and_not_stored() {
    let store = RepoStore::open_in_memory().unwrap();
    let base = serve(store).await;
    let acct = Account::generate();
    let mut sub = acct.sign_relation(
        Op::Add,
        RelKind::Sibling,
        &Tag::parse("character:samus_aran").unwrap(),
        &Tag::parse("character:samus").unwrap(),
    );
    sub.to = "character:zelda".into(); // breaks the signature

    let (rejected, graph) = tokio::task::spawn_blocking(move || {
        let client = RepoClient::new(&base);
        let rejected = client.submit_relation(&sub).is_err();
        (rejected, client.fetch_relations().unwrap())
    })
    .await
    .unwrap();

    assert!(rejected, "tampered submission must be rejected (400)");
    assert!(graph.siblings.is_empty(), "nothing stored");
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_relations_treats_404_as_empty_graph() {
    // A bare router with no routes 404s every path — stand in for a pre-relations repo.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new()).await.unwrap();
    });
    let base = format!("http://{addr}");

    let graph = tokio::task::spawn_blocking(move || RepoClient::new(&base).fetch_relations())
        .await
        .unwrap()
        .unwrap();

    assert!(graph.siblings.is_empty() && graph.parents.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn caps_advertises_relation_incremental() {
    let store = RepoStore::open_in_memory().unwrap();
    let base = serve(store).await;
    let caps = tokio::task::spawn_blocking(move || RepoClient::new(&base).fetch_caps().unwrap())
        .await
        .unwrap();
    assert!(caps.relation_incremental, "this repo serves deltas");
}

#[tokio::test(flavor = "multi_thread")]
async fn since_query_returns_a_delta_no_since_returns_a_graph() {
    let store = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    let sub = acct.sign_relation(
        Op::Add,
        RelKind::Sibling,
        &Tag::parse("a:x").unwrap(),
        &Tag::parse("a:y").unwrap(),
    );
    let base = serve(store).await;

    let b1 = base.clone();
    let delta = tokio::task::spawn_blocking(move || {
        let client = RepoClient::new(&b1);
        client.submit_relation(&sub).unwrap();
        client.fetch_relations_since(0).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(delta.edges.len(), 1);
    assert!(delta.cursor >= 1);

    let graph =
        tokio::task::spawn_blocking(move || RepoClient::new(&base).fetch_relations().unwrap())
            .await
            .unwrap();
    assert_eq!(graph.siblings.len(), 1);
    assert_eq!(graph.cursor, delta.cursor);
}
