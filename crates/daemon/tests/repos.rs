//! End-to-end: a daemon pulls a repo's snapshot and the owned file's tags appear
//! — while unowned tags are never stored and local tags survive a repo drop.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use naiad_core::hash_bytes;
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

#[tokio::test(flavor = "multi_thread")]
async fn pulling_a_repo_tags_owned_files_and_remove_purges() {
    // --- a repo seeded with tags for an owned hash AND an unowned hash ---
    let owned_bytes: &[u8] = b"alpha";
    let owned_hex = hash_bytes(owned_bytes).to_hex();
    let unowned_hex = hash_bytes(b"nobody-owns-this").to_hex();

    let store = RepoStore::open_in_memory().unwrap();
    let repo_acct = naiad_netproto::Account::generate();
    for (hash_hex, tag) in [
        (owned_hex.as_str(), "character:samus"),
        (owned_hex.as_str(), "series:metroid"),
        (unowned_hex.as_str(), "creator:nintendo"),
    ] {
        let h: naiad_core::Hash = hash_hex.parse().unwrap();
        store
            .apply_submission(&repo_acct.sign(
                naiad_netproto::Op::Add,
                &h,
                &naiad_core::Tag::parse(tag).unwrap(),
            ))
            .unwrap();
    }

    let repo = naiad_test_support::spawn_test_repo(store);
    let repo_url = format!("http://{}", repo.addr);

    // --- a library that owns the file at `owned_hex`, plus a local-only tag ---
    let files = naiad_test_support::fixture_dir(&[("a.png", owned_bytes)]);
    let db = Db::open_in_memory().unwrap();
    naiad_daemon::import_path(&db, files.path(), |_| {}).unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    // A pre-existing local-only tag on the owned file.
    let (s, _) = post(
        &state,
        "/api/tags/add",
        json!({ "file": owned_hex, "tags": ["meta:mine"] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // --- subscribe + pull ---
    let (s, _) = post(
        &state,
        "/api/repos",
        json!({ "name": "ptr", "url": repo_url }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, body) = post(&state, "/api/repos/pull", json!({ "name": "ptr" })).await;
    assert_eq!(s, StatusCode::OK);
    let summary: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(summary["matched_files"], 1, "only the owned hash matched");
    assert_eq!(summary["mappings"], 2, "two pulled tags stored");

    // (a) the owned file now shows the pulled tags + (c) the local tag survives.
    let (_, body) = get(&state, &format!("/api/tags?file={owned_hex}&raw=true")).await;
    let tags: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert!(tags.contains(&"character:samus".to_string()));
    assert!(tags.contains(&"series:metroid".to_string()));
    assert!(
        tags.contains(&"meta:mine".to_string()),
        "local tag untouched"
    );
    // (b) the unowned tag was never stored anywhere in the library.
    assert!(
        !tags.contains(&"creator:nintendo".to_string()),
        "tags for unowned hashes are not stored"
    );

    // (d) re-pull is idempotent: the same authoritative set, no duplication.
    let (_, body) = post(&state, "/api/repos/pull", json!({ "name": "ptr" })).await;
    let summary: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        summary["mappings"], 2,
        "re-pull holds the same two mappings"
    );

    // (e) removing the repo with purge=true removes its pulled tags but keeps the local one.
    let (s, _) = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri("/api/repos?name=ptr&purge=true")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (_, body) = get(&state, &format!("/api/tags?file={owned_hex}&raw=true")).await;
    let tags: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        tags,
        vec!["meta:mine".to_string()],
        "only the local tag remains"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn detach_default_keeps_tags_removes_from_list_and_404s_on_second_delete() {
    // A repo seeded with one tag for an owned file.
    let owned_bytes: &[u8] = b"beta";
    let owned_hex = hash_bytes(owned_bytes).to_hex();

    let store = RepoStore::open_in_memory().unwrap();
    let repo_acct = naiad_netproto::Account::generate();
    let h: naiad_core::Hash = owned_hex.parse().unwrap();
    store
        .apply_submission(&repo_acct.sign(
            naiad_netproto::Op::Add,
            &h,
            &naiad_core::Tag::parse("character:samus").unwrap(),
        ))
        .unwrap();

    let repo = naiad_test_support::spawn_test_repo(store);
    let repo_url = format!("http://{}", repo.addr);

    let files = naiad_test_support::fixture_dir(&[("b.png", owned_bytes)]);
    let db = Db::open_in_memory().unwrap();
    naiad_daemon::import_path(&db, files.path(), |_| {}).unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    // Subscribe and pull so the tag lands on the owned file.
    let (s, _) = post(
        &state,
        "/api/repos",
        json!({ "name": "r", "url": repo_url }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = post(&state, "/api/repos/pull", json!({ "name": "r" })).await;
    assert_eq!(s, StatusCode::OK);

    // Verify tag is present before detach.
    let (_, body) = get(&state, &format!("/api/tags?file={owned_hex}&raw=true")).await;
    let tags: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert!(
        tags.contains(&"character:samus".to_string()),
        "tag present after pull"
    );

    // DELETE without purge flag — detach by default.
    let (s, _) = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri("/api/repos?name=r")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // (a) The repo no longer appears in GET /api/repos.
    let (_, body) = get(&state, "/api/repos").await;
    let repos: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(
        repos.iter().all(|r| r["name"] != "r"),
        "detached repo must not appear in the repo list"
    );

    // (b) The pulled tag survives on the file.
    let (_, body) = get(&state, &format!("/api/tags?file={owned_hex}&raw=true")).await;
    let tags: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert!(
        tags.contains(&"character:samus".to_string()),
        "pulled tag must survive a plain detach"
    );

    // (c) A second DELETE of the same name returns 404.
    let (s, _) = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri("/api/repos?name=r")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "second delete must 404");
}
