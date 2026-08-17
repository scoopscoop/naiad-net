//! HTTP E2E tests for the sidecar serve path (#209).
//!
//! Boots a dual-domain server (native BLAKE3 + sidecar SHA-256) built from
//! `write_ptr_seed_fixture` and drives it over real axum oneshot calls,
//! mirroring the pattern of `mirror_mode_e2e.rs`.
//!
//! The caps and round-trip tests construct `DomainConfig` through
//! `DomainConfig::from_settings`, so the composed path
//!   `BridgeConfig → from_settings → build_app → buckets_handler → sidecar`
//! is covered end-to-end as issue #209 requires.  The 413 budget test keeps
//! a hand-built `DomainConfig` because it needs a tiny raw budget that
//! `from_settings` cannot inject.
//!
//! Fixture summary (service id 9):
//!   h1 = "11" + "00"×31  →  {maid, character:samus}
//!   h2 = "33" + "00"×31  →  {maid}
//!   h3 = "aa" + "00"×31  →  (no current mappings)
//!   h4 = "bb" + "00"×31  →  unparseable tag → no bucket_map row after seed

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use naiad_netproto::{Caps, HashDomain, PROTOCOL_VERSION, PullMode, Snapshot};
use naiad_server::bridge::sidecar::Sidecar;
use naiad_server::bridge::sidecar_seed;
use naiad_server::settings::{BridgeConfig, BridgeMode};
use naiad_server::{DomainConfig, RepoStore, Sha256Backend, SidecarBackend, app_domains_budget};
use tower::ServiceExt;

const H1: &str = "1100000000000000000000000000000000000000000000000000000000000000";
const H2: &str = "3300000000000000000000000000000000000000000000000000000000000000";

/// Seed a sidecar from the PTR fixture and return the seeded path.
fn seeded_sidecar(dir: &std::path::Path) -> std::path::PathBuf {
    naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir, 9).unwrap();
    let sc_path = dir.join("sidecar.db");
    let sc = Sidecar::create(&sc_path).unwrap();
    sidecar_seed::seed(dir, Some(9), &sc, false).unwrap();
    drop(sc);
    sc_path
}

