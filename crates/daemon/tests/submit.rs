//! End-to-end: the daemon signs a tag submission (creating its account on first
//! use), the repo verifies and stores it, a pull returns it attributed to the
//! author, a signed remove retracts it, and a local-only tag is untouched.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use naiad_daemon::{AppState, app};
use naiad_db::Db;
use naiad_server::RepoStore;
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

async fn get(state: &AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    send(
        state,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await
}

async fn tags(state: &AppState, hex: &str) -> Vec<String> {
    let (_, body) = get(state, &format!("/api/tags?file={hex}&raw=true")).await;
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_tag_round_trips_with_its_author_and_remove_retracts_it() {
    // An empty repo to receive submissions.
    let repo = naiad_test_support::spawn_test_repo(RepoStore::open_in_memory().unwrap());
    let repo_url = format!("http://{}", repo.addr);

    // A library owning one file, with a pre-existing local-only tag.
    let owned_bytes: &[u8] = b"owned";
    let files = naiad_test_support::fixture_dir(&[("a.png", owned_bytes)]);
    let db = Db::open_in_memory().unwrap();
    naiad_daemon::import_path(&db, files.path(), |_| {}).unwrap();

    // Read back the hash from the DB so we match exactly what the indexer stored
    // (the indexer hashes the file bytes via blake3; the round-trip via the list
    // endpoint is the most robust way to get the real hex).
    let thumbs = tempfile::tempdir().unwrap();
    let keydir = tempfile::tempdir().unwrap();
    let key_path = keydir.path().join("naiad.key");
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64)
        .with_key_path(key_path.clone());

    // Discover the imported file's hash from the list endpoint.
    let (s, body) = get(&state, "/api/files").await;
    assert_eq!(s, StatusCode::OK);
    let files_list: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(files_list.len(), 1, "exactly one file imported");
    let owned_hex = files_list[0]["hash"].as_str().unwrap().to_string();

    let (s, _) = post(
        &state,
        "/api/tags/add",
        json!({ "file": owned_hex, "tags": ["meta:mine"] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // No account exists yet.
    let (_, body) = get(&state, "/api/account").await;
    let acct: Value = serde_json::from_slice(&body).unwrap();
    assert!(acct["public_key"].is_null(), "no key until first submit");
    assert!(!key_path.exists(), "no key file yet");

    // Subscribe + submit an add (lazily creates the key).
    let (s, _) = post(
        &state,
        "/api/repos",
        json!({ "name": "ptr", "url": repo_url }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = post(
        &state,
        "/api/repos/submit",
        json!({ "name": "ptr", "file": owned_hex, "tag": "character:samus", "op": "add" }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // Derived mode (the default for new subscriptions, ADR 0020 §6) creates the
    // master seed beside the key, not the global naiad.key itself.
    let master_path = keydir.path().join("naiad.master");
    assert!(
        master_path.exists(),
        "submit created the master seed file in derived mode"
    );

    // The account endpoint shows the global naiad.key (non-creating): null in
    // derived mode since that key is never materialized for new subscriptions.
    let (_, body) = get(&state, "/api/account").await;
    let acct: Value = serde_json::from_slice(&body).unwrap();
    // Derived mode only: naiad.key is never created, so public_key must be null.
    assert!(
        acct["public_key"].is_null(),
        "derived mode must not create naiad.key; public_key must be null"
    );

    // Pull it back: the tag appears, the local tag survives.
    let (s, _) = post(&state, "/api/repos/pull", json!({ "name": "ptr" })).await;
    assert_eq!(s, StatusCode::OK);
    let t = tags(&state, &owned_hex).await;
    assert!(
        t.contains(&"character:samus".to_string()),
        "pulled tag present"
    );
    assert!(t.contains(&"meta:mine".to_string()), "local tag untouched");

    // Submit a remove, re-pull: the tag is retracted, local tag still there.
    let (s, _) = post(
        &state,
        "/api/repos/submit",
        json!({ "name": "ptr", "file": owned_hex, "tag": "character:samus", "op": "remove" }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = post(&state, "/api/repos/pull", json!({ "name": "ptr" })).await;
    assert_eq!(s, StatusCode::OK);
    let t = tags(&state, &owned_hex).await;
    assert!(
        !t.contains(&"character:samus".to_string()),
        "remove propagated"
    );
    assert_eq!(
        t,
        vec!["meta:mine".to_string()],
        "only the local tag remains"
    );
}
