//! In-process HTTP integration tests for the Hydrus plugin endpoints:
//! `GET /api/plugins`, `POST /api/hydrus/configure`,
//! `POST /api/source/import`, `POST /api/tagger/lookup`.
//!
//! All tests use `tower::ServiceExt::oneshot` — no socket is bound.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use naiad_core::FileRecord;
use naiad_daemon::{AppState, ThumbStore, app};
use naiad_db::Db;
use rusqlite::Connection;
use serde_json::{Value, json};
use tower::ServiceExt;

/// SHA-256 that the fixture's single file is keyed to.
const SHA_HEX: &str = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";

/// Build a minimal Hydrus-shaped DB in `dir`, identical to the plugin-hydrus
/// fixture: one file with SHA=SHA_HEX, tag-service 9, one sibling, one parent,
/// two current mappings for that file.
fn build_hydrus_fixture(dir: &std::path::Path) {
    let master = Connection::open(dir.join("client.master.db")).unwrap();
    master
        .execute_batch(
            "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
             CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
             CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
             CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);",
        )
        .unwrap();
    master
        .execute(
            "INSERT INTO hashes (hash_id, hash) VALUES (1, ?1)",
            [hex::decode(SHA_HEX).unwrap()],
        )
        .unwrap();
    master
        .execute_batch(
            "INSERT INTO namespaces VALUES (1, ''), (2, 'character');
             INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, 'samus_aran');
             INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 2, 3);",
        )
        .unwrap();

    let client = Connection::open(dir.join("client.db")).unwrap();
    client
        .execute_batch(
            "CREATE TABLE current_files_4 (hash_id INTEGER, timestamp_ms INTEGER);
             INSERT INTO current_files_4 VALUES (1, 0);
             CREATE TABLE current_tag_siblings_9 (bad_tag_id INTEGER, good_tag_id INTEGER);
             INSERT INTO current_tag_siblings_9 VALUES (3, 2);
             CREATE TABLE current_tag_parents_9 (child_tag_id INTEGER, parent_tag_id INTEGER);
             INSERT INTO current_tag_parents_9 VALUES (2, 1);",
        )
        .unwrap();

    let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
    mappings
        .execute_batch(
            "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
             INSERT INTO current_mappings_9 VALUES (1, 1), (2, 1);",
        )
        .unwrap();
}

/// Seed a file into the Naiad db with the fixture SHA-256.
/// Returns the BLAKE3 hex of the seeded file.
fn seed_file_with_sha256(db: &Db) -> String {
    // Use content whose real SHA-256 would be SHA_HEX — we can't easily manufacture
    // that, so we directly set the sha256 after insert (mimicking what backfill does).
    let (blake, _) = naiad_core::hash_reader_dual(&b"hydrus-test-file"[..]).unwrap();
    let rec = FileRecord::new(blake, std::path::PathBuf::from("test.jpg"), 16, None);
    db.insert_file(&rec, 1).unwrap();
    let file_id = db.file_id_by_hash(&blake).unwrap().unwrap();
    db.set_sha256(file_id, SHA_HEX).unwrap();
    blake.to_hex()
}

fn make_state(db: Db) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = ThumbStore::open(&dir.path().join("thumbs.db")).unwrap();
    let state = AppState::new(db, store, 64).with_settings_path(dir.path().join("naiad.toml"));
    (state, dir)
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

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("response is valid JSON")
}

#[tokio::test]
async fn plugins_lists_hydrus() {
    let db = Db::open_in_memory().unwrap();
    let (state, _dir) = make_state(db);
    let (status, body) = get(&state, "/api/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json_of(&body);
    let plugins = arr.as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["id"], "hydrus");
    assert_eq!(plugins[0]["tagger"], true);
    assert_eq!(plugins[0]["source"], true);
    assert_eq!(plugins[0]["processor"], false);
}