/// Build a dual-domain sidecar router via `DomainConfig::from_settings`.
///
/// Routes through `BridgeConfig { enabled=true, mode=Sidecar, state_db=<abs> }`
/// so the integration covers the composed path:
///   `BridgeConfig → from_settings → build_app → buckets_handler → sidecar`.
/// k=1 ensures the hash count always exceeds k → `Bucketed` advise mode;
/// `added_sha256` in the resulting `DomainConfig` additionally sets
/// `snapshot_bits`, so the caps Bucketed assertion holds regardless of k.
fn sidecar_router_via_settings(sidecar_path: &std::path::Path) -> Router {
    let bridge = BridgeConfig {
        enabled: true,
        mode: BridgeMode::Sidecar,
        snapshot_dir: None,
        snapshot_service_id: None,
        max_query_bits: 256,
        min_query_bits: 8,
        ptr_url: String::new(),
        ptr_key: String::new(),
        // Absolute path — resolve_beside_db passes it through unchanged.
        state_db: sidecar_path.to_str().unwrap().to_string(),
    };
    // db_path is only used to resolve a relative state_db; since state_db is
    // absolute above, any placeholder value is fine here.
    let domains = DomainConfig::from_settings(
        HashDomain::Blake3,
        &bridge,
        std::path::Path::new("dummy.db"),
        1,
    )
    .expect("from_settings must succeed for a valid seeded sidecar");
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    app_domains_budget(store, None, 1, None, None, domains, usize::MAX, false)
        .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

/// Build a dual-domain sidecar router via struct literal — for the 413 budget
/// test which needs to inject a raw budget that `from_settings` cannot control.
fn sidecar_router_with_budget(sidecar_path: &std::path::Path, budget: usize) -> Router {
    let backend = SidecarBackend::open(sidecar_path, 1).unwrap();
    let domains = DomainConfig {
        native: HashDomain::Blake3,
        added_sha256: Some(Arc::new(backend) as Arc<dyn Sha256Backend>),
        max_query_bits: 256,
        min_query_bits: 8,
    };
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    app_domains_budget(store, None, 1, None, None, domains, budget, false)
        .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

// ── caps ──────────────────────────────────────────────────────────────────────

/// Via `from_settings`: caps must advertise sha256 in hash_domains, NOT in
/// incremental_domains, a Bucketed ceiling, and min_query_bits (#209).
///
/// Exercises: BridgeConfig → from_settings (sidecar branch, path resolution,
/// max/min clamping, added_sha256 wiring) → caps_handler → wire shape.
#[tokio::test]
async fn sidecar_caps_sha256_present_non_incremental_bucketed() {
    let dir = tempfile::tempdir().unwrap();
    let sc_path = seeded_sidecar(dir.path());
    let router = sidecar_router_via_settings(&sc_path);

    let resp = router
        .oneshot(Request::get("/repo/caps").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let caps: Caps = serde_json::from_slice(&body).unwrap();

    // Native domain is BLAKE3.
    assert_eq!(
        caps.hash_domain,
        HashDomain::Blake3,
        "native must be blake3"
    );

    // sha256 is in the served list.
    assert!(
        caps.serves(HashDomain::Sha256),
        "sha256 must be served: {caps:?}"
    );

    // sha256 is NOT in incremental_domains (sidecar has no sequence numbers).
    let incr = caps.incremental_domains.as_deref().unwrap_or(&[]);
    assert!(
        !incr.iter().any(|s| s == "sha256"),
        "sha256 must be excluded from incremental_domains: {incr:?}"
    );

    // Mode must be Bucketed (snapshot_bits set because added_sha256 is present).
    assert!(
        matches!(caps.mode, PullMode::Bucketed { .. }),
        "mode must be Bucketed when sidecar backend is present: {:?}",
        caps.mode
    );

    // min_query_bits must be advertised.
    assert!(
        caps.min_query_bits.is_some(),
        "min_query_bits must be advertised when sha256 domain is served"
    );
}

// ── bucket round-trip ─────────────────────────────────────────────────────────

/// Via `from_settings`: an 8-bit sha256-domain bucket query for prefix 0x11
/// returns h1's tags; h2 (prefix 0x33) is absent from the response.
///
/// Exercises: from_settings → build_app → buckets_handler → sidecar branch →
/// `SidecarBackend::bucket` → range scan → tag rendering.
#[tokio::test]
async fn sidecar_bucket_query_returns_fixture_tags() {
    let dir = tempfile::tempdir().unwrap();
    let sc_path = seeded_sidecar(dir.path());
    let router = sidecar_router_via_settings(&sc_path);

    let req_body = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "prefix_bits": 8,
        "buckets": [H1],    // "11..." — 8-bit prefix covers only h1 in the fixture
        "domain": "sha256",
    });
    let resp = router
        .oneshot(
            Request::post("/repo/buckets")
                .header("content-type", "application/json")
                .body(Body::from(req_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "bucket query must succeed");
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let snap: Snapshot = serde_json::from_slice(&body).unwrap();
    let tags = snap
        .tags
        .get(H1)
        .expect("h1 must appear in bucket response");
    assert!(
        tags.iter().any(|t| t.tag == "character:samus"),
        "h1 must carry character:samus: {tags:?}"
    );
    assert!(
        tags.iter().any(|t| t.tag == "maid"),
        "h1 must carry maid: {tags:?}"
    );
    // h2 lives in the "33" prefix, NOT in the "11" 8-bit bucket.
    assert!(
        !snap.tags.contains_key(H2),
        "h2 must NOT appear in an 8-bit bucket query for prefix 0x11"
    );
}

/// Via `from_settings`: an exact sha256 query (256-bit prefix) for h1 returns
/// only h1 and its tags.
///
/// Exercises: from_settings → buckets_handler → sidecar exact-hash path
/// (256-bit = no upper bound, single row).
#[tokio::test]
async fn sidecar_exact_hash_query_returns_single_hash() {
    let dir = tempfile::tempdir().unwrap();
    let sc_path = seeded_sidecar(dir.path());
    let router = sidecar_router_via_settings(&sc_path);

    let req_body = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "prefix_bits": 256,
        "buckets": [H1],
        "domain": "sha256",
    });
    let resp = router
        .oneshot(
            Request::post("/repo/buckets")
                .header("content-type", "application/json")
                .body(Body::from(req_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "exact query must succeed");
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let snap: Snapshot = serde_json::from_slice(&body).unwrap();
    assert_eq!(snap.tags.len(), 1, "exact query must return exactly 1 hash");
    let tags = snap.tags.get(H1).expect("h1 must appear in exact query");
    assert!(
        tags.iter().any(|t| t.tag == "character:samus"),
        "exact query must carry character:samus: {tags:?}"
    );
    // h2 must not appear (different hash entirely).
    assert!(
        !snap.tags.contains_key(H2),
        "h2 must not appear in h1 exact query"
    );
}

// ── 413 response budget ───────────────────────────────────────────────────────

/// A bucket query that exceeds the per-request response budget returns 413.
/// Budget = 1 byte forces a BudgetExceeded on the first (hash, tag) row.
///
/// Uses a hand-built `DomainConfig` (not `from_settings`) because the raw
/// budget is injected via `app_domains_budget` — a test-only knob that the
/// settings layer does not expose.
#[tokio::test]
async fn sidecar_budget_exceeded_returns_413() {
    let dir = tempfile::tempdir().unwrap();
    let sc_path = seeded_sidecar(dir.path());
    // Budget of 1 byte: any non-empty bucket will exceed it.
    let router = sidecar_router_with_budget(&sc_path, 1);

    let req_body = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "prefix_bits": 8,
        "buckets": [H1],
        "domain": "sha256",
    });
    let resp = router
        .oneshot(
            Request::post("/repo/buckets")
                .header("content-type", "application/json")
                .body(Body::from(req_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "a budget-1 query must return 413"
    );
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    // The server emits a remedy hint so clients know how to proceed.
    assert!(
        body_str.contains("prefix_bits"),
        "413 body must contain remedy hint: {body_str}"
    );
}
