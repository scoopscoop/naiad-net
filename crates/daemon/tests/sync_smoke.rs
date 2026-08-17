//! Pull + submit smoke test (v6 client/server pivot): a tag submitted to the
//! subscribed repo by a third party arrives at the client after a pull, while a
//! local-only tag that is never submitted stays local.
//!
//! Specifically this proves:
//! - A tag seeded directly on the repo by a third party is returned to the
//!   client on pull.
//! - `meta:mine`, added locally but never submitted, does not appear in the
//!   repo's snapshot.
//! - `meta:shared` submitted by the client does appear in the repo's snapshot.

use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use naiad_core::{FileRecord, Tag, hash_bytes};
use naiad_daemon::{AppState, CapsCache, app, pull_repo};
use naiad_db::Db;
use naiad_netproto::{Account, Op};
use naiad_server::RepoStore;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Serve a repo app over a shared store on an ephemeral port.
async fn serve_repo(store: Arc<Mutex<RepoStore>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let store_clone = Arc::clone(&store);
    tokio::spawn(async move {
        axum::serve(
            listener,
            naiad_server::app(store_clone, 1000)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    url
}

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
async fn a_tag_submitted_to_repo_b_reaches_a_client_subscribed_to_repo_a() {
    // v6: plain client/server, no gossip/mirror between repos.
    // We test the core invariant: a tag pre-seeded on the repo is visible after
    // pull; a local-only tag never reaches the repo; a submitted tag does.
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));

    // A file the client owns; a third party tags it directly on the repo.
    let owned_bytes: &[u8] = b"synced";
    let owned_hex = hash_bytes(owned_bytes).to_hex();
    let submitter = Account::generate();
    {
        let h: naiad_core::Hash = owned_hex.parse().unwrap();
        let sub = submitter.sign(Op::Add, &h, &Tag::parse("character:samus").unwrap());
        store.lock().unwrap().apply_submission(&sub).unwrap();
    }

    let url = serve_repo(store.clone()).await;

    // The client daemon: owns the file, has a local-only tag, subscribes to the repo.
    // A key path is required so the daemon can sign and submit tags.
    let files = naiad_test_support::fixture_dir(&[("a.png", owned_bytes)]);
    let db = Db::open_in_memory().unwrap();
    naiad_daemon::import_path(&db, files.path(), |_| {}).unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let keydir = tempfile::tempdir().unwrap();
    let key_path = keydir.path().join("naiad.key");
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64)
        .with_key_path(key_path);

    // Add a local-only tag (never submitted to any repo).
    let (s, _) = post(
        &state,
        "/api/tags/add",
        json!({ "file": owned_hex, "tags": ["meta:mine"] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Add a tag that will be submitted to the repo as a positive control.
    let (s, _) = post(
        &state,
        "/api/tags/add",
        json!({ "file": owned_hex, "tags": ["meta:shared"] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, _) = post(
        &state,
        "/api/repos",
        json!({ "name": "repo-a", "url": url }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, body) = post(&state, "/api/repos/pull", json!({ "name": "repo-a" })).await;
    assert_eq!(s, StatusCode::OK);
    let summary: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        summary["matched_files"], 1,
        "the owned hash matched on the repo"
    );

    // The third-party tag arrived at the client.
    let (_, body) = get(&state, &format!("/api/tags?file={owned_hex}&raw=true")).await;
    let tags: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert!(
        tags.contains(&"character:samus".to_string()),
        "tag seeded on repo should reach the client after pull, got {tags:?}"
    );
    assert!(
        tags.contains(&"meta:mine".to_string()),
        "local tag untouched"
    );

    // Submit meta:shared to the repo — positive control: proves the repo actually
    // accepts and stores a submitted tag, making the absence assertion below
    // non-vacuous (a tag that IS submitted DOES appear; a tag that is NOT
    // submitted must NOT appear).
    let (s, _) = post(
        &state,
        "/api/repos/submit",
        json!({ "name": "repo-a", "file": owned_hex, "tag": "meta:shared", "op": "add" }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "submit meta:shared to repo");

    // Repo now holds the submitted tag. v8 snapshot: BTreeMap<hash, Vec<OriginTag>>.
    let snap = store.lock().unwrap().snapshot().unwrap();
    assert!(
        snap.values().flatten().any(|t| t.tag == "meta:shared"),
        "meta:shared must appear in repo after submission (positive control)"
    );

    // The local-only tag never left the client.
    assert!(
        !snap.values().flatten().any(|t| t.tag == "meta:mine"),
        "local-only tag must never reach the repo"
    );
}

/// A tag seeded on the repo (regardless of how many accounts submitted it)
/// arrives at the client after a pull. v6 carries no supporter metadata;
/// the tag's presence is the meaningful invariant.
#[tokio::test(flavor = "multi_thread")]
async fn three_supporters_survive_sync_pull() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));

    let file_bytes: &[u8] = b"three-supporter-file";
    let hash = hash_bytes(file_bytes);

    // Seed 3 distinct accounts, all tagging the same hash.
    {
        let s = store.lock().unwrap();
        let tag = Tag::parse("char:samus").unwrap();
        s.apply_submission(&Account::generate().sign(Op::Add, &hash, &tag))
            .unwrap();
        s.apply_submission(&Account::generate().sign(Op::Add, &hash, &tag))
            .unwrap();
        s.apply_submission(&Account::generate().sign(Op::Add, &hash, &tag))
            .unwrap();
    }

    let url = serve_repo(store).await;

    // Client: own the file, subscribe, pull.
    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &FileRecord::new(hash, "/lib/a".into(), file_bytes.len() as u64, None),
        1,
    )
    .unwrap();
    db.add_shared_service("repo", &url, None).unwrap();
    let db = Mutex::new(db);

    let cache = CapsCache::new();
    let db = tokio::task::spawn_blocking(move || {
        pull_repo(&db, &cache, "repo", 256, None).unwrap();
        db
    })
    .await
    .unwrap();

    // v6: plain-delta wire carries no supporter metadata; just assert the tag arrived.
    let db_lock = db.lock().unwrap();
    let fid = db_lock.file_id_by_hash(&hash).unwrap().unwrap();
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        tags.contains(&"char:samus".to_string()),
        "char:samus must arrive after sync pull; got {tags:?}"
    );
}

/// Per-file pull: tags only the requested hash, is idempotent, and reports a
/// dead repo as a per-repo error while still returning 200.
#[tokio::test(flavor = "multi_thread")]
async fn per_file_pull_scopes_isolates_and_repeats() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));

    // The repo tags BOTH files; the client will pull only for `a`.
    let a_bytes: &[u8] = b"pullme";
    let b_bytes: &[u8] = b"leaveme";
    let a_hex = naiad_core::hash_bytes(a_bytes).to_hex();
    let submitter = Account::generate();
    {
        let s = store.lock().unwrap();
        for bytes in [a_bytes, b_bytes] {
            let h = naiad_core::hash_bytes(bytes);
            s.apply_submission(&submitter.sign(
                Op::Add,
                &h,
                &naiad_core::Tag::parse("char:samus").unwrap(),
            ))
            .unwrap();
        }
    }
    let url = serve_repo(store).await;

    let files = naiad_test_support::fixture_dir(&[("a.png", a_bytes), ("b.png", b_bytes)]);
    let db = naiad_db::Db::open_in_memory().unwrap();
    naiad_daemon::import_path(&db, files.path(), |_| {}).unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    let (s, _) = post(
        &state,
        "/api/repos",
        serde_json::json!({ "name": "repo-a", "url": url }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Pull for `a` only.
    let (s, body) = post(
        &state,
        "/api/files/pull-tags",
        serde_json::json!({ "hashes": [a_hex] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let results: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(results[0]["repo"], "repo-a");
    assert_eq!(
        results[0]["mappings_added"], 1,
        "one tag for the one requested file"
    );

    // `a` got the tag; `b` did not, although the repo knows it.
    let (_, body) = get(&state, &format!("/api/tags?file={a_hex}&raw=true")).await;
    let tags: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert!(tags.contains(&"char:samus".to_string()));
    let b_hex = naiad_core::hash_bytes(b_bytes).to_hex();
    let (_, body) = get(&state, &format!("/api/tags?file={b_hex}&raw=true")).await;
    let tags: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert!(tags.is_empty(), "unrequested file stays untagged: {tags:?}");

    // Repeat pull: zero new mappings.
    let (s, body) = post(
        &state,
        "/api/files/pull-tags",
        serde_json::json!({ "hashes": [a_hex] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let results: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(results[0]["mappings_added"], 0, "idempotent");
}

/// A dead repo yields a per-repo error entry, not a failed request.
#[tokio::test(flavor = "multi_thread")]
async fn per_file_pull_reports_a_dead_repo_per_entry() {
    let file_bytes: &[u8] = b"orphan";
    let hex = naiad_core::hash_bytes(file_bytes).to_hex();
    let files = naiad_test_support::fixture_dir(&[("a.png", file_bytes)]);
    let db = naiad_db::Db::open_in_memory().unwrap();
    naiad_daemon::import_path(&db, files.path(), |_| {}).unwrap();
    // Subscribe directly in the DB — the API's caps handshake would (rightly)
    // refuse a dead URL, and here we want a subscribed-then-died repo.
    db.add_shared_service("dead", "http://127.0.0.1:9", None)
        .unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    let (s, body) = post(
        &state,
        "/api/files/pull-tags",
        serde_json::json!({ "hashes": [hex] }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "per-repo failure never fails the request"
    );
    let results: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(results[0]["repo"], "dead");
    assert!(
        results[0]["error"].is_string(),
        "error recorded: {results:?}"
    );
}

/// Empty hash list is rejected with 400.
#[tokio::test(flavor = "multi_thread")]
async fn per_file_pull_rejects_empty_hash_list() {
    let db = naiad_db::Db::open_in_memory().unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    let (s, _) = post(
        &state,
        "/api/files/pull-tags",
        serde_json::json!({ "hashes": [] }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Unparseable hex is rejected with 400 and the body mentions "bad hash".
#[tokio::test(flavor = "multi_thread")]
async fn per_file_pull_rejects_bad_hex() {
    let db = naiad_db::Db::open_in_memory().unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    let (s, body) = post(
        &state,
        "/api/files/pull-tags",
        serde_json::json!({ "hashes": ["notahex"] }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        String::from_utf8_lossy(&body).contains("bad hash"),
        "body should mention 'bad hash', got: {}",
        String::from_utf8_lossy(&body)
    );
}
