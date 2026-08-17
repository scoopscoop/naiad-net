//! HTTP tests for the capabilities handshake and the bucket read, over a real
//! `RepoClient` on a bound socket. Mirrors the harness in `snapshot.rs`: an
//! axum server on an ephemeral port, blocking client calls via `spawn_blocking`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use naiad_core::{Hash, Tag, hash_bytes};
use naiad_netproto::{
    Account, MappingStatus, Op, PROTOCOL_VERSION, PullMode, RepoClient, bucket_key,
};
use naiad_server::{RepoStore, app};
use tower::ServiceExt;

/// Wrap a router with a `MockConnectInfo` layer for oneshot tests.
fn with_mock_addr(router: Router) -> Router {
    router.layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

/// Bind an axum repo on `127.0.0.1:0` and return its base URL.
async fn serve(store: RepoStore, k: u64) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Arc::new(Mutex::new(store));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app(store, k).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

fn add(store: &RepoStore, acct: &Account, h: &Hash, tag: &str) {
    store
        .apply_submission(&acct.sign(Op::Add, h, &Tag::parse(tag).unwrap()))
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn caps_advertises_whole_repo_below_k() {
    let store = RepoStore::open_in_memory().unwrap();
    add(&store, &Account::generate(), &hash_bytes(b"a"), "x:y");
    store.write_distinct_hash_count(1).unwrap();
    let base = serve(store, 1000).await; // 1 hash < k
    let caps = tokio::task::spawn_blocking(move || RepoClient::new(&base).fetch_caps())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(caps.mode, PullMode::WholeRepo);
}

#[tokio::test(flavor = "multi_thread")]
async fn caps_advertises_bucketed_above_k_and_buckets_filter_by_range() {
    let store = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    // Four hashes with distinct leading bytes; k=2 → count/k=2 → 1-bit prefix.
    let owned = Hash::from_bytes([0x10; 32]);
    add(&store, &acct, &owned, "owned:tag");
    add(&store, &acct, &Hash::from_bytes([0x40; 32]), "other:a");
    add(&store, &acct, &Hash::from_bytes([0x80; 32]), "other:b");
    add(&store, &acct, &Hash::from_bytes([0xC0; 32]), "other:c");
    store.write_distinct_hash_count(4).unwrap();

    let base = serve(store, 2).await;
    let snap = tokio::task::spawn_blocking(move || {
        let client = RepoClient::new(&base);
        let PullMode::Bucketed { prefix_bits } = client.fetch_caps().unwrap().mode else {
            panic!("expected bucketed mode");
        };
        assert_eq!(prefix_bits, 1);
        // Ask only for the bucket covering our owned hash.
        let key = bucket_key(&owned, prefix_bits);
        client.fetch_buckets(prefix_bits, &[key]).unwrap()
    })
    .await
    .unwrap();

    // The 0x10 hash is in the lower half (top bit 0); 0x80/0xC0 are in the
    // upper half and must NOT appear. (0x40 shares the lower-half bucket.)
    assert!(
        snap.tags
            .contains_key(&Hash::from_bytes([0x10; 32]).to_hex()),
        "owned present"
    );
    assert!(
        !snap
            .tags
            .contains_key(&Hash::from_bytes([0x80; 32]).to_hex()),
        "upper-half hash excluded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn caps_advertises_mapping_incremental_and_full_bucket_carries_cursor() {
    let store = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    let h = Hash::from_bytes([0x10; 32]);
    add(&store, &acct, &h, "owned:tag");
    add(&store, &acct, &Hash::from_bytes([0x80; 32]), "upper:tag");
    let base = serve(store, 1).await;

    let (caps, snap) = tokio::task::spawn_blocking(move || {
        let client = RepoClient::new(&base);
        let caps = client.fetch_caps().unwrap();
        let PullMode::Bucketed { prefix_bits } = caps.mode else {
            panic!("expected bucketed");
        };
        let key = bucket_key(&h, prefix_bits);
        let snap = client.fetch_buckets(prefix_bits, &[key]).unwrap();
        (caps, snap)
    })
    .await
    .unwrap();

    assert!(caps.mapping_incremental);
    assert!(snap.cursor >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn bucket_delta_request_returns_changes_and_rejects_bad_since_length() {
    let store = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    let h = Hash::from_bytes([0x10; 32]);
    add(&store, &acct, &h, "owned:tag");
    add(&store, &acct, &Hash::from_bytes([0x80; 32]), "upper:tag");
    let base = serve(store, 1).await;

    let h_for_delta = h;
    let delta = tokio::task::spawn_blocking({
        let base = base.clone();
        move || {
            let client = RepoClient::new(&base);
            let PullMode::Bucketed { prefix_bits } = client.fetch_caps().unwrap().mode else {
                panic!("expected bucketed");
            };
            let key = bucket_key(&h_for_delta, prefix_bits);
            client
                .fetch_bucket_delta(prefix_bits, &[key], &[0])
                .unwrap()
        }
    })
    .await
    .unwrap();

    assert_eq!(delta.version, PROTOCOL_VERSION);
    assert_eq!(delta.changes.len(), 1);
    assert_eq!(delta.changes[0].hash, h.to_hex());
    assert_eq!(delta.changes[0].status, MappingStatus::Current);

    let req = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "prefix_bits": 1,
        "buckets": [bucket_key(&h, 1)],
        "since": []
    });
    let response = with_mock_addr(app(
        Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap())),
        1,
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/repo/buckets")
            .header("content-type", "application/json")
            .body(Body::from(req.to_string()))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// #146: a client asks for one bucket key per *distinct hash prefix* it owns,
/// and each key is a full 64-char hash hex (67 body bytes). Measured on a
/// 94,317-file library that is 268 KB of keys at 12 bits and 6.0 MB at the
/// default 24-bit ceiling — 4× and 96× the server's 64 KiB body limit, so every
/// full pull from a real library used to 413. `RepoClient` now splits the list
/// and merges the replies; this drives the real limit over a real socket.
#[tokio::test(flavor = "multi_thread")]
async fn a_bucket_key_list_over_the_servers_body_limit_still_pulls() {
    let store = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    // Two tagged hashes whose 24-bit buckets sit far apart in the sorted key
    // list, so they cannot both land in the same chunk by accident.
    let a = Hash::from_bytes([0x11; 32]);
    let b = Hash::from_bytes([0xEE; 32]);
    add(&store, &acct, &a, "tag:a");
    add(&store, &acct, &b, "tag:b");
    let base = serve(store, 1).await;

    let prefix_bits = 24;
    // 2,000 filler buckets (0x000000..0x0007D0) plus the two real ones.
    let mut keys: Vec<String> = (0..2000u32)
        .map(|i| {
            let mut bytes = [0u8; 32];
            bytes[..3].copy_from_slice(&i.to_be_bytes()[1..]);
            bucket_key(&Hash::from_bytes(bytes), prefix_bits)
        })
        .collect();
    keys.push(bucket_key(&a, prefix_bits));
    keys.push(bucket_key(&b, prefix_bits));
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        2002,
        "filler must not collide with the real keys"
    );

    let snap = tokio::task::spawn_blocking(move || {
        RepoClient::new(&base).fetch_buckets(prefix_bits, &keys)
    })
    .await
    .unwrap()
    .expect("a key list well over 64 KiB must pull, not 413");

    assert_eq!(
        snap.tags
            .get(&a.to_hex())
            .map(|v| v.iter().map(|t| t.tag.as_str()).collect::<Vec<_>>()),
        Some(vec!["tag:a"]),
        "the first chunk's hash survived the merge"
    );
    assert_eq!(
        snap.tags
            .get(&b.to_hex())
            .map(|v| v.iter().map(|t| t.tag.as_str()).collect::<Vec<_>>()),
        Some(vec!["tag:b"]),
        "the last chunk's hash survived the merge"
    );
}

/// Pins *why* the client chunks: the server's 64 KiB `DefaultBodyLimit` really
/// does reject an oversized key list, so this must keep failing. If a future
/// change raises the limit, that is a deliberate decision this test should be
/// updated alongside — not something that quietly stops being true.
#[tokio::test]
async fn one_unchunked_oversized_bucket_request_is_still_rejected() {
    let body = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "prefix_bits": 24,
        "buckets": (0..2000).map(|i| format!("{i:064x}")).collect::<Vec<_>>(),
    })
    .to_string();
    assert!(
        body.len() > 64 * 1024,
        "test body must exceed the limit it is pinning: {} bytes",
        body.len()
    );

    let response = with_mock_addr(app(
        Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap())),
        1,
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/repo/buckets")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
