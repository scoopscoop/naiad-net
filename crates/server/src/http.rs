//! The repository's axum surface: anonymous reads (`GET /repo/snapshot`,
//! `GET /repo/caps`, `POST /repo/buckets`, `GET /repo/relations`) and
//! authenticated writes/reads (`POST /repo/submit`, `POST /repo/report`,
//! `GET /repo/reports`, `POST /repo/moderate`). There is deliberately **no**
//! loopback Host-guard — a repo is meant to be publicly reachable.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use naiad_core::LockRecover;
use naiad_netproto::{
    BucketRequest, Caps, DeltaMapping, DomainError, DomainParam, HDR_AUTH_KEY, HDR_AUTH_SIG,
    HDR_AUTH_TS, HINT_SHIFT_CLAMP, HashDomain, MappingDelta, ModerateAction, OriginTag,
    PROTOCOL_VERSION, REPO_BUCKETS, REPO_CAPS, REPO_HEALTH, REPO_MODERATE, REPO_RELATIONS,
    REPO_RELATIONS_SUBMIT, REPO_REPORT, REPO_REPORTS, REPO_SNAPSHOT, REPO_SUBMIT,
    RESPONSE_SIZE_CAP, RelationSubmission, Report, ReportList, ServeHint, Snapshot, StreamHeader,
    StreamRow, StreamTrailer, Submission, bucket_key, bucket_upper, ensure_supported,
    requested_domain, resolve_domain, verify, verify_auth, verify_relation,
};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::domain::{DomainConfig, SNAPSHOT_MIN_QUERY_BITS};

use tower_http::trace::TraceLayer;
use tracing::Level;

use crate::store::now;
use crate::{RepoStore, advise};

/// Round-robin pool of read-only store connections (#202). Selection is
/// lock-free (an atomic cursor); each returned connection is then taken with
/// `lock_recover()` for the duration of one handler's scan, so two reads that
/// land on different connections proceed in parallel under SQLite WAL.
pub(crate) struct ReadPool {
    conns: Vec<Arc<Mutex<RepoStore>>>,
    cursor: AtomicUsize,
}

impl ReadPool {
    /// Build a pool from the supplied connections. Panics in debug builds when
    /// `conns` is empty; in release the first `next()` call would divide by
    /// zero, so callers must always supply at least one connection.
    pub(crate) fn new(conns: Vec<Arc<Mutex<RepoStore>>>) -> Self {
        debug_assert!(
            !conns.is_empty(),
            "ReadPool must have at least one connection"
        );
        Self {
            conns,
            cursor: AtomicUsize::new(0),
        }
    }

    /// Return the next connection in a round-robin order. The cursor advances
    /// with `Relaxed` ordering: we only need per-call uniqueness, not
    /// cross-thread synchronisation of any payload.
    pub(crate) fn next(&self) -> Arc<Mutex<RepoStore>> {
        let i = self.cursor.fetch_add(1, Ordering::Relaxed) % self.conns.len();
        Arc::clone(&self.conns[i])
    }
}

/// EWMA smoothing for the per-domain serve-cost estimate (#173). Recent-weighted
/// but not jumpy: the most recent ~5-6 serves dominate. Tunable by measurement;
/// not load-bearing for correctness (the client corrects any misprediction at
/// runtime from observed latency).
const EWMA_ALPHA: f64 = 0.30;

/// Conservative fallback distinct-hash count used when `repo_meta` has no
/// persisted count yet. 200 M places a PTR-scale store in Bucketed mode at
/// a reasonable width without ever falsely advertising WholeRepo mode on a
/// store that has not yet completed its startup count compute.
const CAPS_FALLBACK_COUNT: u64 = 200_000_000;

/// Normalise a per-bucket serve cost measured at `bits` prefix width onto the
/// reference width `ref_bits`, on the `cost(b) ∝ 2^(-b)` curve (#178).
///
/// A bucket at `bits` covers `2^(ref_bits − bits)` reference-width buckets, so
/// its per-reference-bucket cost is `sample × 2^(bits − ref_bits)`. The signed
/// shift is clamped to `±HINT_SHIFT_CLAMP` so a pathological width pair cannot
/// drive the f64 to 0 or ∞; realistic widths (8..=32 both sides) never engage it.
fn normalize_ms(sample: f64, bits: u32, ref_bits: u32) -> f64 {
    let e = (bits as i32 - ref_bits as i32).clamp(-HINT_SHIFT_CLAMP, HINT_SHIFT_CLAMP);
    sample * 2f64.powi(e)
}

/// Rolling per-domain serve-latency estimate (#173), shared across handler
/// clones. Read by `caps_handler`, written by `buckets_handler`. Lock-free.
struct ServeStats {
    blake3: AtomicU64,
    sha256: AtomicU64,
    /// The prefix width every sample is normalised to before it enters the EWMA
    /// (#178), advertised as `ServeHint.hint_bits`. `Some` only when this repo
    /// normalises — i.e. runs a snapshot backend, where the advertised width
    /// (`max_query_bits`) is a fixed precision ceiling and client queries arrive
    /// at widely varying widths. `None` on a mirror/advise repo, where the
    /// pre-#178 raw EWMA is kept and no `hint_bits` is advertised.
    ref_bits: Option<u32>,
}

impl ServeStats {
    fn new(ref_bits: Option<u32>) -> Self {
        let nan = f64::NAN.to_bits();
        Self {
            blake3: AtomicU64::new(nan),
            sha256: AtomicU64::new(nan),
            ref_bits,
        }
    }

    fn slot(&self, d: HashDomain) -> &AtomicU64 {
        match d {
            HashDomain::Blake3 => &self.blake3,
            HashDomain::Sha256 => &self.sha256,
        }
    }

    /// Fold one sample into the domain's EWMA, normalising it to `ref_bits`
    /// first (#178). `bits` is the prefix width this serve was measured at
    /// (the clamped snapshot query width, or the native `prefix_bits`). When
    /// `ref_bits` is None (a non-normalising repo) the sample is folded raw,
    /// exactly as #173. Lock-free CAS loop; a lost race retries with the
    /// fresher value. First sample seeds the average directly so one serve is
    /// enough to start advertising.
    fn record(&self, d: HashDomain, sample_ms_per_bucket: f64, bits: u32) {
        let sample = match self.ref_bits {
            Some(r) => normalize_ms(sample_ms_per_bucket, bits, r),
            None => sample_ms_per_bucket,
        };
        let slot = self.slot(d);
        let mut cur = slot.load(Ordering::Relaxed);
        loop {
            let old = f64::from_bits(cur);
            let next = if old.is_nan() {
                sample
            } else {
                EWMA_ALPHA * sample + (1.0 - EWMA_ALPHA) * old
            };
            match slot.compare_exchange_weak(
                cur,
                next.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Current estimate, or `None` if this domain has served nothing yet.
    fn hint(&self, d: HashDomain) -> Option<f64> {
        let v = f64::from_bits(self.slot(d).load(Ordering::Relaxed));
        v.is_finite().then_some(v)
    }
}

/// Shared handler state.
#[derive(Clone)]
struct RepoState {
    store: Arc<Mutex<RepoStore>>,
    /// Round-robin pool of read-only connections (#202). Falls back to a
    /// 1-element pool over the writer when no dedicated readers exist (e.g.
    /// in-memory stores used in tests).
    read_pool: Arc<ReadPool>,
    k: u64,
    /// Optional repo identity hint (plain hex pubkey) advertised in caps.
    repo_key: Option<String>,
    /// Optional human-readable operator name advertised in caps.
    name: Option<String>,
    /// Which hash domains this repo serves and how (design §1).
    domains: DomainConfig,
    /// Per-request response-size budget for POST /repo/buckets (#145).
    /// Defaults to RESPONSE_SIZE_CAP; a small value lets tests hit the boundary
    /// with a handful of rows instead of a 64 MiB fixture.
    bucket_budget: usize,
    /// Rolling per-domain serve-latency estimate (#173), shared across handler
    /// clones. Read by `caps_handler`, written by `buckets_handler`. Lock-free.
    serve_stats: Arc<ServeStats>,
    /// Serve-only mode (#202): when `true`, write endpoints return 403 and
    /// pooled read connections have `PRAGMA query_only = ON`. Reads are unaffected.
    read_only: bool,
    /// Path to the bridge sidecar db, Some only in BridgeMode::Sidecar. When set,
    /// `caps_handler` advertises the sidecar's cached distinct-hash count instead
    /// of the (empty) native count, so clients see the real repo size (#236 parity).
    sidecar_count_path: Option<Arc<std::path::PathBuf>>,
}

impl RepoState {
    /// Borrow the next read connection from the pool (round-robin).
    fn reader(&self) -> Arc<Mutex<RepoStore>> {
        self.read_pool.next()
    }

    /// Resolve a raw `domain=` value against the domains this node serves.
    ///
    /// Returns the effective [`HashDomain`] on success, or a ready-to-return
    /// 400 [`Response`] on failure (unrecognised or unserved domain). Factors
    /// out the identical three-line pattern that `snapshot_handler`,
    /// `buckets_handler`, and `submit_handler` would otherwise copy-paste.
    // `Response` is 128 bytes, larger than clippy's threshold for `Err`-variants.
    // The caller always immediately returns it, so there is no boxing benefit.
    #[allow(clippy::result_large_err)]
    fn resolve_request_domain(&self, raw: Option<&str>) -> Result<HashDomain, Response> {
        let served = self.domains.served();
        resolve_domain(raw, &served, self.domains.native).map_err(|e| domain_rejection(&e))
    }
}

/// Build the repository router over a shared store with crowd floor `k`.
/// Uses `HashDomain::Blake3` (the native naiad default) internally.
pub fn app(store: Arc<Mutex<RepoStore>>, k: u64) -> Router {
    app_split(store, None, k, None, None, HashDomain::Blake3)
}

/// Like [`app`], but with an explicit per-request bucket response budget (#145,
/// #176). Tests use this to force streaming continuations (`{"more":…}`) with a
/// tiny budget rather than a 64-MiB dataset.
pub fn app_with_bucket_budget(store: Arc<Mutex<RepoStore>>, k: u64, budget: usize) -> Router {
    app_domains_budget(
        store,
        None,
        k,
        None,
        None,
        DomainConfig::native_only(HashDomain::Blake3),
        budget,
        false,
    )
}

/// Like [`app`], but with a dedicated read-only store, an optional repo-key
/// hint, and an explicit **native** hash domain and no added domains. Kept at
/// this signature so existing embedders and tests are unaffected by the
/// dual-domain work; reach for [`app_domains`] to add a SHA-256 domain.
pub fn app_split(
    store: Arc<Mutex<RepoStore>>,
    read_store: Option<Arc<Mutex<RepoStore>>>,
    k: u64,
    repo_key: Option<String>,
    name: Option<String>,
    hash_domain: HashDomain,
) -> Router {
    app_domains(
        store,
        read_store,
        k,
        repo_key,
        name,
        DomainConfig::native_only(hash_domain),
    )
}

/// Like [`app_split`], but with a full [`DomainConfig`] — the entry point a
/// bridge-enabled `serve` uses, since it may serve an added SHA-256 domain.
pub fn app_domains(
    store: Arc<Mutex<RepoStore>>,
    read_store: Option<Arc<Mutex<RepoStore>>>,
    k: u64,
    repo_key: Option<String>,
    name: Option<String>,
    domains: DomainConfig,
) -> Router {
    app_domains_budget(
        store,
        read_store,
        k,
        repo_key,
        name,
        domains,
        RESPONSE_SIZE_CAP,
        false,
    )
}

/// Like [`app_domains`], but with an explicit per-request bucket response budget
/// (#145). Production callers use [`app_domains`], which passes
/// [`RESPONSE_SIZE_CAP`]; integration tests call this directly to inject a tiny
/// budget (e.g. the 413 path in `crates/server/tests/sidecar_serve_e2e.rs`)
/// without a 64-MiB dataset — which is why this function is `pub` rather than
/// `pub(crate)`.
///
/// Internally wraps `read_store` (or falls back to the writer) in a 1-element
/// [`ReadPool`]. Use [`app_domains_with_pool`] when the caller has already
/// built a multi-connection pool.
///
/// `read_only`: when `true`, write handlers return 403 (#202).
// Positional builder mirrors the repo_key plumbing; a params struct would churn every call site.
#[allow(clippy::too_many_arguments)]
pub fn app_domains_budget(
    store: Arc<Mutex<RepoStore>>,
    read_store: Option<Arc<Mutex<RepoStore>>>,
    k: u64,
    repo_key: Option<String>,
    name: Option<String>,
    domains: DomainConfig,
    bucket_budget: usize,
    read_only: bool,
) -> Router {
    let pool = Arc::new(ReadPool::new(vec![
        read_store.unwrap_or_else(|| Arc::clone(&store)),
    ]));
    build_app(
        store,
        pool,
        k,
        repo_key,
        name,
        domains,
        bucket_budget,
        read_only,
        None,
        None,
    )
}

/// Like [`app_domains`], but accepts a pre-built [`ReadPool`] (#202).
///
/// Called by [`crate::serve_with_shutdown_domains`] which wires the full
/// N-connection pool built from `[serve].read_connections`. The single-reader
/// public API (`app`, `app_split`, `app_domains`, `app_domains_budget`) builds
/// a 1-element pool internally and keeps its existing signature unchanged.
///
/// `read_only`: when `true`, write handlers return 403 (#202).
#[allow(clippy::too_many_arguments)]
pub(crate) fn app_domains_with_pool(
    store: Arc<Mutex<RepoStore>>,
    read_pool: Arc<ReadPool>,
    k: u64,
    repo_key: Option<String>,
    name: Option<String>,
    domains: DomainConfig,
    read_only: bool,
    stats_layer: Option<crate::stats::middleware::StatsLayer>,
    sidecar_count_path: Option<Arc<std::path::PathBuf>>,
) -> Router {
    build_app(
        store,
        read_pool,
        k,
        repo_key,
        name,
        domains,
        RESPONSE_SIZE_CAP,
        read_only,
        stats_layer,
        sidecar_count_path,
    )
}

/// Shared router-building core. All public `app_*` entry points converge here;
/// the only variation is how `read_pool` was constructed.
// Positional builder mirrors the repo_key plumbing; a params struct would churn every call site.
#[allow(clippy::too_many_arguments)]
fn build_app(
    store: Arc<Mutex<RepoStore>>,
    read_pool: Arc<ReadPool>,
    k: u64,
    repo_key: Option<String>,
    name: Option<String>,
    domains: DomainConfig,
    bucket_budget: usize,
    read_only: bool,
    stats_layer: Option<crate::stats::middleware::StatsLayer>,
    sidecar_count_path: Option<Arc<std::path::PathBuf>>,
) -> Router {
    let ref_bits = domains
        .added_sha256
        .as_ref()
        .map(|_| domains.max_query_bits);
    let state = RepoState {
        store,
        read_pool,
        k,
        repo_key,
        name,
        domains,
        bucket_budget,
        serve_stats: Arc::new(ServeStats::new(ref_bits)),
        read_only,
        sidecar_count_path,
    };
    // Submissions and reports are small JSON; cap at 64 KB to avoid surprising
    // memory growth from oversized payloads (default axum limit is 2 MB).
    let router = Router::new()
        // Unauthenticated liveness probe — does NOT touch the store/DB.
        .route(REPO_HEALTH, get(health_handler))
        .route(REPO_SNAPSHOT, get(snapshot_handler))
        .route(REPO_CAPS, get(caps_handler))
        .route(REPO_BUCKETS, post(buckets_handler))
        .route(REPO_RELATIONS_SUBMIT, post(relations_submit_handler))
        .route(REPO_RELATIONS, get(relations_handler))
        .route(REPO_SUBMIT, post(submit_handler))
        .route(REPO_REPORT, post(report_handler))
        .route(REPO_REPORTS, get(reports_handler))
        .route(REPO_MODERATE, post(moderate_handler))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<axum::body::Body>| {
                    tracing::span!(
                        target: "http",
                        Level::TRACE,
                        "http-request",
                        method = %req.method(),
                        path = %req.uri().path(),
                    )
                })
                .on_response(
                    |res: &axum::http::Response<axum::body::Body>,
                     latency: std::time::Duration,
                     _span: &tracing::Span| {
                        tracing::event!(
                            target: "http",
                            Level::TRACE,
                            status = res.status().as_u16(),
                            latency_ms = latency.as_millis() as u64,
                            "http-response",
                        );
                    },
                ),
        )
        // Compress response bodies (gzip/zstd) for clients that advertise it via
        // Accept-Encoding. Halves the wire cost of the full first pull every
        // subscription still pays after migration 0035 clears its marker, and of
        // every snapshot-mode pull (which never becomes incremental). Transport
        // concern below the delta layer — cursors and merge semantics are
        // unchanged whether or not a body arrived compressed (#108).
        // Quality Precise(3): on the warm 1.5 MB /repo/buckets path this ~halves
        // gzip CPU (~47→28 ms) for +9% body size, while zstd's Precise(3) is its
        // own default level so zstd output is unchanged. The naiad client is
        // gzip-only (ureq, workspace Cargo.toml), so this tunes the sync hot path.
        .layer(
            tower_http::compression::CompressionLayer::new()
                .quality(tower_http::compression::CompressionLevel::Precise(3)),
        )
        .with_state(state);
    #[cfg(test)]
    let router = router.layer(axum::extract::connect_info::MockConnectInfo(
        SocketAddr::from(([127, 0, 0, 1], 0)),
    ));
    // Apply the stats layer outermost so it observes every response — including
    // those short-circuited by DefaultBodyLimit, compression, and 4xx guards —
    // after compression has set the final Content-Length (or omitted it for
    // streaming). Use Router::layer, NOT route_layer, so the 404 fallback path
    // is also covered.
    if let Some(sl) = stats_layer {
        router.layer(sl)
    } else {
        router
    }
}

// ── Health probe ─────────────────────────────────────────────────────────────

/// Response body for `GET /health`.
#[derive(serde::Serialize)]
struct HealthResponse {
    /// Always `"ok"`.  A non-200 response implies the process is unhealthy.
    status: &'static str,
    /// Semver build version of the running `naiad-repo` binary.
    server_version: &'static str,
}

/// `GET /health` — static liveness probe.
///
/// Unauthenticated; does **not** touch the store or database. Suitable as the
/// target for Docker HEALTHCHECK, Kubernetes liveness probes, or any external
/// monitor. Returns 200 with a JSON body containing the build version.
async fn health_handler() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        server_version: env!("CARGO_PKG_VERSION"),
    })
}

// ── Shared error helpers ──────────────────────────────────────────────────────

/// Returns `true` when `e` represents a banned-account rejection from the store.
///
/// The two store bail strings this must match (grep these to find the coupling):
///   - `store.rs apply_submission`:  "account {} is banned"
///   - `store.rs insert_report`:     "banned account cannot file reports"
///
/// Using message text is pragmatic; a typed error enum on the store would be
/// cleaner but is deferred.
fn is_banned_err(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("is banned") || msg.contains("banned account")
}

/// Turn a [`DomainError`] into an explicit 400. The recurring bug in the old
/// bridge was silence that looked like success, so every unserved or
/// unrecognised `domain=` is an attributable failure naming what this repo
/// does serve — **never** an empty 200 (spec §6 rows 1-2).
fn domain_rejection(e: &DomainError) -> Response {
    tracing::warn!(target: "http", error = %e, "rejected a domain-discriminated request");
    (StatusCode::BAD_REQUEST, e.to_string()).into_response()
}

// ── Authentication helper ─────────────────────────────────────────────────────

