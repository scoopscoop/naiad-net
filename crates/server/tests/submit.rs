//! The repo verifies request-level auth AND doc-level signatures on write.
//! A valid signed submission is stored and served; a tampered doc sig is
//! rejected with 400; missing auth headers yield 401.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode, header};
use naiad_core::{Tag, hash_bytes};
use naiad_netproto::{
    Account, HDR_AUTH_KEY, HDR_AUTH_SIG, HDR_AUTH_TS, Op, REPO_SNAPSHOT, REPO_SUBMIT, Snapshot,
    Submission,
};
use naiad_server::{RepoStore, app};
use tower::ServiceExt;

fn with_mock_addr(router: Router) -> Router {
    router.layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// POST /repo/submit with valid request-auth headers for `acct` and the body
/// bytes as-is. The body may contain a Submission whose doc-level signature is
/// invalid (e.g. the tag was tampered after signing) — auth check still passes,
/// but the server returns 400 on the doc sig.
async fn submit_with_auth(
    store: Arc<Mutex<RepoStore>>,
    acct: &Account,
    body: Vec<u8>,
) -> StatusCode {
    let ts = unix_now();
    let sig = acct.sign_auth("POST", REPO_SUBMIT, None, ts, &body);
    app(store, 1000)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_SUBMIT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, acct.public_hex())
                .header(HDR_AUTH_TS, ts.to_string())
                .header(HDR_AUTH_SIG, sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// POST /repo/submit with NO auth headers — expect 401.
async fn submit_unauthenticated(store: Arc<Mutex<RepoStore>>, sub: &Submission) -> StatusCode {
    app(store, 1000)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_SUBMIT)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(sub).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn valid_submission_is_stored_and_served() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let acct = Account::generate();
    let h = hash_bytes(b"file");
    let sub = acct.sign(Op::Add, &h, &Tag::parse("character:samus").unwrap());

    let status = submit_with_auth(store.clone(), &acct, serde_json::to_vec(&sub).unwrap()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let resp = with_mock_addr(app(store, 1000))
        .oneshot(
            Request::builder()
                .uri(REPO_SNAPSHOT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let snap: Snapshot = serde_json::from_slice(&body).unwrap();
    // v8 shape: hash → Vec<OriginTag>; tag strings via .tag field.
    let tags = snap.tags.get(&h.to_hex()).unwrap();
    assert!(
        tags.iter().any(|t| t.tag == "character:samus"),
        "submitted tag must appear in snapshot"
    );
}

#[tokio::test]
async fn a_tampered_submission_is_rejected() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let acct = Account::generate();
    let h = hash_bytes(b"file");
    let mut sub = acct.sign(Op::Add, &h, &Tag::parse("character:samus").unwrap());
    // Tamper after signing: doc-level signature no longer matches the tag.
    sub.tag = "character:zelda".into();

    // Auth headers are valid for the tampered body bytes; the server passes
    // auth but fails the doc-level signature check → 400.
    let status = submit_with_auth(store, &acct, serde_json::to_vec(&sub).unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_auth_headers_yield_401() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let acct = Account::generate();
    let h = hash_bytes(b"file");
    let sub = acct.sign(Op::Add, &h, &Tag::parse("character:samus").unwrap());

    assert_eq!(
        submit_unauthenticated(store, &sub).await,
        StatusCode::UNAUTHORIZED
    );
}
