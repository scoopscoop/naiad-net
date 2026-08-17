//! End-to-end HTTP test for `GET /api/tags/detailed`: a pulled tag surfaces
//! the correct presence and shared service names.

use std::sync::Mutex;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use naiad_core::{FileRecord, hash_bytes};
use naiad_daemon::{AppState, CapsCache, ThumbStore, app, pull_repo, submit_to_repo};
use naiad_db::Db;
use naiad_netproto::Op;
use serde_json::Value;
use tower::ServiceExt;

async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

#[tokio::test(flavor = "multi_thread")]
async fn tags_detailed_pulled_tag_has_correct_presence_and_services() {
    // v6: pulled tag surfaces presence=pulled and the shared service name ("ptr").
    let repo_store = naiad_server::RepoStore::open_in_memory().unwrap();
    let repo = naiad_test_support::spawn_test_repo(repo_store);
    let repo_url = format!("http://{}", repo.addr);

    let file_bytes: &[u8] = b"samus-test-file";
    let file_hex = hash_bytes(file_bytes).to_hex();

    let client_db = Db::open_in_memory().unwrap();
    client_db
        .insert_file(
            &FileRecord::new(
                hash_bytes(file_bytes),
                "/lib/s.txt".into(),
                file_bytes.len() as u64,
                None,
            ),
            1,
        )
        .unwrap();
    client_db
        .add_shared_service("ptr", &repo_url, None)
        .unwrap();
    let client_db = Mutex::new(client_db);

    let key_dir = tempfile::tempdir().unwrap();
    let key = key_dir.path().join("naiad.key");

    // Submit a tag and pull it back.
    let key2 = key.clone();
    let hex2 = file_hex.clone();
    let cache = CapsCache::new();
    let client_db = tokio::task::spawn_blocking(move || {
        submit_to_repo(
            &client_db,
            &cache,
            &key2,
            "ptr",
            &hex2,
            "char:samus",
            Op::Add,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .unwrap();
        let stats = pull_repo(&client_db, &cache, "ptr", 256, None).unwrap();
        assert_eq!(stats.mappings, 1, "one pulled mapping expected");
        client_db
    })
    .await
    .unwrap();
    let db = client_db.into_inner().unwrap();

    let store = ThumbStore::open(&key_dir.path().join("thumbs.db")).unwrap();
    let state = AppState::new(db, store, 64).with_settings_path(key_dir.path().join("naiad.toml"));

    // GET /api/tags/detailed → pulled tag present, presence=pulled, services=["ptr"].
    let (s, body) = send(
        &state,
        Request::builder()
            .uri(format!("/api/tags/detailed?file={file_hex}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let t = body
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tag"] == "char:samus")
        .expect("char:samus present in detailed list")
        .clone();
    assert_eq!(t["presence"], "pulled", "tag pulled from shared repo");
    let services = t["services"].as_array().expect("services field present");
    assert_eq!(
        services,
        &[serde_json::Value::String("ptr".into())],
        "pulled tag carries its shared service name"
    );
}
