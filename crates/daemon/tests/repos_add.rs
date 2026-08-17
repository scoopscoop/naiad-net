//! Integration tests for `POST /api/repos` — the subscription handler that
//! resolves repo names from caps, client hints, and URL hostnames.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use naiad_daemon::{AppState, app};
use naiad_db::Db;
use naiad_server::RepoStore;
use serde_json::{Value, json};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serve a minimal repo app over an ephemeral port.  `name` is threaded
/// through to `naiad_server::app_split` so the `/repo/caps` response carries
/// (or omits) the advertised display name.
async fn serve_stub(name: Option<String>) -> String {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    tokio::spawn(async move {
        axum::serve(
            listener,
            naiad_server::app_split(
                store,
                None,
                1000,
                None,
                name,
                naiad_server::HashDomain::Blake3,
            )
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    url
}

/// Drive one request through the daemon's in-process router.
async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

async fn get(state: &AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    send(
        state,
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn post(state: &AppState, uri: &str, body: Value) -> (StatusCode, Vec<u8>) {
    send(
        state,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

/// Build a minimal AppState (no settings store, no read pool).
fn make_state() -> (AppState, tempfile::TempDir) {
    let thumbs = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);
    (state, thumbs)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// When the repo's caps carry a non-empty `name`, the stored/echoed name is
/// the caps-advertised one, regardless of what the client posted.
#[tokio::test(flavor = "multi_thread")]
async fn add_uses_advertised_name() {
    let url = serve_stub(Some("NOS".to_string())).await;
    let (state, _thumbs) = make_state();

    let (status, body) = post(&state, "/api/repos", json!({ "url": url })).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "subscribe failed: {}",
        String::from_utf8_lossy(&body)
    );

    let dto: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(dto["name"], "NOS", "should use caps-advertised name");

    // Confirm the name is persisted: GET /api/repos must list the repo as "NOS".
    let (list_status, list_body) = get(&state, "/api/repos").await;
    assert_eq!(list_status, StatusCode::OK);
    let repos: Value = serde_json::from_slice(&list_body).unwrap();
    let names: Vec<&str> = repos
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"NOS"),
        "stored repo list must include 'NOS', got {names:?}"
    );
}

/// When caps carry no name but the client supplies one, the client name wins.
#[tokio::test(flavor = "multi_thread")]
async fn add_falls_back_to_client_name() {
    let url = serve_stub(None).await;
    let (state, _thumbs) = make_state();

    let (status, body) = post(&state, "/api/repos", json!({ "url": url, "name": "mine" })).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "subscribe failed: {}",
        String::from_utf8_lossy(&body)
    );

    let dto: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        dto["name"], "mine",
        "should fall back to client-supplied name"
    );
}

/// When neither caps nor client supply a name, the URL hostname is used.
#[tokio::test(flavor = "multi_thread")]
async fn add_falls_back_to_hostname() {
    let url = serve_stub(None).await;
    let (state, _thumbs) = make_state();

    let (status, body) = post(&state, "/api/repos", json!({ "url": url })).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "subscribe failed: {}",
        String::from_utf8_lossy(&body)
    );

    let dto: Value = serde_json::from_slice(&body).unwrap();
    // The stub URL is `http://127.0.0.1:<port>`, so the hostname is "127.0.0.1".
    assert_eq!(dto["name"], "127.0.0.1", "should fall back to URL hostname");
}

/// When two repos both advertise the same name the second gets a `-2` suffix.
#[tokio::test(flavor = "multi_thread")]
async fn add_suffixes_name_collision() {
    let url1 = serve_stub(Some("NOS".to_string())).await;
    let url2 = serve_stub(Some("NOS".to_string())).await;
    let (state, _thumbs) = make_state();

    let (s1, b1) = post(&state, "/api/repos", json!({ "url": url1 })).await;
    assert_eq!(
        s1,
        StatusCode::OK,
        "first subscribe failed: {}",
        String::from_utf8_lossy(&b1)
    );
    let dto1: Value = serde_json::from_slice(&b1).unwrap();
    assert_eq!(dto1["name"], "NOS");

    let (s2, b2) = post(&state, "/api/repos", json!({ "url": url2 })).await;
    assert_eq!(
        s2,
        StatusCode::OK,
        "second subscribe failed: {}",
        String::from_utf8_lossy(&b2)
    );
    let dto2: Value = serde_json::from_slice(&b2).unwrap();
    assert_eq!(dto2["name"], "NOS-2", "collision should produce -2 suffix");
}

/// Subscribing the same URL twice must return 400 mentioning "already subscribed".
#[tokio::test(flavor = "multi_thread")]
async fn add_rejects_duplicate_url() {
    let url = serve_stub(Some("NOS".to_string())).await;
    let (state, _thumbs) = make_state();

    let (s1, _) = post(&state, "/api/repos", json!({ "url": url })).await;
    assert_eq!(s1, StatusCode::OK);

    let (s2, body) = post(&state, "/api/repos", json!({ "url": url })).await;
    assert_eq!(
        s2,
        StatusCode::BAD_REQUEST,
        "duplicate URL must be rejected"
    );
    assert!(
        String::from_utf8_lossy(&body).contains("already subscribed"),
        "error body must mention 'already subscribed', got: {}",
        String::from_utf8_lossy(&body)
    );
}

/// An empty URL must be rejected with 400 before any network activity.
#[tokio::test(flavor = "multi_thread")]
async fn add_rejects_empty_url() {
    let (state, _thumbs) = make_state();

    let (status, body) = post(&state, "/api/repos", json!({ "url": "" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!body.is_empty(), "400 response must include a body");
}

/// Detached-name squatting regression: subscribing a DIFFERENT URL that
/// advertises the same name as a previously-detached subscription must produce
/// a suffixed name ("NOS-2"), not silently re-attach the detached row.
#[tokio::test(flavor = "multi_thread")]
async fn add_detached_name_gets_suffix_not_reattached() {
    let url_a = serve_stub(Some("NOS".to_string())).await;
    let url_b = serve_stub(Some("NOS".to_string())).await;
    let (state, _thumbs) = make_state();

    // Subscribe repo A as "NOS".
    let (s1, b1) = post(&state, "/api/repos", json!({ "url": url_a })).await;
    assert_eq!(
        s1,
        StatusCode::OK,
        "first subscribe: {}",
        String::from_utf8_lossy(&b1)
    );
    let dto1: Value = serde_json::from_slice(&b1).unwrap();
    assert_eq!(dto1["name"], "NOS");

    // Detach repo A (no purge) via DELETE /api/repos?name=NOS.
    let (del_status, del_body) = send(
        &state,
        axum::http::Request::builder()
            .method("DELETE")
            .uri("/api/repos?name=NOS")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        del_status,
        StatusCode::OK,
        "detach failed: {}",
        String::from_utf8_lossy(&del_body)
    );

    // Confirm "NOS" is no longer in the subscribed list.
    let (list_s, list_b) = get(&state, "/api/repos").await;
    assert_eq!(list_s, StatusCode::OK);
    let repos: Value = serde_json::from_slice(&list_b).unwrap();
    let names: Vec<&str> = repos
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"NOS"),
        "detached repo must not appear in list: {names:?}"
    );

    // Subscribe a DIFFERENT URL also advertising "NOS" — must get suffix.
    let (s2, b2) = post(&state, "/api/repos", json!({ "url": url_b })).await;
    assert_eq!(
        s2,
        StatusCode::OK,
        "second subscribe: {}",
        String::from_utf8_lossy(&b2)
    );
    let dto2: Value = serde_json::from_slice(&b2).unwrap();
    assert_eq!(
        dto2["name"], "NOS-2",
        "new repo advertising a detached name must get -2 suffix"
    );

    // Verify the detached row still has url=NULL by confirming "NOS" plain
    // still does not appear in the GET /api/repos list.
    let (list_s2, list_b2) = get(&state, "/api/repos").await;
    assert_eq!(list_s2, StatusCode::OK);
    let repos2: Value = serde_json::from_slice(&list_b2).unwrap();
    let names2: Vec<&str> = repos2
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(
        !names2.contains(&"NOS"),
        "detached 'NOS' row must still have url=NULL (not in list): {names2:?}"
    );
    assert!(
        names2.contains(&"NOS-2"),
        "new subscription 'NOS-2' must be in list: {names2:?}"
    );
}

/// A repo advertising an extremely long name (10_000 chars) must be stored
/// with a name of at most 64 chars.
#[tokio::test(flavor = "multi_thread")]
async fn add_clamps_long_advertised_name() {
    let long_name = "A".repeat(10_000);
    let url = serve_stub(Some(long_name)).await;
    let (state, _thumbs) = make_state();

    let (status, body) = post(&state, "/api/repos", json!({ "url": url })).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "subscribe failed: {}",
        String::from_utf8_lossy(&body)
    );

    let dto: Value = serde_json::from_slice(&body).unwrap();
    let stored_name = dto["name"].as_str().unwrap();
    assert!(
        stored_name.len() <= 64,
        "stored name must be ≤64 chars, got {} chars",
        stored_name.len()
    );
}

/// A repo advertising a name containing ASCII control characters (ESC, CR)
/// must have those stripped from the stored name.
#[tokio::test(flavor = "multi_thread")]
async fn add_strips_control_chars_from_advertised_name() {
    // ESC + CR embedded in an otherwise normal name.
    let name_with_controls = "NOS\x1b[31m\rBAD".to_string();
    let url = serve_stub(Some(name_with_controls)).await;
    let (state, _thumbs) = make_state();

    let (status, body) = post(&state, "/api/repos", json!({ "url": url })).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "subscribe failed: {}",
        String::from_utf8_lossy(&body)
    );

    let dto: Value = serde_json::from_slice(&body).unwrap();
    let stored_name = dto["name"].as_str().unwrap();
    assert!(
        !stored_name.chars().any(|c| c.is_control()),
        "stored name must not contain control characters, got: {stored_name:?}"
    );
}
