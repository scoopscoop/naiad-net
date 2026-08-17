//! End-to-end HTTP tests for `GET /api/tags/relations` and the `relations` flag
//! on `GET /api/tags/detailed`.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use naiad_core::{FileRecord, hash_bytes};
use naiad_daemon::{AppState, add_parent, add_sibling, add_tags, app};
use naiad_db::Db;
use serde_json::Value;
use tower::ServiceExt;

/// Send a request and parse the body as JSON. A non-JSON 200 body panics the
/// test; use `send_status` when a non-JSON response is expected (e.g. 400).
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

/// Send a request and return only the status code, discarding the body.
/// Used for error paths whose response body is plain text, not JSON.
async fn send_status(state: &AppState, req: Request<Body>) -> StatusCode {
    app(state.clone()).oneshot(req).await.unwrap().status()
}

/// Build a shared AppState seeded with:
/// - one file tagged with the alias `samus_aran` and the unrelated tag `meta:solo`
/// - sibling `samus_aran` → `character:samus`
/// - parent `character:samus` → `series:metroid`
///
/// Returns the state, the file's hex hash, and temp-dir handles (keep them
/// alive for the test's duration so the paths aren't deleted early).
fn make_state() -> (AppState, String, tempfile::TempDir, tempfile::TempDir) {
    let db = Db::open_in_memory().unwrap();
    let file_bytes: &[u8] = b"samus-test-file";
    let hash = hash_bytes(file_bytes);
    let file_hex = hash.to_hex();

    db.insert_file(
        &FileRecord::new(hash, "/lib/s.txt".into(), file_bytes.len() as u64, None),
        1,
    )
    .unwrap();

    // File carries the alias (`samus_aran`) and an unrelated tag (`meta:solo`).
    add_tags(
        &db,
        &file_hex,
        &["samus_aran".to_string(), "meta:solo".to_string()],
    )
    .unwrap();
    // samus_aran is an alias for character:samus (sibling relation).
    add_sibling(&db, "samus_aran", "character:samus").unwrap();
    // character:samus implies series:metroid (parent relation).
    add_parent(&db, "character:samus", "series:metroid").unwrap();

    let thumbs = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64)
        .with_settings_path(key_dir.path().join("naiad.toml"));
    (state, file_hex, thumbs, key_dir)
}

/// `GET /api/tags/relations?tag=character:samus&file=<hash>&cap=10` must return
/// 200 with the canonical tag, `via_alias=true` (because the file is tagged with
/// the alias `samus_aran`), and populated `aliases` and `parents` sections.
/// The alias item must carry `count=1` (one file tagged with `samus_aran`).
#[tokio::test(flavor = "multi_thread")]
async fn relations_endpoint_returns_sections_and_via_alias() {
    let (state, file_hex, _thumbs, _key_dir) = make_state();

    let uri = format!(
        "/api/tags/relations?tag=character%3Asamus&file={}&cap=10",
        file_hex
    );
    let (status, body) = send(
        &state,
        Request::builder().uri(&uri).body(Body::empty()).unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["canonical"], "character:samus");
    // Top-level merged count: raw(character:samus=0) + raw(samus_aran=1) = 1.
    assert_eq!(body["count"], 1, "canonical merged count");
    assert_eq!(
        body["via_alias"], true,
        "file is tagged via alias samus_aran"
    );

    let aliases = &body["aliases"];
    assert_eq!(
        aliases["total"], 1,
        "one alias (samus_aran → character:samus)"
    );
    assert_eq!(
        aliases["items"][0]["tag"], "samus_aran",
        "alias item tag matches"
    );
    // Alias rows show their OWN raw count. Here the file is tagged with the
    // alias spelling `samus_aran` directly, so its raw count is 1.
    assert_eq!(
        aliases["items"][0]["count"], 1,
        "samus_aran shows its own raw mapping count (1 file uses that spelling)"
    );

    let parents = &body["parents"];
    assert_eq!(parents["total"], 1, "one parent (series:metroid)");
    assert_eq!(
        parents["items"][0]["tag"], "series:metroid",
        "parent item tag matches"
    );

    // No children in this seed.
    assert_eq!(body["children"]["total"], 0);
}

/// Querying without a `file` parameter must still return 200 and populated
/// sections, but `via_alias` must be `false` because no file context exists.
#[tokio::test(flavor = "multi_thread")]
async fn relations_endpoint_without_file_is_not_via_alias() {
    let (state, _file_hex, _thumbs, _key_dir) = make_state();

    let (status, body) = send(
        &state,
        Request::builder()
            .uri("/api/tags/relations?tag=character%3Asamus")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["canonical"], "character:samus");
    assert_eq!(
        body["via_alias"], false,
        "no file → via_alias must be false"
    );

    // Sections are still populated (relations exist regardless of file context).
    assert_eq!(body["aliases"]["total"], 1);
    assert_eq!(body["parents"]["total"], 1);
}

/// An empty `tag` string is not a valid tag and must produce 400.
#[tokio::test(flavor = "multi_thread")]
async fn relations_endpoint_rejects_bad_tag() {
    let db = Db::open_in_memory().unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64)
        .with_settings_path(key_dir.path().join("naiad.toml"));

    let status = send_status(
        &state,
        Request::builder()
            .uri("/api/tags/relations?tag=")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "empty tag must be rejected"
    );
}

/// `GET /api/tags/detailed?file=<hash>` must carry `"relations": true` for
/// `character:samus` (has aliases and a parent) and `"relations": false` for
/// `meta:solo` (no relations defined).
#[tokio::test(flavor = "multi_thread")]
async fn detailed_carries_relations_flag() {
    let (state, file_hex, _thumbs, _key_dir) = make_state();

    let uri = format!("/api/tags/detailed?file={file_hex}");
    let (status, body) = send(
        &state,
        Request::builder().uri(&uri).body(Body::empty()).unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let tags = body.as_array().expect("detailed returns an array");

    // The file is tagged with samus_aran, which resolves to character:samus.
    let samus_entry = tags
        .iter()
        .find(|t| t["tag"] == "character:samus")
        .expect("character:samus must be present in detailed list (canonicalized from samus_aran)");
    assert_eq!(
        samus_entry["relations"], true,
        "character:samus has aliases and parents so relations must be true"
    );

    // meta:solo has no sibling or parent relations → relations must be false.
    let solo_entry = tags
        .iter()
        .find(|t| t["tag"] == "meta:solo")
        .expect("meta:solo must be present in detailed list");
    assert_eq!(
        solo_entry["relations"], false,
        "meta:solo has no relations so relations must be false"
    );
}