#[tokio::test]
async fn configure_returns_no_content() {
    let db = Db::open_in_memory().unwrap();
    let (state, _dir) = make_state(db);
    let fixture_dir = tempfile::tempdir().unwrap();
    build_hydrus_fixture(fixture_dir.path());

    let (status, _) = post(
        &state,
        "/api/hydrus/configure",
        json!({
            "dir": fixture_dir.path().to_str().unwrap(),
            "tag_services": [9]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn configure_then_import_then_lookup() {
    let db = Db::open_in_memory().unwrap();
    let blake3_hex = seed_file_with_sha256(&db);
    let (state, _dir) = make_state(db);
    let fixture_dir = tempfile::tempdir().unwrap();
    build_hydrus_fixture(fixture_dir.path());

    // 1. Configure Hydrus dir.
    let (s, _) = post(
        &state,
        "/api/hydrus/configure",
        json!({
            "dir": fixture_dir.path().to_str().unwrap(),
            "tag_services": [9]
        }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "configure should return 204");

    // 2. Run bulk import.
    let (s, body) = post(
        &state,
        "/api/source/import",
        json!({ "plugin_id": "hydrus" }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "import failed: {}",
        String::from_utf8_lossy(&body)
    );
    let summary = json_of(&body);
    assert!(
        summary["siblings"].as_u64().unwrap() >= 1,
        "expected >=1 sibling, got: {summary}"
    );
    assert!(
        summary["parents"].as_u64().unwrap() >= 1,
        "expected >=1 parent, got: {summary}"
    );
    assert!(
        summary["mappings_resolved"].as_u64().unwrap() >= 1,
        "expected >=1 resolved mapping, got: {summary}"
    );

    // 3. Tagger lookup (apply=false).
    let (s, body) = post(
        &state,
        "/api/tagger/lookup",
        json!({
            "plugin_id": "hydrus",
            "files": [blake3_hex],
            "apply": false
        }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "lookup failed: {}",
        String::from_utf8_lossy(&body)
    );
    let items = json_of(&body);
    let arr = items.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["file"], blake3_hex);
    let tags = arr[0]["tags"].as_array().unwrap();
    assert!(!tags.is_empty(), "expected non-empty tags from lookup");
}

#[tokio::test]
async fn tagger_lookup_apply_true_writes_tags() {
    let db = Db::open_in_memory().unwrap();
    let blake3_hex = seed_file_with_sha256(&db);
    let (state, _dir) = make_state(db);
    let fixture_dir = tempfile::tempdir().unwrap();
    build_hydrus_fixture(fixture_dir.path());

    let (s, _) = post(
        &state,
        "/api/hydrus/configure",
        json!({
            "dir": fixture_dir.path().to_str().unwrap(),
            "tag_services": [9]
        }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Before applying: the file has no raw tags.
    let (s, body) = get(&state, &format!("/api/tags?file={blake3_hex}&raw=true")).await;
    assert_eq!(s, StatusCode::OK);
    let before: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert!(before.is_empty(), "no tags before apply, got {before:?}");

    // Lookup with apply=true writes the candidate tags into the local service.
    let (s, body) = post(
        &state,
        "/api/tagger/lookup",
        json!({
            "plugin_id": "hydrus",
            "files": [blake3_hex],
            "apply": true
        }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "apply lookup failed: {}",
        String::from_utf8_lossy(&body)
    );
    let mut applied: Vec<String> = json_of(&body)[0]["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    applied.sort();
    assert!(!applied.is_empty(), "lookup returned tags to apply");

    // The applied tags now appear in the file's raw stored tags.
    let (s, body) = get(&state, &format!("/api/tags?file={blake3_hex}&raw=true")).await;
    assert_eq!(s, StatusCode::OK);
    let mut stored: Vec<String> = serde_json::from_slice(&body).unwrap();
    stored.sort();
    assert_eq!(stored, applied, "applied tags must be stored on the file");

    // A second apply=false lookup returns the same tags without error or
    // duplication of stored rows (re-apply is idempotent via add_mapping).
    let (s, body) = post(
        &state,
        "/api/tagger/lookup",
        json!({
            "plugin_id": "hydrus",
            "files": [blake3_hex],
            "apply": false
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let mut again: Vec<String> = json_of(&body)[0]["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    again.sort();
    assert_eq!(again, applied, "second lookup returns the same tags");

    // Stored rows are unchanged (no duplicates).
    let (_, body) = get(&state, &format!("/api/tags?file={blake3_hex}&raw=true")).await;
    let mut stored2: Vec<String> = serde_json::from_slice(&body).unwrap();
    stored2.sort();
    assert_eq!(stored2, stored, "stored tags unchanged after re-lookup");
}

#[tokio::test]
async fn configure_persists_and_is_readable() {
    let db = Db::open_in_memory().unwrap();
    let (state, _dir) = make_state(db);
    let fixture_dir = tempfile::tempdir().unwrap();
    build_hydrus_fixture(fixture_dir.path());
    let dir_str = fixture_dir.path().to_str().unwrap().to_string();

    let (s, _) = post(
        &state,
        "/api/hydrus/configure",
        json!({ "dir": dir_str, "tag_services": [9] }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (s, body) = get(&state, "/api/hydrus/config").await;
    assert_eq!(s, StatusCode::OK);
    let cfg = json_of(&body);
    assert_eq!(cfg["dir"], dir_str);
    assert_eq!(cfg["tag_services"], json!([9]));
}
