//! The daemon serves the embedded Svelte UI by default; `--ui-dir` overrides it
//! with a live directory. The typed API routes keep precedence over both.

use std::fs;
use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use naiad_daemon::{AppState, app};
use naiad_db::Db;
use tempfile::TempDir;
use tower::ServiceExt;

async fn get(state: &AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    let resp = app(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

fn ui_state(index_html: &str) -> (AppState, TempDir, TempDir) {
    let ui = TempDir::new().unwrap();
    fs::write(ui.path().join("index.html"), index_html).unwrap();
    let thumbs = TempDir::new().unwrap();
    let db = Db::open_in_memory().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64)
        .with_ui_dir(Some(ui.path().to_path_buf()));
    (state, ui, thumbs)
}

#[tokio::test]
async fn serves_ui_index_and_keeps_api() {
    let (state, _ui, _thumbs) = ui_state("<!doctype html><title>NAIAD UI BUILD</title>");

    // `/` serves the configured UI directory, overriding the embedded default.
    let (status, body) = get(&state, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8(body).unwrap().contains("NAIAD UI BUILD"));

    // The API still works — routes take precedence over the static fallback.
    let (status, body) = get(&state, "/api/files").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array());
}

#[tokio::test]
async fn unknown_path_falls_back_to_ui_index() {
    let (state, _ui, _thumbs) = ui_state("<!doctype html><title>SPA ROOT</title>");
    // An unmatched path serves index.html (SPA fallback), 200.
    let (status, body) = get(&state, "/some/spa/route").await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8(body).unwrap().contains("SPA ROOT"));
}

#[tokio::test]
async fn without_ui_dir_unknown_path_serves_embedded_index() {
    // No `with_ui_dir`: the embedded Svelte UI is the fallback, so an
    // unmatched path serves the embedded `index.html` (SPA routing).
    let thumbs = TempDir::new().unwrap();
    let db = Db::open_in_memory().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    let (status, body) = get(&state, "/some/spa/route").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("id=\"app\""));
}

#[test]
fn run_from_path_rejects_missing_ui_dir() {
    // A `--ui-dir` without an index.html is a startup error, surfaced before the
    // daemon binds (so this returns immediately rather than blocking on serve).
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("naiad.db");
    let missing = dir.path().join("nope");
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let err = naiad_daemon::run_from_path(&db_path, addr, 64, Some(missing), true)
        .expect_err("missing ui dir should error");
    assert!(err.to_string().contains("no index.html"), "got: {err}");
}
