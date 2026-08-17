//! Integration tests for the stats HTTP listener: `GET /` and
//! `GET /api/stats`. Seeds a temp `stats.db`, drives the axum app with
//! `tower::ServiceExt::oneshot`, and asserts JSON shape + HTML marker.

use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use naiad_server::{StatsDb, StatsHttpState, stats_http_app};
use tower::ServiceExt;

// ── helpers ────────────────────────────────────────────────────────────────────

/// Unix timestamp of the current minute (floored to 60 s), near enough to
/// `SystemTime::now()` that range=24h queries will include it.
fn now_minute() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        / 60
        * 60
}

/// Open a fresh temp stats.db, seed rows with **near-now** timestamps so they
/// fall inside the 24h window, and return a read-only `StatsHttpState`.
fn seeded_state(dir: &tempfile::TempDir) -> Arc<StatsHttpState> {
    let path = dir.path().join("stats.db");

    // Writer: seed data in the current minute so range=24h includes them.
    let db = StatsDb::open(&path).expect("StatsDb::open must succeed");
    let ts = now_minute();
    db.write_sample(ts, "cpu_pct", "", 45.0)
        .expect("write_sample must succeed");
    db.write_sample(ts + 60, "cpu_pct", "", 38.0)
        .expect("write_sample must succeed");
    db.write_sample(ts, "rss_bytes", "", 256_000_000.0)
        .expect("write_sample must succeed");
    drop(db); // release the writer

    // Read-only: what the HTTP handler uses.
    let state = StatsHttpState::open(&path, Instant::now())
        .expect("StatsHttpState::open must succeed on an existing db");
    Arc::new(state)
}

// ── tests ──────────────────────────────────────────────────────────────────────

/// `GET /` must return 200 with `text/html; charset=utf-8` and contain the
/// stable marker `<!--naiad-stats-dashboard-->`.
#[tokio::test]
async fn index_serves_dashboard_html_with_marker() {
    let dir = tempfile::tempdir().unwrap();
    let state = seeded_state(&dir);
    let router = stats_http_app(state);

    let resp = router
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .expect("oneshot must not error");

    assert_eq!(resp.status(), StatusCode::OK);

    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/html"),
        "Content-Type must be text/html, got: {ct}"
    );

    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body read must succeed");
    let html = std::str::from_utf8(&body).expect("body must be UTF-8");
    assert!(
        html.contains("<!--naiad-stats-dashboard-->"),
        "page must contain the stable marker comment"
    );
    assert!(
        html.contains("naiad-repo stats"),
        "page must contain the <title>"
    );
}

