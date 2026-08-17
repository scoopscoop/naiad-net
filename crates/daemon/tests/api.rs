//! In-process HTTP tests for the `/api/*` data endpoints, driven via
//! `tower::ServiceExt::oneshot` (no socket bound).

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use naiad_daemon::{AppState, app};
use naiad_db::Db;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A real, decodable PNG.
fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 32])
    });
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

fn state_with_two_files() -> (AppState, tempfile::TempDir, tempfile::TempDir) {
    let files = naiad_test_support::fixture_dir(&[
        ("a.png", &png_bytes(8, 8)),
        ("b.png", &png_bytes(12, 12)),
    ]);
    let db = Db::open_in_memory().unwrap();
    naiad_daemon::import_path(&db, files.path(), |_| {}).unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);
    (state, files, thumbs)
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

async fn delete(state: &AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    send(state, req).await
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

/// Hash of `a.png` via the files endpoint.
async fn hash_of_a(state: &AppState) -> String {
    let (_, body) = get(state, "/api/files").await;
    json_of(&body)
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "a.png")
        .unwrap()["hash"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn files_lists_all() {
    let (state, _f, _t) = state_with_two_files();
    let (status, body) = get(&state, "/api/files").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json_of(&body);
    assert_eq!(arr.as_array().unwrap().len(), 2);
    // FileDto carries hash, name, size, path.
    let first = &arr[0];
    assert_eq!(first["hash"].as_str().unwrap().len(), 64);
    assert!(first["size"].is_number());
    assert!(first["path"].as_str().unwrap().ends_with(".png"));
}

#[tokio::test]
async fn scan_imports_a_folder() {
    let (state, _f, _t) = state_with_two_files();
    let more = naiad_test_support::fixture_dir(&[("c.png", &png_bytes(16, 16))]);
    let (status, body) = post(
        &state,
        "/api/scan",
        json!({ "folder": more.path().to_str().unwrap() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let summary = json_of(&body);
    assert_eq!(summary["imported"], 1);
    assert!(summary["errors"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn scan_stream_emits_a_summary_event() {
    let (state, _f, _t) = state_with_two_files();
    let more = naiad_test_support::fixture_dir(&[("c.png", &png_bytes(16, 16))]);
    let uri = format!(
        "/api/scan/stream?folder={}",
        urlencoding(more.path().to_str().unwrap())
    );
    let resp = app(state.clone())
        .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.starts_with("text/event-stream"), "content-type was {ct}");
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("event: summary"),
        "no summary event in: {text}"
    );
    assert!(text.contains("\"imported\":1"), "wrong summary in: {text}");
}

/// Minimal percent-encoding for a filesystem path in a query string (enough for
/// temp-dir paths in tests).
fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '/' | ':' | '\\' => c.to_string(),
            ' ' => "%20".to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[tokio::test]
async fn rejects_non_loopback_host_header() {
    // DNS-rebinding defense: a request carrying an attacker domain in Host is
    // dropped before reaching the API, while loopback Hosts pass.
    let (state, _f, _t) = state_with_two_files();

    let with_host = |h: &str| {
        Request::builder()
            .uri("/api/files")
            .header(header::HOST, h)
            .body(Body::empty())
            .unwrap()
    };

    let (evil, _) = send(&state, with_host("evil.example")).await;
    assert_eq!(evil, StatusCode::FORBIDDEN);

    let (loopback, _) = send(&state, with_host("127.0.0.1:8080")).await;
    assert_eq!(loopback, StatusCode::OK);

    let (named, _) = send(&state, with_host("localhost")).await;
    assert_eq!(named, StatusCode::OK);
}

#[tokio::test]
async fn tag_add_list_remove_round_trip() {
    let (state, _f, _t) = state_with_two_files();
    let hash = hash_of_a(&state).await;

    let (s1, _) = post(
        &state,
        "/api/tags/add",
        json!({ "file": hash, "tags": ["character:samus", "Creator: Nintendo"] }),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    let (s2, body) = get(&state, &format!("/api/tags?file={hash}&raw=true")).await;
    assert_eq!(s2, StatusCode::OK);
    let tags: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert_eq!(tags, vec!["character:samus", "creator:nintendo"]);

    let (s3, _) = post(
        &state,
        "/api/tags/remove",
        json!({ "file": hash, "tags": ["creator:nintendo"] }),
    )
    .await;
    assert_eq!(s3, StatusCode::OK);

    let (_, body2) = get(&state, &format!("/api/tags?file={hash}&raw=true")).await;
    let after: Vec<String> = serde_json::from_slice(&body2).unwrap();
    assert_eq!(after, vec!["character:samus"]);
}

#[tokio::test]
async fn tag_on_unknown_file_is_bad_request() {
    let (state, _f, _t) = state_with_two_files();
    let (status, _) = post(
        &state,
        "/api/tags/add",
        json!({ "file": "/nope/missing.png", "tags": ["x:y"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn siblings_and_parents_round_trip() {
    let (state, _f, _t) = state_with_two_files();

    let (s1, _) = post(
        &state,
        "/api/siblings/add",
        json!({ "bad": "samus", "ideal": "character:samus aran" }),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, _) = post(
        &state,
        "/api/parents/add",
        json!({ "child": "character:samus aran", "parent": "series:metroid" }),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);

    let (_, sib_body) = get(&state, "/api/siblings").await;
    let sibs = json_of(&sib_body);
    assert_eq!(sibs[0]["bad"], "samus");
    assert_eq!(sibs[0]["ideal"], "character:samus aran");

    let (_, par_body) = get(&state, "/api/parents").await;
    let pars = json_of(&par_body);
    assert_eq!(pars[0]["child"], "character:samus aran");
    assert_eq!(pars[0]["parent"], "series:metroid");

    let (s3, _) = post(
        &state,
        "/api/parents/remove",
        json!({ "child": "character:samus aran", "parent": "series:metroid" }),
    )
    .await;
    assert_eq!(s3, StatusCode::OK);
    let (s4, _) = post(&state, "/api/siblings/remove", json!({ "bad": "samus" })).await;
    assert_eq!(s4, StatusCode::OK);

    let (_, sib_body2) = get(&state, "/api/siblings").await;
    assert!(json_of(&sib_body2).as_array().unwrap().is_empty());
}

/// Minimal percent-encoding for a query value (encodes everything but the
/// RFC 3986 unreserved set), so a Windows path with `:` and `\` survives.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn roots_listed_after_scan_and_removable() {
    let dir = naiad_test_support::fixture_dir(&[("x.jpg", b"x")]);
    let folder = dir.path().to_str().unwrap().to_string();

    let thumbs = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    // Scan registers the folder as a root.
    let (s, _) = post(&state, "/api/scan", json!({ "folder": folder })).await;
    assert_eq!(s, StatusCode::OK);

    // GET /api/roots lists it, absolutized.
    let (s, body) = get(&state, "/api/roots").await;
    assert_eq!(s, StatusCode::OK);
    let roots: Vec<String> = serde_json::from_slice(&body).unwrap();
    let expected = std::path::absolute(dir.path())
        .unwrap()
        .display()
        .to_string();
    assert_eq!(roots, vec![expected.clone()]);

    // DELETE /api/roots?path=... removes it.
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/api/roots?path={}", urlencode(&expected)))
        .body(Body::empty())
        .unwrap();
    let (s, _) = send(&state, req).await;
    assert_eq!(s, StatusCode::OK);

    // It is gone now.
    let (_, body) = get(&state, "/api/roots").await;
    let roots: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert!(roots.is_empty());

    // Removing a non-root is a 404.
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/roots?path=%2Fnope")
        .body(Body::empty())
        .unwrap();
    let (s, _) = send(&state, req).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn roots_remove_with_hide_drops_files_from_search() {
    let dir = naiad_test_support::fixture_dir(&[("x.jpg", b"x"), ("y.jpg", b"y")]);
    let folder = dir.path().to_str().unwrap().to_string();
    let thumbs = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    let (s, _) = post(&state, "/api/scan", json!({ "folder": folder })).await;
    assert_eq!(s, StatusCode::OK);
    let expected = std::path::absolute(dir.path())
        .unwrap()
        .display()
        .to_string();

    // Two files are listed before removal.
    let (_, body) = get(&state, "/api/files").await;
    assert_eq!(json_of(&body).as_array().unwrap().len(), 2);

    // DELETE with hide=true removes the root AND hides its files.
    let (s, _) = delete(
        &state,
        &format!("/api/roots?path={}&hide=true", urlencode(&expected)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (_, body) = get(&state, "/api/files").await;
    assert!(json_of(&body).as_array().unwrap().is_empty());
}

#[tokio::test]
async fn roots_remove_without_hide_keeps_files() {
    let dir = naiad_test_support::fixture_dir(&[("x.jpg", b"x")]);
    let folder = dir.path().to_str().unwrap().to_string();
    let thumbs = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    let (s, _) = post(&state, "/api/scan", json!({ "folder": folder })).await;
    assert_eq!(s, StatusCode::OK);
    let expected = std::path::absolute(dir.path())
        .unwrap()
        .display()
        .to_string();

    // Default (no hide) removes the root but leaves files listed.
    let (s, _) = delete(&state, &format!("/api/roots?path={}", urlencode(&expected))).await;
    assert_eq!(s, StatusCode::OK);

    let (_, body) = get(&state, "/api/files").await;
    assert_eq!(json_of(&body).as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn blocks_crud_round_trip() {
    let thumbs = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    // GET with no rules: empty list.
    let (s, body) = get(&state, "/api/blocks").await;
    assert_eq!(s, StatusCode::OK);
    let rules = json_of(&body);
    assert!(rules.as_array().unwrap().is_empty());

    // POST adds a rule.
    let (s, _) = post(
        &state,
        "/api/blocks",
        json!({ "kind": "tag_pattern", "target": "meme:*", "note": "noise" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // GET now returns the rule.
    let (s, body) = get(&state, "/api/blocks").await;
    assert_eq!(s, StatusCode::OK);
    let rules = json_of(&body);
    let arr = rules.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["kind"], "tag_pattern");
    assert_eq!(arr[0]["target"], "meme:*");
    assert_eq!(arr[0]["note"], "noise");
    let id = arr[0]["id"].as_i64().unwrap();

    // DELETE by id removes it.
    let (s, _) = delete(&state, &format!("/api/blocks?id={id}")).await;
    assert_eq!(s, StatusCode::OK);

    // GET again: empty list.
    let (s, body) = get(&state, "/api/blocks").await;
    assert_eq!(s, StatusCode::OK);
    let rules = json_of(&body);
    assert!(rules.as_array().unwrap().is_empty());

    // DELETE a nonexistent id returns 404.
    let (s, _) = delete(&state, "/api/blocks?id=999999").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn health_returns_ok() {
    let (state, _files, _thumbs) = state_with_two_files();
    let (status, body) = get(&state, "/api/health").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).expect("health body is JSON");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["watch"]["complete"], true, "no watcher → complete");
    // A daemon with no watcher never ran a catch-up scan: an idle, incomplete
    // status the UI ignores (roots_total 0 && imported 0).
    assert_eq!(v["scan"]["running"], false);
    assert_eq!(v["scan"]["complete"], false);
    assert_eq!(v["scan"]["imported"], 0);
    assert_eq!(v["scan"]["roots_total"], 0);
}

// ── rejection routes ─────────────────────────────────────────────────────────

/// Seed a DB with one file and one pulled mapping from `service_name` for
/// `tag_str`. Returns (state, thumbs, hash_hex).
fn state_with_pulled_mapping(
    service_name: &str,
    tag_str: &str,
) -> (AppState, tempfile::TempDir, String) {
    use naiad_core::{FileRecord, Tag, hash_bytes};

    let thumbs = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();

    let content = format!("{service_name}:{tag_str}:content");
    let hash = hash_bytes(content.as_bytes());
    let hash_hex = hash.to_hex();
    db.insert_file(
        &FileRecord::new(hash, "/lib/test.png".into(), content.len() as u64, None),
        1,
    )
    .unwrap();

    // Add shared service with an unreachable URL so caps fetch fails gracefully.
    let svc_id = db
        .add_shared_service(service_name, "http://127.0.0.1:19999", None)
        .unwrap();

    // Merge a pulled mapping so the tag comes from `service_name`.
    let tag = Tag::parse(tag_str).unwrap();
    db.merge_pulled_mappings(svc_id, &[(hash, vec![tag])])
        .unwrap();

    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);
    (state, thumbs, hash_hex)
}

#[tokio::test]
async fn reject_round_trips_undoes_and_offer_field_is_false() {
    let (state, _thumbs, hash_hex) = state_with_pulled_mapping("repo", "series:metroid");

    // POST rejects; the unreachable service URL means caps fetch returns an error,
    // so `reports` field is false (caps unavailable → no reports offered).
    let (status, body) = post(
        &state,
        "/api/reject",
        json!({ "hash": hash_hex, "tag": "series:metroid", "service": "repo" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "first reject: {}",
        String::from_utf8_lossy(&body)
    );
    let resp = json_of(&body);
    assert_eq!(
        resp["reports"], false,
        "unreachable repo → caps fetch fails → reports field is false"
    );

    // Idempotent: second POST returns 200 and still leaves exactly one row.
    let (status2, body2) = post(
        &state,
        "/api/reject",
        json!({ "hash": hash_hex, "tag": "series:metroid", "service": "repo" }),
    )
    .await;
    assert_eq!(
        status2,
        StatusCode::OK,
        "second (idempotent) reject: {}",
        String::from_utf8_lossy(&body2)
    );

    // GET /api/rejections?hash= lists the one rejection.
    let (status3, body3) = get(&state, &format!("/api/rejections?hash={hash_hex}")).await;
    assert_eq!(status3, StatusCode::OK);
    let listed = json_of(&body3);
    let arr = listed.as_array().unwrap();
    assert_eq!(arr.len(), 1, "one rejection after idempotent pair");
    assert_eq!(arr[0]["tag"].as_str().unwrap(), "series:metroid");
    assert_eq!(arr[0]["service"].as_str().unwrap(), "repo");

    // Effective tags no longer carry it; raw still does (Task 2 wiring).
    let (_, eff_body) = get(&state, &format!("/api/tags?file={hash_hex}")).await;
    let eff_tags: Vec<String> = serde_json::from_slice(&eff_body).unwrap();
    assert!(
        !eff_tags.iter().any(|t| t.contains("series:metroid")),
        "rejected tag must be absent from effective tags; got {eff_tags:?}"
    );

    let (_, raw_body) = get(&state, &format!("/api/tags?file={hash_hex}&raw=true")).await;
    let raw_tags: Vec<String> = serde_json::from_slice(&raw_body).unwrap();
    assert!(
        raw_tags.iter().any(|t| t.contains("series:metroid")),
        "rejected tag must still appear in raw tags; got {raw_tags:?}"
    );

    // DELETE undoes the rejection, idempotently.
    let (status_d, _) = delete(
        &state,
        &format!("/api/reject?hash={hash_hex}&tag=series:metroid&service=repo"),
    )
    .await;
    assert_eq!(status_d, StatusCode::OK, "first delete");

    let (status_d2, _) = delete(
        &state,
        &format!("/api/reject?hash={hash_hex}&tag=series:metroid&service=repo"),
    )
    .await;
    assert_eq!(status_d2, StatusCode::OK, "second (idempotent) delete");

    // Listed is now empty.
    let (status_e, body_e) = get(&state, &format!("/api/rejections?hash={hash_hex}")).await;
    assert_eq!(status_e, StatusCode::OK);
    let empty = json_of(&body_e);
    assert!(
        empty.as_array().unwrap().is_empty(),
        "rejections must be empty after undo"
    );
}

/// Direct DB proof that `shared_service_by_name` is the local-exempt gate.
///
/// The seeded local service is named `"my tags"` (id=1, scope='local',
/// url=NULL — migration 0002_tags.sql). The function's SQL is:
///   SELECT id, name, url FROM services WHERE scope = 'shared' AND name = ?1
///
/// This test verifies two things:
/// 1. `shared_service_by_name("my tags")` returns `None` — the scope filter fires.
/// 2. The row actually exists (local_service_id succeeds), so the None is NOT
///    a missing-name error — it is the scope filter.
///
/// The HTTP test `reject_refuses_local_service_mappings` below relies on this:
/// it POSTs `service: "my tags"` and expects 400. The 400 ultimately traces to
/// `shared_service_by_name` returning None (→ SubmitError::BadRequest → 400).
/// Note: even if the scope filter were removed, the local service's null URL
/// would cause a rusqlite type error (NULL → String) that also maps to 400,
/// so the HTTP test alone cannot distinguish the two failure modes. This unit
/// test proves the scope filter is the real gate.
#[test]
fn shared_service_by_name_scope_filter_is_the_gate() {
    let db = Db::open_in_memory().unwrap();

    // The seeded local service (scope='local') is invisible to shared_service_by_name.
    assert!(
        db.shared_service_by_name("my tags").unwrap().is_none(),
        "shared_service_by_name must return None for scope='local' service 'my tags'"
    );

    // Prove the name exists — the None came from the scope filter, not a missing row.
    // local_service_id() finds it via: SELECT id FROM services WHERE scope='local'.
    let local_id = db
        .local_service_id()
        .expect("'my tags' local service must exist");
    assert_eq!(local_id, 1, "migration seeds 'my tags' with id=1");

    // A shared service with the same style of name IS visible.
    db.add_shared_service("shared repo", "http://127.0.0.1:9090", None)
        .unwrap();
    assert!(
        db.shared_service_by_name("shared repo").unwrap().is_some(),
        "shared_service_by_name must find scope='shared' services"
    );
}

#[tokio::test]
async fn reject_refuses_local_service_mappings() {
    // End-to-end local-exempt gate: POST /api/reject with service="my tags"
    // (the real seeded local service, scope='local') must return 400.
    //
    // "my tags" is the actual row name (not an absent string like "local"), so
    // the failure traces through shared_service_by_name → None → BadRequest → 400.
    // The unit test `shared_service_by_name_scope_filter_is_the_gate` (above)
    // proves that the None is caused by the scope='shared' filter, not a missing name.
    use naiad_core::{FileRecord, hash_bytes};

    let thumbs = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();

    let hash = hash_bytes(b"local_file_content");
    let hash_hex = hash.to_hex();
    db.insert_file(&FileRecord::new(hash, "/lib/local.png".into(), 18, None), 1)
        .unwrap();

    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    let (status, _) = post(
        &state,
        "/api/reject",
        json!({ "hash": hash_hex, "tag": "character:samus", "service": "my tags" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "reject with local service 'my tags' must be 400 (local-exempt gate)"
    );
}

#[tokio::test]
async fn rejections_list_all_covers_multiple_files() {
    // Exercises GET /api/rejections with no ?hash= (list-all path).
    // The implementation uses WHERE (?1 IS NULL OR r.file_id = ?1) with a NULL
    // bind for the no-hash case — this test proves that path returns all rows,
    // not just rows for a single file.
    use naiad_core::{FileRecord, Tag, hash_bytes};

    let thumbs = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();

    // Two distinct files.
    let hash_a = hash_bytes(b"file_a_content");
    let hash_b = hash_bytes(b"file_b_content");
    let hex_a = hash_a.to_hex();
    let hex_b = hash_b.to_hex();
    db.insert_file(&FileRecord::new(hash_a, "/lib/a.png".into(), 13, None), 1)
        .unwrap();
    db.insert_file(&FileRecord::new(hash_b, "/lib/b.png".into(), 13, None), 2)
        .unwrap();

    // One shared service; both files get a pulled tag from it.
    let svc_id = db
        .add_shared_service("repo2", "http://127.0.0.1:19998", None)
        .unwrap();
    db.merge_pulled_mappings(
        svc_id,
        &[
            (hash_a, vec![Tag::parse("series:metroid").unwrap()]),
            (hash_b, vec![Tag::parse("series:zelda").unwrap()]),
        ],
    )
    .unwrap();

    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    // Reject both.
    for (hex, tag) in [(&hex_a, "series:metroid"), (&hex_b, "series:zelda")] {
        let (s, body) = post(
            &state,
            "/api/reject",
            json!({ "hash": hex, "tag": tag, "service": "repo2" }),
        )
        .await;
        assert_eq!(
            s,
            StatusCode::OK,
            "reject {tag}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    // GET /api/rejections (no hash) returns both rows.
    let (s, body) = get(&state, "/api/rejections").await;
    assert_eq!(s, StatusCode::OK);
    let all = json_of(&body);
    let arr = all.as_array().unwrap();
    assert_eq!(
        arr.len(),
        2,
        "list-all must return both rejections; got {arr:?}"
    );

    // GET /api/rejections?hash=hex_a scopes to only file A's row.
    let (s2, body2) = get(&state, &format!("/api/rejections?hash={hex_a}")).await;
    assert_eq!(s2, StatusCode::OK);
    let scoped = json_of(&body2);
    let scoped_arr = scoped.as_array().unwrap();
    assert_eq!(
        scoped_arr.len(),
        1,
        "?hash= must scope to one file; got {scoped_arr:?}"
    );
    assert_eq!(scoped_arr[0]["tag"].as_str().unwrap(), "series:metroid");
}

// ── report route ─────────────────────────────────────────────────────────────

/// `POST /api/report` — daemon resolves the service, signs the request, and
/// forwards it to the originating repository. End-to-end: real `spawn_test_repo`
/// server so the full auth round-trip is exercised. Verifies server-side landing
/// by fetching the moderator report queue after the 204.
#[tokio::test(flavor = "multi_thread")]
async fn report_forwards_to_repo_server() {
    use naiad_core::{FileRecord, Tag, hash_bytes};
    use naiad_netproto::{Account, Op, RepoClient};
    use naiad_server::RepoStore;

    // Pre-seed the server store with a moderator account so we can verify
    // server-side landing without needing a mutable handle after spawn.
    let mod_acct = Account::generate();
    let repo_store = RepoStore::open_in_memory().unwrap();
    let content: &[u8] = b"report_e2e_content";
    let hash = hash_bytes(content);
    let hash_hex = hash.to_hex();
    // Submit from the moderator account to register it in the store.
    repo_store
        .apply_submission(&mod_acct.sign(Op::Add, &hash, &Tag::parse("character:samus").unwrap()))
        .unwrap();
    // Promote the account to moderator so it can call GET /repo/reports.
    repo_store
        .set_role(&mod_acct.public_hex(), "moderator")
        .unwrap();

    let repo = naiad_test_support::spawn_test_repo(repo_store);
    let repo_url = format!("http://{}", repo.addr);

    // Daemon DB: one file + shared service pointing at the real server.
    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &FileRecord::new(hash, "/lib/report.png".into(), content.len() as u64, None),
        1,
    )
    .unwrap();
    db.add_shared_service("testrepo", &repo_url, None).unwrap();

    let thumbs = tempfile::tempdir().unwrap();
    let keydir = tempfile::tempdir().unwrap();
    let key_path = keydir.path().join("naiad.key");
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64)
        .with_key_path(key_path);

    // POST /api/report — daemon signs and forwards.
    let (status, body) = post(
        &state,
        "/api/report",
        json!({
            "hash": hash_hex,
            "tag": "character:samus",
            "service": "testrepo",
            "note": "test report note",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "report must forward to repo server: {}",
        String::from_utf8_lossy(&body)
    );

    // Verify server-side landing: moderator fetches the open report queue.
    let url = repo_url.clone();
    let report_list =
        tokio::task::spawn_blocking(move || RepoClient::new(&url).fetch_reports(&mod_acct))
            .await
            .unwrap()
            .expect("moderator must be able to fetch reports");

    assert_eq!(report_list.rows.len(), 1, "one report in the queue");
    let row = &report_list.rows[0];
    assert_eq!(row.hash, hash_hex, "report hash matches");
    assert_eq!(row.tag, "character:samus", "report tag matches");
    assert_eq!(
        row.note.as_deref(),
        Some("test report note"),
        "report note round-trips"
    );
    assert_eq!(row.status, "open", "new report status is open");
}

/// Caps-cache deduplication proof: two rejects against the same service must
/// result in at most one `/repo/caps` network fetch (the second hits the cache).
#[tokio::test(flavor = "multi_thread")]
async fn caps_cache_deduplicates_fetches_per_service() {
    use naiad_core::{FileRecord, Tag, hash_bytes};
    use naiad_server::RepoStore;

    // A real server so caps are actually fetchable.
    let repo = naiad_test_support::spawn_test_repo(RepoStore::open_in_memory().unwrap());
    let repo_url = format!("http://{}", repo.addr);

    let db = Db::open_in_memory().unwrap();
    let hash = hash_bytes(b"caps_cache_proof");
    let hash_hex = hash.to_hex();
    db.insert_file(&FileRecord::new(hash, "/lib/proof.png".into(), 16, None), 1)
        .unwrap();
    let svc_id = db.add_shared_service("cacherepo", &repo_url, None).unwrap();
    // Give the file a pulled mapping so the rejection has something to write.
    db.merge_pulled_mappings(svc_id, &[(hash, vec![Tag::parse("series:zelda").unwrap()])])
        .unwrap();

    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);

    // Two rejects against the same service: caps should only be fetched once.
    let (s1, body1) = post(
        &state,
        "/api/reject",
        json!({ "hash": hash_hex, "tag": "series:zelda", "service": "cacherepo" }),
    )
    .await;
    assert_eq!(
        s1,
        StatusCode::OK,
        "first reject: {}",
        String::from_utf8_lossy(&body1)
    );

    let (s2, body2) = post(
        &state,
        "/api/reject",
        json!({ "hash": hash_hex, "tag": "series:zelda", "service": "cacherepo" }),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::OK,
        "second (idempotent) reject: {}",
        String::from_utf8_lossy(&body2)
    );

    // Both rejects used the caps cache: at most one network fetch.
    assert_eq!(
        state.caps_fetch_count(),
        1,
        "two rejects against the same service must cause at most one caps fetch"
    );
}
