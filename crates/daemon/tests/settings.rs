//! Settings integration tests.
//!
//! The trust floor API (`/api/trust/floor`) was removed in the client/server
//! pivot (Task 9). Those tests are deleted. Only the gallery sort preference
//! test (which uses the surviving `/api/view/sort` route) remains.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use naiad_daemon::{AppState, ThumbStore, app};
use naiad_db::Db;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

async fn get(state: &AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    send(
        state,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await
}

async fn post(state: &AppState, uri: &str, body: Value) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, req).await
}

#[tokio::test]
async fn gallery_sort_preference_is_persisted_in_the_database() {
    let db = Db::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = ThumbStore::open(&dir.path().join("thumbs.db")).unwrap();
    let state = AppState::new(db, store, 64);

    let (s, body) = get(&state, "/api/view/sort").await;
    assert_eq!(s, StatusCode::OK);
    let dto: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(dto, json!({ "key": "imported_at", "direction": "desc" }));

    let (s, _) = post(
        &state,
        "/api/view/sort",
        json!({ "key": "name", "direction": "asc" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, body) = get(&state, "/api/view/sort").await;
    assert_eq!(s, StatusCode::OK);
    let dto: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(dto, json!({ "key": "name", "direction": "asc" }));
}