/// `GET /api/stats?range=24h` must return 200 JSON with the documented
/// top-level keys and `series["cpu_pct"]` must be non-empty with `[ts,value]`
/// pairs (seeded data is in the current minute, well inside the 24h window).
#[tokio::test]
async fn api_stats_24h_returns_documented_json_shape() {
    let dir = tempfile::tempdir().unwrap();
    let state = seeded_state(&dir);
    let router = stats_http_app(state);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/stats?range=24h")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot must not error");

    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body read must succeed");
    let j: serde_json::Value = serde_json::from_slice(&body).expect("body must be valid JSON");

    // Top-level keys must be present.
    assert!(j.get("generated_at").is_some(), "generated_at missing");
    assert_eq!(j["range"], "24h", "range must echo the request param");
    assert!(j.get("server_version").is_some(), "server_version missing");
    assert!(j.get("uptime_secs").is_some(), "uptime_secs missing");
    assert!(j.get("odometers").is_some(), "odometers missing");
    assert!(j.get("series").is_some(), "series missing");
    assert!(j.get("by_endpoint").is_some(), "by_endpoint missing");

    // Odometer sub-keys must exist.
    let odo = &j["odometers"];
    assert!(
        odo.get("req_total").is_some(),
        "odometers.req_total missing"
    );
    assert!(
        odo.get("bytes_shipped_total").is_some(),
        "odometers.bytes_shipped_total missing"
    );
    assert!(
        odo.get("busiest_hour").is_some(),
        "odometers.busiest_hour missing"
    );
    assert!(
        odo["busiest_hour"].get("count").is_some(),
        "busiest_hour.count missing"
    );
    assert!(
        odo["busiest_hour"].get("at").is_some(),
        "busiest_hour.at missing"
    );
    assert!(
        odo.get("top_prefix").is_some(),
        "odometers.top_prefix missing"
    );
    assert!(
        odo["top_prefix"].get("count").is_some(),
        "top_prefix.count missing"
    );
    assert!(
        odo["top_prefix"].get("key").is_some(),
        "top_prefix.key missing"
    );

    // `cpu_pct` must appear in series with non-empty data (seeded near-now rows
    // are inside the 24h window) and every element must be a [ts, value] pair.
    let series = &j["series"];
    let cpu_arr = series
        .get("cpu_pct")
        .and_then(|v| v.as_array())
        .expect("cpu_pct must appear in series for range=24h with near-now seed data");
    assert!(
        !cpu_arr.is_empty(),
        "cpu_pct series must be non-empty — seeded rows must be inside the 24h window"
    );
    for pt in cpu_arr {
        let pair = pt.as_array().expect("series point must be an array");
        assert_eq!(pair.len(), 2, "series point must be a [ts, value] pair");
        assert!(pair[0].is_number(), "ts element must be a number");
        assert!(pair[1].is_number(), "value element must be a number");
    }
    // Spot-check: the first value must be one of the seeded values (45.0 or 38.0).
    let first_val = cpu_arr[0][1].as_f64().unwrap();
    assert!(
        (first_val - 45.0).abs() < 1e-6 || (first_val - 38.0).abs() < 1e-6,
        "cpu_pct value must match a seeded sample (45.0 or 38.0), got {first_val}"
    );
}

/// `GET /api/stats?range=all` must return valid JSON with the documented shape.
#[tokio::test]
async fn api_stats_range_all_returns_valid_shape() {
    let dir = tempfile::tempdir().unwrap();
    let state = seeded_state(&dir);
    let router = stats_http_app(state);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/stats?range=all")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot must not error");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(j["range"], "all");
    assert!(j.get("series").is_some());
    assert!(j.get("odometers").is_some());
}

/// An unrecognised `range` must clamp to `24h` — no 400.
#[tokio::test]
async fn api_stats_unknown_range_clamps_to_24h() {
    let dir = tempfile::tempdir().unwrap();
    let state = seeded_state(&dir);
    let router = stats_http_app(state);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/stats?range=banana")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot must not error");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        j["range"], "24h",
        "unknown range must clamp to 24h, got: {}",
        j["range"]
    );
}

/// A broken `stats.db` (garbage file content) must cause the handler to
/// return `500` with `{"error": "stats query failed"}`.
#[tokio::test]
async fn broken_db_returns_500_json_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("broken.db");
    // Write non-SQLite garbage so the connection opens but queries fail.
    std::fs::write(&db_path, b"not a sqlite database at all").unwrap();

    let state = Arc::new(
        StatsHttpState::open(&db_path, Instant::now())
            .expect("open_with_flags on any existing file succeeds"),
    );
    let router = stats_http_app(state);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot must not error");

    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a broken DB must yield 500"
    );
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        j["error"], "stats query failed",
        "error body must match contract"
    );
}

/// `resolve_stats` must refuse a non-loopback bind address (co-located here
/// to confirm the loopback guard is active; unit tests in settings.rs cover
/// the full ladder).
#[test]
fn resolve_stats_refuses_non_loopback_bind() {
    use naiad_server::settings::{StatsSettings, resolve_stats};
    let file = StatsSettings {
        listen: Some("0.0.0.0:9092".parse().unwrap()),
        ..Default::default()
    };
    let err = resolve_stats(&file, std::path::Path::new("/srv/repo.db"), |_| None)
        .expect_err("non-loopback must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("0.0.0.0") || msg.contains("allow_non_loopback"),
        "error must name the address or the escape hatch: {msg}"
    );
}