/// Extract and verify the `x-naiad-key` / `x-naiad-ts` / `x-naiad-sig`
/// request-auth headers.  Returns the signer's pubkey hex on success.
///
/// All four authenticated endpoints call exactly this function — there is no
/// other call site of [`verify_auth`] in this module.
///
/// `domain` is the *requested* domain — [`requested_domain`] applied to this
/// request's own `?domain=`, `None` on the routes that take no such parameter.
/// It must not be the resolved domain: the signer bound what it put on the wire,
/// so that is what has to be reconstructed here. Passing the resolved value
/// would make every unqualified request verify as if it had asked for the native
/// domain, which is precisely the aliasing #161 closes.
///
/// # Errors
/// Returns a `Response` with status 401 and a plain-text reason on any failure:
/// missing headers, unparseable timestamp, stale timestamp, or bad signature.
#[allow(clippy::result_large_err)]
fn authenticate(
    headers: &HeaderMap,
    method: &str,
    path: &str,
    domain: Option<HashDomain>,
    body: &[u8],
    now_ts: i64,
) -> Result<String, Response> {
    let key = headers
        .get(HDR_AUTH_KEY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ts_str = headers
        .get(HDR_AUTH_TS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let sig = headers
        .get(HDR_AUTH_SIG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let ts: i64 = ts_str.parse().map_err(|_| {
        tracing::warn!(target: "http", key = %key, "auth failed: timestamp not an integer");
        (
            StatusCode::UNAUTHORIZED,
            format!("{HDR_AUTH_TS}: not a valid integer"),
        )
            .into_response()
    })?;

    verify_auth(key, sig, method, path, domain, ts, now_ts, body).map_err(|e| {
        tracing::warn!(target: "http", key = %key, error = %e, "auth failed");
        (StatusCode::UNAUTHORIZED, format!("auth failed: {e:#}")).into_response()
    })?;

    Ok(key.to_string())
}

// ── Anonymous read handlers ───────────────────────────────────────────────────

/// `GET /repo/snapshot` — the whole `hash → tags` set of one domain.
async fn snapshot_handler(
    State(st): State<RepoState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(q): Query<DomainParam>,
) -> Response {
    let started = std::time::Instant::now();
    let domain = match st.resolve_request_domain(q.domain.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if domain != st.domains.native {
        // The only non-native domain in phase 1 is snapshot-mode sha256, and a
        // whole-repo dump of a PTR snapshot is hundreds of GB of JSON. Refuse
        // explicitly rather than start streaming it.
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "whole-repo snapshot is not available for the {domain} domain in snapshot \
                 mode; query POST /repo/buckets with domain={domain} instead"
            ),
        )
            .into_response();
    }
    let reader = st.reader();
    let out = tokio::task::spawn_blocking(move || {
        let store = reader.lock_recover();
        store.read_snapshot(|store| {
            Ok::<_, anyhow::Error>(Snapshot {
                version: PROTOCOL_VERSION,
                cursor: store.mapping_cursor()?,
                tags: store.snapshot()?,
            })
        })
    })
    .await;
    match out {
        Ok(Ok(snapshot)) => {
            let hashes = snapshot.tags.len();
            let tags: usize = snapshot.tags.values().map(Vec::len).sum();
            match served_json(&snapshot) {
                Ok((resp, bytes)) => {
                    tracing::debug!(target: "http", client = %peer(&addr), domain = %domain, hashes, tags, bytes, elapsed_ms = started.elapsed().as_millis() as u64, "served snapshot");
                    resp
                }
                Err(r) => r,
            }
        }
        Ok(Err(e)) => {
            tracing::error!(target: "http", error = %e, "snapshot: store error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(e) => {
            tracing::error!(target: "http", error = %e, "snapshot: task join error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /repo/caps` — advertise the auto-sized prefix length, the served hash
/// domains and the feature set.
async fn caps_handler(
    State(st): State<RepoState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let started = std::time::Instant::now();
    let k = st.k;
    let repo_key = st.repo_key.clone();
    let native = st.domains.native;
    let served = st.domains.served();
    // A repo that serves exactly its native domain emits no `hash_domains` at
    // all (the field is skipped when empty), so pre-dual-domain clients see
    // byte-identical caps to today's.
    let hash_domains = if served.len() > 1 { served } else { Vec::new() };
    // Domains for which we serve incremental deltas: the store-backed ones. The
    // native domain is always store-backed; sha256 is store-backed ONLY in
    // mirror mode (no snapshot backend). A snapshot sha256 backend has no
    // sequence numbers, so it is EXCLUDED — that is what makes the client omit
    // `since` for it and never trip the snapshot-mode `since` 400 below.
    let incremental_domains: Vec<String> = st
        .domains
        .served()
        .into_iter()
        .filter(|d| !(st.domains.added_sha256.is_some() && *d == HashDomain::Sha256))
        .map(|d| d.as_str().to_string())
        .collect();
    // With a snapshot backend the advertised prefix width is the server's
    // precision ceiling rather than the k-anonymity advice, which is what makes
    // exact-hash queries possible with no seed. This raises the width for the
    // native domain too; that is sound only because snapshot mode is a
    // trusted-operator deployment (design §Backend 1, "Precision"). An operator
    // who wants k-anon coarseness sets `[bridge].max_query_bits` to it.
    // `from_settings` guarantees SNAPSHOT_MIN_QUERY_BITS ≤ max_query_bits ≤ 256
    // when a snapshot backend is configured; the former .clamp(1, 256) is dead.
    let snapshot_bits = st.domains.added_sha256.as_ref().map(|_| {
        debug_assert!(
            (SNAPSHOT_MIN_QUERY_BITS..=256).contains(&st.domains.max_query_bits),
            "max_query_bits out of range — from_settings must enforce [{SNAPSHOT_MIN_QUERY_BITS}, 256]"
        );
        st.domains.max_query_bits
    });
    // #195: advertise min_query_bits whenever sha256 is served — whether sha256
    // is the native domain (mirror mode) or an added domain (snapshot mode).
    // Blake3-only repos emit None; the field stays absent from their wire caps
    // and clients that pre-date #179/#195 see a byte-identical response.
    let snapshot_floor =
        if st.domains.native == HashDomain::Sha256 || st.domains.added_sha256.is_some() {
            Some(st.domains.min_query_bits)
        } else {
            None
        };
    // Read the persisted distinct-hash count and store_generation in a single
    // reader checkout — one round-robin advance, one mutex lock/unlock.
    // count_opt: the real persisted count, or None when no row exists yet or
    // on error (we advertise None so the client does not display a crowd
    // number that was never measured).
    // count: the u64 used ONLY for advise() and the mode-floor logic; falls
    // back to CAPS_FALLBACK_COUNT so the mode decision is conservative.
    let (count_opt, store_generation) = {
        let reader = st.reader();
        let guard = reader.lock_recover();
        let count_opt = match guard.read_distinct_hash_count() {
            Ok(Some(n)) => Some(n),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(target: "repo", err = %e, "caps: read_distinct_hash_count failed; advertising None, using fallback for advise");
                None
            }
        };
        // #194: fetch the store-generation id — brief synchronous lock,
        // no I/O inside, never held across an await.
        let store_generation = guard.store_generation().unwrap_or_else(|e| {
            tracing::warn!(target: "repo", err = %e, "caps: failed to read store_generation; advertising None");
            None
        });
        (count_opt, store_generation)
    };
    // Internal use only: advise() and the snapshot-floor lift need a u64.
    let count = count_opt.unwrap_or(CAPS_FALLBACK_COUNT);
    // Sidecar nodes have an empty native store; source the advertised count from
    // the sidecar's cached distinct-hash count when available (#236 parity).
    let wire_count = if let Some(sc_path) = st.sidecar_count_path.clone() {
        let native = count_opt;
        tokio::task::spawn_blocking(move || {
            match crate::bridge::sidecar::Sidecar::open_readonly(sc_path.as_ref().as_path()) {
                Ok(sc) => match sc.cached_bridge_counts() {
                    Ok(Some((hashes, _, _))) => Some(hashes),
                    Ok(None) => native,
                    Err(e) => {
                        tracing::warn!(target: "repo", err = %e, "caps: sidecar cached_bridge_counts failed; using native count");
                        native
                    }
                },
                Err(e) => {
                    tracing::warn!(target: "repo", err = %e, "caps: sidecar open_readonly failed; using native count");
                    native
                }
            }
        })
        .await
        .unwrap_or(count_opt)
    } else {
        count_opt
    };
    let caps = Caps {
        version: PROTOCOL_VERSION,
        mode: match snapshot_bits {
            Some(prefix_bits) => naiad_netproto::PullMode::Bucketed { prefix_bits },
            // Mirror/native path: advise(count, k) gives the k-anon recommendation.
            // #195: when the native domain is sha256, the floor applies to bucketed
            // queries, so the advertised mode must never fall below the floor —
            // otherwise a client that follows the advice sends a below-floor request
            // and receives a 400. Lift Bucketed{bits < floor} to Bucketed{floor}.
            // WholeRepo is left untouched (count < k → /repo/snapshot path, no
            // prefix_bits involved, the floor never fires).
            // Blake3-native repos have no floor and must keep raw advise() output.
            None => match advise(count, k) {
                naiad_netproto::PullMode::Bucketed { prefix_bits }
                    if st.domains.native == HashDomain::Sha256
                        && prefix_bits < st.domains.min_query_bits =>
                {
                    naiad_netproto::PullMode::Bucketed {
                        prefix_bits: st.domains.min_query_bits,
                    }
                }
                other => other,
            },
        },
        relation_incremental: true,
        mapping_incremental: true,
        reports: true,
        repo_key,
        hash_domain: native,
        hash_domains,
        incremental_domains: Some(incremental_domains),
        server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        serve_hint: st
            .domains
            .served()
            .into_iter()
            .filter_map(|d| {
                st.serve_stats.hint(d).map(|ms| {
                    (
                        d.as_str().to_string(),
                        ServeHint {
                            ms_per_bucket: ms,
                            hint_bits: st.serve_stats.ref_bits,
                        },
                    )
                })
            })
            .collect(),
        // #176: this server streams POST /repo/buckets responses (NDJSON);
        // clients opt in per-request. Omitted (false) on non-streaming
        // servers via skip_serializing_if so old caps stay byte-identical.
        streaming: true,
        // #179: advertise the snapshot floor exactly when we advertise
        // the snapshot ceiling: both describe the non-native (snapshot)
        // domain, and the server enforces the floor only when a snapshot
        // backend exists (http.rs:855-881).
        min_query_bits: snapshot_floor,
        // Advertise the distinct-hash count so clients can translate a
        // desired k-anonymity crowd into a bucket width without guessing
        // repo size. Advisory only — never a contract. None when no real
        // count row exists yet (avoids overstating crowd during pre-compute
        // window); the internal advise() still uses the fallback u64.
        // On sidecar nodes this is sourced from the sidecar's cached count
        // rather than the empty native store (#236 parity).
        count: wire_count,
        // #194: absent (None) when the store has never been seeded or
        // predates this feature; clients fall back to the
        // backwards-cursor guard in that case.
        store_generation,
        name: st.name.clone(),
    };
    tracing::debug!(target: "http", client = %peer(&addr), mode = ?caps.mode, elapsed_ms = started.elapsed().as_millis() as u64, "served caps");
    Json(caps).into_response()
}

/// The static remedy body returned with 413 when a bucket response exceeds the
/// server's per-request size budget (#145). It is entirely self-authored: it
/// names no filesystem path, no SQL, no snapshot directory, so it is safe to
/// surface directly (#159) without the log-and-hide dance the 500 path uses.
const BUCKET_BUDGET_REMEDY: &str = "bucket response exceeded the server's \
per-request size budget; request narrower buckets by raising prefix_bits, or \
send fewer keys per request";

/// True if any link in the anyhow chain is a [`naiad_core::BudgetExceeded`]
/// (#145). The snapshot branch wraps it under `.with_context` path detail, so
/// inspect the whole chain rather than just the outermost error; the native
/// branch may return it un-wrapped, and `chain()` finds it either way.
fn is_budget_exceeded(err: &anyhow::Error) -> bool {
    err.chain().any(|c| c.is::<naiad_core::BudgetExceeded>())
}

/// 413 Payload Too Large with the static remedy body.
fn budget_exceeded_response() -> Response {
    (StatusCode::PAYLOAD_TOO_LARGE, BUCKET_BUDGET_REMEDY).into_response()
}

/// The one place the client address enters a log line. Deleting this function
/// and its three call-site `client` fields (plus the three `ConnectInfo`
/// extractor args) removes IP logging entirely (#172: a local debug aid, not a
/// retained record). Returns only the IP, never the ephemeral source port.
fn peer(addr: &SocketAddr) -> impl std::fmt::Display {
    addr.ip()
}

/// Serialize `v` once, returning the JSON body as a response plus its byte
/// length for the `debug` served-line — so the handler can log the exact wire
/// size without serializing twice. On failure, logs and yields a bare 500
/// (matching the existing 500-on-error posture, #159).
// Response is 128 bytes; the caller always immediately returns it on Err, so
// there is no boxing benefit.
#[allow(clippy::result_large_err)]
fn served_json<T: serde::Serialize>(v: &T) -> Result<(Response, usize), Response> {
    match serde_json::to_vec(v) {
        Ok(b) => {
            let len = b.len();
            Ok((
                ([(axum::http::header::CONTENT_TYPE, "application/json")], b).into_response(),
                len,
            ))
        }
        Err(e) => {
            tracing::error!(target: "http", error = %e, "serialize error");
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

/// `POST /repo/buckets` — the union of the requested hash-prefix ranges.
enum BucketsResponse {
    Snapshot(Snapshot),
    Delta(MappingDelta),
}

// ── Streaming helpers (#176) ──────────────────────────────────────────────────

/// Serialize `v` as a single NDJSON line (one JSON object followed by `\n`)
/// and return it as a `Bytes` chunk ready to send into the stream channel.
/// Returns `None` (and logs) if serialization fails; a corrupted frame would
/// break the client's line parser, so we treat it as a producer error.
fn ndjson_line<T: serde::Serialize>(v: &T) -> Option<Bytes> {
    match serde_json::to_vec(v) {
        Ok(mut b) => {
            b.push(b'\n');
            Some(Bytes::from(b))
        }
        Err(e) => {
            tracing::error!(target: "http", error = %e, "ndjson serialize error");
            None
        }
    }
}

/// Build the NDJSON streaming response body for the **added-sha256-domain** branch
/// (`domain != native`, answered from a [`crate::domain::Sha256Backend`]). Called by
/// `buckets_handler` when `req.stream` is set. Emits header → rows → trailer
/// through a bounded `mpsc(16)` channel; `spawn_blocking` drives the producer
/// so the blocking SQLite scan does not block the async executor.
///
/// Budget semantics match §3.4 of the #176 spec:
/// - `BudgetExceeded` on the first bucket → `{"err":…}` trailer (no path leak).
/// - `BudgetExceeded` on a later bucket → stop, emit `{"more":"<key>"}`.
/// - Clean finish → `{"done":true}`.
fn stream_snapshot_domain(
    masked_buckets: Vec<String>,
    resume_at: Option<String>,
    budget: usize,
    backend: Arc<dyn crate::domain::Sha256Backend>,
    bits: u32,
    serve_stats: Arc<ServeStats>,
    domain: HashDomain,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(16);
    tokio::task::spawn_blocking(move || {
        // Apply resume_at: skip keys that come before the cursor.
        let mut buckets = masked_buckets;
        if let Some(ref key) = resume_at {
            buckets.retain(|b| b >= key);
        }

        // Emit header (cursor = 0 for a static snapshot).
        let header = StreamHeader {
            version: PROTOCOL_VERSION,
            cursor: 0,
        };
        let Some(bytes) = ndjson_line(&header) else {
            return;
        };
        if tx.blocking_send(Ok(bytes)).is_err() {
            return;
        }

        let mut remaining = budget;
        let mut served_any = false;

        // #178 §4.4: accumulate ONLY the backend.bucket() call durations so that
        // channel backpressure (bounded channel of 16 + slow client draining the
        // NDJSON body) does not inflate the sample. The wall-clock wrapper would
        // include tx.blocking_send stalls; the accumulator does not.
        let mut scan_time = std::time::Duration::ZERO;
        // Count of buckets that returned Ok — used as the denominator so the
        // More-path sample divides by the actually-served prefix, not the full
        // window (full window would undercount per-bucket cost on truncated serves).
        let mut processed_buckets: usize = 0;

        for b in &buckets {
            let t = std::time::Instant::now();
            let result = backend.bucket(b, bits, remaining);
            scan_time += t.elapsed();

            match result {
                Ok((part, spent)) => {
                    remaining = remaining.saturating_sub(spent);
                    served_any = true;
                    processed_buckets += 1;
                    for (hash, tag_strings) in part {
                        let row = StreamRow {
                            h: hash,
                            t: tag_strings
                                .into_iter()
                                .map(|tag| OriginTag { tag, origin: None })
                                .collect(),
                        };
                        let Some(bytes) = ndjson_line(&row) else {
                            continue;
                        };
                        if tx.blocking_send(Ok(bytes)).is_err() {
                            return;
                        }
                    }
                }
                Err(e) if is_budget_exceeded(&e) => {
                    let trailer = if !served_any {
                        // First bucket alone exceeds one full budget: cannot split
                        // without lowering prefix_bits. Emit in-band error (#159:
                        // no filesystem path in the message).
                        StreamTrailer::Err {
                            err: format!(
                                "bucket {b} at {bits} bits exceeds the per-request budget; \
                                 raise prefix_bits to query narrower buckets"
                            ),
                        }
                    } else {
                        // Budget exhausted after serving ≥1 bucket. This bucket is
                        // the continuation cursor. Divide by processed_buckets (the
                        // actual served prefix) so the sample reflects real per-bucket
                        // cost rather than an underestimate from using the full window.
                        // Use fractional ms so a sub-ms scan does not truncate to 0.
                        serve_stats.record(
                            domain,
                            scan_time.as_secs_f64() * 1000.0 / processed_buckets as f64,
                            bits,
                        );
                        StreamTrailer::More { more: b.clone() }
                    };
                    if let Some(bytes) = ndjson_line(&trailer) {
                        let _ = tx.blocking_send(Ok(bytes));
                    }
                    return;
                }
                Err(e) => {
                    // Mid-scan store error: log server-side (may contain snapshot
                    // path), surface only a generic message to the client (#159).
                    tracing::error!(target: "http", error = %e, "buckets: snapshot backend error during stream");
                    let trailer = StreamTrailer::Err {
                        err: "internal server error".to_string(),
                    };
                    if let Some(bytes) = ndjson_line(&trailer) {
                        let _ = tx.blocking_send(Ok(bytes));
                    }
                    return;
                }
            }
        }

        // Clean finish — record the sample (#178 §4.4) if we served ≥1 bucket.
        // Fractional ms (as_secs_f64 × 1000) so a sub-ms in-memory scan does not
        // truncate to 0 and fold a zero into the EWMA.
        if served_any && processed_buckets > 0 {
            serve_stats.record(
                domain,
                scan_time.as_secs_f64() * 1000.0 / processed_buckets as f64,
                bits,
            );
        }
        if let Some(bytes) = ndjson_line(&StreamTrailer::Done { done: true }) {
            let _ = tx.blocking_send(Ok(bytes));
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("static streaming response builder is valid")
}

/// Build the NDJSON streaming response body for the **native `since = None`**
/// branch (`domain == native`, full snapshot). Analogous to
/// [`stream_snapshot_domain`] but reads from [`RepoStore`] and uses the store's
/// `mapping_cursor()` for the header.
///
/// Native = scan-then-stream per budget window, lock held only for the scan;
/// snapshot-domain = fully incremental (no mutex).
fn stream_native_snapshot(
    masked_buckets: Vec<String>,
    resume_at: Option<String>,
    budget: usize,
    reader: Arc<Mutex<RepoStore>>,
    prefix_bits: u32,
    serve_stats: Arc<ServeStats>,
    domain: HashDomain,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(16);
    tokio::task::spawn_blocking(move || {
        // --- Under the lock: scan and buffer all rows into Vec<Bytes> ---
        let store = reader.lock_recover();
        // Clone tx for header send within read_snapshot closure.
        let tx2 = tx.clone();
        // #178 §4.4: the closure returns (buffered_bytes, served_any,
        // processed_buckets, scan_ms_f64) so we can record a normalised sample
        // after releasing the lock. `processed_buckets` is the count of buckets
        // that returned Ok — the actual served prefix — used as the denominator.
        // `scan_ms_f64` is fractional milliseconds (as_secs_f64 × 1000) so a
        // sub-ms in-memory scan does not truncate to 0 and fold a zero sample.
        let scan_result: anyhow::Result<(Vec<Bytes>, bool, usize, f64)> =
            store.read_snapshot(move |store| {
                // Cursor read once before streaming starts (§3.2).
                let cursor = store.mapping_cursor()?;
                let header = StreamHeader { version: PROTOCOL_VERSION, cursor };
                let Some(header_bytes) = ndjson_line(&header) else {
                    return Ok((vec![], false, 0, 0.0));
                };
                // Send header immediately (fast — just the cursor value).
                if tx2.blocking_send(Ok(header_bytes)).is_err() {
                    return Ok((vec![], false, 0, 0.0));
                }

                // Apply resume_at: skip keys before the cursor.
                let mut buckets = masked_buckets;
                if let Some(ref key) = resume_at {
                    buckets.retain(|b| b >= key);
                }

                // #178 §4.4: time the scan loop under the store lock so we
                // measure server-side scan cost, not channel-drain time. The
                // native arm buffers all rows before releasing the lock, so
                // there is no channel-send backpressure inside this timing window.
                let scan_started = std::time::Instant::now();

                let mut remaining = budget;
                let mut served_any = false;
                let mut processed_buckets: usize = 0;
                let mut buffered: Vec<Bytes> = Vec::new();

                for b in &buckets {
                    let Ok(lo) = b.parse::<naiad_core::Hash>() else { continue; };
                    let lo_key = bucket_key(&lo, prefix_bits);
                    let hi = bucket_upper(&lo, prefix_bits);
                    match store.bucket(&lo_key, &hi, remaining) {
                        Ok((part, spent)) => {
                            remaining = remaining.saturating_sub(spent);
                            served_any = true;
                            processed_buckets += 1;
                            for (hash, tags) in part {
                                let row = StreamRow { h: hash, t: tags };
                                if let Some(bytes) = ndjson_line(&row) {
                                    buffered.push(bytes);
                                }
                            }
                        }
                        Err(e) if is_budget_exceeded(&e) => {
                            let scan_ms_f64 = scan_started.elapsed().as_secs_f64() * 1000.0;
                            let trailer = if !served_any {
                                StreamTrailer::Err {
                                    err: format!(
                                        "bucket {b} at {prefix_bits} bits exceeds the per-request budget; \
                                         raise prefix_bits to query narrower buckets"
                                    ),
                                }
                            } else {
                                StreamTrailer::More { more: b.clone() }
                            };
                            if let Some(bytes) = ndjson_line(&trailer) {
                                buffered.push(bytes);
                            }
                            return Ok((buffered, served_any, processed_buckets, scan_ms_f64));
                        }
                        Err(e) => {
                            let scan_ms_f64 = scan_started.elapsed().as_secs_f64() * 1000.0;
                            tracing::error!(target: "http", error = %e, "buckets: native store error during stream");
                            let trailer =
                                StreamTrailer::Err { err: "internal server error".to_string() };
                            if let Some(bytes) = ndjson_line(&trailer) {
                                buffered.push(bytes);
                            }
                            return Ok((buffered, served_any, processed_buckets, scan_ms_f64));
                        }
                    }
                }

                // Clean finish.
                let scan_ms_f64 = scan_started.elapsed().as_secs_f64() * 1000.0;
                if let Some(bytes) = ndjson_line(&StreamTrailer::Done { done: true }) {
                    buffered.push(bytes);
                }
                Ok((buffered, served_any, processed_buckets, scan_ms_f64))
            });
        // read_snapshot borrows the guard, so it lives until end of scope —
        // drop it explicitly before the client-paced sends below, or a slow
        // reader would hold the store mutex for the whole stream.
        drop(store);
        match scan_result {
            Ok((buffered, served_any, processed_buckets, scan_ms_f64)) => {
                // #178 §4.4: record the normalised sample now that the lock is
                // released. `scan_ms_f64` is fractional ms so a sub-ms scan is
                // recorded faithfully rather than being truncated to 0.
                if served_any && processed_buckets > 0 {
                    serve_stats.record(domain, scan_ms_f64 / processed_buckets as f64, prefix_bits);
                }
                for bytes in buffered {
                    if tx.blocking_send(Ok(bytes)).is_err() {
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::error!(target: "http", error = %e, "buckets: read_snapshot error during stream");
                let trailer = StreamTrailer::Err {
                    err: "internal server error".to_string(),
                };
                if let Some(bytes) = ndjson_line(&trailer) {
                    let _ = tx.blocking_send(Ok(bytes));
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("static streaming response builder is valid")
}

async fn buckets_handler(
    State(st): State<RepoState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<BucketRequest>,
) -> Response {
    let started = std::time::Instant::now();
    if let Err(e) = ensure_supported(req.version) {
        tracing::warn!(target: "http", client = %peer(&addr), version = req.version, error = %e, "bucket request rejected: unsupported protocol version");
        return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response();
    }
    if let Some(since) = &req.since {
        if since.len() != req.buckets.len() {
            tracing::warn!(target: "http", client = %peer(&addr), since = since.len(), buckets = req.buckets.len(), "bucket request rejected: since length does not match buckets length");
            return (
                StatusCode::BAD_REQUEST,
                "since length must match buckets length",
            )
                .into_response();
        }
    }
    let domain = match st.resolve_request_domain(req.domain.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };

    // #195: apply the floor to ANY sha256-domain query, regardless of whether
    // sha256 is the native domain (mirror mode) or an added domain (snapshot mode).
    // A request below the floor would trigger an unreasonably expensive scan.
    // The floor defaults to SNAPSHOT_MIN_QUERY_BITS (8) and may be raised by
    // the operator via [bridge].min_query_bits (NAIAD_REPO_BRIDGE_MIN_QUERY_BITS).
    // `from_settings` clamps min_query_bits into [SNAPSHOT_MIN_QUERY_BITS,
    // max_query_bits], so the served range is never inverted.
    // Blake3-native queries are completely unaffected — guard is strictly on sha256.
    if domain == HashDomain::Sha256 {
        let min_query_bits = st.domains.min_query_bits;
        if req.prefix_bits < min_query_bits {
            tracing::warn!(target: "http", client = %peer(&addr), domain = %domain, prefix_bits = req.prefix_bits, min_query_bits, "bucket request rejected: prefix_bits below sha256 floor");
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "sha256-domain bucket queries require at least {min_query_bits} \
                     prefix_bits (served range: {min_query_bits}..={}); \
                     got {}",
                    st.domains.max_query_bits, req.prefix_bits
                ),
            )
                .into_response();
        }
    }

    // Non-native domain: sha256 answered from the added backend (snapshot or
    // sidecar) rather than the RepoStore. `served` already guarantees the
    // backend exists, but re-check rather than unwrap.
    if domain != st.domains.native {
        let Some(backend) = st.domains.added_sha256.clone() else {
            return domain_rejection(&DomainError::NotServed {
                requested: domain,
                served: st.domains.served(), // defensive path — re-compute
            });
        };
        if req.since.is_some() {
            tracing::warn!(target: "http", client = %peer(&addr), domain = %domain, "bucket request rejected: since unsupported for snapshot-mode domain");
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "the {domain} domain has no incremental deltas in snapshot mode; \
                     omit `since` and pull the full buckets (issue #142)"
                ),
            )
                .into_response();
        }
        let bits = st.domains.clamp_query_bits(req.prefix_bits);
        // Parse each key and mask it to the effective prefix width BEFORE sort/dedup
        // (#153). Without masking, distinct full-hash keys that share a `bits`-wide
        // prefix survive dedup as different 64-char strings, each triggering an
        // identical range scan through the Mutex — dedup does nothing precisely when
        // `clamp_query_bits` bites. With masking the deduped list is exactly the
        // set of distinct buckets actually scanned.
        //
        // A malformed key is a client error (#159): return 400 naming the key so
        // the caller can fix its request. The body must not leak server paths.
        let mut masked_buckets: Vec<String> = Vec::with_capacity(req.buckets.len());
        for b in &req.buckets {
            let hash = match b.parse::<naiad_core::Hash>() {
                Ok(h) => h,
                Err(_) => {
                    tracing::warn!(target: "http", client = %peer(&addr), key = ?b, "bucket request rejected: malformed bucket key");
                    return (
                        StatusCode::BAD_REQUEST,
                        format!("malformed bucket key: {b:?}"),
                    )
                        .into_response();
                }
            };
            masked_buckets.push(bucket_key(&hash, bits));
        }
        masked_buckets.sort_unstable();
        masked_buckets.dedup();
        // No per-request bucket-count cap. Since #146 the client splits its key
        // list across however many requests it takes to fit this router's 64 KiB
        // body limit (`BUCKET_REQUEST_BODY_BUDGET` in netproto) and merges the
        // replies, so the count that arrives here is already bounded by that
        // limit — roughly 850 keys — and a further cap would only break pulls.
        // In a trusted snapshot deployment a bucketed pull of every key is
        // semantically a full pull, which this operator mode permits.
        // Per-bucket cost is bounded by the floor; repeated work by the dedup.
        // Response size is now bounded per request by st.bucket_budget (#145),
        // checked inside the row drain.
        let budget = st.bucket_budget;
        let bucket_count = masked_buckets.len();

        // Streaming path (#176): when the client opts in and the server supports it,
        // emit rows as NDJSON instead of buffering the whole window. The budget
        // becomes a stream cutoff + continuation cursor rather than a 413 ceiling.
        if req.stream {
            let resume_at = req.resume_at.clone();
            tracing::debug!(target: "http", client = %peer(&addr), domain = %domain, buckets = bucket_count, bits, "streaming buckets (snapshot-domain)");
            return stream_snapshot_domain(
                masked_buckets,
                resume_at,
                budget,
                backend,
                bits,
                Arc::clone(&st.serve_stats),
                domain,
            );
        }

        let out = tokio::task::spawn_blocking(move || {
            let mut tags: BTreeMap<String, Vec<OriginTag>> = BTreeMap::new();
            let mut remaining = budget;
            for b in &masked_buckets {
                let (part, spent) = backend.bucket(b, bits, remaining)?;
                remaining = remaining.saturating_sub(spent);
                // Snapshot-backend returns plain tag strings with no origin
                // (Hydrus PTR has no generation-source concept). Wrap each
                // string as an OriginTag with origin = None.
                for (hash, tag_strings) in part {
                    tags.entry(hash).or_default().extend(
                        tag_strings
                            .into_iter()
                            .map(|tag| OriginTag { tag, origin: None }),
                    );
                }
            }
            // A static snapshot has no sequence: cursor 0 tells the client
            // there is no incremental state to keep.
            Ok::<_, anyhow::Error>(Snapshot {
                version: PROTOCOL_VERSION,
                cursor: 0,
                tags,
            })
        })
        .await;
        return match out {
            Ok(Ok(s)) => {
                let hashes = s.tags.len();
                let tags: usize = s.tags.values().map(Vec::len).sum();
                match served_json(&s) {
                    Ok((resp, bytes)) => {
                        let elapsed = started.elapsed();
                        let elapsed_ms = elapsed.as_millis() as u64; // integer for tracing
                        tracing::debug!(target: "http", client = %peer(&addr), domain = %domain, buckets = bucket_count, bits, hashes, tags, bytes, elapsed_ms, "served buckets");
                        // #173/#178: fold into the per-domain serve-cost EWMA, normalised
                        // to ref_bits at the clamped snapshot query width. Fractional ms
                        // so a sub-ms serve does not truncate to 0 in the EWMA.
                        if bucket_count > 0 {
                            st.serve_stats.record(
                                domain,
                                elapsed.as_secs_f64() * 1000.0 / bucket_count as f64,
                                bits,
                            );
                        }
                        resp
                    }
                    Err(r) => r,
                }
            }
            Ok(Err(e)) if is_budget_exceeded(&e) => budget_exceeded_response(),
            Ok(Err(e)) => {
                // Log the full error server-side (may contain the snapshot path and
                // SQL detail). Return a bare 500 with no body so the client never
                // sees filesystem paths — matching the native branch's behaviour (#159).
                tracing::error!(target: "http", error = %e, "buckets: snapshot backend error");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
            Err(e) => {
                tracing::error!(target: "http", error = %e, "buckets: task join error");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        };
    }

    // Validate all bucket keys before dispatch: a malformed key is a client
    // error (#159). Previously the native branch silently continued past bad
    // keys; that is a server-hiding-client-error pattern. An explicit 400
    // naming the key lets the caller fix its request without relying on logs.
    for b in &req.buckets {
        if b.parse::<naiad_core::Hash>().is_err() {
            tracing::warn!(target: "http", client = %peer(&addr), key = ?b, "bucket request rejected: malformed bucket key");
            return (
                StatusCode::BAD_REQUEST,
                format!("malformed bucket key: {b:?}"),
            )
                .into_response();
        }
    }
    let prefix_bits = req.prefix_bits.min(256);
    let buckets = req.buckets;
    let since = req.since;
    let stream = req.stream;
    let resume_at = req.resume_at;
    let reader = st.reader();
    let budget = st.bucket_budget;
    let bucket_count = buckets.len();

    // Streaming path (#176): since = None + stream opt-in → NDJSON stream.
    // since = Some(..) stays materialized (non-goal, §2).
    if stream && since.is_none() {
        // Pre-build the masked/sorted/deduped bucket list so resume_at
        // comparisons are key-consistent (same transformation as the snapshot branch).
        let mut masked: Vec<String> = Vec::with_capacity(buckets.len());
        for b in &buckets {
            let Ok(lo) = b.parse::<naiad_core::Hash>() else {
                continue;
            };
            masked.push(bucket_key(&lo, prefix_bits));
        }
        masked.sort_unstable();
        masked.dedup();
        tracing::debug!(target: "http", client = %peer(&addr), domain = %domain, buckets = masked.len(), bits = prefix_bits, "streaming buckets (native)");
        return stream_native_snapshot(
            masked,
            resume_at,
            budget,
            reader,
            prefix_bits,
            Arc::clone(&st.serve_stats),
            domain,
        );
    }

    let out = tokio::task::spawn_blocking(move || {
        let store = reader.lock_recover();
        store.read_snapshot(|store| match since {
            None => {
                let mut tags: BTreeMap<_, _> = BTreeMap::new();
                let mut remaining = budget;
                for b in &buckets {
                    let Ok(lo) = b.parse::<naiad_core::Hash>() else {
                        // Defensive: keys were validated before spawn_blocking;
                        // this path is unreachable in practice.
                        continue;
                    };
                    let lo_key = bucket_key(&lo, prefix_bits);
                    let hi = bucket_upper(&lo, prefix_bits);
                    let (part, spent) = store.bucket(&lo_key, &hi, remaining)?;
                    remaining = remaining.saturating_sub(spent);
                    tags.extend(part);
                }
                Ok::<_, anyhow::Error>(BucketsResponse::Snapshot(Snapshot {
                    version: PROTOCOL_VERSION,
                    cursor: store.mapping_cursor()?,
                    tags,
                }))
            }
            Some(since) => {
                let mut changes: Vec<DeltaMapping> = Vec::new();
                let mut remaining = budget;
                for (b, s) in buckets.iter().zip(since) {
                    let Ok(lo) = b.parse::<naiad_core::Hash>() else {
                        // Defensive: keys were validated before spawn_blocking.
                        continue;
                    };
                    let lo_key = bucket_key(&lo, prefix_bits);
                    let hi = bucket_upper(&lo, prefix_bits);
                    let (part, spent) = store.bucket_delta(&lo_key, &hi, s, remaining)?;
                    remaining = remaining.saturating_sub(spent);
                    changes.extend(part);
                }
                Ok(BucketsResponse::Delta(MappingDelta {
                    version: PROTOCOL_VERSION,
                    cursor: store.mapping_cursor()?,
                    changes,
                }))
            }
        })
    })
    .await;
    match out {
        Ok(Ok(BucketsResponse::Snapshot(s))) => {
            let hashes = s.tags.len();
            let tags: usize = s.tags.values().map(Vec::len).sum();
            match served_json(&s) {
                Ok((resp, bytes)) => {
                    let elapsed = started.elapsed();
                    let elapsed_ms = elapsed.as_millis() as u64; // integer for tracing
                    tracing::debug!(target: "http", client = %peer(&addr), domain = %domain, buckets = bucket_count, bits = prefix_bits, hashes, tags, bytes, elapsed_ms, "served buckets");
                    // #173/#178: fold into the per-domain serve-cost EWMA at prefix_bits.
                    // Fractional ms so a sub-ms serve does not truncate to 0 in the EWMA.
                    if bucket_count > 0 {
                        st.serve_stats.record(
                            domain,
                            elapsed.as_secs_f64() * 1000.0 / bucket_count as f64,
                            prefix_bits,
                        );
                    }
                    resp
                }
                Err(r) => r,
            }
        }
        Ok(Ok(BucketsResponse::Delta(d))) => {
            let changes = d.changes.len();
            match served_json(&d) {
                Ok((resp, bytes)) => {
                    let elapsed = started.elapsed();
                    let elapsed_ms = elapsed.as_millis() as u64; // integer for tracing
                    tracing::debug!(target: "http", client = %peer(&addr), domain = %domain, buckets = bucket_count, bits = prefix_bits, changes, bytes, elapsed_ms, "served buckets (delta)");
                    // Delta serves fold in deliberately — the signal is cache temperature,
                    // orthogonal to snapshot-vs-delta (#178: pass prefix_bits). Fractional ms.
                    if bucket_count > 0 {
                        st.serve_stats.record(
                            domain,
                            elapsed.as_secs_f64() * 1000.0 / bucket_count as f64,
                            prefix_bits,
                        );
                    }
                    resp
                }
                Err(r) => r,
            }
        }
        Ok(Err(e)) if is_budget_exceeded(&e) => budget_exceeded_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `POST /repo/relations/submit` — verify and apply one signed relation op.
async fn relations_submit_handler(
    State(st): State<RepoState>,
    Json(sub): Json<RelationSubmission>,
) -> Response {
    if st.read_only {
        tracing::warn!(target: "http", "relation submit rejected: repo is read-only");
        return (
            StatusCode::FORBIDDEN,
            "this repo is serving read-only; writes are disabled",
        )
            .into_response();
    }
    if let Err(e) = verify_relation(&sub) {
        tracing::warn!(target: "http", author = %sub.author, error = %e, "relation signature rejected");
        return (StatusCode::BAD_REQUEST, format!("invalid relation: {e:#}")).into_response();
    }
    let out = tokio::task::spawn_blocking(move || {
        let store = st.store.lock_recover();
        store.apply_relation(&sub)
    })
    .await;
    match out {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => {
            tracing::error!(target: "http", error = %e, "relations_submit: store error");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
        Err(e) => {
            tracing::error!(target: "http", error = %e, "relations_submit: task join error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Query string for `GET /repo/relations`.
#[derive(serde::Deserialize)]
struct RelationsQuery {
    since: Option<u64>,
}

enum RelationsResponse {
    Graph(naiad_netproto::RelationGraph),
    Delta(naiad_netproto::RelationDelta),
}

/// `GET /repo/relations` — full relation graph or incremental delta.
async fn relations_handler(
    State(st): State<RepoState>,
    Query(q): Query<RelationsQuery>,
) -> Response {
    let reader = st.reader();
    let out = tokio::task::spawn_blocking(move || {
        let store = reader.lock_recover();
        store.read_snapshot(|store| match q.since {
            Some(since) => {
                Ok::<_, anyhow::Error>(RelationsResponse::Delta(naiad_netproto::RelationDelta {
                    version: PROTOCOL_VERSION,
                    cursor: store.relation_cursor()?,
                    edges: store.edges_since(since)?,
                }))
            }
            None => Ok(RelationsResponse::Graph(store.relations()?)),
        })
    })
    .await;
    match out {
        Ok(Ok(RelationsResponse::Graph(g))) => Json(g).into_response(),
        Ok(Ok(RelationsResponse::Delta(d))) => Json(d).into_response(),
        Ok(Err(e)) => {
            tracing::error!(target: "http", error = %e, "relations: store error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(e) => {
            tracing::error!(target: "http", error = %e, "relations: task join error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── Authenticated handlers ────────────────────────────────────────────────────

/// `POST /repo/submit` — request-auth + apply one signed tag operation.
///
/// Status codes: 400 domain gate (before auth), 401 bad request auth, 400
/// malformed body or bad doc sig, 403 banned account, 500 store error.
async fn submit_handler(
    State(st): State<RepoState>,
    Query(q): Query<DomainParam>,
    headers: HeaderMap,
    uri: Uri,
    method: Method,
    body: Bytes,
) -> Response {
    if st.read_only {
        tracing::warn!(target: "http", "submit rejected: repo is read-only");
        return (
            StatusCode::FORBIDDEN,
            "this repo is serving read-only; writes are disabled",
        )
            .into_response();
    }
    // 0. Domain gate, BEFORE auth: there is no point verifying a signature for
    //    a domain this node cannot accept writes for. Safe to run pre-auth
    //    because `?domain=` is itself signed (#161) — the worst an on-path
    //    rewrite achieves here is a 400 instead of the 401 it earns at step 1,
    //    and either way the submission never lands in the wrong domain.
    let domain = match st.resolve_request_domain(q.domain.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if domain != st.domains.native {
        // Phase 3 (the push relay) is where submits to an added SHA-256 domain
        // become meaningful. A static snapshot is by definition more than two
        // weeks behind the PTR head, so Hydrus would reject every relayed
        // contribution anyway — say so instead of accepting and dropping it.
        tracing::warn!(target: "http", %domain, "submit rejected: push not available in snapshot mode");
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "push not available in snapshot mode: this repo cannot accept submissions \
                 for the {domain} domain"
            ),
        )
            .into_response();
    }

    let now_ts = now();
    // 1. Verify request-level auth.
    let pubkey = match authenticate(
        &headers,
        method.as_str(),
        uri.path(),
        requested_domain(q.domain.as_deref()),
        &body,
        now_ts,
    ) {
        Ok(k) => k,
        Err(r) => return r,
    };

    // 2. Parse the body.
    let sub: Submission = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "http", key = %pubkey, error = %e, "submission body rejected: malformed JSON");
            return (StatusCode::BAD_REQUEST, format!("bad request body: {e}")).into_response();
        }
    };

    // 3. Verify the document-level naiad-sub signature explicitly so we can
    //    return a clean 400 before hitting the store.
    if let Err(e) = verify(&sub) {
        tracing::warn!(target: "http", author = %sub.author, error = %e, "submission signature rejected");
        return (
            StatusCode::BAD_REQUEST,
            format!("bad submission signature: {e:#}"),
        )
            .into_response();
    }

    // 4. Apply — at this point the sig is valid; the only expected failure is
    //    a banned account (403).  Other store errors → 500.
    //    Pragmatic note: we match on the error message text to distinguish
    //    "is banned" from genuine store failures.  A typed error enum on the
    //    store would be cleaner but is deferred.
    let out = tokio::task::spawn_blocking(move || {
        let store = st.store.lock_recover();
        store.apply_submission(&sub)
    })
    .await;
    match out {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => {
            if is_banned_err(&e) {
                tracing::warn!(target: "http", "submit rejected: account is banned");
                (StatusCode::FORBIDDEN, "account is banned").into_response()
            } else {
                tracing::error!(target: "http", error = %e, "submit: store error");
                (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
            }
        }
        Err(e) => {
            tracing::error!(target: "http", error = %e, "submit: task join error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /repo/report` — request-auth, then file an anonymous report.
///
/// Status codes: 400 non-native domain (#160), 401 bad auth, 400 malformed
/// body, 403 banned account, 500 store.
async fn report_handler(
    State(st): State<RepoState>,
    Query(q): Query<DomainParam>,
    headers: HeaderMap,
    uri: Uri,
    method: Method,
    body: Bytes,
) -> Response {
    if st.read_only {
        tracing::warn!(target: "http", "report rejected: repo is read-only");
        return (
            StatusCode::FORBIDDEN,
            "this repo is serving read-only; writes are disabled",
        )
            .into_response();
    }
    // Domain gate BEFORE auth: reports and moderation apply only to the repo's
    // native domain. A snapshot domain is read-only — there is no mapping to
    // delete and no remedy to apply — so accepting a report against it would
    // silently pollute the queue forever (#160).
    let domain = match st.resolve_request_domain(q.domain.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if domain != st.domains.native {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "reports apply only to the repo's native domain ({native}); \
                 the {domain} domain is served by a read-only snapshot and reports \
                 against it cannot be remedied",
                native = st.domains.native,
            ),
        )
            .into_response();
    }
    let now_ts = now();
    let pubkey = match authenticate(
        &headers,
        method.as_str(),
        uri.path(),
        requested_domain(q.domain.as_deref()),
        &body,
        now_ts,
    ) {
        Ok(k) => k,
        Err(r) => return r,
    };

    let report: Report = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "http", key = %pubkey, error = %e, "report body rejected: malformed JSON");
            return (StatusCode::BAD_REQUEST, format!("bad report body: {e}")).into_response();
        }
    };

    let out = tokio::task::spawn_blocking(move || {
        let store = st.store.lock_recover();
        store.insert_report(
            &report.hash,
            &report.tag,
            &pubkey,
            report.note.as_deref(),
            now_ts,
        )
    })
    .await;
    match out {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => {
            if is_banned_err(&e) {
                tracing::warn!(target: "http", "report rejected: account is banned");
                (StatusCode::FORBIDDEN, "account is banned").into_response()
            } else {
                tracing::error!(target: "http", error = %e, "report: store error");
                (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
            }
        }
        Err(e) => {
            tracing::error!(target: "http", error = %e, "report: task join error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /repo/reports` — moderator-only open report queue.
///
/// Status codes: 401 bad auth, 403 not a moderator, 500 store.
async fn reports_handler(State(st): State<RepoState>, headers: HeaderMap, uri: Uri) -> Response {
    let now_ts = now();
    let pubkey = match authenticate(&headers, "GET", uri.path(), None, b"", now_ts) {
        Ok(k) => k,
        Err(r) => return r,
    };

    // Run moderator check and data read under one lock acquisition (reader).
    let reader = st.reader();
    let out = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<_>>> {
        let store = reader.lock_recover();
        if !store.is_moderator(&pubkey)? {
            return Ok(None);
        }
        Ok(Some(store.open_reports()?))
    })
    .await;

    match out {
        Ok(Ok(Some(rows))) => Json(ReportList {
            version: PROTOCOL_VERSION,
            rows,
        })
        .into_response(),
        Ok(Ok(None)) => {
            tracing::warn!(target: "http", "reports denied: not a moderator");
            (StatusCode::FORBIDDEN, "not a moderator").into_response()
        }
        Ok(Err(e)) => {
            tracing::error!(target: "http", error = %e, "reports: store error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(e) => {
            tracing::error!(target: "http", error = %e, "reports: task join error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /repo/moderate` — moderator-only action dispatch.
///
/// Status codes: 400 non-native domain (#160), 401 bad auth, 400 malformed
/// body, 403 not a moderator, 500 store.
async fn moderate_handler(
    State(st): State<RepoState>,
    Query(q): Query<DomainParam>,
    headers: HeaderMap,
    uri: Uri,
    method: Method,
    body: Bytes,
) -> Response {
    if st.read_only {
        tracing::warn!(target: "http", "moderate rejected: repo is read-only");
        return (
            StatusCode::FORBIDDEN,
            "this repo is serving read-only; writes are disabled",
        )
            .into_response();
    }
    // Domain gate BEFORE auth: moderation actions (DeleteMapping, Ban,
    // Dismiss) operate on the native RepoStore. Running them against a
    // snapshot domain is a silent no-op at best (the snapshot is read-only)
    // and misleading at worst (#160). Reject explicitly so callers understand
    // why no action was taken.
    let domain = match st.resolve_request_domain(q.domain.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if domain != st.domains.native {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "moderation applies only to the repo's native domain ({native}); \
                 the {domain} domain is served by a read-only snapshot — \
                 moderation actions against it are silent no-ops",
                native = st.domains.native,
            ),
        )
            .into_response();
    }
    let now_ts = now();
    let pubkey = match authenticate(
        &headers,
        method.as_str(),
        uri.path(),
        requested_domain(q.domain.as_deref()),
        &body,
        now_ts,
    ) {
        Ok(k) => k,
        Err(r) => return r,
    };

    let action: ModerateAction = match serde_json::from_slice(&body) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(target: "http", key = %pubkey, error = %e, "moderate action body rejected: malformed JSON");
            return (StatusCode::BAD_REQUEST, format!("bad action body: {e}")).into_response();
        }
    };

    // Moderator check and action dispatch under one lock acquisition.
    let out = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let store = st.store.lock_recover();
        if !store.is_moderator(&pubkey)? {
            return Ok(false);
        }
        match action {
            ModerateAction::DeleteMapping { hash, tag } => {
                store.moderator_delete_mapping(&hash, &tag)?;
            }
            ModerateAction::Ban { pubkey: target } => {
                store.set_banned(&target, true)?;
            }
            ModerateAction::Dismiss { report_id } => {
                store.close_report(report_id as i64)?;
            }
        }
        Ok(true)
    })
    .await;

    match out {
        Ok(Ok(true)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(false)) => {
            tracing::warn!(target: "http", "moderate denied: not a moderator");
            (StatusCode::FORBIDDEN, "not a moderator").into_response()
        }
        Ok(Err(e)) => {
            tracing::error!(target: "http", error = %e, "moderate: store error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(e) => {
            tracing::error!(target: "http", error = %e, "moderate: task join error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── In-module unit tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use naiad_netproto::{Account, HashDomain};

    const NOW: i64 = 1_700_000_000;
    /// A request with no `?domain=` — what every in-tree client sends.
    const NO_DOMAIN: Option<HashDomain> = None;

    /// Canonical test router: Blake3, k=1000, no repo-key, no read-store.
    fn test_router() -> Router {
        let store = RepoStore::open_in_memory().unwrap();
        app_split(
            Arc::new(Mutex::new(store)),
            None,
            1000,
            None,
            None,
            HashDomain::Blake3,
        )
    }

    fn make_headers(acct: &Account, method: &str, path: &str, body: &[u8]) -> HeaderMap {
        let sig = acct.sign_auth(method, path, NO_DOMAIN, NOW, body);
        let mut h = HeaderMap::new();
        h.insert(
            HDR_AUTH_KEY,
            HeaderValue::from_str(&acct.public_hex()).unwrap(),
        );
        h.insert(
            HDR_AUTH_TS,
            HeaderValue::from_str(&NOW.to_string()).unwrap(),
        );
        h.insert(HDR_AUTH_SIG, HeaderValue::from_str(&sig).unwrap());
        h
    }

    #[test]
    fn authenticate_round_trip() {
        let acct = Account::generate();
        let headers = make_headers(&acct, "POST", "/repo/submit", b"payload");
        let result = authenticate(&headers, "POST", "/repo/submit", NO_DOMAIN, b"payload", NOW);
        assert_eq!(result.unwrap(), acct.public_hex());
    }

    #[test]
    fn authenticate_wrong_key_rejected() {
        let signer = Account::generate();
        let other = Account::generate();
        // Put other's pubkey but signer's signature.
        let sig = signer.sign_auth("POST", "/repo/submit", NO_DOMAIN, NOW, b"");
        let mut h = HeaderMap::new();
        h.insert(
            HDR_AUTH_KEY,
            HeaderValue::from_str(&other.public_hex()).unwrap(),
        );
        h.insert(
            HDR_AUTH_TS,
            HeaderValue::from_str(&NOW.to_string()).unwrap(),
        );
        h.insert(HDR_AUTH_SIG, HeaderValue::from_str(&sig).unwrap());
        let r = authenticate(&h, "POST", "/repo/submit", NO_DOMAIN, b"", NOW);
        assert!(r.is_err(), "mismatched key/sig must fail");
    }

    #[test]
    fn authenticate_stale_timestamp_rejected() {
        let acct = Account::generate();
        let old_ts = NOW - 400; // > 300 s stale
        let sig = acct.sign_auth("POST", "/repo/submit", NO_DOMAIN, old_ts, b"");
        let mut h = HeaderMap::new();
        h.insert(
            HDR_AUTH_KEY,
            HeaderValue::from_str(&acct.public_hex()).unwrap(),
        );
        h.insert(
            HDR_AUTH_TS,
            HeaderValue::from_str(&old_ts.to_string()).unwrap(),
        );
        h.insert(HDR_AUTH_SIG, HeaderValue::from_str(&sig).unwrap());
        // Verify at NOW: difference is 400 s > AUTH_FRESHNESS_SECS (300).
        let r = authenticate(&h, "POST", "/repo/submit", NO_DOMAIN, b"", NOW);
        assert!(r.is_err(), "stale timestamp must fail");
    }

    #[test]
    fn authenticate_missing_headers_rejected() {
        let h = HeaderMap::new();
        let r = authenticate(&h, "POST", "/repo/submit", NO_DOMAIN, b"", NOW);
        assert!(r.is_err(), "missing headers must fail");
    }

    #[test]
    fn authenticate_tampered_body_rejected() {
        let acct = Account::generate();
        let headers = make_headers(&acct, "POST", "/repo/submit", b"original body");
        // Verify with tampered body.
        let r = authenticate(
            &headers,
            "POST",
            "/repo/submit",
            NO_DOMAIN,
            b"tampered body",
            NOW,
        );
        assert!(r.is_err(), "tampered body must fail");
    }

    #[test]
    fn authenticate_tampered_path_rejected() {
        let acct = Account::generate();
        let headers = make_headers(&acct, "POST", "/repo/submit", b"");
        let r = authenticate(&headers, "POST", "/repo/other", NO_DOMAIN, b"", NOW);
        assert!(r.is_err(), "tampered path must fail");
    }

    #[test]
    fn authenticate_non_integer_ts_rejected() {
        let acct = Account::generate();
        let mut h = HeaderMap::new();
        h.insert(
            HDR_AUTH_KEY,
            HeaderValue::from_str(&acct.public_hex()).unwrap(),
        );
        h.insert(HDR_AUTH_TS, HeaderValue::from_static("not-a-number"));
        h.insert(
            HDR_AUTH_SIG,
            HeaderValue::from_static("aa".repeat(64).leak() as &str),
        );
        let r = authenticate(&h, "POST", "/repo/submit", NO_DOMAIN, b"", NOW);
        assert!(r.is_err(), "non-integer ts must fail");
    }

    #[tokio::test]
    async fn caps_reflects_hash_domain() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let store = RepoStore::open_in_memory().unwrap();
        let router = app_split(
            Arc::new(Mutex::new(store)),
            None,
            1000,
            None,
            None,
            HashDomain::Sha256,
        );

        let req = Request::builder()
            .uri("/repo/caps")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(
            body_str.contains("\"sha256\""),
            "caps body should contain sha256, got: {body_str}"
        );
    }

    /// Verify that the moderator gate inside `reports_handler` and
    /// `moderate_handler` is exercised correctly via direct store queries.
    /// Full e2e tests (with a live server) land in Task 5.
    #[test]
    fn is_moderator_returns_false_for_missing_and_contributor_accounts() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        // Account does not exist yet.
        assert!(!store.is_moderator(&acct.public_hex()).unwrap());
        // Create as contributor.
        store.ensure_account(&acct.public_hex(), NOW).unwrap();
        assert!(!store.is_moderator(&acct.public_hex()).unwrap());
    }

    #[test]
    fn is_moderator_true_after_set_role() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        store.ensure_account(&acct.public_hex(), NOW).unwrap();
        store.set_role(&acct.public_hex(), "moderator").unwrap();
        assert!(store.is_moderator(&acct.public_hex()).unwrap());
        // Banned moderator is not a moderator.
        store.set_banned(&acct.public_hex(), true).unwrap();
        assert!(!store.is_moderator(&acct.public_hex()).unwrap());
    }

    // ── /health tests ─────────────────────────────────────────────────────────

    /// `GET /health` returns 200 with `status == "ok"` and the crate version,
    /// and requires no authentication headers whatsoever.
    #[tokio::test]
    async fn health_returns_200_and_ok_status() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder()
            .uri(REPO_HEALTH)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = test_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200, "health endpoint must return 200");

        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["status"], "ok", "health body must have status: \"ok\"");
        assert_eq!(
            json["server_version"],
            env!("CARGO_PKG_VERSION"),
            "health body server_version must match the crate version"
        );
    }

    /// `GET /repo/caps` includes the `server_version` field.
    #[tokio::test]
    async fn caps_includes_server_version() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = test_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            caps.server_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "caps must include server_version equal to the crate version"
        );
    }
    // Note: caps_without_server_version_deserialises_as_none has been moved and
    // expanded to caps_server_version_defaults_none_and_round_trips in
    // naiad-netproto::bucket tests where it belongs alongside the Caps type.

    /// `GET /repo/caps` advertises the operator name when one is configured.
    #[tokio::test]
    async fn caps_includes_operator_name_when_set() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let store = RepoStore::open_in_memory().unwrap();
        let router = app_split(
            Arc::new(Mutex::new(store)),
            None,
            1000,
            None,
            Some("NOS".to_string()),
            HashDomain::Blake3,
        );
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            caps.name.as_deref(),
            Some("NOS"),
            "caps must include the configured operator name"
        );
    }

    /// `GET /repo/caps` omits the `name` field entirely when no name is configured.
    #[tokio::test]
    async fn caps_omits_name_when_unset() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = test_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(
            !body_str.contains("\"name\""),
            "unnamed server must emit byte-identical caps: {body_str}"
        );
    }

    // ── Dual-domain routing tests ─────────────────────────────────────────────

    const FIX_SHA: &str = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";

    /// A dual-domain router: native blake3 store plus a snapshot-mode sha256
    /// backend over a fixture Hydrus snapshot. The `TempDir` must stay alive.
    ///
    /// `min_query_bits` sets the bucket-query floor; pass
    /// `crate::domain::SNAPSHOT_MIN_QUERY_BITS` for the default.
    fn dual_domain_router(max_query_bits: u32, min_query_bits: u32) -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        naiad_plugin_hydrus::fixture::write_snapshot(
            dir.path(),
            9,
            &[(FIX_SHA, "character:samus"), (FIX_SHA, "maid")],
        )
        .expect("write snapshot fixture");
        let backend =
            crate::domain::SnapshotBackend::open(dir.path(), Some(9)).expect("open backend");
        let domains = crate::domain::DomainConfig {
            native: HashDomain::Blake3,
            added_sha256: Some(Arc::new(backend) as Arc<dyn crate::domain::Sha256Backend>),
            max_query_bits,
            min_query_bits,
        };
        let store = RepoStore::open_in_memory().unwrap();
        let router = app_domains(Arc::new(Mutex::new(store)), None, 1000, None, None, domains);
        (router, dir)
    }

    async fn post_buckets(router: Router, body: serde_json::Value) -> (u16, String) {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder()
            .method("POST")
            .uri(REPO_BUCKETS)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn caps_advertises_added_sha256_domain() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            caps.hash_domain,
            HashDomain::Blake3,
            "the scalar field stays native so old clients see a plain blake3 repo"
        );
        assert_eq!(
            caps.hash_domains,
            vec![HashDomain::Blake3, HashDomain::Sha256],
            "the bridge ADDS sha256"
        );
        assert_eq!(
            caps.mode,
            naiad_netproto::PullMode::Bucketed { prefix_bits: 256 },
            "snapshot mode advertises the server's precision ceiling"
        );
    }

    #[tokio::test]
    async fn caps_of_a_native_repo_omits_hash_domains() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = test_router().oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).to_string();
        assert!(
            !body.contains("hash_domains"),
            "a single-domain repo must emit today's exact caps bytes: {body}"
        );
    }

    #[tokio::test]
    async fn buckets_with_unserved_domain_is_an_explicit_error() {
        let (status, body) = post_buckets(
            test_router(),
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 8,
                "buckets": ["00".repeat(32)],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 400, "never an empty 200: {body}");
        assert!(
            body.contains("sha256"),
            "names the requested domain: {body}"
        );
        assert!(body.contains("blake3"), "lists what is served: {body}");
    }

    #[tokio::test]
    async fn buckets_with_unrecognized_domain_is_an_explicit_error() {
        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 8,
                "buckets": ["00".repeat(32)],
                "domain": "md5",
            }),
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("md5"), "names the bad value: {body}");
        assert!(
            body.contains("blake3") && body.contains("sha256"),
            "lists the served domains: {body}"
        );
    }

    #[tokio::test]
    async fn buckets_in_sha256_domain_are_served_from_the_snapshot() {
        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let snap: naiad_netproto::Snapshot = serde_json::from_str(&body).unwrap();
        let tags = snap.tags.get(FIX_SHA).expect("fixture sha present");
        assert!(
            tags.iter().any(|t| t.tag == "character:samus") && tags.iter().any(|t| t.tag == "maid"),
            "snapshot answers with the Hydrus tags: {tags:?}"
        );
    }

    #[tokio::test]
    async fn buckets_in_sha256_domain_clamp_to_max_query_bits() {
        // Ceiling of 16 bits (above SNAPSHOT_MIN_QUERY_BITS=8): an exact-hash
        // request is answered coarsely (a superset), never at the requested precision.
        let (router, _dir) = dual_domain_router(16, SNAPSHOT_MIN_QUERY_BITS);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 200, "clamping is not an error: {body}");
        let snap: naiad_netproto::Snapshot = serde_json::from_str(&body).unwrap();
        assert!(snap.tags.contains_key(FIX_SHA), "still a superset: {body}");
    }

    #[tokio::test]
    async fn buckets_in_sha256_domain_below_min_bits_is_rejected() {
        // Default floor (SNAPSHOT_MIN_QUERY_BITS = 8): a 0-bit request is rejected.
        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 0,
                "buckets": ["00".repeat(32)],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 400, "below-floor query must be rejected: {body}");
        assert!(
            body.contains('8') || body.contains("SNAPSHOT_MIN_QUERY_BITS"),
            "must name the floor: {body}"
        );
    }

    #[tokio::test]
    async fn buckets_in_sha256_domain_raised_floor_is_enforced() {
        // Raised floor (16 bits): a 14-bit request is rejected and the body names
        // the served range; an at-floor 16-bit request passes.
        let (router, _dir) = dual_domain_router(256, 16);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 14,
                "buckets": ["00".repeat(32)],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 400, "below raised floor must be rejected: {body}");
        assert!(
            body.contains("16"),
            "400 body must name the effective floor (16): {body}"
        );
        assert!(
            body.contains("16..="),
            "400 body must name the served range (16..=...): {body}"
        );

        // An at-floor request (exactly 16 bits) must succeed.
        let (router2, _dir2) = dual_domain_router(256, 16);
        let (status2, body2) = post_buckets(
            router2,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 16,
                "buckets": [FIX_SHA],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status2, 200, "at-floor request must succeed: {body2}");
    }

    #[tokio::test]
    async fn submit_to_sha256_domain_in_snapshot_mode_is_rejected() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let req = Request::builder()
            .method("POST")
            .uri(format!("{REPO_SUBMIT}?domain=sha256"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            400,
            "must be rejected before auth, not 401 and not silently accepted"
        );
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).to_string();
        assert!(
            body.contains("push not available in snapshot mode"),
            "exact spec §6 wording: {body}"
        );
    }

    #[tokio::test]
    async fn snapshot_endpoint_rejects_the_sha256_domain() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let req = Request::builder()
            .uri(format!("{REPO_SNAPSHOT}?domain=sha256"))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).to_string();
        assert!(
            body.contains("/repo/buckets"),
            "must point at the endpoint that does work: {body}"
        );
    }

    /// When `max_query_bits` equals the floor (what `from_settings` produces
    /// after raising a sub-floor config), caps must advertise exactly the floor
    /// and an on-floor request must be served (not 400'd).
    #[tokio::test]
    async fn caps_at_snapshot_floor_ceiling_advertises_floor_bits() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        // SNAPSHOT_MIN_QUERY_BITS = 8: this is what from_settings produces when
        // the operator configured, e.g., max_query_bits = 4.
        let (router, _dir) = dual_domain_router(SNAPSHOT_MIN_QUERY_BITS, SNAPSHOT_MIN_QUERY_BITS);
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            caps.mode,
            naiad_netproto::PullMode::Bucketed {
                prefix_bits: SNAPSHOT_MIN_QUERY_BITS
            },
            "caps must advertise the raised ceiling, not the raw configured value"
        );
    }

    /// After `from_settings` raises a sub-floor ceiling to 8, a client querying
    /// at exactly 8 prefix_bits must be served (not floor-rejected).
    #[tokio::test]
    async fn buckets_sha256_at_floor_ceiling_is_served() {
        // Ceiling = 8 = floor: the query passes the floor check, clamp_query_bits
        // returns 8, and the snapshot backend answers with a superset.
        let (router, _dir) = dual_domain_router(SNAPSHOT_MIN_QUERY_BITS, SNAPSHOT_MIN_QUERY_BITS);
        // FIX_SHA starts with "11", so its 8-bit bucket lo is "11" + "00" * 31.
        let lo = format!("{}{}", &FIX_SHA[..2], "00".repeat(31));
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": SNAPSHOT_MIN_QUERY_BITS,
                "buckets": [lo],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(
            status, 200,
            "an on-floor request at a floor ceiling must be served: {body}"
        );
        let snap: naiad_netproto::Snapshot = serde_json::from_str(&body).unwrap();
        // The 8-bit bucket over "11..." includes FIX_SHA (which starts with 0x11).
        assert!(
            snap.tags.contains_key(FIX_SHA),
            "the fixture hash must appear in its own 8-bit bucket: {body}"
        );
    }

    /// A request that sends the same sha256 bucket key repeated three times must
    /// return the same body as a single-key request — the server deduplicates
    /// the keys before scanning so no index scan is executed more than once.
    #[tokio::test]
    async fn buckets_sha256_repeated_key_deduplicated_server_side() {
        let single_body = {
            let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
            let (status, body) = post_buckets(
                router,
                serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "prefix_bits": 256,
                    "buckets": [FIX_SHA],
                    "domain": "sha256",
                }),
            )
            .await;
            assert_eq!(status, 200, "single-key baseline: {body}");
            body
        };

        let triple_body = {
            let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
            let (status, body) = post_buckets(
                router,
                serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "prefix_bits": 256,
                    "buckets": [FIX_SHA, FIX_SHA, FIX_SHA],
                    "domain": "sha256",
                }),
            )
            .await;
            assert_eq!(status, 200, "triple-key request: {body}");
            body
        };

        assert_eq!(
            single_body, triple_body,
            "three identical keys must collapse to one scan — body must match the single-key response"
        );
    }

    /// `since` on a sha256 snapshot request must be rejected with a 400 that
    /// names issue #142 (incremental deltas are not available in snapshot mode).
    #[tokio::test]
    async fn buckets_sha256_with_since_is_rejected_naming_issue_142() {
        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 8,
                "buckets": ["11".to_string() + &"00".repeat(31)],
                "since": [0u64],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 400, "since on sha256 must be rejected: {body}");
        assert!(
            body.contains("142"),
            "must reference issue #142 so operators know the roadmap: {body}"
        );
    }

    // ── #153 regression: dedup runs AFTER prefix masking ─────────────────────

    /// Regression for #153: two distinct full-hash keys that share the same
    /// `bits`-wide prefix must be deduplicated to a single scan. Before the
    /// fix, they survived dedup as different 64-char strings and each triggered
    /// an identical range scan — dedup was a no-op precisely when `max_query_bits`
    /// was low.
    ///
    /// Strategy: use `max_query_bits = 16`. Build two keys both with the same
    /// 16-bit prefix as FIX_SHA ("1122"), but with different lower bytes. Both
    /// should mask to the identical 16-bit bucket and return the same tags as a
    /// single-key request against that bucket.
    #[tokio::test]
    async fn buckets_sha256_distinct_keys_same_prefix_deduplicated_after_masking() {
        // FIX_SHA starts with "1122..."; with 16-bit masking both keys below
        // resolve to "1122" + "00" * 30.
        let key_a = FIX_SHA.to_string(); // "11223344..."
        // Key B: same 16-bit prefix ("1122"), different lower bytes.
        let key_b = format!("1122{}", "ff".repeat(30)); // "1122ffffffffffff..."

        // Single-key baseline: ask for just FIX_SHA's 16-bit bucket.
        let single_body = {
            let (router, _dir) = dual_domain_router(16, SNAPSHOT_MIN_QUERY_BITS);
            let (status, body) = post_buckets(
                router,
                serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "prefix_bits": 16,
                    "buckets": [key_a.clone()],
                    "domain": "sha256",
                }),
            )
            .await;
            assert_eq!(status, 200, "single-key baseline: {body}");
            body
        };

        // Two-key request: both keys, differing only below bit 16. After masking
        // they collapse to the same bucket entry. The result must be identical.
        let two_key_body = {
            let (router, _dir) = dual_domain_router(16, SNAPSHOT_MIN_QUERY_BITS);
            let (status, body) = post_buckets(
                router,
                serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "prefix_bits": 16,
                    "buckets": [key_a, key_b],
                    "domain": "sha256",
                }),
            )
            .await;
            assert_eq!(status, 200, "two-key request: {body}");
            body
        };

        assert_eq!(
            single_body, two_key_body,
            "two keys sharing the same 16-bit prefix must dedup to one scan after masking (#153)"
        );
    }

    // ── #159 regression: malformed bucket key → 400, 500 body is empty ───────

    /// Regression for #159: a malformed bucket key in the snapshot domain must
    /// yield 400 naming the key, not a 500 with a server-side path in the body.
    #[tokio::test]
    async fn buckets_sha256_malformed_key_yields_400_naming_the_key() {
        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": ["zz"],   // not a valid 64-char hex hash
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 400, "malformed key must yield 400, not 500: {body}");
        assert!(
            body.contains("zz"),
            "400 response must name the malformed key: {body}"
        );
        // Must not leak any server-side path.
        assert!(
            !body.contains('/') && !body.contains('\\'),
            "400 body must not contain filesystem paths: {body}"
        );
    }

    /// Regression for #159: a malformed bucket key in the native (blake3) domain
    /// must also yield 400 naming the key — not be silently skipped.
    #[tokio::test]
    async fn buckets_blake3_malformed_key_yields_400_naming_the_key() {
        let (status, body) = post_buckets(
            test_router(),
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 8,
                "buckets": ["not-a-hash"],
            }),
        )
        .await;
        assert_eq!(
            status, 400,
            "malformed key in native branch must yield 400, not be silently skipped: {body}"
        );
        assert!(
            body.contains("not-a-hash"),
            "400 response must name the malformed key: {body}"
        );
    }

    // ── #160 regression: /repo/report and /repo/moderate domain discriminator ──

    /// Helper: POST to an endpoint with valid auth headers for the given account.
    async fn post_with_auth(
        router: Router,
        uri: &str,
        acct: &Account,
        body_json: serde_json::Value,
    ) -> (u16, String) {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let body_bytes = serde_json::to_vec(&body_json).unwrap();
        let ts = crate::store::now();
        let sig = acct.sign_auth("POST", uri, NO_DOMAIN, ts, &body_bytes);
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header(HDR_AUTH_KEY, acct.public_hex())
            .header(HDR_AUTH_TS, ts.to_string())
            .header(HDR_AUTH_SIG, sig)
            .body(axum::body::Body::from(body_bytes))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// Regression for #160: `POST /repo/report?domain=sha256` on a dual-domain
    /// snapshot node must return 400 before auth, explaining that reports apply
    /// only to the native domain.
    #[tokio::test]
    async fn report_with_non_native_domain_yields_400() {
        let acct = Account::generate();
        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let report_body = serde_json::json!({
            "hash": FIX_SHA,
            "tag": "character:samus",
        });
        let (status, body) = post_with_auth(
            router,
            &format!("{REPO_REPORT}?domain=sha256"),
            &acct,
            report_body,
        )
        .await;
        assert_eq!(
            status, 400,
            "report to non-native domain must be 400, not 401 or accepted: {body}"
        );
        assert!(
            body.contains("native") || body.contains("read-only"),
            "error must explain why sha256 reports are rejected: {body}"
        );
    }

    /// Regression for #160: `POST /repo/moderate?domain=sha256` on a dual-domain
    /// snapshot node must return 400 before auth.
    #[tokio::test]
    async fn moderate_with_non_native_domain_yields_400() {
        let acct = Account::generate();
        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let action_body = serde_json::json!({
            "DeleteMapping": { "hash": FIX_SHA, "tag": "character:samus" }
        });
        let (status, body) = post_with_auth(
            router,
            &format!("{REPO_MODERATE}?domain=sha256"),
            &acct,
            action_body,
        )
        .await;
        assert_eq!(
            status, 400,
            "moderate to non-native domain must be 400, not 401 or accepted: {body}"
        );
        assert!(
            body.contains("native") || body.contains("read-only"),
            "error must explain why sha256 moderation is rejected: {body}"
        );
    }

    /// Regression for #160: with no domain param (defaults to native), report and
    /// moderate must pass the domain gate and proceed to auth/role checks as
    /// before — 401 (no valid auth token here), not 400.
    #[tokio::test]
    async fn report_without_domain_param_reaches_auth() {
        use axum::body::to_bytes;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        // No auth headers → 401, not 400. If 400, the domain gate misfired.
        let req = Request::builder()
            .method("POST")
            .uri(REPO_REPORT) // no domain= param → defaults to native
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "no auth must yield 401, not 400 domain rejection"
        );
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            !body_str.contains("domain"),
            "401 body must not mention domain: {body_str}"
        );
    }

    #[tokio::test]
    async fn moderate_without_domain_param_reaches_auth() {
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let req = Request::builder()
            .method("POST")
            .uri(REPO_MODERATE) // no domain= param → defaults to native
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "no auth must yield 401, not 400 domain rejection"
        );
    }

    // ── Task 7: incremental_domains emission and compression ──────────────────

    /// Test helper: build a router with the given DomainConfig, send GET /repo/caps,
    /// and deserialise the response body as Caps.
    async fn caps_for_router(router: Router) -> naiad_netproto::Caps {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        serde_json::from_slice::<naiad_netproto::Caps>(&body).unwrap()
    }

    /// `caps_handler` must emit `incremental_domains` correctly per backend type:
    /// - Mirror-mode sha256 (native sha256, no snapshot) → `["sha256"]`
    /// - Plain blake3 (native blake3, no snapshot) → `["blake3"]`
    /// - Dual-domain snapshot (native blake3 + snapshot sha256) → `["blake3"]`
    ///   (sha256 excluded because the snapshot backend has no sequence numbers)
    /// `mapping_incremental` stays `true` for pre-#142 wire compatibility.
    #[tokio::test]
    async fn caps_incremental_domains_per_backend() {
        // Mirror mode: native sha256, no snapshot ⇒ ["sha256"].
        {
            let store = RepoStore::open_in_memory().unwrap();
            let router = app_domains(
                Arc::new(Mutex::new(store)),
                None,
                1000,
                None,
                None,
                DomainConfig::native_only(HashDomain::Sha256),
            );
            let caps = caps_for_router(router).await;
            assert_eq!(
                caps.incremental_domains,
                Some(vec!["sha256".to_string()]),
                "mirror sha256: incremental_domains must be [sha256]"
            );
            assert!(
                caps.mapping_incremental,
                "mapping_incremental stays true for wire back-compat"
            );
        }

        // Plain blake3: native blake3, no snapshot ⇒ ["blake3"].
        {
            let store = RepoStore::open_in_memory().unwrap();
            let router = app_domains(
                Arc::new(Mutex::new(store)),
                None,
                1000,
                None,
                None,
                DomainConfig::native_only(HashDomain::Blake3),
            );
            let caps = caps_for_router(router).await;
            assert_eq!(
                caps.incremental_domains,
                Some(vec!["blake3".to_string()]),
                "plain blake3: incremental_domains must be [blake3]"
            );
        }

        // Dual-domain snapshot: native blake3 + snapshot sha256 ⇒ ["blake3"]
        // (sha256 excluded — snapshot backend has no seq counter).
        {
            let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
            let caps = caps_for_router(router).await;
            assert_eq!(
                caps.incremental_domains,
                Some(vec!["blake3".to_string()]),
                "dual-domain snapshot: sha256 must be excluded from incremental_domains"
            );
        }
    }

    /// A request that carries `Accept-Encoding: gzip` against the compressed
    /// repo router must receive a `Content-Encoding: gzip` response that
    /// decodes back to a valid `Caps` JSON body.
    #[tokio::test]
    async fn compressed_response_round_trips() {
        use axum::body::to_bytes;
        use axum::http::{Request, header};
        use tower::ServiceExt;

        let store = RepoStore::open_in_memory().unwrap();
        let app = app_domains(
            Arc::new(Mutex::new(store)),
            None,
            1000,
            None,
            None,
            DomainConfig::native_only(HashDomain::Blake3),
        );
        let res = app
            .oneshot(
                Request::builder()
                    .uri(REPO_CAPS)
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers()
                .get(header::CONTENT_ENCODING)
                .map(|v| v.as_bytes()),
            Some(b"gzip".as_ref()),
            "response with Accept-Encoding: gzip must carry Content-Encoding: gzip"
        );
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let mut gz = flate2::read::GzDecoder::new(&body[..]);
        let mut out = String::new();
        std::io::Read::read_to_string(&mut gz, &mut out).unwrap();
        let _caps: naiad_netproto::Caps = serde_json::from_str(&out).unwrap();
    }

    // ── Budget / 413 tests ────────────────────────────────────────────────────

    /// A second fixture sha with a distinct leading byte (0x22) so it lands in a
    /// different 256-bit bucket than FIX_SHA (0x11). Exactly 64 hex chars.
    const FIX_SHA2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    /// Like [`dual_domain_router`], but with an explicit bucket budget and a
    /// caller-supplied mapping set, so a test can size the union precisely.
    fn dual_domain_router_budget(
        max_query_bits: u32,
        budget: usize,
        mappings: &[(&str, &str)],
    ) -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        naiad_plugin_hydrus::fixture::write_snapshot(dir.path(), 9, mappings)
            .expect("write snapshot fixture");
        let backend =
            crate::domain::SnapshotBackend::open(dir.path(), Some(9)).expect("open backend");
        let domains = crate::domain::DomainConfig {
            native: HashDomain::Blake3,
            added_sha256: Some(Arc::new(backend) as Arc<dyn crate::domain::Sha256Backend>),
            max_query_bits,
            min_query_bits: crate::domain::SNAPSHOT_MIN_QUERY_BITS,
        };
        let store = RepoStore::open_in_memory().unwrap();
        let router = app_domains_budget(
            Arc::new(Mutex::new(store)),
            None,
            1000,
            None,
            None,
            domains,
            budget,
            false,
        );
        (router, dir)
    }

    #[tokio::test]
    async fn buckets_snapshot_union_under_budget_is_ok() {
        let mappings = [(FIX_SHA, "maid"), (FIX_SHA2, "maid")];
        // Generous budget: both single-row buckets fit comfortably.
        let (router, _dir) = dual_domain_router_budget(256, 1_000_000, &mappings);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA, FIX_SHA2],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 200, "under budget must be 200: {body}");
        let snap: naiad_netproto::Snapshot = serde_json::from_str(&body).unwrap();
        assert!(
            snap.tags.contains_key(FIX_SHA) && snap.tags.contains_key(FIX_SHA2),
            "the union covers both buckets: {body}"
        );
    }

    #[tokio::test]
    async fn buckets_snapshot_union_over_budget_is_413_with_remedy() {
        let mappings = [(FIX_SHA, "maid"), (FIX_SHA2, "maid")];
        // Budget admits exactly one bucket's single row; the union of two trips it.
        let budget = naiad_core::approx_row_cost(64, "maid".len());
        let (router, _dir) = dual_domain_router_budget(256, budget, &mappings);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA, FIX_SHA2],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 413, "union over budget must be 413: {body}");
        assert!(
            body.contains("prefix_bits") && body.contains("fewer keys"),
            "413 body carries the remedy text: {body}"
        );
        assert!(
            !body.contains('/') && !body.contains('\\'),
            "413 body must leak no filesystem path (#159): {body}"
        );
    }

    #[tokio::test]
    async fn buckets_snapshot_single_oversized_bucket_is_413() {
        // One bucket, two tags, budget below even one row: the 413 can only come
        // from INSIDE mappings_for_prefix's drain, not between buckets.
        let mappings = [(FIX_SHA, "character:samus"), (FIX_SHA, "maid")];
        let (router, _dir) = dual_domain_router_budget(256, 1, &mappings);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 413, "a single oversized bucket must 413: {body}");
        assert_eq!(
            body, BUCKET_BUDGET_REMEDY,
            "exact static remedy body: {body}"
        );
    }

    /// A 64-hex native (blake3-domain) hash for the RepoStore native branch.
    const FIX_BLAKE: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    /// A native-only router with an explicit bucket budget, its RepoStore seeded
    /// with two current mappings on FIX_BLAKE via the trusted bulk path.
    fn native_router_budget(budget: usize) -> Router {
        let store = RepoStore::open_in_memory().unwrap();
        let seed: Vec<(String, String, bool)> = vec![
            (FIX_BLAKE.to_string(), "character:samus".to_string(), false),
            (FIX_BLAKE.to_string(), "maid".to_string(), false),
        ];
        store.apply_mappings_bulk(seed).unwrap();
        app_domains_budget(
            Arc::new(Mutex::new(store)),
            None,
            1000,
            None,
            None,
            crate::domain::DomainConfig::native_only(HashDomain::Blake3),
            budget,
            false,
        )
    }

    #[tokio::test]
    async fn buckets_native_snapshot_under_budget_is_ok() {
        let router = native_router_budget(1_000_000);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_BLAKE],
            }),
        )
        .await;
        assert_eq!(status, 200, "native happy path stays 200: {body}");
        let snap: naiad_netproto::Snapshot = serde_json::from_str(&body).unwrap();
        assert!(
            snap.tags.contains_key(FIX_BLAKE),
            "native tags present: {body}"
        );
    }

    #[tokio::test]
    async fn buckets_native_snapshot_over_budget_is_413() {
        // since = None → native snapshot path (RepoStore::bucket). Budget below
        // one row forces the 413 from inside the drain.
        let router = native_router_budget(1);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_BLAKE],
            }),
        )
        .await;
        assert_eq!(status, 413, "native snapshot path must be guarded: {body}");
        assert_eq!(body, BUCKET_BUDGET_REMEDY, "exact remedy body: {body}");
    }

    #[tokio::test]
    async fn buckets_native_delta_over_budget_is_413() {
        // since = Some(..) → native delta path (RepoStore::bucket_delta).
        let router = native_router_budget(1);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_BLAKE],
                "since": [0],
            }),
        )
        .await;
        assert_eq!(status, 413, "native delta path must be guarded: {body}");
        assert_eq!(body, BUCKET_BUDGET_REMEDY, "exact remedy body: {body}");
    }

    /// Guard for the ConnectInfo extractor wiring: confirms that every in-crate
    /// oneshot test keeps passing with the cfg(test) MockConnectInfo layer and
    /// that the extractor does not panic when the layer is present.
    #[tokio::test]
    async fn read_handlers_have_connect_info() {
        use tower::ServiceExt;
        // test_router() now carries the cfg(test) MockConnectInfo layer.
        let req = axum::http::Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = test_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK); // no ConnectInfo-missing panic
    }

    #[test]
    fn is_budget_exceeded_detects_wrapped_and_ignores_plain() {
        use anyhow::Context as _;
        // Wrapped deep in a `.context` chain exactly as SnapshotBackend::bucket
        // wraps it — the predicate must still find it, so it maps to 413.
        let wrapped: anyhow::Error = Err::<(), _>(naiad_core::BudgetExceeded { budget: 64 })
            .context("querying Hydrus snapshot /some/server/path at 8 prefix bits")
            .unwrap_err();
        assert!(
            is_budget_exceeded(&wrapped),
            "BudgetExceeded buried under path context must be found (→ 413)"
        );
        // A plain, unrelated backend error is NOT a budget error: it must fall
        // through to the existing bare-500 arm, never a 413 (#159 — the 500 path
        // logs the full error, which may carry a snapshot path, and returns an
        // empty body; that arm is unchanged by this work).
        let plain = anyhow::anyhow!("no such table: mappings.current_mappings_9");
        assert!(
            !is_budget_exceeded(&plain),
            "a non-budget error must not be reclassified as 413"
        );
    }

    // ── Task 2: ServeStats EWMA unit tests (#173) ─────────────────────────────

    /// First sample seeds the EWMA directly (no prior state, NaN seeds to
    /// sample). Second sample folds in via EWMA_ALPHA.
    #[test]
    fn serve_stats_ewma_seeds_and_updates() {
        // #178: None ref_bits = raw fold (pre-#178 behaviour); bits arg is unused.
        let stats = ServeStats::new(None);

        // Unset domain → None.
        assert!(
            stats.hint(HashDomain::Blake3).is_none(),
            "fresh stats must return None before any sample"
        );

        // First sample seeds directly.
        stats.record(HashDomain::Blake3, 10.0, 32);
        let after_first = stats
            .hint(HashDomain::Blake3)
            .expect("must be Some after first sample");
        assert!(
            (after_first - 10.0).abs() < 1e-9,
            "first sample must seed EWMA directly: got {after_first}"
        );

        // Second sample: expected = 0.3 * 20.0 + 0.7 * 10.0 = 6.0 + 7.0 = 13.0.
        stats.record(HashDomain::Blake3, 20.0, 32);
        let after_second = stats
            .hint(HashDomain::Blake3)
            .expect("must be Some after second sample");
        let expected = EWMA_ALPHA * 20.0 + (1.0 - EWMA_ALPHA) * 10.0;
        assert!(
            (after_second - expected).abs() < 1e-9,
            "second sample must fold via EWMA_ALPHA: got {after_second}, expected {expected}"
        );
    }

    /// Recording blake3 must leave the sha256 slot untouched (None).
    #[test]
    fn serve_stats_domains_are_independent() {
        let stats = ServeStats::new(None);
        stats.record(HashDomain::Blake3, 5.0, 32);
        assert!(
            stats.hint(HashDomain::Blake3).is_some(),
            "blake3 slot must be set after recording blake3"
        );
        assert!(
            stats.hint(HashDomain::Sha256).is_none(),
            "sha256 slot must remain None when only blake3 was recorded"
        );
    }

    // ── Task 2: #178 normalisation unit tests ────────────────────────────────

    /// #178: samples are normalised to `ref_bits` before entering the EWMA, in
    /// both directions, and the first sample seeds directly (normalised).
    #[test]
    fn serve_stats_normalises_to_ref_bits() {
        let stats = ServeStats::new(Some(32));
        // Fine sample at the reference width: no shift.
        stats.record(HashDomain::Sha256, 0.17, 32);
        let h = stats.hint(HashDomain::Sha256).unwrap();
        assert!(
            (h - 0.17).abs() < 1e-9,
            "seed at ref width must be unshifted: {h}"
        );
        // Coarse sample at 24 bits folds as 60 * 2^-8 = 0.234375 via EWMA.
        stats.record(HashDomain::Sha256, 60.0, 24);
        let expected = EWMA_ALPHA * (60.0 / 256.0) + (1.0 - EWMA_ALPHA) * 0.17;
        let h = stats.hint(HashDomain::Sha256).unwrap();
        assert!(
            (h - expected).abs() < 1e-6,
            "coarse sample must scale DOWN: {h} vs {expected}"
        );
        // Finer-than-ref direction on a fresh domain: 1.0 at 40 bits seeds 2^8 = 256.
        stats.record(HashDomain::Blake3, 1.0, 40);
        let h = stats.hint(HashDomain::Blake3).unwrap();
        assert!((h - 256.0).abs() < 1e-9, "fine sample must scale UP: {h}");
    }

    /// #178: `ref_bits = None` (mirror/advise repo) folds raw — pre-#178 behaviour.
    #[test]
    fn serve_stats_raw_fold_without_ref_bits() {
        let stats = ServeStats::new(None);
        stats.record(HashDomain::Sha256, 60.0, 24);
        let h = stats.hint(HashDomain::Sha256).unwrap();
        assert!(
            (h - 60.0).abs() < 1e-9,
            "None ref_bits must store the raw sample: {h}"
        );
    }

    /// #178: pathological width pairs saturate at ±HINT_SHIFT_CLAMP instead of
    /// driving the f64 to Inf or 0.
    #[test]
    fn serve_stats_shift_clamp_saturates() {
        let stats = ServeStats::new(Some(0));
        stats.record(HashDomain::Sha256, 1.0, 200);
        let h = stats.hint(HashDomain::Sha256).unwrap();
        assert!(
            h.is_finite() && (h - 2f64.powi(40)).abs() < 1.0,
            "overflow side: {h}"
        );
        let stats = ServeStats::new(Some(200));
        stats.record(HashDomain::Sha256, 1.0, 0);
        let h = stats.hint(HashDomain::Sha256).unwrap();
        assert!(
            h > 0.0 && (h - 2f64.powi(-40)).abs() < 1e-15,
            "underflow side: {h}"
        );
    }

    /// Fresh-boot caps JSON contains no `serve_hint` key (the serde
    /// `skip_serializing_if` attr from Task 1 silences an empty map).
    #[tokio::test]
    async fn caps_fresh_boot_has_no_serve_hint_key() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = test_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            !body.contains("serve_hint"),
            "fresh-boot caps must omit the serve_hint key entirely: {body}"
        );
    }

    /// After one bucket POST, caps carry the served domain with a positive
    /// finite ms_per_bucket value.
    #[tokio::test]
    async fn caps_serve_hint_populated_after_one_bucket_serve() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let store = RepoStore::open_in_memory().unwrap();
        // Seed one mapping so the bucket response is non-empty.
        store
            .apply_mappings_bulk(vec![(FIX_BLAKE.to_string(), "maid".to_string(), false)])
            .unwrap();
        let app = app_domains(
            Arc::new(Mutex::new(store)),
            None,
            1000,
            None,
            None,
            crate::domain::DomainConfig::native_only(HashDomain::Blake3),
        );

        // POST /repo/buckets to record a sample.
        let bucket_req = Request::builder()
            .method("POST")
            .uri(REPO_BUCKETS)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "prefix_bits": 256,
                    "buckets": [FIX_BLAKE],
                }))
                .unwrap(),
            ))
            .unwrap();
        // Router is Clone; clone it before oneshot consumes it.
        let app_for_caps = app.clone();
        let bucket_resp = app.oneshot(bucket_req).await.unwrap();
        assert_eq!(
            bucket_resp.status().as_u16(),
            200,
            "bucket POST must succeed"
        );

        let caps_req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let caps_resp = app_for_caps.oneshot(caps_req).await.unwrap();
        assert_eq!(caps_resp.status().as_u16(), 200);
        let caps_bytes = to_bytes(caps_resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&caps_bytes).unwrap();

        let hint = caps
            .serve_hint
            .get("blake3")
            .expect("caps must contain blake3 serve_hint after a bucket serve");
        assert!(
            hint.ms_per_bucket.is_finite() && hint.ms_per_bucket >= 0.0,
            "ms_per_bucket must be a non-negative finite value: {}",
            hint.ms_per_bucket
        );
        // #178: native-only repo has no snapshot backend → ref_bits = None →
        // hint_bits must be absent (None) from every serve_hint entry.
        assert_eq!(
            hint.hint_bits, None,
            "native-only repo must not stamp hint_bits (no snapshot backend)"
        );
    }

    /// Dual-domain repo: serve one blake3 bucket + one sha256 bucket → both
    /// keys present in serve_hint with independent finite values.
    #[tokio::test]
    async fn caps_serve_hint_dual_domain_both_keys_present() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let (app, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);

        // POST blake3 bucket (native store, seeded implicitly as empty — bucket
        // returns successfully even with no data as long as key is valid).
        let blake3_req = Request::builder()
            .method("POST")
            .uri(REPO_BUCKETS)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "prefix_bits": 256,
                    "buckets": [FIX_BLAKE],
                }))
                .unwrap(),
            ))
            .unwrap();

        // POST sha256 bucket (snapshot backend).
        let sha256_req = Request::builder()
            .method("POST")
            .uri(REPO_BUCKETS)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "prefix_bits": 256,
                    "buckets": [FIX_SHA],
                    "domain": "sha256",
                }))
                .unwrap(),
            ))
            .unwrap();

        let app_b3 = app.clone();
        let app_sha = app.clone();
        let app_caps = app.clone();

        let r1 = app_b3.oneshot(blake3_req).await.unwrap();
        assert_eq!(
            r1.status().as_u16(),
            200,
            "blake3 bucket serve must succeed"
        );

        let r2 = app_sha.oneshot(sha256_req).await.unwrap();
        assert_eq!(
            r2.status().as_u16(),
            200,
            "sha256 bucket serve must succeed"
        );

        let caps_req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let caps_resp = app_caps.oneshot(caps_req).await.unwrap();
        assert_eq!(caps_resp.status().as_u16(), 200);
        let caps_bytes = to_bytes(caps_resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&caps_bytes).unwrap();

        let b3_hint = caps
            .serve_hint
            .get("blake3")
            .expect("blake3 key must be present after serving blake3 buckets");
        let sha_hint = caps
            .serve_hint
            .get("sha256")
            .expect("sha256 key must be present after serving sha256 buckets");

        assert!(
            b3_hint.ms_per_bucket.is_finite() && b3_hint.ms_per_bucket >= 0.0,
            "blake3 ms_per_bucket must be finite non-negative: {}",
            b3_hint.ms_per_bucket
        );
        assert!(
            sha_hint.ms_per_bucket.is_finite() && sha_hint.ms_per_bucket >= 0.0,
            "sha256 ms_per_bucket must be finite non-negative: {}",
            sha_hint.ms_per_bucket
        );
        // The two values are independent — no assertion on equality.
        // #178: dual_domain_router uses max_query_bits=256 with a snapshot backend
        // → ref_bits = Some(256) → hint_bits must be Some(256) on all entries.
        assert_eq!(
            b3_hint.hint_bits,
            Some(256),
            "snapshot-backed repo must stamp hint_bits = max_query_bits on blake3 entry"
        );
        assert_eq!(
            sha_hint.hint_bits,
            Some(256),
            "snapshot-backed repo must stamp hint_bits = max_query_bits on sha256 entry"
        );
    }

    // ── #176 Streaming tests (spec §5.2, tests 9–15) ──────────────────────────

    /// Parse all NDJSON lines from a response body. Returns (header, rows, trailer).
    async fn parse_ndjson_body(
        body: axum::body::Body,
    ) -> (
        naiad_netproto::StreamHeader,
        Vec<naiad_netproto::StreamRow>,
        naiad_netproto::StreamTrailer,
    ) {
        use axum::body::to_bytes;
        let bytes = to_bytes(body, 64 * 1024 * 1024).await.unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let mut lines = text.lines();

        let header_line = lines.next().expect("header line");
        let header: naiad_netproto::StreamHeader = serde_json::from_str(header_line)
            .unwrap_or_else(|e| panic!("parse header {header_line:?}: {e}"));

        let mut rows = Vec::new();
        let mut trailer = None;
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("parse ndjson line {line:?}: {e}"));
            if v.get("done").is_some() || v.get("more").is_some() || v.get("err").is_some() {
                trailer = Some(
                    serde_json::from_value::<naiad_netproto::StreamTrailer>(v)
                        .unwrap_or_else(|e| panic!("parse trailer {line:?}: {e}")),
                );
            } else if v.get("h").is_some() {
                let row: naiad_netproto::StreamRow =
                    serde_json::from_value(v).unwrap_or_else(|e| panic!("parse row {line:?}: {e}"));
                rows.push(row);
            }
        }
        (header, rows, trailer.expect("trailer line"))
    }

    /// Helper to POST /repo/buckets as JSON and return the raw response.
    async fn post_buckets_raw(
        router: Router,
        body: serde_json::Value,
    ) -> axum::http::Response<axum::body::Body> {
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder()
            .method("POST")
            .uri(REPO_BUCKETS)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        router.oneshot(req).await.unwrap()
    }

    /// Test 9: POST /repo/buckets with stream:true → application/x-ndjson,
    /// header + rows + done trailer; parsed union equals the materialized response.
    #[tokio::test]
    async fn buckets_streams_when_opted_in() {
        let mappings = [(FIX_SHA, "character:samus"), (FIX_SHA, "maid")];
        let (router_stream, _dir1) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let (router_mat, _dir2) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);

        // Streaming request.
        let stream_resp = post_buckets_raw(
            router_stream,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA],
                "domain": "sha256",
                "stream": true,
            }),
        )
        .await;
        assert_eq!(stream_resp.status(), 200, "streaming must be 200");
        let ct = stream_resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("application/x-ndjson"),
            "streaming must set application/x-ndjson content-type: {ct}"
        );
        let (header, rows, trailer) = parse_ndjson_body(stream_resp.into_body()).await;
        assert_eq!(header.version, PROTOCOL_VERSION);
        assert!(matches!(
            trailer,
            naiad_netproto::StreamTrailer::Done { .. }
        ));
        // The two tags for FIX_SHA must appear in the rows.
        let all_tags: Vec<_> = rows
            .iter()
            .flat_map(|r| r.t.iter().map(|t| t.tag.as_str()))
            .collect();
        assert!(
            all_tags.contains(&"character:samus"),
            "samus tag in rows: {all_tags:?}"
        );
        assert!(all_tags.contains(&"maid"), "maid tag in rows: {all_tags:?}");

        // Materialized request (no stream field) — must be application/json.
        let mat_resp = post_buckets_raw(
            router_mat,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(mat_resp.status(), 200);
        let mat_ct = mat_resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            mat_ct.contains("application/json"),
            "materialized is application/json: {mat_ct}"
        );
        let _ = mappings; // suppress lint
    }

    /// Test 9b: Native streamed-vs-materialized equivalence. Same request with
    /// and without `stream:true` against native `since=None` → identical tag
    /// union and cursor.
    #[tokio::test]
    async fn native_buckets_stream_equals_materialized() {
        let router_stream = native_router_budget(1_000_000);
        let router_mat = native_router_budget(1_000_000);

        // Streaming request.
        let stream_resp = post_buckets_raw(
            router_stream,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_BLAKE],
                "stream": true,
            }),
        )
        .await;
        assert_eq!(stream_resp.status(), 200, "native streaming must be 200");
        let ct = stream_resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("application/x-ndjson"),
            "native streaming must set application/x-ndjson: {ct}"
        );
        let (stream_header, stream_rows, stream_trailer) =
            parse_ndjson_body(stream_resp.into_body()).await;
        assert_eq!(stream_header.version, PROTOCOL_VERSION);
        assert!(
            matches!(stream_trailer, naiad_netproto::StreamTrailer::Done { .. }),
            "native streaming must finish with done trailer"
        );

        // Materialized request (no stream field).
        let (mat_status, mat_body) = post_buckets(
            router_mat,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_BLAKE],
            }),
        )
        .await;
        assert_eq!(
            mat_status, 200,
            "native materialized must be 200: {mat_body}"
        );
        let mat_snap: naiad_netproto::Snapshot = serde_json::from_str(&mat_body)
            .unwrap_or_else(|e| panic!("materialized parse failed: {e}\n{mat_body}"));

        // Build tag union from streaming rows.
        let stream_tags: std::collections::BTreeSet<_> = stream_rows
            .iter()
            .flat_map(|r| r.t.iter().map(|t| t.tag.as_str()))
            .collect();
        let mat_tags: std::collections::BTreeSet<_> = mat_snap
            .tags
            .values()
            .flat_map(|ts| ts.iter().map(|t| t.tag.as_str()))
            .collect();
        assert_eq!(
            stream_tags, mat_tags,
            "streamed and materialized tag sets must be identical"
        );
        assert_eq!(
            stream_header.cursor, mat_snap.cursor,
            "streamed and materialized cursors must match"
        );
    }

    /// Test 10: Same request with stream unset → application/json, byte-identical
    /// to pre-#176 (old-client compat guard).
    #[tokio::test]
    async fn buckets_materialized_fallback_without_stream_flag() {
        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let (status, body) = post_buckets(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA],
                "domain": "sha256",
            }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        // Must be valid JSON Snapshot (not NDJSON).
        let snap: naiad_netproto::Snapshot = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("must be materialized Snapshot JSON: {e}\nbody: {body}"));
        assert!(
            snap.tags.contains_key(FIX_SHA),
            "fixture hash present: {body}"
        );
    }

    /// Test 11: Budget cutoff → `more` trailer; follow-up with resume_at gets
    /// the remainder + done; union of both equals the full materialized response.
    #[tokio::test]
    async fn buckets_stream_budget_cutoff_yields_more_and_resume() {
        // Two distinct sha256 buckets. Budget admits exactly one; the second triggers cutoff.
        let row_cost = naiad_core::approx_row_cost(64, "maid".len());
        let mappings = [(FIX_SHA, "maid"), (FIX_SHA2, "maid")];
        let (router1, _dir1) = dual_domain_router_budget(256, row_cost, &mappings);
        let (router2, _dir2) = dual_domain_router_budget(256, row_cost, &mappings);

        // First streaming request: must get a `more` trailer.
        let resp1 = post_buckets_raw(
            router1,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA, FIX_SHA2],
                "domain": "sha256",
                "stream": true,
            }),
        )
        .await;
        assert_eq!(resp1.status(), 200);
        let (_, rows1, trailer1) = parse_ndjson_body(resp1.into_body()).await;
        let cursor_key = match &trailer1 {
            naiad_netproto::StreamTrailer::More { more } => more.clone(),
            other => panic!("expected More trailer, got {other:?}"),
        };
        assert!(!rows1.is_empty(), "first response has at least one row");

        // Second streaming request: resume from the cursor key → done.
        let resp2 = post_buckets_raw(
            router2,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA, FIX_SHA2],
                "domain": "sha256",
                "stream": true,
                "resume_at": cursor_key,
            }),
        )
        .await;
        assert_eq!(resp2.status(), 200);
        let (_, rows2, trailer2) = parse_ndjson_body(resp2.into_body()).await;
        assert!(
            matches!(trailer2, naiad_netproto::StreamTrailer::Done { .. }),
            "second response must be done: {trailer2:?}"
        );

        // Union of both responses must cover both hashes.
        let all_hashes: std::collections::HashSet<_> = rows1
            .iter()
            .chain(rows2.iter())
            .map(|r| r.h.as_str())
            .collect();
        assert!(all_hashes.contains(FIX_SHA), "FIX_SHA in union");
        assert!(all_hashes.contains(FIX_SHA2), "FIX_SHA2 in union");
    }

    /// Test 12: Single oversized bucket → err trailer (not 413 — bytes already flowed).
    /// The error message must not contain a filesystem path (#159 guard).
    #[tokio::test]
    async fn buckets_stream_single_oversized_bucket_yields_err_trailer() {
        let mappings = [(FIX_SHA, "character:samus"), (FIX_SHA, "maid")];
        let (router, _dir) = dual_domain_router_budget(256, 1, &mappings);

        let resp = post_buckets_raw(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA],
                "domain": "sha256",
                "stream": true,
            }),
        )
        .await;
        // Still 200 — bytes have already flowed (header was emitted).
        assert_eq!(resp.status(), 200, "streaming always 200");
        let (_, _rows, trailer) = parse_ndjson_body(resp.into_body()).await;
        let err_msg = match trailer {
            naiad_netproto::StreamTrailer::Err { err } => err,
            other => panic!("expected Err trailer, got {other:?}"),
        };
        // #159: error must not leak a filesystem path.
        assert!(
            !err_msg.contains('/') && !err_msg.contains('\\'),
            "err trailer must not leak filesystem paths: {err_msg}"
        );
        assert!(
            err_msg.contains("budget"),
            "err message mentions budget: {err_msg}"
        );
    }

    /// Test 13: Native since=None streams; since=Some does NOT (stays materialized).
    #[tokio::test]
    async fn buckets_native_since_none_streams_since_some_does_not() {
        let router_stream = native_router_budget(1_000_000);
        let router_delta = native_router_budget(1_000_000);

        // since=None + stream:true → NDJSON.
        let snap_resp = post_buckets_raw(
            router_stream,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_BLAKE],
                "stream": true,
            }),
        )
        .await;
        assert_eq!(snap_resp.status(), 200);
        let ct = snap_resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("x-ndjson"),
            "since=None with stream:true → NDJSON: {ct}"
        );
        let (_, rows, trailer) = parse_ndjson_body(snap_resp.into_body()).await;
        assert!(matches!(
            trailer,
            naiad_netproto::StreamTrailer::Done { .. }
        ));
        let tags: Vec<_> = rows
            .iter()
            .flat_map(|r| r.t.iter().map(|t| t.tag.as_str()))
            .collect();
        assert!(
            tags.contains(&"character:samus"),
            "snapshot rows have data: {tags:?}"
        );

        // since=Some(..) + stream:true → still materialized application/json (non-goal).
        let delta_resp = post_buckets_raw(
            router_delta,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_BLAKE],
                "since": [0],
                "stream": true,
            }),
        )
        .await;
        assert_eq!(delta_resp.status(), 200);
        let delta_ct = delta_resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            delta_ct.contains("application/json"),
            "since=Some with stream:true → materialized JSON (non-goal): {delta_ct}"
        );
    }

    /// Test 14: First byte arrives early — the header line is the very first
    /// NDJSON line in the response body, emitted before any bucket row data.
    /// Uses a real TCP listener + ureq (blocking HTTP client, via spawn_blocking)
    /// so chunked-transfer encoding is handled transparently and the body reader
    /// can be consumed incrementally without buffering the whole response first.
    #[tokio::test]
    async fn buckets_stream_header_arrives_before_all_buckets_processed() {
        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);

        // Bind a real TCP listener so axum can serve a genuine streaming response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        );
        tokio::spawn(async move { server.await.unwrap_or(()) });

        let request_body = serde_json::json!({
            "version": PROTOCOL_VERSION,
            "prefix_bits": 256,
            "buckets": [FIX_SHA],
            "domain": "sha256",
            "stream": true,
        });
        let body_bytes = serde_json::to_vec(&request_body).unwrap();
        let url = format!("http://{}/repo/buckets", addr);

        // ureq is a blocking HTTP/1.1 client; use spawn_blocking so the tokio
        // runtime thread is not stalled while the server task runs.
        let first_line = tokio::task::spawn_blocking(move || {
            let resp = ureq::post(&url)
                .set("content-type", "application/json")
                .send_bytes(&body_bytes)
                .expect("ureq request failed");
            // into_reader() gives a body reader that handles chunked encoding;
            // we read only the very first line so the header's early-delivery
            // property is naturally exercised.
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(resp.into_reader());
            let mut line = String::new();
            reader.read_line(&mut line).expect("read first line");
            line.trim_end_matches(['\r', '\n']).to_string()
        })
        .await
        .expect("spawn_blocking panicked");

        // The first line must parse as StreamHeader — proving header-before-rows.
        let header: naiad_netproto::StreamHeader = serde_json::from_str(&first_line)
            .unwrap_or_else(|e| {
                panic!("first NDJSON line must be StreamHeader: {e}\ngot: {first_line}")
            });
        assert_eq!(header.version, PROTOCOL_VERSION);
    }

    /// Test 15: GET /repo/caps on a streaming-capable server has streaming:true;
    /// the wire snapshot confirms this is the only new key vs pre-#176.
    #[tokio::test]
    async fn caps_advertises_streaming_true() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = test_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&bytes).unwrap();
        assert!(caps.streaming, "server caps must advertise streaming: true");

        // The wire form must contain exactly "streaming":true.
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("\"streaming\":true"),
            "wire caps must contain \"streaming\":true: {body}"
        );
    }

    // ── #178 Task 3: streaming serve-hint tests ───────────────────────────────

    /// §9.2 test 8 (unit level — math proof): demonstrates the two possible
    /// outcomes so the HTTP-level guard below can bound the correct one. With
    /// `ref_bits == clamped_bits` the normalisation factor is 2^0 = 1, so the
    /// stored hint equals the raw sample. With `req_bits=256 >> ref_bits=24` the
    /// shift is 232, clamped to HINT_SHIFT_CLAMP=40, so the stored value is
    /// `raw × 2^40 ≈ 1.1e12 × raw`. The HTTP-level guard below exploits this
    /// separation: even a 1 µs raw sample produces ≈ 1e9 on the regressed path,
    /// well above the < 1e6 assertion threshold.
    #[test]
    fn serve_stats_clamped_bits_vs_req_bits_differ_by_shift() {
        // Clamped path: ref_bits=24, bits=24 → factor 2^0 = 1 (no shift).
        let stats_clamped = ServeStats::new(Some(24));
        stats_clamped.record(HashDomain::Sha256, 1.0, 24);
        let hint_clamped = stats_clamped.hint(HashDomain::Sha256).unwrap();
        assert!(
            (hint_clamped - 1.0).abs() < 1e-9,
            "clamped fold (bits==ref) stores the raw sample unchanged: {hint_clamped}"
        );
        // Unclamped (regressed) path: ref_bits=24, bits=256 → shift=232 clamped to 40 → factor 2^40.
        let stats_raw = ServeStats::new(Some(24));
        stats_raw.record(HashDomain::Sha256, 1.0, 256);
        let hint_raw = stats_raw.hint(HashDomain::Sha256).unwrap();
        let expected_raw = 2f64.powi(naiad_netproto::HINT_SHIFT_CLAMP);
        assert!(
            (hint_raw - expected_raw).abs() < 1.0,
            "unclamped fold (bits=256 >> ref=24, shift clamped to 40) scales by 2^40: {hint_raw} vs {expected_raw}"
        );
        // The two values differ by 2^40 — a regressed call site with bits=256
        // instead of clamped bits=24 produces a value ≈ 1.1e12× larger.
        assert!(
            hint_raw > hint_clamped * 1e11,
            "clamped and unclamped must be separated by > 11 orders of magnitude: {hint_clamped} vs {hint_raw}"
        );
    }

    /// §9.2 test 8 (HTTP-level regression guard): streaming snapshot-domain serve
    /// with `prefix_bits=256` against a `max_query_bits=24` fixture — so
    /// `clamp_query_bits` reduces bits to 24, giving shift=0 and factor=1.
    ///
    /// Relies on fractional-ms sampling (as_secs_f64 × 1000): an in-memory
    /// `backend.bucket()` call completes in microseconds (≈ 0.001–0.1 ms), yielding
    /// a stored sample in that range. A regressed call site passing raw
    /// `req.prefix_bits=256` with `ref_bits=24` produces shift=232, clamped to
    /// `HINT_SHIFT_CLAMP=40`, factor `2^40 ≈ 1.1e12` — even 1 µs becomes ≈ 1.1e9.
    /// The `> 0.0` assertion also pins the fractional-ms fix: with integer-ms
    /// sampling a sub-ms scan would truncate to 0 and the upper bound would be
    /// vacuous (0 × 2^40 = 0). Both assertions together are robust.
    #[tokio::test]
    async fn streaming_snapshot_clamped_bits_http_regression() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        // max_query_bits=24 → ref_bits=Some(24). Request with prefix_bits=256
        // (above the ceiling) → clamp_query_bits returns 24 → shift=0, factor=1.
        // The fixture has real rows (FIX_SHA) so backend.bucket() does real work.
        let (router, _dir) = dual_domain_router(24, SNAPSHOT_MIN_QUERY_BITS);
        let router_caps = router.clone();

        let stream_resp = post_buckets_raw(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,       // finer than ceiling; clamped to 24
                "buckets": [FIX_SHA],
                "domain": "sha256",
                "stream": true,
            }),
        )
        .await;
        assert_eq!(
            stream_resp.status(),
            200,
            "streaming with prefix_bits above ceiling must succeed (clamp path)"
        );
        let (_header, _rows, _trailer) = parse_ndjson_body(stream_resp.into_body()).await;

        let caps_req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let caps_resp = router_caps.oneshot(caps_req).await.unwrap();
        assert_eq!(caps_resp.status(), 200);
        let caps_bytes = to_bytes(caps_resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&caps_bytes).unwrap();

        let hint = caps
            .serve_hint
            .get("sha256")
            .expect("serve_hint must be populated after the streaming serve");

        // ref_bits=Some(24) → hint_bits must carry that value.
        assert_eq!(
            hint.hint_bits,
            Some(24),
            "hint_bits must equal max_query_bits=24 (the ref_bits)"
        );

        // > 0.0 pins the fractional-ms fix: integer truncation would fold 0.0
        // and the upper-bound check below would be vacuous (0 × 2^40 = 0).
        assert!(
            hint.ms_per_bucket > 0.0,
            "ms_per_bucket must be positive (fractional-ms sampling, not integer truncation): {}",
            hint.ms_per_bucket
        );

        // Correct path (shift 0): stored ≈ raw sub-ms cost, well below 1e6.
        // Regressed path (bits=256, ref=24): shift clamped to 40 →
        //   stored = raw_ms × 2^40 ≥ 0.001 × 1.1e12 ≈ 1.1e9. Both the > 0 and
        //   < 1e6 assertions together make the guard non-vacuous.
        assert!(
            hint.ms_per_bucket < 1e6,
            "ms_per_bucket={} should be sub-ms (correct clamped fold, shift 0); \
             a regressed fold at shift 40 would produce ≥ 1e9",
            hint.ms_per_bucket
        );
    }

    /// #178 §4.4: a STREAMING snapshot-domain bucket serve must feed the EWMA —
    /// before this task only materialised serves recorded, so an all-streaming
    /// deployment advertised no serve_hint at all.
    #[tokio::test]
    async fn caps_serve_hint_populated_after_streaming_serve() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let (router, _dir) = dual_domain_router(256, SNAPSHOT_MIN_QUERY_BITS);
        let router_caps = router.clone();

        // POST /repo/buckets with stream:true (snapshot-domain sha256).
        let stream_resp = post_buckets_raw(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_SHA],
                "domain": "sha256",
                "stream": true,
            }),
        )
        .await;
        assert_eq!(
            stream_resp.status(),
            200,
            "streaming bucket POST must succeed"
        );

        // Drain the NDJSON body fully so the spawn_blocking producer has finished
        // (and recorded the sample) before we query caps.
        let (_header, _rows, _trailer) = parse_ndjson_body(stream_resp.into_body()).await;

        // GET /repo/caps and assert serve_hint["sha256"] is populated.
        let caps_req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let caps_resp = router_caps.oneshot(caps_req).await.unwrap();
        assert_eq!(caps_resp.status(), 200);
        let caps_bytes = to_bytes(caps_resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&caps_bytes).unwrap();

        let hint = caps
            .serve_hint
            .get("sha256")
            .expect("caps must contain sha256 serve_hint after a streaming serve (#178 §4.4)");
        assert!(
            hint.ms_per_bucket.is_finite() && hint.ms_per_bucket >= 0.0,
            "ms_per_bucket must be a non-negative finite value: {}",
            hint.ms_per_bucket
        );
        // Snapshot-backed repo with max_query_bits=256 → ref_bits=Some(256) →
        // hint_bits must be Some(256).
        assert_eq!(
            hint.hint_bits,
            Some(256),
            "snapshot-backed repo must stamp hint_bits=max_query_bits after streaming serve"
        );
    }

    /// #178 §4.4: a STREAMING native-domain (blake3) bucket serve must also feed
    /// the EWMA. Native-only repo has no snapshot backend → ref_bits=None →
    /// hint_bits must be absent.
    #[tokio::test]
    async fn caps_serve_hint_populated_after_native_streaming_serve() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let router = native_router_budget(1_000_000);
        let router_caps = router.clone();

        // POST /repo/buckets with stream:true (native blake3 domain).
        let stream_resp = post_buckets_raw(
            router,
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "prefix_bits": 256,
                "buckets": [FIX_BLAKE],
                "stream": true,
            }),
        )
        .await;
        assert_eq!(
            stream_resp.status(),
            200,
            "native streaming bucket POST must succeed"
        );

        // Drain the NDJSON body fully.
        let (_header, _rows, _trailer) = parse_ndjson_body(stream_resp.into_body()).await;

        // GET /repo/caps and assert serve_hint["blake3"] is populated.
        let caps_req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let caps_resp = router_caps.oneshot(caps_req).await.unwrap();
        assert_eq!(caps_resp.status(), 200);
        let caps_bytes = to_bytes(caps_resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&caps_bytes).unwrap();

        let hint = caps.serve_hint.get("blake3").expect(
            "caps must contain blake3 serve_hint after a native streaming serve (#178 §4.4)",
        );
        assert!(
            hint.ms_per_bucket.is_finite() && hint.ms_per_bucket >= 0.0,
            "ms_per_bucket must be a non-negative finite value: {}",
            hint.ms_per_bucket
        );
        // Native-only repo → ref_bits=None → hint_bits absent.
        assert_eq!(
            hint.hint_bits, None,
            "native-only repo must not stamp hint_bits (no snapshot backend)"
        );
    }

    // ── #179 server caps tests ────────────────────────────────────────────────

    /// §8.2 test 5: a snapshot-backed repo advertises `min_query_bits` equal to
    /// the configured floor, and that same floor is what the 400 path uses.
    #[tokio::test]
    async fn caps_advertises_min_query_bits_in_snapshot_mode() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let (router, _dir) = dual_domain_router(256, 16);

        // GET /repo/caps → min_query_bits == 16.
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            caps.min_query_bits,
            Some(16),
            "snapshot repo must advertise min_query_bits == 16"
        );

        // The 400 path rejects a below-floor request. Pin that the floor in the
        // 400 error matches the advertised one (both read from min_query_bits).
        let below_floor = serde_json::json!({
            "version": naiad_netproto::PROTOCOL_VERSION,
            "prefix_bits": 12,
            "buckets": ["00".repeat(32)],
            "domain": "sha256"
        });
        let (status, body) = post_buckets(router, below_floor).await;
        assert_eq!(status, 400, "below-floor request must be rejected: {body}");
        assert!(
            body.contains("16"),
            "400 body must mention the floor (16): {body}"
        );
    }

    /// §8.2 test 6 (blake3 leg): a blake3-native repo emits **no** `min_query_bits`
    /// key — byte-identical to pre-#179 caps, and unchanged by #195.
    #[tokio::test]
    async fn caps_omits_min_query_bits_for_blake3_native() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        // Native-only (blake3).
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = test_router().oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            !body.contains("min_query_bits"),
            "blake3-native caps must not contain min_query_bits: {body}"
        );
        let caps: naiad_netproto::Caps = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            caps.min_query_bits, None,
            "blake3-native caps must have min_query_bits == None"
        );
    }

    /// #195: a mirror-mode (native sha256, no snapshot backend) repo **must**
    /// advertise `min_query_bits = Some(floor)` so the client respects the floor
    /// on the sha256 domain even when sha256 is native.
    #[tokio::test]
    async fn caps_advertises_min_query_bits_in_mirror_mode() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let mirror_router = app_split(
            Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap())),
            None,
            1000,
            None,
            None,
            HashDomain::Sha256,
        );
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = mirror_router.oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            caps.min_query_bits,
            Some(crate::domain::SNAPSHOT_MIN_QUERY_BITS),
            "mirror-mode caps must advertise min_query_bits == SNAPSHOT_MIN_QUERY_BITS (#195)"
        );
    }

    /// #195: a mirror-mode (native sha256) bucket query below the floor must be
    /// rejected with 400, matching snapshot-mode behaviour.
    #[tokio::test]
    async fn mirror_sha256_floor_enforced_in_buckets() {
        let mirror_router = app_split(
            Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap())),
            None,
            1000,
            None,
            None,
            HashDomain::Sha256,
        );
        // SNAPSHOT_MIN_QUERY_BITS == 8; a request with prefix_bits < 8 must be rejected.
        let below_floor = serde_json::json!({
            "version": naiad_netproto::PROTOCOL_VERSION,
            "prefix_bits": 4,
            "buckets": ["00".repeat(32)],
        });
        let (status, body) = post_buckets(mirror_router, below_floor).await;
        assert_eq!(
            status, 400,
            "below-floor mirror request must be rejected: {body}"
        );
        assert!(
            body.contains(&crate::domain::SNAPSHOT_MIN_QUERY_BITS.to_string()),
            "400 body must mention the floor: {body}"
        );
    }

    /// #195: a mirror-mode bucket query at or above the floor must proceed (200),
    /// confirming the floor check does not block valid requests.
    #[tokio::test]
    async fn mirror_sha256_at_floor_proceeds() {
        let mirror_router = app_split(
            Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap())),
            None,
            1000,
            None,
            None,
            HashDomain::Sha256,
        );
        // SNAPSHOT_MIN_QUERY_BITS == 8; a request with prefix_bits == 8 must succeed.
        let at_floor = serde_json::json!({
            "version": naiad_netproto::PROTOCOL_VERSION,
            "prefix_bits": crate::domain::SNAPSHOT_MIN_QUERY_BITS,
            "buckets": ["00".repeat(32)],
        });
        let (status, body) = post_buckets(mirror_router, at_floor).await;
        assert_eq!(status, 200, "at-floor mirror request must succeed: {body}");
    }

    /// #195: a blake3-native repo must NOT apply the sha256 floor — a very coarse
    /// query (prefix_bits < SNAPSHOT_MIN_QUERY_BITS) on blake3 must return 200.
    #[tokio::test]
    async fn blake3_native_coarse_query_no_floor() {
        // prefix_bits = 2 is below SNAPSHOT_MIN_QUERY_BITS (8); must be 200 on blake3.
        let (status, body) = post_buckets(
            test_router(),
            serde_json::json!({
                "version": naiad_netproto::PROTOCOL_VERSION,
                "prefix_bits": 2,
                "buckets": ["00".repeat(32)],
            }),
        )
        .await;
        assert_eq!(
            status, 200,
            "blake3-native coarse query must not be floored (#195): {body}"
        );
    }

    // ── #195 advise/floor consistency tests ──────────────────────────────────

    /// Helper: build a mirror-mode (native sha256) router seeded with `count`
    /// distinct sha256 hashes and served with anonymity parameter `k` and a
    /// specific `floor`. Used to exercise the advise()/floor lift path.
    fn mirror_router_seeded(count: usize, k: u64, floor: u32) -> Router {
        let store = RepoStore::open_in_memory().unwrap();
        // Seed `count` distinct sha256-keyed mappings so advise() sees them.
        let seeds: Vec<(String, String, bool)> = (0..count)
            .map(|i| {
                // Produce distinct 64-char hex strings: pad i into 64 hex chars.
                let hash = format!("{:0>64x}", i);
                (hash, format!("tag:{i}"), false)
            })
            .collect();
        store.apply_mappings_bulk(seeds).unwrap();
        store.write_distinct_hash_count(count as u64).unwrap();
        let domains = crate::domain::DomainConfig {
            native: HashDomain::Sha256,
            added_sha256: None,
            max_query_bits: 256,
            min_query_bits: floor,
        };
        app_domains(Arc::new(Mutex::new(store)), None, k, None, None, domains)
    }

    /// #195 advise/floor consistency: when a mirror repo's `advise(count, k)`
    /// would return `Bucketed{bits}` with `bits < floor`, the caps handler must
    /// LIFT the advertised mode to `Bucketed{floor}` so the client's effective
    /// prefix never falls below the floor.
    ///
    /// Scenario: count=4, k=1 → advise returns Bucketed{2} (since (4/1).ilog2()=2).
    /// With floor=8: advertised mode must be Bucketed{8}, not Bucketed{2}.
    #[tokio::test]
    async fn mirror_caps_lifts_below_floor_advise_to_floor() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        // count=4, k=1: advise raw = Bucketed{(4/1).ilog2()} = Bucketed{2}
        // floor=8: must be lifted to Bucketed{8}.
        let floor = 8u32;
        let router = mirror_router_seeded(4, 1, floor);
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            caps.mode,
            naiad_netproto::PullMode::Bucketed { prefix_bits: floor },
            "mirror caps must lift below-floor advise({}) to floor={floor} (#195)",
            2u32 // raw advise result
        );
    }

    /// #195 advertise/enforce agreement: a mirror where advise was lifted to
    /// floor must ACCEPT a query at exactly the floor width (200, not 400).
    /// This is the end-to-end invariant: advertised bits == enforced floor.
    #[tokio::test]
    async fn mirror_caps_lifted_mode_query_at_floor_is_accepted() {
        let floor = 8u32;
        // Same setup as above: advise(4,1) raw = 2, lifted to 8.
        let router = mirror_router_seeded(4, 1, floor);
        // Query at the lifted (floor) width.
        let at_floor = serde_json::json!({
            "version": naiad_netproto::PROTOCOL_VERSION,
            "prefix_bits": floor,
            "buckets": ["00".repeat(32)],
        });
        let (status, body) = post_buckets(router, at_floor).await;
        assert_eq!(
            status, 200,
            "query at the lifted floor width must be accepted (200), not rejected (400): {body}"
        );
    }

    /// #195: a blake3-native repo must NOT lift its advise() result — its caps
    /// advertise the raw advise() value unchanged (no floor applies to blake3).
    #[tokio::test]
    async fn blake3_native_advise_not_lifted_by_floor() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        // test_router() uses blake3-native, k=1000. Empty store → advise(0, 1000) = WholeRepo.
        // Use native_router_budget with 2 entries and k=1 to force Bucketed{1}.
        // Then verify the mode is Bucketed{1}, not Bucketed{floor}.
        let store = RepoStore::open_in_memory().unwrap();
        // 2 distinct blake3-keyed hashes.
        let seeds: Vec<(String, String, bool)> = (0..2_usize)
            .map(|i| (format!("{:0>64x}", i), format!("tag:{i}"), false))
            .collect();
        store.apply_mappings_bulk(seeds).unwrap();
        store.write_distinct_hash_count(2).unwrap();
        // k=1: advise(2, 1) = Bucketed{1}. blake3 has no floor → must stay Bucketed{1}.
        let router = app_split(
            Arc::new(Mutex::new(store)),
            None,
            1,
            None,
            None,
            HashDomain::Blake3,
        );
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            caps.mode,
            naiad_netproto::PullMode::Bucketed { prefix_bits: 1 },
            "blake3-native must NOT lift advise result — raw advise stays (no sha256 floor)"
        );
        assert_eq!(
            caps.min_query_bits, None,
            "blake3-native must not advertise min_query_bits"
        );
    }

    // ── Persisted-count caps tests (#202, Task 5) ────────────────────────────

    /// `GET /repo/caps` returns 200 and uses the persisted distinct-hash count
    /// when a `repo_meta` row exists.
    #[tokio::test]
    async fn caps_handler_with_persisted_count_returns_200() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let store = RepoStore::open_in_memory().unwrap();
        // Persist a count so caps reads from the row (not the fallback).
        store.write_distinct_hash_count(50_000).unwrap();
        let router = app_split(
            Arc::new(Mutex::new(store)),
            None,
            1000,
            None,
            None,
            HashDomain::Blake3,
        );
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200, "caps must return 200");
        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&body).unwrap();
        // 50_000 hashes with k=1000 → advise picks Bucketed.
        assert!(
            matches!(caps.mode, naiad_netproto::PullMode::Bucketed { .. }),
            "with 50k hashes and k=1000 the mode must be Bucketed: {:?}",
            caps.mode
        );
    }

    /// `GET /repo/caps` returns 200 and uses CAPS_FALLBACK_COUNT when no
    /// `repo_meta` count row exists (pre-upgrade store).
    #[tokio::test]
    async fn caps_handler_without_count_row_returns_200_with_fallback() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        // Empty in-memory store: no repo_meta row → fallback (200 M).
        let store = RepoStore::open_in_memory().unwrap();
        let router = app_split(
            Arc::new(Mutex::new(store)),
            None,
            1000,
            None,
            None,
            HashDomain::Blake3,
        );
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "caps must return 200 even without a count row"
        );
        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&body).unwrap();
        // CAPS_FALLBACK_COUNT (200 M) >> k=1000 → mode must be Bucketed (not WholeRepo).
        assert!(
            matches!(caps.mode, naiad_netproto::PullMode::Bucketed { .. }),
            "fallback count must put a large repo into Bucketed mode: {:?}",
            caps.mode
        );
        // No real count row → wire count must be None (avoid overstating crowd).
        assert_eq!(
            caps.count, None,
            "empty store must advertise count: None, got {:?}",
            caps.count
        );
    }

    /// Review item 5: sha256-native (mirror) repo with NO count row must return
    /// 200 with Bucketed mode, and the advertised `prefix_bits` must be ≥ the
    /// repo's `min_query_bits` floor (the fallback count is large enough to
    /// trigger Bucketed; the floor lift must fire even on the fallback path).
    #[tokio::test]
    async fn mirror_sha256_no_count_row_caps_200_width_gte_floor() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let floor = 16u32;
        // Empty sha256-native store — no count row → CAPS_FALLBACK_COUNT (200 M).
        let store = RepoStore::open_in_memory().unwrap();
        let domains = crate::domain::DomainConfig {
            native: HashDomain::Sha256,
            added_sha256: None,
            max_query_bits: 256,
            min_query_bits: floor,
        };
        let router = app_domains(Arc::new(Mutex::new(store)), None, 1000, None, None, domains);
        let req = Request::builder()
            .uri(REPO_CAPS)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200, "caps must return 200");
        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&body).unwrap();
        match caps.mode {
            naiad_netproto::PullMode::Bucketed { prefix_bits } => {
                assert!(
                    prefix_bits >= floor,
                    "advertised prefix_bits ({prefix_bits}) must be >= floor ({floor})"
                );
            }
            other => panic!("expected Bucketed mode with fallback count, got {other:?}"),
        }
    }

    /// A 4-connection pool cycles through all four connections in order; the
    /// 5th call wraps around to the 1st (round-robin). Checks `Arc::ptr_eq`
    /// rather than `==` so the test is sensitive to connection identity, not
    /// just value equality (#202).
    #[test]
    fn read_pool_round_robins_across_connections() {
        let pool = ReadPool::new(
            (0..4)
                .map(|_| Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap())))
                .collect(),
        );
        let a = pool.next();
        let b = pool.next();
        let c = pool.next();
        let d = pool.next();
        let e = pool.next();
        assert!(
            !Arc::ptr_eq(&a, &b) && !Arc::ptr_eq(&b, &c) && !Arc::ptr_eq(&c, &d),
            "all four connections must be distinct"
        );
        assert!(
            Arc::ptr_eq(&a, &e),
            "5th checkout must wrap to the 1st connection"
        );
    }

    /// 1-conn parity: a ReadPool with a single connection returns the same Arc
    /// on every call (#202 carry-over from Task 6 review).
    #[test]
    fn read_pool_single_conn_parity() {
        let pool = ReadPool::new(vec![Arc::new(Mutex::new(
            RepoStore::open_in_memory().unwrap(),
        ))]);
        let a = pool.next();
        let b = pool.next();
        let c = pool.next();
        assert!(
            Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&b, &c),
            "single-conn pool must return the same Arc every time"
        );
    }

    /// read_only mode: write handlers return 403 for validly-signed payloads
    /// that would succeed on a non-read-only router. Proves precedence: the same
    /// signed payload earns 204/2xx on `read_only=false` and 403 on `read_only=true`.
    /// GET /repo/caps still returns 200 in both modes (#202).
    #[tokio::test]
    async fn read_only_refuses_writes_but_serves_reads() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use naiad_core::{Hash, Tag};
        use naiad_netproto::{Op, RelKind};
        use tower::ServiceExt;

        // ── Helper: build a properly auth-signed request for a submit body. ────
        // Mirrors the `post_with_auth` helper used by the #160 tests above, but
        // takes raw bytes (submit_handler takes Bytes, not Json).
        let make_submit_req = |uri: &str, acct: &Account, body_bytes: Vec<u8>| {
            let ts = now();
            let sig = acct.sign_auth("POST", uri, NO_DOMAIN, ts, &body_bytes);
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header(HDR_AUTH_KEY, acct.public_hex())
                .header(HDR_AUTH_TS, ts.to_string())
                .header(HDR_AUTH_SIG, sig)
                .body(axum::body::Body::from(body_bytes))
                .unwrap()
        };

        // Build signed payloads.
        let acct = Account::generate();
        let hash: Hash = "0000000000000000000000000000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let tag = Tag::parse("character:samus").unwrap();
        let sub = acct.sign(Op::Add, &hash, &tag);
        let sub_bytes = serde_json::to_vec(&sub).unwrap();

        let from_tag = Tag::parse("character:samus_aran").unwrap();
        let to_tag = Tag::parse("character:samus").unwrap();
        let rel_sub = acct.sign_relation(Op::Add, RelKind::Sibling, &from_tag, &to_tag);
        let rel_bytes = serde_json::to_vec(&rel_sub).unwrap();

        // Report and moderate payloads (submit_handler takes Bytes; any body
        // that passes auth gets to the payload-parsing step).
        let report_body = serde_json::to_vec(&serde_json::json!({
            "hash": hash.to_hex(),
            "tag": "character:samus",
        }))
        .unwrap();
        let moderate_body = serde_json::to_vec(&serde_json::json!({
            "action": "dismiss",
            "id": 1,
        }))
        .unwrap();

        // ── Non-read-only router: submit and relations/submit must succeed. ───
        {
            let store_rw = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
            let pool_rw = Arc::new(ReadPool::new(vec![Arc::clone(&store_rw)]));
            let router_rw = app_domains_with_pool(
                store_rw,
                pool_rw,
                1000,
                None,
                None,
                crate::domain::DomainConfig::native_only(HashDomain::Blake3),
                false, // read_only = false
                None,  // stats_layer
                None,  // sidecar_count_path
            );

            // POST /repo/submit with a valid signed submission → 204 No Content.
            let resp = router_rw
                .clone()
                .oneshot(make_submit_req(REPO_SUBMIT, &acct, sub_bytes.clone()))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NO_CONTENT,
                "valid signed submit must succeed (204) on a non-read-only router"
            );

            // POST /repo/relations/submit with a valid signed relation → 204.
            let ts = now();
            let rel_sig = acct.sign_auth("POST", REPO_RELATIONS_SUBMIT, NO_DOMAIN, ts, &rel_bytes);
            let rel_req = Request::builder()
                .method("POST")
                .uri(REPO_RELATIONS_SUBMIT)
                .header("content-type", "application/json")
                .header(HDR_AUTH_KEY, acct.public_hex())
                .header(HDR_AUTH_TS, ts.to_string())
                .header(HDR_AUTH_SIG, rel_sig)
                .body(axum::body::Body::from(rel_bytes.clone()))
                .unwrap();
            let resp = router_rw.oneshot(rel_req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NO_CONTENT,
                "valid signed relation must succeed (204) on a non-read-only router"
            );
        }

        // ── read_only router: all write endpoints must return 403. ───────────
        let store_ro = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
        let pool_ro = Arc::new(ReadPool::new(vec![Arc::clone(&store_ro)]));
        let router_ro = app_domains_with_pool(
            store_ro,
            pool_ro,
            1000,
            None,
            None,
            crate::domain::DomainConfig::native_only(HashDomain::Blake3),
            true, // read_only
            None, // stats_layer
            None, // sidecar_count_path
        );

        // GET /repo/caps must succeed (200) — reads are unaffected.
        let caps_resp = router_ro
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/repo/caps")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            caps_resp.status(),
            StatusCode::OK,
            "caps must still serve in read_only mode"
        );

        // POST /repo/submit with a validly-signed submission → 403.
        let submit_resp = router_ro
            .clone()
            .oneshot(make_submit_req(REPO_SUBMIT, &acct, sub_bytes))
            .await
            .unwrap();
        assert_eq!(
            submit_resp.status(),
            StatusCode::FORBIDDEN,
            "submit must be refused in read_only mode even with a valid signature"
        );
        let body = to_bytes(submit_resp.into_body(), 1024).await.unwrap();
        assert!(
            std::str::from_utf8(&body).unwrap().contains("read-only"),
            "403 body must mention read-only"
        );

        // POST /repo/report with auth headers → 403 (auth check is after read_only).
        let report_resp = router_ro
            .clone()
            .oneshot(make_submit_req(REPO_REPORT, &acct, report_body))
            .await
            .unwrap();
        assert_eq!(
            report_resp.status(),
            StatusCode::FORBIDDEN,
            "report must be refused in read_only mode"
        );

        // POST /repo/moderate with auth headers → 403.
        let moderate_resp = router_ro
            .clone()
            .oneshot(make_submit_req(REPO_MODERATE, &acct, moderate_body))
            .await
            .unwrap();
        assert_eq!(
            moderate_resp.status(),
            StatusCode::FORBIDDEN,
            "moderate must be refused in read_only mode"
        );

        // POST /repo/relations/submit with a validly-signed relation → 403.
        // `Json<RelationSubmission>` is extracted before the handler body, so we
        // must send a structurally valid payload; the 403 fires before verify_relation.
        let ts = now();
        let rel_sig = acct.sign_auth("POST", REPO_RELATIONS_SUBMIT, NO_DOMAIN, ts, &rel_bytes);
        let rel_req = Request::builder()
            .method("POST")
            .uri(REPO_RELATIONS_SUBMIT)
            .header("content-type", "application/json")
            .header(HDR_AUTH_KEY, acct.public_hex())
            .header(HDR_AUTH_TS, ts.to_string())
            .header(HDR_AUTH_SIG, rel_sig)
            .body(axum::body::Body::from(rel_bytes))
            .unwrap();
        let rel_resp = router_ro.oneshot(rel_req).await.unwrap();
        assert_eq!(
            rel_resp.status(),
            StatusCode::FORBIDDEN,
            "relations/submit must be refused in read_only mode even with a valid signed payload"
        );
    }

    /// When `sidecar_count_path` is set and the sidecar cache is populated,
    /// `GET /repo/caps` must advertise the sidecar's cached hash count rather
    /// than the empty native store's count (#236 parity).
    #[tokio::test]
    async fn caps_sidecar_count_path_advertises_sidecar_hash_count() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let sc_path = dir.path().join("sidecar.db");

        // Build a sidecar with two hashes and a populated count cache.
        {
            let s = crate::bridge::sidecar::Sidecar::create(&sc_path).unwrap();
            s.write_tag_set(&[0x01u8; 32], &[10, 20]).unwrap();
            s.write_tag_set(&[0x02u8; 32], &[30]).unwrap();
            let tx = s.conn().unchecked_transaction().unwrap();
            s.insert_defs_tags(&[
                (10, "character:samus".into()),
                (20, "series:metroid".into()),
                (30, "rating:safe".into()),
            ])
            .unwrap();
            tx.commit().unwrap();
            s.recompute_bridge_counts().unwrap();
            // The cache now holds hashes=2, tags=3, mappings=3.
        }

        let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
        let pool = Arc::new(ReadPool::new(vec![Arc::clone(&store)]));
        let sidecar_count_path = Some(Arc::new(sc_path));
        let router = app_domains_with_pool(
            store,
            pool,
            1000,
            None,
            None,
            crate::domain::DomainConfig::native_only(HashDomain::Blake3),
            false,
            None,
            sidecar_count_path,
        );

        let resp = router
            .oneshot(
                Request::builder()
                    .uri(REPO_CAPS)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "caps must return 200");
        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&body).unwrap();
        // The sidecar cache holds 2 hashes; caps must advertise Some(2).
        assert_eq!(
            caps.count,
            Some(2),
            "caps must advertise the sidecar cached hash count (2), got {:?}",
            caps.count
        );
    }

    /// When `sidecar_count_path` is set but the sidecar cache has NOT been
    /// populated yet (refresher not yet run), caps must fall back to the native
    /// store's count (None for an empty in-memory store).
    #[tokio::test]
    async fn caps_sidecar_count_path_unpopulated_cache_falls_back_to_native() {
        use axum::body::to_bytes;
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let sc_path = dir.path().join("sidecar_fresh.db");
        // Fresh sidecar with no recompute — cache is absent.
        crate::bridge::sidecar::Sidecar::create(&sc_path).unwrap();

        let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
        let pool = Arc::new(ReadPool::new(vec![Arc::clone(&store)]));
        let sidecar_count_path = Some(Arc::new(sc_path));
        let router = app_domains_with_pool(
            store,
            pool,
            1000,
            None,
            None,
            crate::domain::DomainConfig::native_only(HashDomain::Blake3),
            false,
            None,
            sidecar_count_path,
        );

        let resp = router
            .oneshot(
                Request::builder()
                    .uri(REPO_CAPS)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "caps must return 200");
        let body = to_bytes(resp.into_body(), 1024 * 16).await.unwrap();
        let caps: naiad_netproto::Caps = serde_json::from_slice(&body).unwrap();
        // No cache + empty native store → None.
        assert_eq!(
            caps.count, None,
            "unpopulated sidecar cache with empty native store must yield count: None, got {:?}",
            caps.count
        );
    }
}
