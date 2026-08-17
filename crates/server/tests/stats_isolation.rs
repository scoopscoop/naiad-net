//! Isolation tests for the stats subsystem (#235, Task 8).
//!
//! Verifies:
//! (a) A deliberately broken `stats.db` (a directory at the configured path)
//!     causes `spawn_stats` to return `None` (best-effort open failure) without
//!     panicking, and that the main repo router is completely unaffected.
//! (b) `stats enabled = false` → `spawn_stats` returns `None` and the main
//!     router still serves normally.
//!
//! The main router is driven with `tower::ServiceExt::oneshot` to confirm
//! correct status codes on `GET /health` and `POST /repo/buckets`, independent
//! of stats state.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use naiad_server::{
    RepoStore, app,
    settings::{StatsConfig, StatsSettings, resolve_stats},
    spawn_stats,
};
use tower::ServiceExt;

// ── helpers ────────────────────────────────────────────────────────────────────

/// Build a minimal stats config with the given db_path.
fn stats_cfg_with_path(db_path: std::path::PathBuf) -> StatsConfig {
    StatsConfig {
        enabled: true,
        listen: "127.0.0.1:0".parse().unwrap(), // port 0 = OS-assigned; fine for tests
        allow_non_loopback: false,
        db_path,
    }
}

/// Wrap a router with `MockConnectInfo` for oneshot use.
fn with_mock_addr(router: axum::Router) -> axum::Router {
    router.layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

/// Open a fresh in-memory-equivalent temp store.
fn temp_store() -> (tempfile::TempDir, RepoStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = RepoStore::open(dir.path().join("repo.db")).unwrap();
    (dir, store)
}

// ── (a) broken stats.db ────────────────────────────────────────────────────────

/// A directory at `stats.db` path → `spawn_stats` returns `None`, main router
/// unaffected: `GET /health` still responds 200.
#[tokio::test]
async fn broken_stats_db_directory_returns_none_and_main_router_serves() {
    let dir = tempfile::tempdir().unwrap();
    // Create a **directory** where stats.db would go — SQLite cannot open a dir.
    let stats_dir = dir.path().join("stats.db");
    std::fs::create_dir_all(&stats_dir).unwrap();

    let cfg = stats_cfg_with_path(stats_dir);
    // spawn_stats must return None (open failure) and not panic.
    let handle = spawn_stats(&cfg, dir.path(), None, None).await;
    assert!(
        handle.is_none(),
        "spawn_stats must return None when stats.db path is a directory"
    );

    // The main repo router must still serve GET /health = 200.
    let (_keep, store) = temp_store();
    let router = with_mock_addr(app(Arc::new(Mutex::new(store)), 1));
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot must not fail");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /health must return 200 regardless of stats state"
    );
}

/// Same as above but with a corrupt file (non-SQLite bytes) → `spawn_stats`
/// returns `None`.
#[tokio::test]
async fn broken_stats_db_corrupt_file_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let stats_path = dir.path().join("stats.db");
    // Write non-SQLite garbage — SQLite open will succeed but DDL will fail.
    std::fs::write(&stats_path, b"this is not a sqlite database").unwrap();

    let cfg = stats_cfg_with_path(stats_path);
    let handle = spawn_stats(&cfg, dir.path(), None, None).await;
    assert!(
        handle.is_none(),
        "spawn_stats must return None when stats.db contains corrupt data"
    );
}

// ── (b) stats disabled ────────────────────────────────────────────────────────

/// `enabled = false` → `resolve_stats` yields disabled config; `spawn_stats`
/// returns `None`. Main router serves normally.
#[tokio::test]
async fn stats_disabled_returns_none_and_main_router_serves() {
    // resolve_stats with enabled=false.
    let file = StatsSettings {
        enabled: Some(false),
        ..Default::default()
    };
    let cfg = resolve_stats(&file, std::path::Path::new("/tmp/repo.db"), |_| None)
        .expect("resolve_stats must succeed for enabled=false");
    assert!(!cfg.enabled, "config must reflect enabled=false");

    let handle = spawn_stats(&cfg, std::path::Path::new("/tmp/repo.db"), None, None).await;
    assert!(
        handle.is_none(),
        "spawn_stats must return None when enabled=false"
    );

    // Main router must serve normally — no stats involvement.
    let (_keep, store) = temp_store();
    let router = with_mock_addr(app(Arc::new(Mutex::new(store)), 1));

    // GET /health
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot must not fail");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /health must return 200 when stats is disabled"
    );

    // POST /repo/buckets with a minimal valid request (empty body → 400 is fine;
    // what matters is the server is alive and responding).
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/repo/buckets")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prefix":"00","bits":8,"domain":"blake3"}"#))
                .unwrap(),
        )
        .await
        .expect("oneshot must not fail");
    // 200 (empty store, no hashes) or 4xx (malformed) — any response proves the
    // router is alive; what it must not be is a panic or a connection error.
    assert!(
        resp.status().as_u16() < 500,
        "POST /repo/buckets must not return 5xx (server alive, stats disabled): {}",
        resp.status()
    );
}
