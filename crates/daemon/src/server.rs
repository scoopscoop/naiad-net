//! The axum router and handlers. Media routes (`/thumb`, `/file`) serve image
//! files; `/api/*` routes are the typed data API; the bundled Svelte UI is the
//! router fallback. Blocking DB/image work runs in `spawn_blocking`; the `Db`
//! lives behind a `Mutex` because rusqlite's connection is not `Sync`.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, Path as PathParam, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::convert::Infallible;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

use naiad_api::{
    AccountDto, BackupReq, BackupSummary, BlockAddReq, BlockRuleDto, FileDto, FilePullRepoResult,
    FilePullReq, GallerySortDto, HydrusConfigDto, HydrusConfigReq, ImportProgress, ParentDto,
    PluginDto, PullConnecting, PullError, PullProgress, PullRepoOutcome, PullStage, PullSummary,
    RejectRequest, RejectResponse, RejectionDto, RelationEdgeDto, RelationPullReq,
    RelationPullSummary, RelationSectionDto, RelationStatusDto, RelationSubmitReq, RelationTagDto,
    RelationsImportSummary, RelationsProgress, RepoAddReq, RepoDto, RepoPriorityReq, RepoPullReq,
    RepoPullSummary, RepoQueryBitsReq, ReportRequest, ScanError, ScanProgress, ScanReq,
    ScanSummary, SiblingDto, SiblingRemoveReq, SourceImportReq, SourceImportSummary, SubmitReq,
    TagDetailDto, TagRelationsDto, TaggerLookupItem, TaggerLookupReq, TagsReq,
};
use naiad_core::Hash;
use naiad_db::{Db, FileListing};
use serde::Deserialize;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing::Level;

use crate::AppState;
use crate::lock::LockRecover;
use crate::ops;
use crate::settings::PrivacySettings;
use crate::thumb::make_thumbnail;

/// Warn when waiting this long for a thumbnail generation permit.
const THUMB_PERMIT_WARN: Duration = Duration::from_secs(2);

/// Warn when a single thumbnail decode takes this long.
const THUMB_DECODE_WARN: Duration = Duration::from_secs(5);

/// Lock-wait durations at or above this threshold are logged at WARN under target `db`.
const DB_LOCK_WARN: Duration = Duration::from_secs(1);

const GALLERY_SORT_SETTING_KEY: &str = "ui.gallery_sort";

/// Build the axum router. Exposed so tests can drive it via `oneshot`.
///
/// When `state.ui_dir` is set, a `ServeDir` for that directory becomes the
/// router's fallback (so `/api/*`, `/thumb`, `/file` still match first) and its
/// `index.html` is the not-found fallback for SPA paths. Otherwise the embedded
/// Svelte UI is served as the fallback.
pub fn app(state: AppState) -> Router {
    let ui_dir = state.ui_dir.clone();
    let bound = state.bound_addr;
    let allow_remote = state.allow_remote;
    let router = Router::new()
        .route(naiad_api::API_FILES, get(files_handler))
        .route(naiad_api::API_SEARCH, get(search_handler))
        .route(naiad_api::API_SCAN, post(scan_handler))
        .route(naiad_api::API_SCAN_STREAM, get(scan_stream_handler))
        .route(naiad_api::API_TAGS, get(tags_handler))
        .route(naiad_api::API_TAGS_DETAILED, get(tags_detailed_handler))
        .route(naiad_api::API_TAGS_RELATIONS, get(tags_relations_handler))
        .route(naiad_api::API_TAGS_ADD, post(tags_add_handler))
        .route(naiad_api::API_TAGS_REMOVE, post(tags_remove_handler))
        .route(naiad_api::API_TAGS_COMPLETE, get(tags_complete_handler))
        .route(naiad_api::API_NAMESPACES, get(namespaces_handler))
        .route(naiad_api::API_SIBLINGS, get(siblings_handler))
        .route(naiad_api::API_SIBLINGS_ADD, post(siblings_add_handler))
        .route(
            naiad_api::API_SIBLINGS_REMOVE,
            post(siblings_remove_handler),
        )
        .route(naiad_api::API_PARENTS, get(parents_handler))
        .route(naiad_api::API_PARENTS_ADD, post(parents_add_handler))
        .route(naiad_api::API_PARENTS_REMOVE, post(parents_remove_handler))
        .route(
            naiad_api::API_ROOTS,
            get(roots_list_handler).delete(roots_remove_handler),
        )
        .route(
            naiad_api::API_REPOS,
            get(repos_list_handler)
                .post(repos_add_handler)
                .delete(repos_remove_handler),
        )
        .route(naiad_api::API_REPOS_PULL, post(repos_pull_handler))
        .route(
            naiad_api::API_FILES_PULL_TAGS,
            post(files_pull_tags_handler),
        )
        .route(
            naiad_api::API_FILES_PULL_TAGS_STREAM,
            post(files_pull_tags_stream_handler),
        )
        .route(naiad_api::API_REPOS_SUBMIT, post(repos_submit_handler))
        .route(naiad_api::API_REPOS_PRIORITY, post(repos_priority_handler))
        .route(
            naiad_api::API_REPOS_QUERY_BITS,
            post(repos_query_bits_handler),
        )
        .route(
            naiad_api::API_RELATIONS_SUBMIT,
            post(relations_submit_handler),
        )
        .route(naiad_api::API_RELATIONS_PULL, post(relations_pull_handler))
        .route(naiad_api::API_RELATIONS, get(relations_list_handler))
        .route(
            naiad_api::API_RELATIONS_STATUS,
            get(relations_status_handler),
        )
        .route(
            naiad_api::API_BLOCKS,
            get(blocks_handler)
                .post(blocks_add_handler)
                .delete(blocks_remove_handler),
        )
        .route(
            naiad_api::API_REJECT,
            post(reject_handler).delete(reject_remove_handler),
        )
        .route(naiad_api::API_REJECTIONS, get(rejections_list_handler))
        .route(naiad_api::API_REPORT, post(report_handler))
        .route(
            naiad_api::API_VIEW_SORT,
            get(view_sort_get_handler).post(view_sort_set_handler),
        )
        .route(naiad_api::API_PLUGINS, get(plugins_handler))
        .route(
            naiad_api::API_HYDRUS_CONFIGURE,
            post(hydrus_configure_handler),
        )
        .route(naiad_api::API_HYDRUS_CONFIG, get(hydrus_config_get_handler))
        .route(naiad_api::API_TAGGER_LOOKUP, post(tagger_lookup_handler))
        .route(naiad_api::API_SOURCE_IMPORT, post(source_import_handler))
        .route(
            naiad_api::API_SOURCE_IMPORT_STREAM,
            get(source_import_stream_handler),
        )
        .route(
            naiad_api::API_HYDRUS_RELATIONS,
            post(hydrus_relations_handler),
        )
        .route(
            naiad_api::API_HYDRUS_RELATIONS_STREAM,
            get(hydrus_relations_stream_handler),
        )
        .route(naiad_api::API_ACCOUNT, get(account_handler))
        .route(naiad_api::API_HEALTH, get(health_handler))
        .route(naiad_api::API_BACKUP, post(backup_handler))
        .route("/thumb/{hash}", get(thumb_handler))
        .route("/file/{hash}", get(file_handler))
        .route(naiad_api::THUMB_STREAM, get(crate::thumb_stream::handler));

    let router = match ui_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            router.fallback_service(ServeDir::new(dir.as_ref()).fallback(ServeFile::new(index)))
        }
        None => router.fallback(crate::ui::embedded_ui),
    };

    router
        .layer(middleware::from_fn(move |req, next| {
            source_guard(req, next, allow_remote)
        }))
        .layer(middleware::from_fn(move |req, next| {
            origin_guard(req, next, bound, allow_remote)
        }))
        .layer(middleware::from_fn(move |req, next| {
            host_guard(req, next, bound, allow_remote)
        }))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<axum::body::Body>| {
                    // The UI polls /api/health every few seconds to drive its
                    // activity dot; the route does no DB work and carries no
                    // per-request diagnostic value, so it is excluded from the
                    // trace layer entirely to keep DEBUG logs meaningful (#229).
                    if req.uri().path() == naiad_api::API_HEALTH {
                        return tracing::Span::none();
                    }
                    tracing::span!(
                        target: "http",
                        Level::TRACE,
                        "http-request",
                        method = %req.method(),
                        path = %req.uri().path(),
                    )
                })
                .on_request(
                    |req: &axum::http::Request<axum::body::Body>, span: &tracing::Span| {
                        // Span::none() marks the excluded health route; merely
                        // disabled spans (TRACE off) still carry metadata and
                        // report is_none() == false, so DEBUG logging survives.
                        if span.is_none() {
                            return;
                        }
                        tracing::event!(
                            target: "http",
                            Level::DEBUG,
                            method = %req.method(),
                            path = %req.uri().path(),
                            "http-request",
                        );
                    },
                )
                .on_response(
                    |res: &axum::http::Response<axum::body::Body>,
                     latency: std::time::Duration,
                     span: &tracing::Span| {
                        if span.is_none() {
                            return;
                        }
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
        .with_state(state)
}

/// Reject any request whose `Host` header is present but not a loopback
/// authority (nor the server's own bound address). This is the standard defense
/// against **DNS rebinding**: a malicious page that rebinds its domain to
/// `127.0.0.1` still sends `Host: evil.example`, so we drop it before it can
/// reach the library API or `/file` media bytes. A *missing* `Host` (non-browser
/// clients such as the CLI or curl) is allowed — DNS rebinding is a browser
/// attack, and browsers always send `Host`.
///
/// When `allow_remote` is set (the unsupported `[net].allow_remote` opt-in) any
/// `Host` is accepted: remote clients name the daemon by a LAN IP or hostname
/// that can never match a wildcard bind, so keeping the check would 403 every
/// request the opt-in exists to permit. The origin guard still rejects
/// cross-site browser requests in that mode.
async fn host_guard(
    req: Request,
    next: Next,
    bound: Option<SocketAddr>,
    allow_remote: bool,
) -> Response {
    if host_allowed(req.headers().get(header::HOST), bound, allow_remote) {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "forbidden: Host header is not a local address",
        )
            .into_response()
    }
}

/// Whether a request carrying this `Host` header may proceed. `None` (absent
/// header) is allowed; a present header must name a loopback host, the bound
/// address, or be exempted by `allow_remote`.
fn host_allowed(host: Option<&HeaderValue>, bound: Option<SocketAddr>, allow_remote: bool) -> bool {
    let Some(host) = host else { return true };
    let Ok(host) = host.to_str() else {
        return false;
    };
    let name = host_part(host);
    is_allowed_client_host(name, bound, allow_remote)
}

/// The host portion of an HTTP `Host` value, without the optional `:port`.
/// Handles all four forms:
/// - `host` or `host:port` — returns `host`.
/// - `[::1]` or `[::1]:port` — bracketed IPv6; returns `::1`.
/// - `::1` — bare IPv6 literal (multiple colons, no brackets); returned as-is
///   because there is no port suffix to strip.
fn host_part(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        // Bracketed IPv6 literal: take up to the closing bracket.
        return rest.split(']').next().unwrap_or(rest);
    }
    // A bare IPv6 address contains more than one colon; returning only up to
    // the first colon would yield an empty string for "::1" and would mangle
    // any other IPv6 address. Return the whole string when multiple colons are
    // present (no port stripping is possible without brackets).
    let colon_count = host.bytes().filter(|&b| b == b':').count();
    if colon_count > 1 {
        return host;
    }
    host.split(':').next().unwrap_or(host)
}

/// Whether `name` denotes the local machine, by loopback name or loopback IP
/// (`localhost`, `127.0.0.0/8`, `::1`).
fn is_loopback_host(name: &str) -> bool {
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether a client host is permitted to reach this daemon. Returns `true` when
/// any of the following hold:
/// - `allow_remote` is set (operator opt-in — all hosts are allowed).
/// - `name` is a loopback host (`localhost`, `127.x.x.x`, `::1`).
/// - `name` exactly matches the bound socket's IP (the server's own LAN address).
///
/// This is the single source of truth for the "loopback-or-bound-or-remote" check
/// shared by `host_allowed` and `origin_allowed`. Keeping it in one place ensures
/// that both guard layers honour the same policy and that future changes affect
/// both consistently.
fn is_allowed_client_host(name: &str, bound: Option<SocketAddr>, allow_remote: bool) -> bool {
    allow_remote || is_loopback_host(name) || matches!(bound, Some(b) if name == b.ip().to_string())
}

/// Reject any request whose browser-provided origin metadata indicates a
/// cross-site request. This closes the SSE-GET CSRF hole: a drive-by page
/// issuing `new EventSource("http://127.0.0.1:8080/api/scan/stream?…")` sends
/// `Sec-Fetch-Site: cross-site`, which we reject before the request reaches
/// any handler. Same-origin and same-site UI requests, the Tauri shell, and
/// CLI/curl (which send neither header) are all allowed.
///
/// When `allow_remote` is set, an `Origin` header naming a non-loopback host is
/// also permitted (the operator has explicitly opted in to remote access, so the
/// remote browser UI must be able to reach the daemon).
async fn origin_guard(
    req: Request,
    next: Next,
    bound: Option<SocketAddr>,
    allow_remote: bool,
) -> Response {
    let h = req.headers();
    if origin_allowed(
        h.get("sec-fetch-site"),
        h.get(header::ORIGIN),
        bound,
        allow_remote,
    ) {
        next.run(req).await
    } else {
        tracing::warn!(
            target: "http",
            method = %req.method(),
            path = %req.uri().path(),
            sec_fetch_site = ?h.get("sec-fetch-site"),
            origin = ?h.get(header::ORIGIN),
            "request rejected: cross-origin (CSRF guard)"
        );
        (StatusCode::FORBIDDEN, "forbidden: cross-origin request").into_response()
    }
}

/// Whether a request with this `Sec-Fetch-Site` and `Origin` may proceed.
///
/// Decision precedence:
/// - `Sec-Fetch-Site` present and `same-origin`, `same-site`, or `none` →
///   **allow** (browser-attested, unforgeable; `Origin` is not checked — this
///   short-circuit lets a remote browser UI with a non-loopback `Origin` still
///   work under `allow_remote = true`). `same-site` covers legitimate requests
///   from a daemon served under a subdomain.
/// - `Sec-Fetch-Site` present but `cross-site` or any other value → **reject**.
/// - `Origin` present (no `Sec-Fetch-Site`): host must be loopback, the bound
///   address, or `allow_remote` must be set → else **reject** (`null` and
///   garbage are also rejected).
/// - Neither header → **allow** (CLI / curl / ureq, non-browser clients).
fn origin_allowed(
    sec_fetch_site: Option<&HeaderValue>,
    origin: Option<&HeaderValue>,
    bound: Option<SocketAddr>,
    allow_remote: bool,
) -> bool {
    if let Some(sfs) = sec_fetch_site {
        // same-origin / same-site / none are browser-attested and unforgeable.
        // cross-site and any other value → reject.
        return matches!(
            sfs.to_str(),
            Ok("same-origin") | Ok("same-site") | Ok("none")
        );
    }
    if let Some(origin) = origin {
        let Ok(origin_str) = origin.to_str() else {
            return false;
        };
        // "null" origin (sandboxed iframe, some file:// contexts) is not local.
        let host = origin_host(origin_str);
        return is_allowed_client_host(host, bound, allow_remote);
    }
    true
}

/// The host of an `Origin` URL: strip the `scheme://`, then take the authority's
/// host via `host_part` (which also unwraps bracketed IPv6 and drops `:port`).
/// A bare `null` (or anything without `://`) yields itself as a non-loopback
/// sentinel so it fails the loopback check.
fn origin_host(origin: &str) -> &str {
    let authority = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    host_part(authority)
}

/// Reject any connection whose peer IP is non-loopback when `allow_remote` is
/// false. This is defense in depth behind the bind-policy gate: even if a
/// wildcard bind is somehow reachable from the LAN, we reject the connection at
/// the socket layer so library data never leaves the machine.
///
/// The peer address is read **optionally** from `ConnectInfo<SocketAddr>` so a
/// missing extension (in-process test harness, which drives the router without a
/// bound socket) is treated as local and not rejected.
async fn source_guard(req: Request, next: Next, allow_remote: bool) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);
    if peer_allowed(peer, allow_remote) {
        next.run(req).await
    } else {
        tracing::warn!(
            target: "http",
            peer = ?peer,
            method = %req.method(),
            path = %req.uri().path(),
            "connection rejected: non-local peer"
        );
        (StatusCode::FORBIDDEN, "forbidden: non-local connection").into_response()
    }
}

/// Whether a connection from this peer may proceed. When remote is allowed, any
/// peer is fine. Otherwise the peer IP must be loopback. An *unknown* peer
/// (`None`) is treated as local, so the in-process router test harness — which
/// drives the router without a socket, so no `ConnectInfo` extension exists —
/// keeps working.
fn peer_allowed(peer: Option<SocketAddr>, allow_remote: bool) -> bool {
    if allow_remote {
        return true;
    }
    match peer {
        Some(addr) => addr.ip().is_loopback(),
        None => true,
    }
}

/// An error mapped to an HTTP status + plain-text body.
pub(crate) struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

/// Map any error to `400 Bad Request` with its message (client-input failures:
/// bad query, unknown file reference, unparsable tag).
pub(crate) fn bad(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
}

/// Map any error to `500 Internal Server Error` (unexpected DB failures).
///
/// Emits one `error!` line under target `http` before returning: an unexpected
/// 500 is by definition a failure that would otherwise return silently, so this
/// funnel is the DRY choke point that makes every `.map_err(internal)` 500
/// visible in logs without touching the 28 call sites.
pub(crate) fn internal(e: impl std::fmt::Display) -> ApiError {
    let msg = format!("{e:#}");
    tracing::error!(target: "http", error = %msg, "request failed: internal server error (500)");
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg)
}

/// Convert a DB listing to the wire DTO.
///
/// `name`/`path` are best-effort *display* strings: a non-UTF-8 filename (the DB
/// stores raw OS bytes, ADR 0003) is rendered lossily (`U+FFFD`) rather than
/// dropped, so it never arrives empty. The wire identity is `hash` — clients
/// reference files by hash, never by round-tripping these display paths.
pub(crate) fn to_dto(f: &FileListing) -> FileDto {
    FileDto {
        hash: f.hash.to_hex(),
        name: f
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size: f.size,
        path: f.path.to_string_lossy().into_owned(),
        imported_at: f.imported_at,
        created_at: f.created_at,
        modified_at: f.modified_at,
        mime: f.mime.clone(),
    }
}

#[derive(Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    local_only: bool,
    #[serde(default)]
    raw: bool,
}

/// `GET /api/search?q=...` — run the query; empty `q` returns all files
/// (`local_only` does not apply: files are not service-scoped, only their tag
/// mappings are). `&raw=true` disables relation expansion (literal tag match).
/// A query that fails to parse returns `400`.
async fn search_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<FileDto>>, ApiError> {
    let started = Instant::now();
    let tokens: Vec<String> = naiad_core::tokenize(&params.q);
    let token_count = tokens.len();
    let scope = if params.local_only {
        naiad_db::ReadScope::LocalOnly
    } else {
        naiad_db::ReadScope::Merged
    };
    let expansion = if params.raw {
        naiad_db::Expansion::Raw
    } else {
        naiad_db::Expansion::Expanded
    };
    let listings = on_read_db(&state, move |db| {
        if tokens.is_empty() {
            Ok(db.list_files()?)
        } else {
            Ok(ops::search(db, &tokens, scope, expansion)?)
        }
    })
    .await?;
    // First gallery query is served: release the background cache warmup, which
    // was held so its cold-page reads did not starve this query (#121).
    state.startup_gate.fire();
    tracing::debug!(
        target: "search",
        q = %params.q,
        tokens = token_count,
        results = listings.len(),
        ms = started.elapsed().as_millis() as u64,
        "search"
    );
    Ok(Json(listings.iter().map(to_dto).collect()))
}

/// `GET /api/files` — every file in the library.
/// Uses the read pool when available (same as other read handlers). Query
/// errors map to 500 (original behavior) rather than 400.
async fn files_handler(State(state): State<AppState>) -> Result<Json<Vec<FileDto>>, ApiError> {
    let listings: Vec<FileListing> = match &state.read_pool {
        Some(pool) => pool
            .run(|db| db.list_files())
            .await
            .map_err(internal)?
            .map_err(internal)?,
        None => {
            // Writer fallback: same 500 mapping as the pool branch above.
            run_locked_raw(state.db.clone(), |db| Ok(db.list_files()?))
                .await
                .map_err(internal)?
                .map_err(internal)?
        }
    };
    // Same gate release as `search_handler`: `/api/files` is the other first
    // gallery-list read a client may issue (#121).
    state.startup_gate.fire();
    Ok(Json(listings.iter().map(to_dto).collect()))
}

/// Liveness probe for the UI's daemon status dot. Returns once the router is
/// up; carries the background watch-registration status so the UI can show a
/// "watching folders — root N/M" job without a second endpoint.
async fn health_handler(State(state): State<AppState>) -> Json<HealthDto> {
    let watch = state
        .watch
        .as_ref()
        .map(|w| w.status())
        .unwrap_or_else(|| crate::watch::WatchStatus::new(0));
    let scan = state
        .catchup
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let warmup = state.warmup.status();
    Json(HealthDto {
        status: "ok",
        watch,
        scan,
        warmup,
    })
}

/// Response body of `GET /api/health`. `watch` defaults to a complete, empty
/// registration when the daemon runs without a file watcher; `scan` defaults to
/// an idle, never-run catch-up status in the same case; `warmup` reports the
/// idle phase when no cache warmup was spawned.
#[derive(serde::Serialize)]
struct HealthDto {
    status: &'static str,
    watch: crate::watch::WatchStatus,
    scan: crate::catchup::CatchupStatus,
    /// Phase of the startup cache warmup, so the UI can show a "Preparing
    /// library" job during the window where the deferred catch-up scan still
    /// reports all-zero counters (#130).
    warmup: crate::warmup::WarmupStatus,
}

/// `GET /thumb/{hash}` — cached aspect-preserving thumbnail, generated on first request.
async fn thumb_handler(
    State(state): State<AppState>,
    PathParam(hash): PathParam<String>,
) -> Response {
    let started = Instant::now();
    // Validate the hash before any cache look-up.
    let parsed_hash: Hash = match hash.parse() {
        Ok(h) => h,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    // Normalise to lowercase hex so cache keys are always canonical regardless
    // of URL casing. `Hash::from_str` accepts uppercase; keying by the raw URL
    // string would miss cached entries and accumulate duplicate store rows.
    let hash_hex = parsed_hash.to_hex();
    // Fast path: an already-generated thumbnail is a store read. It must
    // not queue for a generation permit — during scrolling, cached tiles would
    // stall head-of-line behind slow decodes of first-seen ones (#51).
    if let Some(bytes) = state
        .thumb_store
        .get_async(&hash_hex, state.thumb_size)
        .await
    {
        log_latency("thumb-hit", &hash, started);
        return thumb_response(bytes);
    }
    // Newest-first queue: under a backlog (e.g. after a deep fling) the most
    // recent request — approximately what is on screen — is admitted first (#54).
    let permit_wait_start = Instant::now();
    let permit = state.thumb_permits.acquire().await;
    let permit_wait = permit_wait_start.elapsed();
    if permit_wait >= THUMB_PERMIT_WARN {
        tracing::warn!(
            target: "thumb",
            hash = &hash[..12],
            wait_ms = permit_wait.as_millis() as u64,
            "thumbnail permit wait exceeded 2 s"
        );
    }
    // Resolve location via the pool (or writer fallback) before entering
    // spawn_blocking — callers never hold a DB connection during image decode.
    let path = match present_location(&state, parsed_hash).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(
                target: "thumb",
                hash = &hash[..12],
                "no present location: {}",
                e.1
            );
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let store = state.thumb_store.clone();
    let size = state.thumb_size;
    let hash_for_log = hash_hex.clone();
    let out = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        thumb_bytes(&path, &store, size, &hash_hex)
    })
    .await;
    log_latency("thumb-gen", &hash_for_log, started);
    match out {
        Ok(Ok(bytes)) => thumb_response(bytes),
        Ok(Err(e)) => {
            tracing::warn!(
                target: "thumb",
                hash = &hash_for_log[..12],
                "thumbnail generation failed: {e:#}"
            );
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            tracing::warn!(
                target: "thumb",
                hash = &hash_for_log[..12],
                "thumbnail task panicked: {e}"
            );
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// A 200 response carrying JPEG thumbnail bytes, immutable-cacheable.
fn thumb_response(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

/// `GET /file/{hash}` — original bytes from a present location.
/// Resolves the path via the pool before reading, so no DB connection is held
/// during the file read.
async fn file_handler(
    State(state): State<AppState>,
    PathParam(hash): PathParam<String>,
) -> Response {
    let parsed_hash: Hash = match hash.parse() {
        Ok(h) => h,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let path = match present_location(&state, parsed_hash).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(
                target: "http",
                hash = &hash[..12],
                "file request: no present location: {}",
                e.1
            );
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "http",
                hash = &hash[..12],
                "file read failed for present location: {e}"
            );
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let ct = content_type_for(&path);
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

/// Produce (or read from cache) the JPEG thumbnail bytes for `hash_hex`.
/// `path` is the already-resolved on-disk location of the original file;
/// callers resolve it via the pool before entering `spawn_blocking` so no DB
/// connection is held during image decode or file I/O.
pub(crate) fn thumb_bytes(
    path: &Path,
    store: &crate::thumb_store::ThumbStore,
    size: u32,
    hash_hex: &str,
) -> anyhow::Result<Vec<u8>> {
    // Re-check the cache: a request can wait a long time for its permit, and
    // another request may have generated the thumbnail meanwhile (races are
    // benign — both writes produce the same bytes, INSERT OR REPLACE).
    if let Some(bytes) = store.get(hash_hex, size) {
        return Ok(bytes);
    }
    let original = std::fs::read(path)?;
    let gen_start = Instant::now();
    let thumb = make_thumbnail(&original, size)?;
    let gen_elapsed = gen_start.elapsed();
    if gen_elapsed >= THUMB_DECODE_WARN {
        tracing::warn!(
            target: "thumb",
            hash = &hash_hex[..12],
            elapsed_ms = gen_elapsed.as_millis() as u64,
            "thumbnail generation slow (>5 s)"
        );
    } else {
        tracing::debug!(
            target: "thumb",
            hash = &hash_hex[..12],
            elapsed_ms = gen_elapsed.as_millis() as u64,
            "thumbnail generated"
        );
    }
    store.put(hash_hex, size, &thumb);
    Ok(thumb)
}

/// Resolve a content hash to a present on-disk path via the read pool (or
/// writer fallback). Async so callers never hold a DB connection while doing
/// image decode or file I/O.
pub(crate) async fn present_location(state: &AppState, hash: Hash) -> Result<PathBuf, ApiError> {
    on_read_db(state, move |db| {
        db.locations_of(&hash)?
            .into_iter()
            .find(|l| l.present)
            .map(|l| l.path)
            .ok_or_else(|| anyhow::anyhow!("no present location for {hash}"))
    })
    .await
}

/// Guess a `Content-Type` from a path's extension.
fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("tif" | "tiff") => "image/tiff",
        _ => "application/octet-stream",
    }
}

/// Run a blocking closure on the **writer** DB, mapping a join failure to `500`.
/// The closure's own error is mapped to `400` (client-input failures). Use this
/// for any handler that writes.
async fn on_db<T, F>(state: &AppState, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Db) -> anyhow::Result<T> + Send + 'static,
{
    run_locked(state.db.clone(), f).await
}

/// Read-only work: pooled connection when available, else the writer. The
/// pool's own instrumentation logs the wait, so no double logging here.
/// Closure errors map to 400; callers that need 500 semantics (e.g.
/// `files_handler`) dispatch on the pool directly via `run_locked_raw`.
async fn on_read_db<T, F>(state: &AppState, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Db) -> anyhow::Result<T> + Send + 'static,
{
    match &state.read_pool {
        Some(pool) => pool.run(f).await.map_err(internal)?.map_err(bad),
        None => run_locked(state.db.clone(), f).await,
    }
}

/// Tag-oriented read lane: dedicated cancellable connection when available,
/// else the pool, else the writer. Completion, namespace, and detail handlers
/// share it without competing with general reads; dropped dedicated-lane work
/// is interrupted when running or skipped when still queued (#50, #70, #76).
async fn on_tag_db<T, F>(state: &AppState, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Db) -> anyhow::Result<T> + Send + 'static,
{
    match &state.tag_db {
        Some(db) => db.run(f).await.map_err(internal)?.map_err(bad),
        None => on_read_db(state, f).await,
    }
}

/// Wait, off the tag lane and off the read pool, for the background warmup to
/// finish building the merged relation graph before an interactive tag read runs
/// on the single-connection tag lane.
///
/// Both completion (`complete_tags` → `relation_completion`) and the detail panel
/// resolve through the merged relation graph, whose first build is a one-time
/// ~34s cold operation. If a tag-lane read triggers that build itself, it holds
/// the single tag-lane connection for the whole build, serializing every other
/// completion/detail/namespace read behind it for tens of seconds — and the
/// stuck requests pin the browser's few sockets so detail images cannot load
/// (#126, #115). The background warmup builds the graph first (see
/// `spawn_cache_warmup`); waiting for it here means the lane read finds a warm
/// cache instead of building it inline.
///
/// This is a pure `Notify` wait — it never touches the read pool, so completion
/// still answers immediately when the pool is saturated. It only waits once the
/// warmup has actually been released (the first gallery query fired
/// [`AppState::startup_gate`]); before that no build is in flight, so it returns
/// at once. Bounded by a timeout so a wedged warmup can never hang the request.
async fn await_relation_graph(state: &AppState) {
    // `graph_ready` is pre-fired at construction (fail-open, #132): an unarmed
    // gate — one from an `AppState` built without `with_read_db` — is already
    // fired, so the first clause short-circuits and no consumer can eat the 60s
    // backstop on a never-armed gate. The second clause skips the wait before
    // the first gallery query fires the startup gate, since no build is in
    // flight yet and the wait would be spurious.
    if state.graph_ready.is_fired() || !state.startup_gate.is_fired() {
        return;
    }
    state
        .graph_ready
        .wait(crate::RELATION_GRAPH_WAIT_TIMEOUT)
        .await;
}

/// One log line per DB op: DEBUG normally, WARN when the lock wait crossed
/// [`DB_LOCK_WARN`] (that contention is what #50 is about).
pub(crate) fn log_db_op(op: &'static str, lock_wait: Duration, work: Duration) {
    let (lock_wait_ms, elapsed_ms) = (lock_wait.as_millis() as u64, work.as_millis() as u64);
    if lock_wait >= DB_LOCK_WARN {
        tracing::warn!(target: "db", op, lock_wait_ms, elapsed_ms, "db lock wait above threshold");
    } else {
        tracing::debug!(target: "db", op, lock_wait_ms, elapsed_ms, "db op");
    }
}

/// Timing + locking core shared by [`run_locked`] and any caller that needs a
/// different error mapping for the closure result (e.g. `files_handler` maps
/// query errors to 500 rather than 400). Returns the raw join + closure result.
async fn run_locked_raw<T, F>(
    db: std::sync::Arc<std::sync::Mutex<Db>>,
    f: F,
) -> Result<anyhow::Result<T>, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce(&Db) -> anyhow::Result<T> + Send + 'static,
{
    // The closure's type name embeds the defining function's module path
    // (e.g. `naiad_daemon::server::tags_complete_handler::{{closure}}`), which
    // identifies the endpoint without threading a label through every caller.
    let op = std::any::type_name::<F>();
    tokio::task::spawn_blocking(move || {
        let wait_start = Instant::now();
        let db = db.lock_recover();
        let lock_wait = wait_start.elapsed();
        let work_start = Instant::now();
        let out = f(&db);
        log_db_op(op, lock_wait, work_start.elapsed());
        out
    })
    .await
}

async fn run_locked<T, F>(db: std::sync::Arc<std::sync::Mutex<Db>>, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Db) -> anyhow::Result<T> + Send + 'static,
{
    run_locked_raw(db, f).await.map_err(internal)?.map_err(bad)
}

/// `POST /api/scan` — scan a folder synchronously; per-file errors are collected
/// into the summary rather than streamed.
async fn scan_handler(
    State(state): State<AppState>,
    Json(req): Json<ScanReq>,
) -> Result<Json<ScanSummary>, ApiError> {
    let db = state.db.clone();
    let folder = req.folder.clone();
    let summary = tokio::task::spawn_blocking(move || -> anyhow::Result<ScanSummary> {
        // `scan_streaming` hashes off-lock and writes in brief locked bursts, so
        // a long scan doesn't freeze search/thumbnail/tag requests behind the
        // single `Mutex<Db>`.
        let mut errors = Vec::new();
        let s = ops::scan_streaming(
            &db,
            &folder,
            ops::ScanProfile::Interactive,
            |e| {
                errors.push(ScanError {
                    path: e.path.display().to_string(),
                    message: e.source.to_string(),
                });
            },
            |_, _, _| {},
        )?;
        Ok(ScanSummary {
            imported: s.imported as usize,
            marked_missing: s.marked_missing as usize,
            errors,
        })
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;

    // Begin live-watching the newly-registered root (no-op if watching is off).
    if let Some(w) = &state.watch {
        let abs = std::path::absolute(&req.folder).unwrap_or_else(|_| PathBuf::from(&req.folder));
        w.register(abs);
    }

    Ok(Json(summary))
}

#[derive(Deserialize)]
struct FolderQuery {
    folder: String,
}

enum ScanMsg {
    Progress(ScanProgress),
    Done(ScanSummary),
    Failed(String),
}

/// `GET /api/scan/stream?folder=…` — SSE variant of the scan: emits `progress`
/// ticks as files are indexed, then a terminal `summary` (or `error`) event. The
/// synchronous `POST /api/scan` remains the canonical scan for the CLI.
async fn scan_stream_handler(
    State(state): State<AppState>,
    Query(q): Query<FolderQuery>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ScanMsg>();
    let db = state.db.clone();
    let watch = state.watch.clone();
    let folder = q.folder.clone();
    tokio::task::spawn_blocking(move || {
        let mut errors = Vec::new();
        let tx_prog = tx.clone();
        let res = ops::scan_streaming(
            &db,
            &folder,
            ops::ScanProfile::Interactive,
            |e| {
                errors.push(ScanError {
                    path: e.path.display().to_string(),
                    message: e.source.to_string(),
                });
            },
            |imported, skipped, total| {
                let _ = tx_prog.send(ScanMsg::Progress(ScanProgress {
                    imported,
                    skipped,
                    total,
                }));
            },
        );
        match res {
            Ok(s) => {
                if let Some(w) = &watch {
                    let abs =
                        std::path::absolute(&folder).unwrap_or_else(|_| PathBuf::from(&folder));
                    w.register(abs);
                }
                let _ = tx.send(ScanMsg::Done(ScanSummary {
                    imported: s.imported as usize,
                    marked_missing: s.marked_missing as usize,
                    errors,
                }));
            }
            Err(e) => {
                let _ = tx.send(ScanMsg::Failed(e.to_string()));
            }
        }
    });

    let stream = UnboundedReceiverStream::new(rx).map(|msg| {
        let ev = match msg {
            ScanMsg::Progress(p) => Event::default().event("progress").json_data(p),
            ScanMsg::Done(s) => Event::default().event("summary").json_data(s),
            ScanMsg::Failed(m) => Ok(Event::default().event("error").data(m)),
        };
        Ok(ev.unwrap_or_else(|_| Event::default().event("error").data("serialize error")))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `GET /api/roots` — list watched folders (display strings).
async fn roots_list_handler(State(state): State<AppState>) -> Result<Json<Vec<String>>, ApiError> {
    let roots = on_read_db(&state, ops::list_roots).await?;
    Ok(Json(
        roots.iter().map(|p| p.display().to_string()).collect(),
    ))
}

#[derive(Deserialize)]
struct RootQuery {
    path: String,
    #[serde(default)]
    hide: bool,
}

/// `DELETE /api/roots?path=...` — stop watching a folder. 404 if not a root.
async fn roots_remove_handler(
    State(state): State<AppState>,
    Query(q): Query<RootQuery>,
) -> Result<StatusCode, ApiError> {
    let path = q.path.clone();
    let hide = q.hide;
    let removed = on_db(&state, move |db| {
        let p = std::path::Path::new(&path);
        let removed = ops::remove_root(db, p)?;
        if removed && hide {
            ops::mark_missing_under(db, p)?;
        }
        Ok(removed)
    })
    .await?;
    if removed {
        Ok(StatusCode::OK)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("not a watched root: {}", q.path),
        ))
    }
}

/// Snapshot the current subscribed-repo list as `RepoEntry` values ready for
/// `SettingsStore::set_repos`. DB rows are the source of truth for membership
/// (name/url); the per-repo `max_query_bits` override lives ONLY in `naiad.toml`,
/// so it is carried over here by name — otherwise every add/remove would wipe it.
fn current_repo_entries(
    db: &Db,
    prev: &[crate::settings::RepoEntry],
) -> anyhow::Result<Vec<crate::settings::RepoEntry>> {
    Ok(db
        .list_shared_services()?
        .into_iter()
        .map(|s| {
            let max_query_bits = prev
                .iter()
                .find(|r| r.name == s.name)
                .and_then(|r| r.max_query_bits);
            crate::settings::RepoEntry {
                name: s.name,
                url: s.url,
                max_query_bits,
            }
        })
        .collect())
}

/// `GET /api/repos` — list subscribed repositories.
async fn repos_list_handler(State(state): State<AppState>) -> Result<Json<Vec<RepoDto>>, ApiError> {
    let caps_cache = state.caps_cache.clone();
    let repos = on_read_db(&state, move |db| {
        Ok(ops::list_repos(db)?
            .into_iter()
            .map(|s| {
                // §7.1 / #179: populate all caps-derived fields from session-cached
                // data — no network call in a list endpoint.
                let cached = caps_cache.peek(s.id);
                let min_qb = cached.as_ref().and_then(|c| c.min_query_bits);
                let advertised_bits = cached.as_ref().and_then(|c| match c.mode {
                    naiad_netproto::PullMode::Bucketed { prefix_bits } => Some(prefix_bits),
                    naiad_netproto::PullMode::WholeRepo => None,
                });
                let count = cached.as_ref().and_then(|c| c.count);
                RepoDto {
                    name: s.name,
                    url: s.url,
                    max_query_bits: None, // populated below, after the lock
                    min_query_bits: min_qb,
                    advertised_bits,
                    count,
                }
            })
            .collect::<Vec<_>>())
    })
    .await?;
    // Second pass: populate max_query_bits outside the DB closure. This two-pass
    // approach keeps `state` (which owns the settings and caps_cache) out of the
    // `move` closure that runs on the DB thread — the closure already moved `caps_cache`.
    let repos: Vec<RepoDto> = repos
        .into_iter()
        .map(|mut r| {
            r.max_query_bits = Some(repo_max_query_bits(&state, &r.name));
            r
        })
        .collect();
    Ok(Json(repos))
}

/// `POST /api/repos` — subscribe to a repository. Validates the URL answers
/// the `/repo/caps` handshake BEFORE anything persists: a repo that cannot
/// shake hands never enters the system (400, DB and toml untouched). Name is
/// resolved as: caps-advertised name (trimmed, non-empty) → client-supplied
/// `name` (trimmed, non-empty) → URL hostname. Name collisions auto-suffix
/// (`-2`, `-3`, …). Duplicate URLs are rejected with 400.
/// On success the DB row and the `[[repos]]` toml section are updated together;
/// a failed toml write rolls the DB change back and fails the call.
async fn repos_add_handler(
    State(state): State<AppState>,
    Json(req): Json<RepoAddReq>,
) -> Result<Json<RepoDto>, ApiError> {
    let url = req.url.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        return Err(bad("repo url must be non-empty"));
    }
    // Pre-check for duplicate URL before the network handshake so the error is
    // a clean 400 rather than a confusing "already subscribed" from inside
    // on_db (which also maps to 400, but doing the check here avoids the
    // unnecessary caps round-trip for duplicate subscriptions).
    {
        let check_url = url.clone();
        let existing = on_read_db(&state, move |db| {
            let svcs = db.list_shared_services()?;
            Ok(svcs
                .into_iter()
                .find(|s| s.url.trim().trim_end_matches('/') == check_url.as_str()))
        })
        .await?;
        if let Some(existing) = existing {
            return Err(bad(format!(
                "already subscribed to {url} as {}",
                existing.name
            )));
        }
    }
    // Off-lock handshake — validates the server AND yields its advertised name.
    let caps = {
        let probe_url = url.clone();
        tokio::task::spawn_blocking(move || {
            naiad_netproto::RepoClient::new(&probe_url).fetch_caps()
        })
        .await
        .map_err(internal)?
        .map_err(|e| bad(format!("{url} did not answer the caps handshake: {e:#}")))?
    };
    let raw_name = caps
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            req.name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        });
    // Sanitize the resolved name: strip ASCII control characters and clamp to
    // 64 chars (char-boundary safe).  caps.name is remote-controlled; unbounded
    // or control-char names would be written into naiad.toml under the DB lock
    // and echoed to terminals.
    let base_name = {
        let sanitized: String = raw_name
            .as_deref()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        if sanitized.is_empty() {
            host_from_url(&url)
        } else {
            sanitized
        }
    };
    // In-process test states have no settings store — then the DB
    // change stands alone (there is no toml to keep in sync).
    let settings = state.settings.clone();
    let echo_url = url.clone();
    let resolved_name = on_db(&state, move |db| {
        // Race-safe duplicate URL check (inside the write lock).
        // If a duplicate slipped through the pre-check, bail here.
        let svcs = db.list_shared_services()?;
        if let Some(existing) = svcs
            .iter()
            .find(|s| s.url.trim().trim_end_matches('/') == url.as_str())
        {
            anyhow::bail!("already subscribed to {url} as {}", existing.name);
        }
        // Suffix-until-unique name collision resolution.  Use
        // `shared_service_name_taken` (not `shared_service_by_name`) so
        // detached rows are visible: `subscribe_shared_service` re-attaches a
        // detached row of the same name, which must never happen implicitly
        // under a server-advertised name.
        let mut name = base_name.clone();
        let mut n = 2u32;
        while db.shared_service_name_taken(&name)? {
            name = format!("{base_name}-{n}");
            n += 1;
        }
        // origin = None: the CLI/API surface was removed as inert (#166).
        // The DB-layer parameter is kept because the Hydrus bridge (#124)
        // will wire a real origin when subscribing shared services.
        let id = db.subscribe_shared_service(&name, &url, None)?;
        if let Some(settings) = settings {
            // The toml write is intentionally inside the DB lock so the DB row
            // and the toml section are updated atomically from the caller's
            // perspective. This is acceptable because subscribe is rare and
            // the toml write is small local IO (no network, no large file).
            let prev = settings.settings().repos.unwrap_or_default();
            if let Err(toml_err) = settings.set_repos(&current_repo_entries(db, &prev)?) {
                // Roll back: a brand-new row is dropped (it has no tags yet);
                // a re-attached one goes back to detached. If that also fails,
                // log both errors so neither is swallowed.
                match db.detach_service(id) {
                    Ok(()) => {}
                    Err(rollback_err) => {
                        tracing::error!(
                            target: "sync",
                            "add rollback: toml write failed ({toml_err:#}) AND \
                             detach rollback failed ({rollback_err:#}); DB and toml may be \
                             out of sync until boot reconcile heals it"
                        );
                    }
                }
                anyhow::bail!(
                    "persisting naiad.toml failed ({toml_err:#}); subscription rolled back"
                );
            }
        }
        Ok(name)
    })
    .await?;
    Ok(Json(RepoDto {
        name: resolved_name,
        url: echo_url,
        max_query_bits: None,
        min_query_bits: None,
        advertised_bits: None,
        count: None,
    }))
}

/// Extract the hostname from a URL string.
///
/// Strips the scheme (`"://"` prefix), the path (first `/`), a `user@` prefix
/// (text before the last `@`), and a trailing `:port` when the port is all
/// digits. Bracketed IPv6 addresses (`[::1]`) are unwrapped. Returns the whole
/// trimmed input if no host can be isolated.
fn host_from_url(url: &str) -> String {
    let s = url.trim();
    // Strip scheme.
    let after_scheme = if let Some(pos) = s.find("://") {
        &s[pos + 3..]
    } else {
        s
    };
    // Cut at first `/` to drop the path.
    let authority = match after_scheme.find('/') {
        Some(pos) => &after_scheme[..pos],
        None => after_scheme,
    };
    // Strip `user@` prefix (everything up to and including the last `@`).
    let host_and_port = match authority.rfind('@') {
        Some(pos) => &authority[pos + 1..],
        None => authority,
    };
    // Handle bracketed IPv6: `[::1]` or `[::1]:port`.
    if let Some(rest) = host_and_port.strip_prefix('[') {
        if let Some(bracket_end) = rest.find(']') {
            let inner = &rest[..bracket_end];
            if !inner.is_empty() {
                return inner.to_string();
            }
        }
    }
    // Strip a trailing `:port` when the port part is non-empty and all decimal digits.
    let host = match host_and_port.rfind(':') {
        Some(colon) => {
            let after = &host_and_port[colon + 1..];
            if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
                &host_and_port[..colon]
            } else {
                host_and_port
            }
        }
        None => host_and_port,
    };
    if host.is_empty() {
        s.to_string()
    } else {
        host.to_string()
    }
}

#[derive(Deserialize)]
struct RepoRemoveQuery {
    name: String,
    /// Also delete every tag the repo contributed. Default: keep them.
    #[serde(default)]
    purge: bool,
}

/// `DELETE /api/repos?name=...[&purge=true]` — unsubscribe. Default keeps
/// the repo's pulled tags (detach); `purge=true` deletes them too. 404 if
/// not subscribed. The toml write commits BEFORE an irreversible purge; a
/// failed write re-attaches and fails the call.
async fn repos_remove_handler(
    State(state): State<AppState>,
    Query(q): Query<RepoRemoveQuery>,
) -> Result<StatusCode, ApiError> {
    let settings = state.settings.clone(); // None in test states: skip write-through
    let name = q.name.clone();
    let purge = q.purge;
    let removed = on_db(&state, move |db| {
        let Some(svc) = db.shared_service_by_name(&name)? else {
            return Ok(false);
        };
        db.detach_service(svc.id)?;
        if let Some(settings) = settings {
            // The toml write is intentionally inside the DB lock so the DB
            // state and toml are updated atomically. This is acceptable because
            // unsubscribe is rare and the toml write is small local IO.
            let prev = settings.settings().repos.unwrap_or_default();
            if let Err(toml_err) = settings.set_repos(&current_repo_entries(db, &prev)?) {
                // Re-attach so DB and toml stay in sync; if that also fails,
                // log both errors so neither is swallowed before we bail.
                match db.set_service_url(svc.id, &svc.url) {
                    Ok(()) => {}
                    Err(reattach_err) => {
                        tracing::error!(
                            target: "sync",
                            "removal rollback: toml write failed ({toml_err:#}) AND \
                             re-attach failed ({reattach_err:#}); DB and toml may be out of sync"
                        );
                    }
                }
                anyhow::bail!("persisting naiad.toml failed ({toml_err:#}); removal rolled back");
            }
        }
        if purge {
            // Only after the toml committed — a purge is irreversible.
            db.drop_service(svc.id)?;
        }
        Ok(true)
    })
    .await?;
    if removed {
        Ok(StatusCode::OK)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no such repo: {}", q.name),
        ))
    }
}

/// `POST /api/repos/priority` — set the merge priority of a subscribed repo.
async fn repos_priority_handler(
    State(state): State<AppState>,
    Json(req): Json<RepoPriorityReq>,
) -> Result<StatusCode, ApiError> {
    on_db(&state, move |db| {
        ops::set_repo_priority(db, &req.name, req.priority)
    })
    .await?;
    Ok(StatusCode::OK)
}

/// `POST /api/repos/query-bits` — set (or clear, with `null`) the per-repo
/// privacy ceiling `max_query_bits`. Persisted to `naiad.toml`; applied on the
/// next pull via `repo_max_query_bits` (mtime-gated settings cache).
async fn repos_query_bits_handler(
    State(state): State<AppState>,
    Json(req): Json<RepoQueryBitsReq>,
) -> Result<StatusCode, ApiError> {
    if let Some(bits) = req.max_query_bits {
        if !(1..=256).contains(&bits) {
            return Err(bad("max_query_bits must be in [1, 256]"));
        }
    }
    let settings = state
        .settings
        .as_ref()
        .ok_or_else(|| bad("settings store unavailable"))?
        .clone();
    let name = req.name.clone();
    let bits = req.max_query_bits;
    let found = on_db(&state, move |db| {
        let Some(_) = db.shared_service_by_name(&name)? else {
            return Ok(false);
        };
        let prev = settings.settings().repos.unwrap_or_default();
        let mut entries = current_repo_entries(db, &prev)?;
        // The service exists in the DB, so it must appear in entries.
        if let Some(e) = entries.iter_mut().find(|r| r.name == name) {
            e.max_query_bits = bits;
        }
        settings
            .set_repos(&entries)
            .map_err(|e| anyhow::anyhow!("persisting naiad.toml failed: {e:#}"))?;
        Ok(true)
    })
    .await?;
    if found {
        Ok(StatusCode::OK)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no such repo: {}", req.name),
        ))
    }
}

/// Resolve the privacy ceiling for one repo: the `[[repos]]` per-repo
/// `max_query_bits` override when present (clamped to [1, 256]), else the
/// global `[privacy] max_query_bits`. Read per-request so hand edits apply on
/// the next pull without a daemon restart (mtime-gated settings cache).
fn repo_max_query_bits(state: &AppState, repo: &str) -> u32 {
    state.settings.as_ref().map_or_else(
        || PrivacySettings::default().max_query_bits,
        |s| {
            let settings = s.settings();
            let global = settings.privacy.max_query_bits;
            settings
                .repos
                .as_ref()
                .and_then(|rs| rs.iter().find(|r| r.name == repo))
                .map_or(global, |r| r.effective_max_query_bits(global))
        },
    )
}

/// `POST /api/repos/pull` — pull a repo's snapshot and merge owned matches.
/// Returns a summary of matched files and applied mapping rows. `key_path` is
/// the daemon's account key file; reserved for future use (currently unused).
async fn repos_pull_handler(
    State(state): State<AppState>,
    Json(req): Json<RepoPullReq>,
) -> Result<Json<RepoPullSummary>, ApiError> {
    let db = state.db.clone();
    let caps_cache = state.caps_cache.clone();
    let key_path = state.key_path.clone().map(|arc| (*arc).clone());
    let max_query_bits = repo_max_query_bits(&state, &req.name);
    let repo_name = req.name.clone();
    let stats = tokio::task::spawn_blocking(move || {
        ops::pull_repo(
            &db,
            &caps_cache,
            &req.name,
            max_query_bits,
            key_path.as_deref(),
        )
    })
    .await
    .map_err(internal)?
    .map_err(bad)?;
    // §7.3 / #179: drain any pending clamp notice for this service and surface
    // it in the summary so the UI can toast. The service id is resolved by a
    // fresh DB read: the spawn_blocking closure has already completed and
    // released the DB lock by this point, so a second read is the straightforward
    // way to map the repo name to its id without threading the id out of the closure.
    let notice = {
        let svc = on_read_db(&state, move |db| {
            Ok(ops::list_repos(db)?
                .into_iter()
                .find(|s| s.name == repo_name))
        })
        .await
        .ok()
        .flatten();
        svc.and_then(|s| state.caps_cache.drain_pending_notice(s.id))
    };
    Ok(Json(RepoPullSummary {
        matched_files: stats.matched_files,
        mappings: stats.mappings,
        notice,
    }))
}

/// `POST /api/files/pull-tags` — pull tags for specific files from every
/// subscribed repo, highest priority first. One repo failing never aborts
/// the others: its entry carries `error`, the response stays 200.
async fn files_pull_tags_handler(
    State(state): State<AppState>,
    Json(req): Json<FilePullReq>,
) -> Result<Json<Vec<FilePullRepoResult>>, ApiError> {
    if req.hashes.is_empty() {
        return Err(bad("no hashes given"));
    }
    let mut hashes = req
        .hashes
        .iter()
        .map(|h| {
            h.parse::<naiad_core::Hash>()
                .map_err(|e| bad(format!("bad hash {h:?}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    hashes.sort_unstable();
    hashes.dedup();
    // Subscribed repos, priority order (highest first, ties by id).
    let repos = on_read_db(&state, |db| {
        let mut with_p = ops::list_repos(db)?
            .into_iter()
            .map(|s| {
                let p = db.service_priority(s.id)?;
                Ok((p, s))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        with_p.sort_by_key(|(p, s)| (std::cmp::Reverse(*p), s.id));
        Ok(with_p.into_iter().map(|(_, s)| s).collect::<Vec<_>>())
    })
    .await?;
    let db = state.db.clone();
    let caps_cache = state.caps_cache.clone();
    let bits: Vec<u32> = repos
        .iter()
        .map(|svc| repo_max_query_bits(&state, &svc.name))
        .collect();
    let results = tokio::task::spawn_blocking(move || {
        repos
            .into_iter()
            .zip(bits)
            .map(|(svc, max_query_bits)| {
                match ops::pull_repo_for_hashes(
                    &db,
                    &caps_cache,
                    &svc.name,
                    max_query_bits,
                    &hashes,
                    &naiad_netproto::NoopObserver,
                ) {
                    Ok(outcome) => {
                        // §7.3 / #179: drain any pending clamp notice for this
                        // At most one notice per repo+domain per session;
                        // the first one is the floor-clamp advisory (#179).
                        let notice = caps_cache.drain_pending_notice(svc.id);
                        FilePullRepoResult {
                            repo: svc.name,
                            mappings_added: outcome.stats.mappings,
                            missing_sha256: outcome.missing_sha256.len() as u64,
                            error: None,
                            notice,
                        }
                    }
                    // A repo that failed outright never got as far as resolving
                    // interop hashes, so 0 is the honest count — not "none
                    // missing".
                    Err(e) => FilePullRepoResult {
                        repo: svc.name,
                        mappings_added: 0,
                        missing_sha256: 0,
                        error: Some(format!("{e:#}")),
                        notice: None,
                    },
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(internal)?;
    Ok(Json(results))
}

/// One lifecycle message from a streamed pull, mapped 1:1 onto SSE events.
enum PullMsg {
    Connecting(PullConnecting),
    Progress(PullProgress),
    /// Sub-repo progress (#172); interleaves with Connecting/Progress in order.
    Stage(PullStage),
    Summary(PullSummary),
    /// Stream-fatal: no repos or a bad request. Terminal.
    Error(String),
}

/// Per-repo progress observer for the streamed pull. Reuses the SSE handler's
/// existing `UnboundedSender` — `send` is non-blocking and thread-safe, so the
/// blocking pull thread posts `Stage` frames directly with no channel bridge
/// (#172 §4.5). `Cell`s are single-threaded: one `SseObserver` per repo lives
/// only on the one blocking worker and never crosses threads.
struct SseObserver {
    tx: tokio::sync::mpsc::UnboundedSender<PullMsg>,
    repo: String,
    index: usize,
    total: usize,
    /// Wall-clock start of this repo's fetch; read-only after construction.
    started: std::time::Instant,
    /// Running per-repo byte total; monotonic across both hash-domain legs.
    bytes: std::cell::Cell<u64>,
    /// Active hash-domain leg, set by `pull_repo_for_hashes` via `set_domain`.
    domain: std::cell::Cell<Option<&'static str>>,
    /// Cumulative hashes from all COMPLETED domain legs. Fixed for the duration
    /// of one leg; updated at each `set_domain` call to absorb the finished leg.
    hashes_base: std::cell::Cell<u64>,
    /// Cumulative tags from all COMPLETED domain legs. Mirrors `hashes_base`.
    tags_base: std::cell::Cell<u64>,
    /// Latest cross-leg cumulative hash count (monotonic). Carries the last-known
    /// value for `Merging`/`Done` where no chunk phase fires.
    last_hashes: std::cell::Cell<u64>,
    /// Latest cross-leg cumulative tag count (monotonic). Mirrors `last_hashes`.
    last_tags: std::cell::Cell<u64>,
    /// Last seen `total` (bucket count), for the `Done` arm's `chunk=chunk_total`
    /// semantics.
    last_total: std::cell::Cell<usize>,
    /// Cumulative window shrink-retries for this repo's pull (#177). Incremented
    /// on each `WindowRetry` phase; carried on every subsequent `PullStage`.
    retries: std::cell::Cell<u64>,
}

impl naiad_netproto::PullObserver for SseObserver {
    /// Update the active domain leg. On every domain switch the completed leg's
    /// cumulative hashes/tags are frozen into `hashes_base`/`tags_base` so the
    /// next leg's per-leg running totals add to the right cross-leg base.
    fn set_domain(&self, domain: Option<&'static str>) {
        self.hashes_base.set(self.last_hashes.get());
        self.tags_base.set(self.last_tags.get());
        self.domain.set(domain);
    }

    fn on_phase(&self, phase: naiad_netproto::PullPhase) {
        use naiad_netproto::PullPhase::{
            ChunkReceived, Done, Merging, RequestSent, RowReceived, WindowRetry,
        };
        // A closed receiver (client hung up) is ignored, exactly like the
        // existing Connecting/Progress sends.
        match phase {
            RequestSent {
                done,
                total,
                window,
            } => {
                self.last_total.set(total);
                let _ = self.tx.send(PullMsg::Stage(PullStage {
                    repo: self.repo.clone(),
                    index: self.index,
                    total: self.total,
                    phase: "request".into(),
                    chunk: done,
                    chunk_total: total,
                    bytes: self.bytes.get(),
                    domain: self.domain.get().map(str::to_string),
                    hashes: self.last_hashes.get(),
                    tags: self.last_tags.get(),
                    elapsed_ms: self.started.elapsed().as_millis() as u64,
                    window,
                    retries: self.retries.get(),
                }));
            }
            ChunkReceived {
                done,
                total,
                window,
                chunk_bytes,
                hashes,
                tags,
                request_ms,
                ..
            } => {
                self.bytes.set(self.bytes.get() + chunk_bytes as u64);
                self.last_total.set(total);
                let cumulative = self.bytes.get();
                let cumulative_hashes = self.hashes_base.get() + hashes as u64;
                let cumulative_tags = self.tags_base.get() + tags as u64;
                self.last_hashes.set(cumulative_hashes);
                self.last_tags.set(cumulative_tags);
                let repo = self.repo.as_str();
                let domain = self.domain.get().unwrap_or("");
                tracing::debug!(
                    target: "sync",
                    repo,
                    domain,
                    window,
                    done,
                    total,
                    chunk_bytes,
                    cumulative,
                    hashes,
                    tags,
                    request_ms,
                    "pull window"
                );
                let _ = self.tx.send(PullMsg::Stage(PullStage {
                    repo: self.repo.clone(),
                    index: self.index,
                    total: self.total,
                    phase: "chunk".into(),
                    chunk: done,
                    chunk_total: total,
                    bytes: cumulative,
                    domain: self.domain.get().map(str::to_string),
                    hashes: cumulative_hashes,
                    tags: cumulative_tags,
                    elapsed_ms: self.started.elapsed().as_millis() as u64,
                    window,
                    retries: self.retries.get(),
                }));
            }
            Merging => {
                let _ = self.tx.send(PullMsg::Stage(PullStage {
                    repo: self.repo.clone(),
                    index: self.index,
                    total: self.total,
                    phase: "merging".into(),
                    chunk: 0,
                    chunk_total: 0,
                    bytes: self.bytes.get(),
                    domain: self.domain.get().map(str::to_string),
                    hashes: self.last_hashes.get(),
                    tags: self.last_tags.get(),
                    elapsed_ms: self.started.elapsed().as_millis() as u64,
                    window: 0,
                    retries: self.retries.get(),
                }));
            }
            Done => {
                let t = self.last_total.get();
                let _ = self.tx.send(PullMsg::Stage(PullStage {
                    repo: self.repo.clone(),
                    index: self.index,
                    total: self.total,
                    phase: "done".into(),
                    chunk: t,
                    chunk_total: t,
                    bytes: self.bytes.get(),
                    domain: self.domain.get().map(str::to_string),
                    hashes: self.last_hashes.get(),
                    tags: self.last_tags.get(),
                    elapsed_ms: self.started.elapsed().as_millis() as u64,
                    window: 0,
                    retries: self.retries.get(),
                }));
            }
            // Within-window streaming row tick (#176): silently update the
            // running hash/tag totals so that the *next* ChunkReceived event
            // (emitted once the window completes) carries accurate numbers.
            // We deliberately do not send an SSE event per row to avoid
            // flooding the client at PTR scale (95k+ entries).
            // Clamp to be monotonic: a scratch-discard on retry can make
            // within-window counts momentarily tick backward (#177).
            RowReceived { hashes, tags } => {
                self.last_hashes
                    .set(self.last_hashes.get().max(hashes as u64));
                self.last_tags.set(self.last_tags.get().max(tags as u64));
            }
            // Window shrink-retry (#177): increment the retry counter and emit
            // a "retry" stage so the UI shows recovery instead of silence.
            WindowRetry {
                done,
                total,
                new_window,
                old_window,
                attempt,
                reason,
            } => {
                self.retries.set(self.retries.get() + 1);
                let retries = self.retries.get();
                let repo = self.repo.as_str();
                let domain = self.domain.get().unwrap_or("");
                tracing::debug!(
                    target: "sync",
                    repo,
                    domain,
                    done,
                    total,
                    old_window,
                    new_window,
                    attempt,
                    ?reason,
                    retries,
                    "pull window retry"
                );
                let _ = self.tx.send(PullMsg::Stage(PullStage {
                    repo: self.repo.clone(),
                    index: self.index,
                    total: self.total,
                    phase: "retry".into(),
                    chunk: done,
                    chunk_total: total,
                    bytes: self.bytes.get(),
                    domain: self.domain.get().map(str::to_string),
                    hashes: self.last_hashes.get(),
                    tags: self.last_tags.get(),
                    elapsed_ms: self.started.elapsed().as_millis() as u64,
                    window: new_window,
                    retries,
                }));
            }
        }
    }
}

/// Resolve the pull inputs up front: validate hashes and gather the subscribed
/// repos in priority order. `Err(message)` is a "cannot even begin" condition
/// that becomes a single terminal `error` event.
async fn prepare_pull_stream(
    state: &AppState,
    req: FilePullReq,
) -> Result<(Vec<Hash>, Vec<naiad_db::SharedService>), String> {
    if req.hashes.is_empty() {
        return Err("no hashes given".to_string());
    }
    let mut hashes = req
        .hashes
        .iter()
        .map(|h| {
            h.parse::<Hash>()
                .map_err(|e| format!("bad hash {h:?}: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    hashes.sort_unstable();
    hashes.dedup();
    // Subscribed repos, priority order (highest first, ties by id) — identical
    // resolution to files_pull_tags_handler.
    let repos = on_read_db(state, |db| {
        let mut with_p = ops::list_repos(db)?
            .into_iter()
            .map(|s| {
                let p = db.service_priority(s.id)?;
                Ok((p, s))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        with_p.sort_by_key(|(p, s)| (std::cmp::Reverse(*p), s.id));
        Ok(with_p.into_iter().map(|(_, s)| s).collect::<Vec<_>>())
    })
    .await
    .map_err(|e| e.1)?; // ApiError is (StatusCode, String); carry the message.
    if repos.is_empty() {
        return Err("no subscribed repositories".to_string());
    }
    Ok((hashes, repos))
}

/// `POST /api/files/pull-tags/stream` — SSE variant of the per-file pull.
/// Emits `connecting`/`progress` per repo, then a terminal `summary` (or a
/// single `error` for a cannot-begin condition). Per-repo failures stay
/// non-fatal: they land in `summary.results[].error`, exactly as the JSON
/// endpoint records them. Mirrors `hydrus_relations_stream_handler`.
async fn files_pull_tags_stream_handler(
    State(state): State<AppState>,
    Json(req): Json<FilePullReq>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<PullMsg>();

    match prepare_pull_stream(&state, req).await {
        Err(message) => {
            let _ = tx.send(PullMsg::Error(message));
        }
        Ok((hashes, repos)) => {
            let db = state.db.clone();
            let caps_cache = state.caps_cache.clone();
            let bits: Vec<u32> = repos
                .iter()
                .map(|svc| repo_max_query_bits(&state, &svc.name))
                .collect();
            tokio::task::spawn_blocking(move || {
                let total = repos.len();
                let mut results: Vec<PullRepoOutcome> = Vec::with_capacity(total);
                let mut cum_files: u64 = 0;
                let mut cum_mappings: u64 = 0;
                for (i, (svc, max_query_bits)) in repos.into_iter().zip(bits).enumerate() {
                    let _ = tx.send(PullMsg::Connecting(PullConnecting {
                        repo: svc.name.clone(),
                        index: i + 1,
                        total,
                    }));
                    let obs = SseObserver {
                        tx: tx.clone(),
                        repo: svc.name.clone(),
                        index: i + 1,
                        total,
                        started: std::time::Instant::now(),
                        bytes: std::cell::Cell::new(0),
                        domain: std::cell::Cell::new(None),
                        hashes_base: std::cell::Cell::new(0),
                        tags_base: std::cell::Cell::new(0),
                        last_hashes: std::cell::Cell::new(0),
                        last_tags: std::cell::Cell::new(0),
                        last_total: std::cell::Cell::new(0),
                        retries: std::cell::Cell::new(0),
                    };
                    let outcome = match ops::pull_repo_for_hashes(
                        &db,
                        &caps_cache,
                        &svc.name,
                        max_query_bits,
                        &hashes,
                        &obs,
                    ) {
                        Ok(outcome) => {
                            cum_files += outcome.stats.matched_files;
                            cum_mappings += outcome.stats.mappings;
                            // §7.3 / #179 / #192: drain any pending clamp notice
                            // for this service so the streamed summary surfaces
                            // it, exactly as the non-streamed FilePullRepoResult
                            // path does. At most one per repo+domain per session.
                            let notice = caps_cache.drain_pending_notice(svc.id);
                            PullRepoOutcome {
                                repo: svc.name.clone(),
                                matched_files: outcome.stats.matched_files,
                                mappings: outcome.stats.mappings,
                                missing_sha256: outcome.missing_sha256.len() as u64,
                                error: None,
                                notice,
                            }
                        }
                        // As above: a repo that failed outright never resolved
                        // any interop hash, so 0 is the honest count. Note the
                        // counts stay per-repo — `PullSummary` deliberately has
                        // no cumulative field to add them into.
                        Err(e) => PullRepoOutcome {
                            repo: svc.name.clone(),
                            matched_files: 0,
                            mappings: 0,
                            missing_sha256: 0,
                            error: Some(format!("{e:#}")),
                            notice: None,
                        },
                    };
                    results.push(outcome);
                    let _ = tx.send(PullMsg::Progress(PullProgress {
                        repos_done: i + 1,
                        repos_total: total,
                        repo: svc.name,
                        matched_files: cum_files,
                        mappings: cum_mappings,
                    }));
                }
                let _ = tx.send(PullMsg::Summary(PullSummary {
                    results,
                    matched_files: cum_files,
                    mappings: cum_mappings,
                }));
            });
        }
    }

    let stream = UnboundedReceiverStream::new(rx).map(|msg| {
        let ev = match msg {
            PullMsg::Connecting(c) => Event::default().event("connecting").json_data(c),
            PullMsg::Progress(p) => Event::default().event("progress").json_data(p),
            PullMsg::Stage(s) => Event::default().event("stage").json_data(s),
            PullMsg::Summary(s) => Event::default().event("summary").json_data(s),
            PullMsg::Error(m) => Event::default()
                .event("error")
                .json_data(PullError { message: m }),
        };
        Ok(ev.unwrap_or_else(|_| Event::default().event("error").data("serialize error")))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `POST /api/repos/submit` — sign one tag op and submit it to a subscribed repo.
async fn repos_submit_handler(
    State(state): State<AppState>,
    Json(req): Json<SubmitReq>,
) -> Result<StatusCode, ApiError> {
    let op = match req.op.as_str() {
        "add" => naiad_netproto::Op::Add,
        "remove" => naiad_netproto::Op::Remove,
        other => return Err(bad(format!("unknown op {other:?} (want add|remove)"))),
    };
    let key = state
        .key_path
        .clone()
        .ok_or_else(|| internal("no account key location configured"))?;
    let db = state.db.clone();
    let caps_cache = state.caps_cache.clone();
    let result = tokio::task::spawn_blocking(move || {
        ops::submit_to_repo(&db, &caps_cache, &key, &req.name, &req.file, &req.tag, op)
    })
    .await
    .map_err(internal)?;
    match result {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(ops::SubmitError::BadRequest(e)) => Err(bad(e)),
        Err(ops::SubmitError::Unsupported(e)) => Err(bad(e)),
        Err(ops::SubmitError::Upstream(e)) => Err(internal(e)),
    }
}

/// `POST /api/relations/submit` — sign one relation op and submit it to a repo.
async fn relations_submit_handler(
    State(state): State<AppState>,
    Json(req): Json<RelationSubmitReq>,
) -> Result<StatusCode, ApiError> {
    let kind = match req.kind.as_str() {
        "sibling" => naiad_netproto::RelKind::Sibling,
        "parent" => naiad_netproto::RelKind::Parent,
        other => return Err(bad(format!("unknown kind {other:?} (want sibling|parent)"))),
    };
    let op = match req.op.as_str() {
        "add" => naiad_netproto::Op::Add,
        "remove" => naiad_netproto::Op::Remove,
        other => return Err(bad(format!("unknown op {other:?} (want add|remove)"))),
    };
    let key = state
        .key_path
        .clone()
        .ok_or_else(|| internal("no account key location configured"))?;
    let db = state.db.clone();
    let caps_cache = state.caps_cache.clone();
    let result = tokio::task::spawn_blocking(move || {
        ops::submit_relation(
            &db,
            &caps_cache,
            &key,
            &req.name,
            kind,
            &req.from,
            &req.to,
            op,
        )
    })
    .await
    .map_err(internal)?;
    match result {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(ops::SubmitError::BadRequest(e)) => Err(bad(e)),
        Err(ops::SubmitError::Unsupported(e)) => Err(bad(e)),
        Err(ops::SubmitError::Upstream(e)) => Err(internal(e)),
    }
}

/// `POST /api/relations/pull` — bulk-pull a repo's relation graph and merge it.
async fn relations_pull_handler(
    State(state): State<AppState>,
    Json(req): Json<RelationPullReq>,
) -> Result<Json<RelationPullSummary>, ApiError> {
    let db = state.db.clone();
    let caps_cache = state.caps_cache.clone();
    let stats =
        tokio::task::spawn_blocking(move || ops::pull_relations(&db, &caps_cache, &req.name))
            .await
            .map_err(internal)?
            .map_err(bad)?;
    Ok(Json(RelationPullSummary {
        siblings: stats.siblings,
        parents: stats.parents,
    }))
}

/// `GET /api/account` — the local public key (non-creating) and key-file path.
async fn account_handler(State(state): State<AppState>) -> Result<Json<AccountDto>, ApiError> {
    let key = state
        .key_path
        .clone()
        .ok_or_else(|| internal("no account key location configured"))?;
    let key_path = key.display().to_string();
    let public_key = tokio::task::spawn_blocking(move || crate::account::load(&key))
        .await
        .map_err(internal)?
        .map_err(internal)?
        .map(|a| a.public_hex());
    Ok(Json(AccountDto {
        public_key,
        key_path,
    }))
}

#[derive(Deserialize)]
struct TagQuery {
    file: String,
    #[serde(default)]
    raw: bool,
    #[serde(default)]
    local_only: bool,
}

/// `GET /api/tags?file=&raw=` — a file's tags. `raw=true` is the literal stored
/// mappings; otherwise the computed (sibling/parent-expanded) set.
async fn tags_handler(
    State(state): State<AppState>,
    Query(q): Query<TagQuery>,
) -> Result<Json<Vec<String>>, ApiError> {
    let tags = on_read_db(&state, move |db| {
        if q.raw {
            Ok(ops::list_tags(db, &q.file)?
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>())
        } else {
            let scope = if q.local_only {
                naiad_db::ReadScope::LocalOnly
            } else {
                naiad_db::ReadScope::Merged
            };
            Ok(ops::display_tags(db, &q.file, scope)?)
        }
    })
    .await?;
    Ok(Json(tags))
}

#[derive(Deserialize)]
struct TagDetailQuery {
    file: String,
    #[serde(default)]
    local_only: bool,
}

/// `GET /api/tags/detailed?file=&local_only=` — a file's effective tags, each
/// with provenance and (for pulled-only tags) supporting authors + weights.
async fn tags_detailed_handler(
    State(state): State<AppState>,
    Query(q): Query<TagDetailQuery>,
) -> Result<Json<Vec<TagDetailDto>>, ApiError> {
    let scope = if q.local_only {
        naiad_db::ReadScope::LocalOnly
    } else {
        naiad_db::ReadScope::Merged
    };
    // A cold detail read cache-misses on the merged relation graph and would
    // build it (~34s) on the single tag-lane connection, stalling every other tag
    // read. Wait (off-lane, off-pool) for the background warmup to build it first
    // so this read is a cache hit (#126).
    await_relation_graph(&state).await;
    // Detail-panel queries resolve through the merged relation graph, so they
    // belong on the dedicated tag lane — not the shared read pool that also serves
    // /thumb and /file. Routing them here keeps a slow detail read from starving
    // thumbnails (#70; #50 only moved autocomplete/namespaces).
    let rows = on_tag_db(&state, move |db| {
        ops::display_tags_detailed(db, &q.file, scope)
    })
    .await?;
    let dto = rows
        .into_iter()
        .map(|t| TagDetailDto {
            tag: t.tag.to_string(),
            presence: match t.presence {
                naiad_db::TagPresence::Local => "local",
                naiad_db::TagPresence::Pulled => "pulled",
                naiad_db::TagPresence::Both => "both",
            }
            .to_string(),
            services: t.services,
            relations: t.relations,
            origin: t.origin,
        })
        .collect();
    Ok(Json(dto))
}

fn default_relations_cap() -> usize {
    10
}

#[derive(Deserialize)]
struct TagRelationsQuery {
    tag: String,
    file: Option<String>,
    #[serde(default)]
    local_only: bool,
    #[serde(default = "default_relations_cap")]
    cap: usize,
}

/// `GET /api/tags/relations?tag=&file=&local_only=&cap=` — aliases, parents, and
/// children for one tag, optionally anchored to a file to determine `via_alias`.
///
/// Dispatches on the same dedicated tag-DB lane as `tags/detailed` (#70/#76) so
/// a slow relation-graph walk cannot starve thumbnail or general read traffic.
/// `cap` is clamped server-side to `1..=10`.
async fn tags_relations_handler(
    State(state): State<AppState>,
    Query(q): Query<TagRelationsQuery>,
) -> Result<Json<TagRelationsDto>, ApiError> {
    let cap = q.cap.clamp(1, 10);
    let rows = on_tag_db(&state, move |db| {
        let scope = if q.local_only {
            naiad_db::ReadScope::LocalOnly
        } else {
            naiad_db::ReadScope::Merged
        };
        ops::tag_relations(db, &q.tag, q.file.as_deref(), scope, cap)
    })
    .await?;
    let dto = TagRelationsDto {
        canonical: rows.canonical.to_string(),
        count: rows.count,
        via_alias: rows.via_alias,
        aliases: to_relation_section(rows.aliases),
        parents: to_relation_section(rows.parents),
        children: to_relation_section(rows.children),
    };
    Ok(Json(dto))
}

/// Map one DB relation section to its wire DTO.
fn to_relation_section(s: naiad_db::RelationSection) -> RelationSectionDto {
    RelationSectionDto {
        items: s
            .items
            .into_iter()
            .map(|t| RelationTagDto {
                tag: t.tag.to_string(),
                count: t.count,
            })
            .collect(),
        total: s.total,
    }
}

/// `POST /api/tags/add`.
async fn tags_add_handler(
    State(state): State<AppState>,
    Json(req): Json<TagsReq>,
) -> Result<StatusCode, ApiError> {
    on_db(&state, move |db| ops::add_tags(db, &req.file, &req.tags)).await?;
    Ok(StatusCode::OK)
}

/// `POST /api/tags/remove`.
async fn tags_remove_handler(
    State(state): State<AppState>,
    Json(req): Json<TagsReq>,
) -> Result<StatusCode, ApiError> {
    on_db(&state, move |db| ops::remove_tags(db, &req.file, &req.tags)).await?;
    Ok(StatusCode::OK)
}

/// Token match strategy forwarded from the client query parameter.
#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum CompleteModeParam {
    #[default]
    Prefix,
    Substring,
}

impl From<CompleteModeParam> for naiad_db::CompletionMode {
    fn from(p: CompleteModeParam) -> Self {
        match p {
            CompleteModeParam::Prefix => naiad_db::CompletionMode::Prefix,
            CompleteModeParam::Substring => naiad_db::CompletionMode::Substring,
        }
    }
}

#[derive(Deserialize)]
struct CompleteParams {
    #[serde(default)]
    q: String,
    #[serde(default = "default_complete_limit")]
    limit: usize,
    #[serde(default)]
    mode: CompleteModeParam,
}

fn default_complete_limit() -> usize {
    20
}

/// `GET /api/tags/complete?q=&limit=&mode=` — typeahead suggestions for the current
/// search token. `namespaces` is populated only when `q` has no `:` (still
/// choosing a namespace); `tags` are ranked by current-mapping count, descending.
/// `mode` is `prefix` (default) or `substring`; prefix is the original behaviour.
/// Suggestions are unfiltered by trust/block/local-only — a typeahead, not a search.
async fn tags_complete_handler(
    State(state): State<AppState>,
    Query(p): Query<CompleteParams>,
) -> Result<Json<naiad_api::CompleteResponse>, ApiError> {
    let started = Instant::now();
    let q = p.q.trim().to_string();
    if q.is_empty() {
        return Ok(Json(naiad_api::CompleteResponse {
            namespaces: Vec::new(),
            tags: Vec::new(),
        }));
    }
    let limit = p.limit.clamp(1, 50);
    let want_ns = !q.contains(':');
    let q_for_log = q.clone();
    let mode = naiad_db::CompletionMode::from(p.mode);
    // Completion's alias overlay resolves through the merged relation graph, whose
    // first (~34s) build must not land on the single tag-lane connection. Wait
    // (off-lane, off-pool) for the background warmup to build it first (#126).
    await_relation_graph(&state).await;
    let (namespaces, tags) = on_tag_db(&state, move |db| {
        let tags = db.complete_tags(&q, limit, mode)?;
        let namespaces = if want_ns {
            db.complete_namespaces(&q, limit)?
        } else {
            Vec::new()
        };
        Ok((namespaces, tags))
    })
    .await?;
    tracing::debug!(target: "search", "tags_complete {q_for_log} took {:?}", started.elapsed());
    Ok(Json(naiad_api::CompleteResponse {
        namespaces: namespaces
            .into_iter()
            .map(|n| naiad_api::NamespaceSuggestionDto {
                namespace: n.namespace,
                tag_count: n.tag_count,
            })
            .collect(),
        tags: tags
            .into_iter()
            .map(|t| naiad_api::TagSuggestionDto {
                namespace: t.namespace,
                subtag: t.subtag,
                count: t.count,
                alias_source: t.alias_source,
            })
            .collect(),
    }))
}

/// `GET /api/namespaces` - every non-empty namespace in the library, ranked by
/// distinct current tag count. Used by the nav rail, not typeahead.
async fn namespaces_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<naiad_api::NamespaceSuggestionDto>>, ApiError> {
    let out = on_tag_db(&state, move |db| Ok(db.complete_namespaces("", 200)?)).await?;
    Ok(Json(
        out.into_iter()
            .map(|n| naiad_api::NamespaceSuggestionDto {
                namespace: n.namespace,
                tag_count: n.tag_count,
            })
            .collect(),
    ))
}

/// Log a thumbnail route's latency at `debug` under the `thumb` target. Only the
/// thumb hit/gen routes use this; other routes log their own latency inline so
/// each stays under its own subsystem target.
fn log_latency(route: &str, detail: &str, started: Instant) {
    tracing::debug!(target: "thumb", "{route} {detail} took {:?}", started.elapsed());
}

/// `GET /api/siblings`.
async fn siblings_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<SiblingDto>>, ApiError> {
    let sibs = on_read_db(&state, |db| {
        Ok(ops::list_siblings(db)?
            .into_iter()
            .map(|(bad, ideal)| SiblingDto {
                bad: bad.to_string(),
                ideal: ideal.to_string(),
            })
            .collect::<Vec<_>>())
    })
    .await?;
    Ok(Json(sibs))
}

/// `POST /api/siblings/add`.
async fn siblings_add_handler(
    State(state): State<AppState>,
    Json(req): Json<SiblingDto>,
) -> Result<StatusCode, ApiError> {
    on_db(&state, move |db| ops::add_sibling(db, &req.bad, &req.ideal)).await?;
    Ok(StatusCode::OK)
}

/// `POST /api/siblings/remove`.
async fn siblings_remove_handler(
    State(state): State<AppState>,
    Json(req): Json<SiblingRemoveReq>,
) -> Result<StatusCode, ApiError> {
    on_db(&state, move |db| ops::remove_sibling(db, &req.bad)).await?;
    Ok(StatusCode::OK)
}

/// `GET /api/parents`.
async fn parents_handler(State(state): State<AppState>) -> Result<Json<Vec<ParentDto>>, ApiError> {
    let pars = on_read_db(&state, |db| {
        Ok(ops::list_parents(db)?
            .into_iter()
            .map(|(child, parent)| ParentDto {
                child: child.to_string(),
                parent: parent.to_string(),
            })
            .collect::<Vec<_>>())
    })
    .await?;
    Ok(Json(pars))
}

/// `GET /api/relations`.
async fn relations_list_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<RelationEdgeDto>>, ApiError> {
    let edges = on_read_db(&state, |db| {
        Ok(ops::list_relations(db)?
            .into_iter()
            .map(|e| RelationEdgeDto {
                kind: e.kind.as_str().to_string(),
                from: e.from.to_string(),
                to: e.to.to_string(),
                service: e.service,
                author: e.author,
            })
            .collect::<Vec<_>>())
    })
    .await?;
    Ok(Json(edges))
}

/// `GET /api/relations/status`.
async fn relations_status_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<RelationStatusDto>>, ApiError> {
    let rows = on_read_db(&state, |db| {
        Ok(ops::relation_status(db)?
            .into_iter()
            .map(|s| RelationStatusDto {
                service: s.service,
                siblings: s.siblings,
                parents: s.parents,
                last_pull: s.last_pull,
            })
            .collect::<Vec<_>>())
    })
    .await?;
    Ok(Json(rows))
}

/// `GET /api/blocks`.
async fn blocks_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<BlockRuleDto>>, ApiError> {
    let rows = on_read_db(&state, |db| {
        Ok(ops::list_blocks(db)?
            .into_iter()
            .map(|b| BlockRuleDto {
                id: b.id,
                kind: b.kind.as_str().to_string(),
                target: b.target,
                note: b.note,
                created_at: b.created_at,
            })
            .collect::<Vec<_>>())
    })
    .await?;
    Ok(Json(rows))
}

/// `POST /api/blocks`.
async fn blocks_add_handler(
    State(state): State<AppState>,
    Json(req): Json<BlockAddReq>,
) -> Result<StatusCode, ApiError> {
    on_db(&state, move |db| {
        ops::add_block(db, &req.kind, &req.target, req.note.as_deref())?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::OK)
}

#[derive(serde::Deserialize)]
struct BlockRemoveParams {
    id: i64,
}

/// `DELETE /api/blocks?id=...` — remove a block rule. 404 if no such id.
async fn blocks_remove_handler(
    State(state): State<AppState>,
    Query(params): Query<BlockRemoveParams>,
) -> Result<StatusCode, ApiError> {
    let removed = on_db(&state, move |db| ops::remove_block(db, params.id)).await?;
    if removed {
        Ok(StatusCode::OK)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no block rule with id {}", params.id),
        ))
    }
}

/// `POST /api/reject` — reject one pulled mapping. Writes locally under a
/// brief DB lock, then checks the caps cache for the `reports` capability. An
/// unreachable repo returns `reports: false`; the local rejection has already
/// succeeded and must not fail because of it.
async fn reject_handler(
    State(state): State<AppState>,
    Json(req): Json<RejectRequest>,
) -> Result<Json<RejectResponse>, ApiError> {
    let db = state.db.clone();
    let caps_cache = state.caps_cache.clone();
    let result = tokio::task::spawn_blocking(move || {
        ops::reject_mapping(
            &db,
            &caps_cache,
            &req.service,
            &req.hash,
            &req.tag,
            req.note.as_deref(),
        )
    })
    .await
    .map_err(internal)?;
    match result {
        Ok(reports) => Ok(Json(RejectResponse { reports })),
        Err(ops::SubmitError::BadRequest(e)) => Err(bad(e)),
        Err(ops::SubmitError::Unsupported(e)) => Err(bad(e)),
        Err(ops::SubmitError::Upstream(e)) => Err(internal(e)),
    }
}

/// `POST /api/report` — forward an anonymous report to the originating repo.
/// Fire-and-forget: no local record is written. Returns 204 on success, 400
/// on bad input or unsupported capability, 500 on upstream/key errors.
async fn report_handler(
    State(state): State<AppState>,
    Json(req): Json<ReportRequest>,
) -> Result<StatusCode, ApiError> {
    let key = state
        .key_path
        .clone()
        .ok_or_else(|| internal("no account key location configured"))?;
    let db = state.db.clone();
    let caps_cache = state.caps_cache.clone();
    let result = tokio::task::spawn_blocking(move || {
        ops::report_mapping(
            &db,
            &caps_cache,
            &key,
            &req.service,
            &req.hash,
            &req.tag,
            req.note.as_deref(),
        )
    })
    .await
    .map_err(internal)?;
    match result {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(ops::SubmitError::BadRequest(e)) => Err(bad(e)),
        Err(ops::SubmitError::Unsupported(e)) => Err(bad(e)),
        Err(ops::SubmitError::Upstream(e)) => Err(internal(e)),
    }
}

#[derive(serde::Deserialize)]
struct RejectRemoveParams {
    hash: String,
    tag: String,
    service: String,
}

/// `DELETE /api/reject?hash=&tag=&service=` — undo a rejection. Idempotent.
async fn reject_remove_handler(
    State(state): State<AppState>,
    Query(params): Query<RejectRemoveParams>,
) -> Result<StatusCode, ApiError> {
    let hash = params.hash.clone();
    let tag = params.tag.clone();
    let service = params.service.clone();
    // Wrap the SubmitError result in Ok so it passes through run_locked_raw's
    // anyhow layer intact; the match arm below maps variants to 4xx/5xx.
    let result = run_locked_raw(state.db.clone(), move |db| {
        Ok(ops::undo_rejection(db, &service, &hash, &tag))
    })
    .await
    .map_err(internal)? // JoinError → 500
    .map_err(internal)?; // anyhow::Error (never raised) → 500
    match result {
        Ok(()) => Ok(StatusCode::OK),
        Err(ops::SubmitError::BadRequest(e)) => Err(bad(e)),
        Err(ops::SubmitError::Unsupported(e)) => Err(bad(e)),
        Err(ops::SubmitError::Upstream(e)) => Err(internal(e)),
    }
}

#[derive(serde::Deserialize)]
struct RejectionsListParams {
    hash: Option<String>,
}

/// `GET /api/rejections?hash=` — list rejections, optionally scoped to one file.
async fn rejections_list_handler(
    State(state): State<AppState>,
    Query(params): Query<RejectionsListParams>,
) -> Result<Json<Vec<RejectionDto>>, ApiError> {
    let rows = on_read_db(&state, move |db| {
        ops::list_rejections_op(db, params.hash.as_deref())
    })
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| RejectionDto {
                hash: r.hash,
                service: r.service,
                tag: r.tag,
                note: r.note,
                created_at: r.created_at,
            })
            .collect(),
    ))
}

fn default_gallery_sort() -> GallerySortDto {
    GallerySortDto {
        key: "imported_at".to_string(),
        direction: "desc".to_string(),
    }
}

fn valid_gallery_sort_key(key: &str) -> bool {
    matches!(
        key,
        "imported_at" | "created_at" | "modified_at" | "name" | "size" | "type"
    )
}

fn valid_gallery_sort_direction(direction: &str) -> bool {
    matches!(direction, "asc" | "desc")
}

fn validate_gallery_sort(sort: GallerySortDto) -> Result<GallerySortDto, ApiError> {
    if valid_gallery_sort_key(&sort.key) && valid_gallery_sort_direction(&sort.direction) {
        Ok(sort)
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "invalid gallery sort".to_string(),
        ))
    }
}

fn encode_gallery_sort(sort: &GallerySortDto) -> String {
    format!("{}:{}", sort.key, sort.direction)
}

fn decode_gallery_sort(raw: Option<String>) -> GallerySortDto {
    let Some(raw) = raw else {
        return default_gallery_sort();
    };
    let Some((key, direction)) = raw.split_once(':') else {
        return default_gallery_sort();
    };
    if valid_gallery_sort_key(key) && valid_gallery_sort_direction(direction) {
        GallerySortDto {
            key: key.to_string(),
            direction: direction.to_string(),
        }
    } else {
        default_gallery_sort()
    }
}

/// `GET /api/view/sort` — read the gallery sort preference from `app_settings`.
async fn view_sort_get_handler(
    State(state): State<AppState>,
) -> Result<Json<GallerySortDto>, ApiError> {
    let raw = on_read_db(&state, |db| Ok(db.app_setting(GALLERY_SORT_SETTING_KEY)?)).await?;
    Ok(Json(decode_gallery_sort(raw)))
}

/// `POST /api/view/sort` — persist the gallery sort preference to `app_settings`.
async fn view_sort_set_handler(
    State(state): State<AppState>,
    Json(req): Json<GallerySortDto>,
) -> Result<StatusCode, ApiError> {
    let sort = validate_gallery_sort(req)?;
    let value = encode_gallery_sort(&sort);
    on_db(&state, move |db| {
        db.set_app_setting(GALLERY_SORT_SETTING_KEY, &value)?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::OK)
}

/// `POST /api/backup` — back up the library database via `VACUUM INTO`.
///
/// Opens a fresh read-only connection to the DB file so `VACUUM INTO` runs
/// concurrently with other reads and writes without holding the writer mutex.
/// `dest: null/absent` → default timestamped path under `<db_dir>/backups/`.
/// Returns [`BackupSummary`] with the written path, file size, and elapsed time.
async fn backup_handler(
    State(state): State<AppState>,
    Json(req): Json<BackupReq>,
) -> Result<Json<BackupSummary>, ApiError> {
    let db_dir =
        state.db_dir.clone().map(|a| (*a).clone()).ok_or_else(|| {
            internal("no db_dir configured (daemon not started from a file path)")
        })?;
    let src_db_path =
        state.db_path.clone().map(|a| (*a).clone()).ok_or_else(|| {
            internal("no db_path configured (daemon not started from a file path)")
        })?;
    let dest = req.dest.clone();
    // The Db mutex is NOT held here: do_backup opens its own read-only
    // connection so the writer can continue while the snapshot runs.
    let result =
        tokio::task::spawn_blocking(move || ops::do_backup(&src_db_path, &db_dir, dest.as_deref()))
            .await
            .map_err(internal)?;
    match result {
        Ok(r) => Ok(Json(BackupSummary {
            dest: r.dest.to_string_lossy().into_owned(),
            bytes: r.bytes,
            duration_ms: r.duration_ms,
        })),
        Err(ops::BackupError::BadRequest(e)) => Err(bad(e)),
        Err(ops::BackupError::Internal(e)) => Err(internal(e)),
    }
}

/// `POST /api/parents/add`.
async fn parents_add_handler(
    State(state): State<AppState>,
    Json(req): Json<ParentDto>,
) -> Result<StatusCode, ApiError> {
    on_db(&state, move |db| {
        ops::add_parent(db, &req.child, &req.parent)
    })
    .await?;
    Ok(StatusCode::OK)
}

/// `POST /api/parents/remove`.
async fn parents_remove_handler(
    State(state): State<AppState>,
    Json(req): Json<ParentDto>,
) -> Result<StatusCode, ApiError> {
    on_db(&state, move |db| {
        ops::remove_parent(db, &req.child, &req.parent)
    })
    .await?;
    Ok(StatusCode::OK)
}

/// `GET /api/plugins` — list registered plugins with their capabilities.
async fn plugins_handler() -> Json<Vec<PluginDto>> {
    let list = crate::plugins::list_plugins()
        .into_iter()
        .map(|(id, name, tagger, processor, source)| PluginDto {
            id,
            name,
            tagger,
            processor,
            source,
        })
        .collect();
    Json(list)
}

/// Build the live Hydrus config from the settings file. Unconfigured (no settings
/// file, or no `[hydrus]` dir) yields the default, which import/lookup reject with
/// a "not configured" error — same behavior as before, now file-backed.
fn hydrus_config(state: &AppState) -> crate::plugins::HydrusConfig {
    let Some(settings) = state.settings.as_ref() else {
        return crate::plugins::HydrusConfig::default();
    };
    let h = settings.hydrus();
    crate::plugins::HydrusConfig {
        dir: h.dir.map(std::path::PathBuf::from),
        tag_services: h.tag_services,
    }
}

/// `POST /api/hydrus/configure` — persist the Hydrus DB directory and tag services
/// to `naiad.toml`.
async fn hydrus_configure_handler(
    State(state): State<AppState>,
    Json(req): Json<HydrusConfigReq>,
) -> Result<StatusCode, ApiError> {
    let store = state
        .settings
        .clone()
        .ok_or_else(|| internal("no settings file location configured"))?;
    tokio::task::spawn_blocking(move || {
        let dir = (!req.dir.trim().is_empty()).then_some(req.dir.as_str());
        store.set_hydrus(dir, &req.tag_services)
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/hydrus/config` — read the persisted Hydrus config from `naiad.toml`.
async fn hydrus_config_get_handler(State(state): State<AppState>) -> Json<HydrusConfigDto> {
    let cfg = hydrus_config(&state);
    Json(HydrusConfigDto {
        dir: cfg.dir.map(|p| p.to_string_lossy().into_owned()),
        tag_services: cfg.tag_services,
    })
}

/// `POST /api/tagger/lookup` — look up tags for files from the Hydrus tagger.
/// When `apply=true`, the tags are also written into the local service.
async fn tagger_lookup_handler(
    State(state): State<AppState>,
    Json(req): Json<TaggerLookupReq>,
) -> Result<Json<Vec<TaggerLookupItem>>, ApiError> {
    let db = state.db.clone();
    let cfg = hydrus_config(&state);
    let items = tokio::task::spawn_blocking(move || {
        let db = db.lock_recover();
        let mut out = Vec::new();
        for f in req.files {
            let tags =
                crate::plugins::lookup(&db, &cfg, &f, req.apply).map_err(|e| e.to_string())?;
            out.push(TaggerLookupItem {
                file: f,
                tags: tags.iter().map(|t| t.to_string()).collect(),
            });
        }
        Ok::<_, String>(out)
    })
    .await
    .map_err(internal)?
    .map_err(bad)?;
    Ok(Json(items))
}

/// `POST /api/source/import` — bulk Hydrus import. `library_only` pulls tags just
/// for files already in the library; otherwise every Hydrus-owned file plus the
/// relation graph. Reads (`/file`, `/search`, …) use a separate connection, so
/// the UI stays responsive while this writer runs.
///
/// For the full (non-library) import the writer mutex is held only in short
/// bursts — setup, one burst per write batch (4 096 records), and a final
/// resolve — so interactive writes can interleave.
async fn source_import_handler(
    State(state): State<AppState>,
    Json(req): Json<SourceImportReq>,
) -> Result<Json<SourceImportSummary>, ApiError> {
    let db = state.db.clone();
    let cfg = hydrus_config(&state);
    let outcome = tokio::task::spawn_blocking(move || {
        if req.library_only {
            crate::plugins::run_library_import(&db, &cfg).map_err(|e| e.to_string())
        } else {
            crate::plugins::run_import(&db, &cfg).map_err(|e| e.to_string())
        }
    })
    .await
    .map_err(internal)?
    .map_err(bad)?;
    Ok(Json(SourceImportSummary {
        mappings_staged: outcome.mappings_staged,
        mappings_resolved: outcome.mappings_resolved,
        siblings: outcome.siblings,
        parents: outcome.parents,
        sha256_backfilled: outcome.sha256_backfilled,
    }))
}

#[derive(Deserialize)]
struct ImportStreamQuery {
    #[allow(dead_code)]
    plugin_id: Option<String>,
}

enum ImportMsg {
    Progress(ImportProgress),
    Done(SourceImportSummary),
    Failed(String),
}

/// `GET /api/source/import/stream` — SSE variant of the **library** import: emits
/// `progress` ticks as files are tagged (tags are committed in batches, so they
/// land file-by-file and survive an interrupted stream), then a terminal
/// `summary` (or `error`) event. The synchronous `POST /api/source/import`
/// remains the canonical path for the full import and the CLI.
async fn source_import_stream_handler(
    State(state): State<AppState>,
    Query(_q): Query<ImportStreamQuery>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ImportMsg>();
    let db = state.db.clone();
    let cfg = hydrus_config(&state);
    tokio::task::spawn_blocking(move || {
        let tx_prog = tx.clone();
        let res = crate::plugins::run_library_import_with_progress(
            &db,
            &cfg,
            |files, total, mappings| {
                let _ = tx_prog.send(ImportMsg::Progress(ImportProgress {
                    files,
                    total,
                    mappings,
                }));
            },
        );
        match res {
            Ok(o) => {
                let _ = tx.send(ImportMsg::Done(SourceImportSummary {
                    mappings_staged: o.mappings_staged,
                    mappings_resolved: o.mappings_resolved,
                    siblings: o.siblings,
                    parents: o.parents,
                    sha256_backfilled: o.sha256_backfilled,
                }));
            }
            Err(e) => {
                let _ = tx.send(ImportMsg::Failed(e.to_string()));
            }
        }
    });

    let stream = UnboundedReceiverStream::new(rx).map(|msg| {
        let ev = match msg {
            ImportMsg::Progress(p) => Event::default().event("progress").json_data(p),
            ImportMsg::Done(s) => Event::default().event("summary").json_data(s),
            ImportMsg::Failed(m) => Ok(Event::default().event("error").data(m)),
        };
        Ok(ev.unwrap_or_else(|_| Event::default().event("error").data("serialize error")))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `POST /api/hydrus/relations` — pull the full Hydrus sibling/parent graph
/// (issue #41): no mapping work, no sha256 backfill. Synchronous; the UI uses
/// the SSE variant below for a determinate progress bar.
///
/// The writer mutex is held only in short bursts (one per write batch of
/// 4 096 relation records), so interactive writes can interleave during a
/// large relation pull.
async fn hydrus_relations_handler(
    State(state): State<AppState>,
) -> Result<Json<RelationsImportSummary>, ApiError> {
    let db = state.db.clone();
    let cfg = hydrus_config(&state);
    let outcome = tokio::task::spawn_blocking(move || {
        crate::plugins::run_relations_import(&db, &cfg).map_err(|e| e.to_string())
    })
    .await
    .map_err(internal)?
    .map_err(bad)?;
    Ok(Json(RelationsImportSummary {
        siblings: outcome.siblings,
        parents: outcome.parents,
    }))
}

enum RelationsMsg {
    Progress(RelationsProgress),
    Done(RelationsImportSummary),
    Failed(String),
}

/// `GET /api/hydrus/relations/stream` — SSE variant of the relations pull:
/// determinate `progress` ticks (total known up front), then a terminal
/// `summary` (or `error`) event. Mirrors `source_import_stream_handler`.
///
/// The writer mutex is held only in short bursts (one per write batch of
/// 4 096 relation records), so interactive writes can interleave during a
/// large relation stream.
async fn hydrus_relations_stream_handler(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<RelationsMsg>();
    let db = state.db.clone();
    let cfg = hydrus_config(&state);
    tokio::task::spawn_blocking(move || {
        let tx_prog = tx.clone();
        let res = crate::plugins::run_relations_import_with_progress(
            &db,
            &cfg,
            |edges_done, edges_total, siblings, parents| {
                let _ = tx_prog.send(RelationsMsg::Progress(RelationsProgress {
                    edges_done,
                    edges_total,
                    siblings,
                    parents,
                }));
            },
        );
        match res {
            Ok(o) => {
                let _ = tx.send(RelationsMsg::Done(RelationsImportSummary {
                    siblings: o.siblings,
                    parents: o.parents,
                }));
            }
            Err(e) => {
                let _ = tx.send(RelationsMsg::Failed(e.to_string()));
            }
        }
    });

    let stream = UnboundedReceiverStream::new(rx).map(|msg| {
        let ev = match msg {
            RelationsMsg::Progress(p) => Event::default().event("progress").json_data(p),
            RelationsMsg::Done(s) => Event::default().event("summary").json_data(s),
            RelationsMsg::Failed(m) => Ok(Event::default().event("error").data(m)),
        };
        Ok(ev.unwrap_or_else(|_| Event::default().event("error").data("serialize error")))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Router-level lane-isolation tests (complement the unit tests in `mod tests`):
/// drive real read-pool + tag-db connections through the axum router to prove
/// each lane stays responsive while the other is saturated or held.
#[cfg(test)]
mod lane_tests {
    use super::app;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_store(dir: &tempfile::TempDir) -> crate::thumb_store::ThumbStore {
        crate::thumb_store::ThumbStore::open(&dir.path().join("thumbs.db")).unwrap()
    }

    /// Generate a small valid PNG via the `image` crate (same dep as production).
    fn make_png() -> Vec<u8> {
        let img = image::RgbImage::from_fn(8, 8, |x, y| {
            image::Rgb([(x * 30) as u8, (y * 30) as u8, 128])
        });
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// Create a file-backed DB with one imported PNG tagged "character:alice".
    /// Returns (db_dir, files_dir, db_path, writer_db, hash_hex).
    /// All TempDirs and the Db must be kept alive by the caller.
    fn seed_db() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        std::path::PathBuf,
        naiad_db::Db,
        String,
    ) {
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let files_dir = tempfile::tempdir().unwrap();

        let file_path = files_dir.path().join("test.png");
        std::fs::write(&file_path, make_png()).unwrap();

        // Open the writer, import the file, add a tag, then keep the writer
        // alive — the pool will open its own read-only connections from the
        // same WAL-mode file and sees all committed data immediately.
        let db = naiad_db::Db::open(&db_path).unwrap();
        crate::ops::import_path(&db, files_dir.path(), |_| {}).unwrap();
        let files = db.list_files().unwrap();
        assert!(
            !files.is_empty(),
            "import must have found at least one file"
        );
        let hash = files[0].hash.to_hex();
        crate::ops::add_tags(&db, &hash, &["character:alice".to_string()]).unwrap();

        (db_dir, files_dir, db_path, db, hash)
    }

    /// GET helper: fires one request through the router.
    async fn get(state: &crate::AppState, uri: &str) -> StatusCode {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        app(state.clone()).oneshot(req).await.unwrap().status()
    }

    /// Test 1: tag completion answers promptly even when every pool permit is held.
    ///
    /// tags_complete_handler uses on_tag_db → dedicated tag_db connection, so it
    /// bypasses the semaphore entirely.
    #[tokio::test(flavor = "multi_thread")]
    async fn completion_answers_while_pool_saturated() {
        let (db_dir, _files, db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();

        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);

        // Grab every pool permit — any on_read_db call must now wait.
        let _all = state.read_pool.as_ref().unwrap().exhaust().await;

        let status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            get(&state, "/api/tags/complete?q=c"),
        )
        .await
        .expect("tags/complete timed out while pool was saturated");

        assert_eq!(status, StatusCode::OK);
        drop(db_dir);
    }

    /// An `AppState` with no read pool never spawns a warmup; by construction
    /// both `graph_ready` and `warmup_done` are pre-fired (fail-open). So every
    /// consumer — `await_relation_graph` and the `with_watch` catch-up defer —
    /// falls through instantly instead of eating its 60s/300s backstop (#132).
    #[tokio::test(flavor = "multi_thread")]
    async fn tag_reads_do_not_wait_on_a_warmup_that_was_never_spawned() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        // No `with_read_db`: no pool, no tag lane, and no warmup task.
        let state = crate::AppState::new(db, test_store(&thumbs), 64);

        // By construction both gates must be pre-fired on a pool-less state.
        assert!(
            state.graph_ready.is_fired(),
            "graph_ready must be pre-fired on AppState::new (fail-open, #132)"
        );
        assert!(
            state.warmup_done.is_fired(),
            "warmup_done must be pre-fired on AppState::new (fail-open, #132)"
        );

        // A first gallery query fires the startup gate; with fail-open gates,
        // await_relation_graph must still return promptly.
        state.startup_gate.fire();
        assert!(state.startup_gate.is_fired());

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::await_relation_graph(&state),
        )
        .await
        .expect("must not wait for a graph nobody is building");

        // The `with_watch` catch-up defer waits on `warmup_done` with a 300s
        // backstop — it must fall through immediately on an unarmed state.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.warmup_done.wait(crate::CATCHUP_SCAN_DEFER_TIMEOUT),
        )
        .await
        .expect("warmup_done.wait must return immediately on a pre-fired gate");

        let status = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            get(&state, "/api/tags/complete?q=c"),
        )
        .await
        .expect("tags/complete must answer without a warmup");
        assert_eq!(status, StatusCode::OK);
        drop(db_dir);
    }

    /// The background warmup builds the relation graph and releases the
    /// graph-ready + warmup-done signals once the first gallery query fires the
    /// startup gate. Interactive tag reads wait on `graph_ready` (off-lane,
    /// off-pool) so the ~34s cold build never lands on the tag lane (#126).
    #[tokio::test(flavor = "multi_thread")]
    async fn warmup_releases_graph_and_scan_gates_after_first_query() {
        let (db_dir, _files, db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);

        // After with_read_db the gates are re-armed (no longer pre-fired).
        // This proves the re-arm happened: if spawn_cache_warmup left the gates
        // pre-fired, this assert would fail, catching any "forgot to re-arm" mutation.
        assert!(
            !state.graph_ready.is_fired(),
            "graph_ready must be re-armed (not pre-fired) after with_read_db"
        );
        assert!(
            !state.warmup_done.is_fired(),
            "warmup_done must be re-armed (not pre-fired) after with_read_db"
        );
        super::await_relation_graph(&state).await; // returns at once (gate unfired before first query)
        assert!(!state.graph_ready.is_fired());

        // Simulate the first `/api/search` releasing the warmup.
        state.startup_gate.fire();

        // The warmup builds the graph, fires graph_ready, warms completion, then
        // fires warmup_done (which releases the deferred catch-up scan).
        state
            .warmup_done
            .wait(std::time::Duration::from_secs(10))
            .await;
        assert!(
            state.warmup_done.is_fired(),
            "warmup_done must fire so the catch-up scan is released"
        );
        assert!(
            state.graph_ready.is_fired(),
            "graph_ready must fire once the warmup has built the relation graph"
        );
        drop(db_dir);
    }

    /// Test 2: search and a cached thumbnail answer while the tag lane is locked.
    ///
    /// search uses on_read_db (pool, not tag_db).
    /// A cached thumbnail hits the file system fast path — no DB at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn search_and_cached_thumb_answer_while_tag_lane_held() {
        let (db_dir, files_dir, db_path, db, hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();

        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);

        // Warm the thumbnail cache so the second request hits the fast path.
        // 10 s: cold decode + cache write; 2× the 5 s timeouts used below.
        let thumb_status = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            get(&state, &format!("/thumb/{hash}")),
        )
        .await
        .expect("initial thumb request timed out");
        assert_eq!(
            thumb_status,
            StatusCode::OK,
            "initial thumb generation failed"
        );

        // Hold the tag_db mutex on a background thread; signal via channels.
        let tag_db = state.tag_db.as_ref().unwrap().db.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        std::thread::spawn(move || {
            let _guard = tag_db.lock().unwrap();
            locked_tx.send(()).unwrap(); // signal: mutex is now held
            release_rx.recv().unwrap(); // wait for test to say "release"
            // _guard drops here, releasing the mutex
        });

        locked_rx.recv().expect("locker thread did not signal");

        // Search must answer despite tag_db being locked (uses pool, not tag_db).
        let search_status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            get(&state, "/api/search?q="),
        )
        .await
        .expect("search timed out while tag lane was held");
        assert_eq!(search_status, StatusCode::OK);

        // Cached thumbnail must answer (fast path: reads cache file, no DB).
        let thumb_status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            get(&state, &format!("/thumb/{hash}")),
        )
        .await
        .expect("cached thumb timed out while tag lane was held");
        assert_eq!(thumb_status, StatusCode::OK);

        // Release the tag lane.
        release_tx.send(()).unwrap();

        drop(db_dir);
        drop(files_dir);
    }

    /// GET helper that returns the full response body bytes.
    async fn get_body(state: &crate::AppState, uri: &str) -> (StatusCode, bytes::Bytes) {
        use http_body_util::BodyExt;
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app(state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, body)
    }

    /// Test 3: `mode=substring` returns an interior match that prefix does not.
    ///
    /// The seeded DB has tag "character:alice". The token "lic" is an interior
    /// substring but not a prefix, so prefix mode returns nothing and substring
    /// mode returns at least one suggestion containing "alice".
    #[tokio::test(flavor = "multi_thread")]
    async fn completion_mode_substring_finds_interior_match() {
        let (db_dir, _files, db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);

        // Prefix mode must NOT find "alice" for token "lic".
        let (status, body) = get_body(&state, "/api/tags/complete?q=lic&mode=prefix").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["tags"].as_array().unwrap().is_empty(),
            "prefix mode must not match interior token"
        );

        // Substring mode MUST find "alice" for the same token.
        let (status, body) = get_body(&state, "/api/tags/complete?q=lic&mode=substring").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tags = json["tags"].as_array().unwrap();
        assert!(
            !tags.is_empty(),
            "substring mode must find interior match for 'lic' in 'alice'"
        );
        let found = tags
            .iter()
            .any(|t| t["subtag"].as_str().unwrap_or("").contains("alice"));
        assert!(found, "expected 'alice' tag in substring results");

        drop(db_dir);
    }

    /// Test 4: omitting `mode` behaves as prefix (backward-compatible default).
    #[tokio::test(flavor = "multi_thread")]
    async fn completion_mode_default_is_prefix() {
        let (db_dir, _files, db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);

        // No mode param — interior token "lic" must return no tags (prefix behaviour).
        let (status, body) = get_body(&state, "/api/tags/complete?q=lic").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["tags"].as_array().unwrap().is_empty(),
            "omitted mode must default to prefix and not match interior token"
        );

        // No mode param — prefix token "ali" must return results (same as prefix mode).
        let (status, body) = get_body(&state, "/api/tags/complete?q=ali").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            !json["tags"].as_array().unwrap().is_empty(),
            "omitted mode must default to prefix and match prefix token"
        );

        drop(db_dir);
    }

    /// Test 5: alias-surfaced rows carry `alias_source`; direct matches do not.
    ///
    /// Seeds character:alice (via seed_db), adds sibling alice_chan → character:alice,
    /// then verifies:
    ///   * querying the alias spelling emits `"alias_source":"alice_chan"` on the row
    ///   * querying a prefix that matches the canonical directly emits no alias_source key
    #[tokio::test(flavor = "multi_thread")]
    async fn tags_complete_emits_alias_source_for_alias_surfaced_rows() {
        let (db_dir, _files, db_path, db, _hash) = seed_db();
        // Add sibling: alice_chan (bad) → character:alice (ideal/canonical).
        crate::ops::add_sibling(&db, "alice_chan", "character:alice").unwrap();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);

        // Query that matches only the alias spelling → step-4 injection with alias_source.
        let (status, body) = get_body(&state, "/api/tags/complete?q=alice_chan").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["tags"][0]["alias_source"].as_str(),
            Some("alice_chan"),
            "alias-surfaced row must carry alias_source: {json}"
        );

        // Query that directly matches character:alice (step-1 base scan) → no alias_source.
        let (status, body) = get_body(&state, "/api/tags/complete?q=character:alice").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["tags"][0].get("alias_source").is_none()
                || json["tags"][0]["alias_source"].is_null(),
            "direct-match row must not carry alias_source: {json}"
        );

        drop(db_dir);
    }

    /// Cross-site Origin on a state-changing SSE GET endpoint → 403 FORBIDDEN.
    /// This is the regression guard for the SSE-GET CSRF hole. Also proves the
    /// `None` peer path: the in-process harness drives the router without a socket,
    /// so no `ConnectInfo` extension exists; source_guard must treat absent as local.
    #[tokio::test(flavor = "multi_thread")]
    async fn cross_site_origin_on_scan_stream_is_forbidden() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64);

        let req = Request::builder()
            .uri(naiad_api::API_SCAN_STREAM)
            .header("Origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        let status = app(state).oneshot(req).await.unwrap().status();
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "cross-site origin must be rejected"
        );

        drop(db_dir);
    }

    /// Cross-site Sec-Fetch-Site also → 403 FORBIDDEN.
    #[tokio::test(flavor = "multi_thread")]
    async fn cross_site_sec_fetch_site_on_scan_stream_is_forbidden() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64);

        let req = Request::builder()
            .uri(naiad_api::API_SCAN_STREAM)
            .header("Sec-Fetch-Site", "cross-site")
            .body(Body::empty())
            .unwrap();
        let status = app(state).oneshot(req).await.unwrap().status();
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "cross-site Sec-Fetch-Site must be rejected"
        );

        drop(db_dir);
    }

    /// Same-origin request (Sec-Fetch-Site: same-origin) on the SSE endpoint passes
    /// the origin guard. The handler itself may return a different status, but not 403
    /// from the guard.
    #[tokio::test(flavor = "multi_thread")]
    async fn same_origin_request_passes_origin_guard() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64);

        let req = Request::builder()
            .uri(naiad_api::API_SCAN_STREAM)
            .header("Sec-Fetch-Site", "same-origin")
            .header("Origin", "http://127.0.0.1:8080")
            .body(Body::empty())
            .unwrap();
        let status = app(state).oneshot(req).await.unwrap().status();
        // The guard passes (not 403); the handler may return 400 for a missing param.
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "same-origin must not be blocked by origin guard"
        );

        drop(db_dir);
    }

    /// No Origin headers (CLI shape) passes the origin guard.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_origin_headers_passes_origin_guard() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64);

        // A bare GET with no Origin or Sec-Fetch-Site (CLI / curl shape).
        let req = Request::builder()
            .uri(naiad_api::API_SCAN_STREAM)
            .body(Body::empty())
            .unwrap();
        let status = app(state).oneshot(req).await.unwrap().status();
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "CLI-shape request must not be blocked"
        );

        drop(db_dir);
    }

    /// DNS-rebinding Host still → 403 (existing host_guard regression test).
    #[tokio::test(flavor = "multi_thread")]
    async fn dns_rebinding_host_still_rejected() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64);

        let req = Request::builder()
            .uri(naiad_api::API_SCAN_STREAM)
            .header("Host", "evil.example")
            .body(Body::empty())
            .unwrap();
        let status = app(state).oneshot(req).await.unwrap().status();
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "DNS-rebinding Host must still be rejected"
        );

        drop(db_dir);
    }

    /// Sec-Fetch-Site: same-site must pass the origin guard.
    /// A daemon served at e.g. daemon.local with UI at app.local would produce
    /// same-site requests; we must not 403 those.
    #[tokio::test(flavor = "multi_thread")]
    async fn same_site_sec_fetch_site_passes_origin_guard() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64);

        let req = Request::builder()
            .uri(naiad_api::API_SCAN_STREAM)
            .header("Sec-Fetch-Site", "same-site")
            .body(Body::empty())
            .unwrap();
        let status = app(state).oneshot(req).await.unwrap().status();
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "same-site Sec-Fetch-Site must not be blocked by origin guard"
        );

        drop(db_dir);
    }

    /// With `allow_remote`, a remote browser sending only an `Origin` header
    /// (no Sec-Fetch-Site) naming a LAN address must pass origin_guard.
    #[tokio::test(flavor = "multi_thread")]
    async fn allow_remote_accepts_foreign_origin_header() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_allow_remote(true);

        // A remote browser at 192.168.1.50 sends Origin but no Sec-Fetch-Site.
        let req = Request::builder()
            .uri(naiad_api::API_HEALTH)
            .header("Origin", "http://192.168.1.50:8080")
            .body(Body::empty())
            .unwrap();
        let status = app(state).oneshot(req).await.unwrap().status();
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "allow_remote must permit a foreign Origin header"
        );

        drop(db_dir);
    }

    /// With the `allow_remote` opt-in, a foreign `Host` (a remote client naming
    /// the daemon by LAN IP or hostname, which can never match a wildcard bind)
    /// must pass the host guard — otherwise the opt-in permits nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn allow_remote_accepts_foreign_host() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_allow_remote(true);

        let req = Request::builder()
            .uri(naiad_api::API_HEALTH)
            .header("Host", "192.168.1.50:8080")
            .body(Body::empty())
            .unwrap();
        let status = app(state).oneshot(req).await.unwrap().status();
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "allow_remote must accept a non-loopback Host"
        );

        drop(db_dir);
    }

    /// `/api/health` carries the watch block; with no watcher it is complete/empty.
    #[tokio::test]
    async fn health_returns_watch_block() {
        let (db_dir, _files, db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["watch"]["total"], 0);
        assert_eq!(v["watch"]["complete"], true);
        assert!(v["watch"]["failed"].as_array().unwrap().is_empty());
        drop(db_dir);
    }

    /// `/api/health` carries the warmup phase (#130). With a read pool attached
    /// but no first gallery query yet, the warmup task is spawned and *parked* on
    /// the startup gate — it must report `queued`, not `graph`. Claiming the
    /// graph step here made the UI say "building tag relations" for up to
    /// `CACHE_WARMUP_GATE_TIMEOUT` while nothing was being read.
    #[tokio::test]
    async fn health_reports_queued_while_the_warmup_is_parked() {
        let (db_dir, _files, db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);
        assert!(!state.startup_gate.is_fired(), "no query has run yet");

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["warmup"]["phase"], "queued");
        assert_eq!(v["warmup"]["complete"], false);
        drop(db_dir);
    }

    /// Once the startup gate releases, the warmup advances through the real
    /// phases and settles at `done` — the transition the UI renders as
    /// "Preparing library → ready".
    #[tokio::test(flavor = "multi_thread")]
    async fn health_warmup_advances_to_done_after_the_first_query() {
        let (db_dir, _files, db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);
        assert_eq!(
            state.warmup.status().phase,
            crate::warmup::WarmupPhase::Queued
        );

        // Simulate the first `/api/search` releasing the warmup.
        state.startup_gate.fire();
        state
            .warmup_done
            .wait(std::time::Duration::from_secs(10))
            .await;

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["warmup"]["phase"], "done");
        assert_eq!(v["warmup"]["complete"], true);
        drop(db_dir);
    }

    /// A daemon built without a read pool never warms anything. It must report
    /// the idle phase as *complete*, so the UI does not grow a "Preparing
    /// library" job that can never settle.
    #[tokio::test]
    async fn health_reports_idle_warmup_without_a_read_pool() {
        let (db_dir, _files, _db_path, db, _hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64);

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["warmup"]["phase"], "idle");
        assert_eq!(v["warmup"]["complete"], true);
        drop(db_dir);
    }

    /// A pull with no subscribed repos emits exactly one `error` event.
    #[tokio::test]
    async fn pull_stream_no_repos_emits_single_error() {
        let (db_dir, _files, db_path, db, hash) = seed_db();
        let thumbs = tempfile::tempdir().unwrap();
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_read_db(&db_path);

        let body = serde_json::to_vec(&serde_json::json!({ "hashes": [hash] })).unwrap();
        let res = app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/files/pull-tags/stream")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        // SSE framing: exactly one `error` event, no `summary`.
        assert!(
            text.contains("event: error"),
            "want an error event, got: {text}"
        );
        assert!(
            text.contains("no subscribed repositories"),
            "want the no-repos message, got: {text}"
        );
        assert!(!text.contains("event: summary"), "must not emit a summary");
        assert_eq!(text.matches("event: error").count(), 1, "exactly one error");
        drop(db_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        host_allowed, host_from_url, host_part, is_allowed_client_host, is_loopback_host,
        origin_allowed, origin_host, peer_allowed, to_dto,
    };
    use axum::http::HeaderValue;

    // --- host_from_url unit tests ---

    #[test]
    fn host_from_url_strips_scheme_path_port() {
        assert_eq!(
            host_from_url("http://ptr.example.net:9090/x"),
            "ptr.example.net"
        );
    }

    #[test]
    fn host_from_url_bare_ip_with_port() {
        assert_eq!(host_from_url("203.0.113.10:9090"), "203.0.113.10");
    }

    #[test]
    fn host_from_url_bracketed_ipv6() {
        assert_eq!(host_from_url("http://[::1]:9090"), "::1");
    }

    #[test]
    fn host_from_url_strips_userinfo() {
        assert_eq!(host_from_url("http://user@host:1/"), "host");
    }

    #[test]
    fn host_from_url_garbage_returns_trimmed_input() {
        // A string with no recognizable host — returns the whole trimmed input.
        assert_eq!(host_from_url("  :::  "), ":::");
    }

    #[test]
    fn to_dto_renders_non_utf8_names_lossily_not_empty() {
        use naiad_core::hash_bytes;
        use naiad_db::FileListing;
        use std::path::PathBuf;

        // A path whose file name is not valid UTF-8: an ill-formed UTF-16 unit
        // (lone surrogate) on Windows, a raw invalid byte on Unix.
        #[cfg(windows)]
        let path: PathBuf = {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt;
            // p i c <lone-surrogate> . j p g
            let units: [u16; 8] = [0x70, 0x69, 0x63, 0xD800, 0x2E, 0x6A, 0x70, 0x67];
            let mut p = PathBuf::from(r"C:\lib");
            p.push(OsString::from_wide(&units));
            p
        };
        #[cfg(unix)]
        let path: PathBuf = {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            let mut bytes = b"/lib/pic".to_vec();
            bytes.push(0xFF); // invalid UTF-8 byte
            bytes.extend_from_slice(b".jpg");
            PathBuf::from(OsString::from_vec(bytes))
        };

        let dto = to_dto(&FileListing {
            hash: hash_bytes(b"x"),
            size: 3,
            path,
            imported_at: 1,
            created_at: Some(1),
            modified_at: Some(1),
            mime: None,
        });

        // Identity is exact; display fields are present (never empty) and lossy.
        assert_eq!(dto.hash, hash_bytes(b"x").to_hex());
        assert!(!dto.name.is_empty(), "non-UTF-8 name must not arrive empty");
        assert!(
            dto.name.contains('\u{FFFD}'),
            "invalid units render as U+FFFD, got {:?}",
            dto.name
        );
        assert!(dto.path.contains('\u{FFFD}'));
    }

    #[test]
    fn host_part_strips_port_and_brackets() {
        // Plain host and host:port
        assert_eq!(host_part("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_part("localhost"), "localhost");
        assert_eq!(host_part("  evil.example:80  "), "evil.example");
        // Bracketed IPv6 with and without port
        assert_eq!(host_part("[::1]:54321"), "::1");
        assert_eq!(host_part("[::1]"), "::1");
        // Bare IPv6 (no brackets) — must not be mangled to an empty string
        assert_eq!(host_part("::1"), "::1");
        assert_eq!(host_part("fe80::1"), "fe80::1");
    }

    #[test]
    fn loopback_hosts_recognized() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LocalHost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.4.5.6")); // all of 127.0.0.0/8
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("evil.example"));
        assert!(!is_loopback_host("10.0.0.5"));
        assert!(!is_loopback_host("0.0.0.0"));
    }

    fn host(v: &str) -> HeaderValue {
        HeaderValue::from_str(v).unwrap()
    }

    #[test]
    fn allows_absent_host_and_loopback_only() {
        // No Host header (non-browser client): allowed.
        assert!(host_allowed(None, None, false));
        // Loopback authorities: allowed regardless of bound addr.
        assert!(host_allowed(Some(&host("127.0.0.1:8080")), None, false));
        assert!(host_allowed(Some(&host("localhost")), None, false));
        assert!(host_allowed(Some(&host("[::1]:9000")), None, false));
        // A rebound attacker domain: rejected.
        assert!(!host_allowed(Some(&host("evil.example")), None, false));
        assert!(!host_allowed(Some(&host("evil.example:8080")), None, false));
    }

    #[test]
    fn allows_the_bound_non_loopback_address() {
        let bound = Some("192.168.1.50:8080".parse().unwrap());
        // The server's own bound LAN address is allowed (operator opted in)...
        assert!(host_allowed(Some(&host("192.168.1.50:8080")), bound, false));
        assert!(host_allowed(Some(&host("192.168.1.50")), bound, false));
        // ...but other names still are not.
        assert!(!host_allowed(Some(&host("192.168.1.99")), bound, false));
        assert!(!host_allowed(Some(&host("evil.example")), bound, false));
    }

    // --- origin_host parsing ---

    #[test]
    fn origin_host_strips_scheme_and_port() {
        assert_eq!(origin_host("http://127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(origin_host("https://[::1]:9000"), "::1");
        assert_eq!(origin_host("http://localhost"), "localhost");
        assert_eq!(origin_host("null"), "null");
    }

    // --- origin_allowed decision table ---

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn origin_allowed_sec_fetch_site_same_origin_and_none_allow() {
        assert!(origin_allowed(Some(&hv("same-origin")), None, None, false));
        assert!(origin_allowed(Some(&hv("none")), None, None, false));
        // short-circuit: even a cross-site origin is ignored when Sec-Fetch-Site is same-origin
        assert!(origin_allowed(
            Some(&hv("same-origin")),
            Some(&hv("https://evil.example")),
            None,
            false,
        ));
    }

    /// same-site is now accepted (daemon behind a subdomain); cross-site and garbage are not.
    #[test]
    fn origin_allowed_sec_fetch_site_same_site_accepted_cross_site_rejected() {
        // same-site → allow (Finding 7 fix)
        assert!(origin_allowed(Some(&hv("same-site")), None, None, false));
        // cross-site → reject
        assert!(!origin_allowed(Some(&hv("cross-site")), None, None, false));
        // garbage → reject
        assert!(!origin_allowed(Some(&hv("garbage")), None, None, false));
    }

    #[test]
    fn origin_allowed_origin_loopback_hosts_pass() {
        assert!(origin_allowed(
            None,
            Some(&hv("http://127.0.0.1:8080")),
            None,
            false,
        ));
        assert!(origin_allowed(
            None,
            Some(&hv("http://localhost")),
            None,
            false
        ));
        assert!(origin_allowed(
            None,
            Some(&hv("https://[::1]:9000")),
            None,
            false
        ));
    }

    #[test]
    fn origin_allowed_cross_site_origin_rejected() {
        assert!(!origin_allowed(
            None,
            Some(&hv("https://evil.example")),
            None,
            false,
        ));
    }

    #[test]
    fn origin_allowed_bound_addr_host_passes() {
        let bound: Option<std::net::SocketAddr> = Some("192.168.1.50:8080".parse().unwrap());
        assert!(origin_allowed(
            None,
            Some(&hv("http://192.168.1.50:8080")),
            bound,
            false,
        ));
    }

    #[test]
    fn origin_allowed_null_and_garbage_rejected() {
        assert!(!origin_allowed(None, Some(&hv("null")), None, false));
    }

    #[test]
    fn origin_allowed_no_headers_passes() {
        assert!(origin_allowed(None, None, None, false));
    }

    // --- is_allowed_client_host (shared helper) ---

    #[test]
    fn is_allowed_client_host_allow_remote_bypasses_all_checks() {
        // With allow_remote=true any host string is allowed, regardless of bound.
        assert!(is_allowed_client_host("evil.example", None, true));
        assert!(is_allowed_client_host("192.168.99.99", None, true));
    }

    #[test]
    fn is_allowed_client_host_loopback_passes_without_allow_remote() {
        assert!(is_allowed_client_host("127.0.0.1", None, false));
        assert!(is_allowed_client_host("::1", None, false));
        assert!(is_allowed_client_host("localhost", None, false));
    }

    #[test]
    fn is_allowed_client_host_bound_addr_passes_without_allow_remote() {
        let bound: Option<std::net::SocketAddr> = Some("192.168.1.50:8080".parse().unwrap());
        assert!(is_allowed_client_host("192.168.1.50", bound, false));
        // A different LAN IP is still rejected.
        assert!(!is_allowed_client_host("192.168.1.99", bound, false));
    }

    // --- origin_allowed + allow_remote ---

    #[test]
    fn origin_allowed_allow_remote_permits_foreign_origin() {
        // With allow_remote, an Origin that names a LAN IP (non-loopback, non-bound)
        // must be allowed when no Sec-Fetch-Site is present.
        assert!(origin_allowed(
            None,
            Some(&hv("http://192.168.1.50:8080")),
            None,
            true, // allow_remote
        ));
    }

    #[test]
    fn origin_allowed_allow_remote_cross_site_sec_fetch_still_rejected() {
        // Sec-Fetch-Site: cross-site is always rejected regardless of allow_remote —
        // it is browser-attested proof that the request is from a different site.
        assert!(!origin_allowed(
            Some(&hv("cross-site")),
            Some(&hv("http://192.168.1.50:8080")),
            None,
            true,
        ));
    }

    // --- bare IPv6 in host_allowed ---

    #[test]
    fn host_allowed_bare_ipv6_loopback_passes() {
        // A browser or non-standard client sending bare "::1" (no brackets) in the
        // Host header must still be recognised as loopback.
        assert!(host_allowed(Some(&host("::1")), None, false));
    }

    #[test]
    fn host_allowed_bracketed_ipv6_loopback_passes() {
        // Standard form "[::1]" and "[::1]:port" must both pass.
        assert!(host_allowed(Some(&host("[::1]")), None, false));
        assert!(host_allowed(Some(&host("[::1]:9090")), None, false));
    }

    // --- peer_allowed decision table ---

    #[test]
    fn peer_allowed_loopback_peers_pass() {
        let lo4: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let lo6: std::net::SocketAddr = "[::1]:12345".parse().unwrap();
        assert!(peer_allowed(Some(lo4), false));
        assert!(peer_allowed(Some(lo6), false));
    }

    #[test]
    fn peer_allowed_lan_peer_rejected_when_remote_not_allowed() {
        let lan: std::net::SocketAddr = "192.168.1.9:5555".parse().unwrap();
        assert!(!peer_allowed(Some(lan), false));
    }

    #[test]
    fn peer_allowed_lan_peer_passes_when_allow_remote() {
        let lan: std::net::SocketAddr = "192.168.1.9:5555".parse().unwrap();
        assert!(peer_allowed(Some(lan), true));
    }

    #[test]
    fn peer_allowed_none_peer_always_passes() {
        // No ConnectInfo (in-process test harness) → treated as local.
        assert!(peer_allowed(None, false));
        assert!(peer_allowed(None, true));
    }

    // ── SseObserver phase→PullStage mapping (#174) ───────────────────────────

    /// Drive a scripted two-leg (blake3 → sha256) phase sequence through
    /// `SseObserver` and assert the emitted `PullStage` values:
    ///
    /// - bytes/hashes/tags monotonically non-decreasing ACROSS the leg reset;
    /// - chunk/chunk_total resets at the second domain leg;
    /// - elapsed_ms present and monotonic on every "chunk" stage;
    /// - Merging and Done carry last-known hashes/tags with window=0;
    /// - Done has chunk == chunk_total.
    #[test]
    fn sse_observer_phase_to_pull_stage_mapping() {
        use super::{PullMsg, SseObserver};
        use naiad_netproto::PullPhase::{ChunkReceived, Done, Merging, RequestSent};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PullMsg>();
        let obs = SseObserver {
            tx,
            repo: "test-repo".to_string(),
            index: 1,
            total: 2,
            started: std::time::Instant::now(),
            bytes: std::cell::Cell::new(0),
            domain: std::cell::Cell::new(None),
            hashes_base: std::cell::Cell::new(0),
            tags_base: std::cell::Cell::new(0),
            last_hashes: std::cell::Cell::new(0),
            last_tags: std::cell::Cell::new(0),
            last_total: std::cell::Cell::new(0),
            retries: std::cell::Cell::new(0),
        };

        use naiad_netproto::PullObserver as _;

        // ── Blake3 leg ───────────────────────────────────────────────────────
        obs.set_domain(Some("blake3"));

        obs.on_phase(RequestSent {
            done: 0,
            total: 10,
            window: 5,
        });
        // Small sleep so Instant::elapsed() produces distinct non-zero values.
        std::thread::sleep(std::time::Duration::from_millis(1));

        obs.on_phase(ChunkReceived {
            done: 5,
            total: 10,
            window: 5,
            chunk_bytes: 100,
            cumulative_bytes: 100,
            hashes: 3,
            tags: 7,
            request_ms: 50,
        });
        std::thread::sleep(std::time::Duration::from_millis(1));

        obs.on_phase(ChunkReceived {
            done: 10,
            total: 10,
            window: 5,
            chunk_bytes: 120,
            cumulative_bytes: 220,
            hashes: 5,
            tags: 12,
            request_ms: 40,
        });
        std::thread::sleep(std::time::Duration::from_millis(1));

        // ── SHA-256 leg ──────────────────────────────────────────────────────
        obs.set_domain(Some("sha256"));

        obs.on_phase(RequestSent {
            done: 0,
            total: 8,
            window: 4,
        });
        std::thread::sleep(std::time::Duration::from_millis(1));

        obs.on_phase(ChunkReceived {
            done: 4,
            total: 8,
            window: 4,
            chunk_bytes: 80,
            cumulative_bytes: 80,
            hashes: 2,
            tags: 4,
            request_ms: 30,
        });
        std::thread::sleep(std::time::Duration::from_millis(1));

        obs.on_phase(ChunkReceived {
            done: 8,
            total: 8,
            window: 4,
            chunk_bytes: 90,
            cumulative_bytes: 170,
            hashes: 4,
            tags: 9,
            request_ms: 35,
        });
        std::thread::sleep(std::time::Duration::from_millis(1));

        // ── Close ────────────────────────────────────────────────────────────
        obs.set_domain(None);
        obs.on_phase(Merging);
        obs.on_phase(Done);

        // Drain the channel.  The sender is still alive inside `obs`, so we
        // collect exactly what was sent without blocking.
        let mut stages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let PullMsg::Stage(s) = msg {
                stages.push(s);
            }
        }

        // Expected phase sequence (ignoring RequestSent stages for brevity in
        // the per-field assertions below):
        //   0: request  (b3 leg)
        //   1: chunk    done=5,  total=10, bytes=100,  hashes=3,  tags=7
        //   2: chunk    done=10, total=10, bytes=220,  hashes=5,  tags=12
        //   3: request  (sha256 leg)
        //   4: chunk    done=4,  total=8,  bytes=300,  hashes=7,  tags=16
        //   5: chunk    done=8,  total=8,  bytes=390,  hashes=9,  tags=21
        //   6: merging
        //   7: done     chunk==chunk_total==8
        assert_eq!(
            stages.len(),
            8,
            "expected 8 stage events; got {}",
            stages.len()
        );

        // ── Phase tags ───────────────────────────────────────────────────────
        let phases: Vec<&str> = stages.iter().map(|s| s.phase.as_str()).collect();
        assert_eq!(
            phases,
            [
                "request", "chunk", "chunk", "request", "chunk", "chunk", "merging", "done"
            ]
        );

        // ── bytes — monotonically non-decreasing ─────────────────────────────
        let bytes: Vec<u64> = stages.iter().map(|s| s.bytes).collect();
        for w in bytes.windows(2) {
            assert!(w[0] <= w[1], "bytes decreased: {w:?}");
        }
        // Blake3 leg ends at 220; sha256 chunks add 80+90 = 170; total = 390.
        assert_eq!(
            bytes[5], 390,
            "cumulative bytes after both legs must be 390"
        );

        // ── hashes — monotonically non-decreasing, cross-leg base applied ────
        let hashes: Vec<u64> = stages.iter().map(|s| s.hashes).collect();
        for w in hashes.windows(2) {
            assert!(w[0] <= w[1], "hashes decreased: {w:?}");
        }
        // Blake3 leg ends at 5.  SHA-256 leg has per-leg hashes 2, 4 → cross-leg
        // cumulative 5+2=7, 5+4=9.
        assert_eq!(hashes[1], 3, "blake3 chunk1 hashes=3");
        assert_eq!(hashes[2], 5, "blake3 chunk2 hashes=5");
        assert_eq!(hashes[4], 7, "sha256 chunk1 hashes=5+2=7");
        assert_eq!(hashes[5], 9, "sha256 chunk2 hashes=5+4=9");

        // ── tags — monotonically non-decreasing, cross-leg base applied ──────
        let tags: Vec<u64> = stages.iter().map(|s| s.tags).collect();
        for w in tags.windows(2) {
            assert!(w[0] <= w[1], "tags decreased: {w:?}");
        }
        // Blake3 ends at 12.  SHA-256: 12+4=16, 12+9=21.
        assert_eq!(tags[2], 12, "blake3 chunk2 tags=12");
        assert_eq!(tags[4], 16, "sha256 chunk1 tags=12+4=16");
        assert_eq!(tags[5], 21, "sha256 chunk2 tags=12+9=21");

        // ── per-leg chunk/chunk_total reset at sha256 leg ────────────────────
        // Blake3 leg: chunk_total == 10 for both its stages.
        assert_eq!(stages[1].chunk_total, 10, "blake3 leg chunk_total");
        assert_eq!(stages[2].chunk_total, 10, "blake3 leg chunk_total");
        // SHA-256 leg: chunk_total resets to 8.
        assert_eq!(
            stages[4].chunk_total, 8,
            "sha256 leg chunk_total reset to 8"
        );
        assert_eq!(stages[5].chunk_total, 8, "sha256 leg chunk_total");

        // ── elapsed_ms — present and monotonic on every "chunk" stage ────────
        let chunk_stages: Vec<_> = stages.iter().filter(|s| s.phase == "chunk").collect();
        let elapsed: Vec<u64> = chunk_stages.iter().map(|s| s.elapsed_ms).collect();
        // With 1 ms sleeps, elapsed must grow; assert at least non-decreasing.
        for w in elapsed.windows(2) {
            assert!(w[0] <= w[1], "elapsed_ms not monotonic: {w:?}");
        }
        // All chunk stages must have a positive elapsed_ms (sleeps ensure this).
        for (i, e) in elapsed.iter().enumerate() {
            assert!(*e > 0, "chunk stage {i}: elapsed_ms must be > 0 (got 0)");
        }

        // ── Merging: carries last-known hashes/tags, window=0 ────────────────
        let merging = &stages[6];
        assert_eq!(merging.phase, "merging");
        assert_eq!(merging.hashes, 9, "merging carries last_hashes=9");
        assert_eq!(merging.tags, 21, "merging carries last_tags=21");
        assert_eq!(merging.window, 0);
        assert_eq!(merging.chunk, 0);
        assert_eq!(merging.chunk_total, 0);

        // ── Done: chunk == chunk_total == last_total (8), window=0 ───────────
        let done = &stages[7];
        assert_eq!(done.phase, "done");
        assert_eq!(done.chunk, 8, "done.chunk == last_total");
        assert_eq!(done.chunk_total, 8, "done.chunk_total == last_total");
        assert_eq!(done.window, 0);
        assert_eq!(done.hashes, 9, "done carries last_hashes=9");
        assert_eq!(done.tags, 21, "done carries last_tags=21");
    }

    /// #177 §6.2 Test 9 — `SseObserver` maps `WindowRetry` → `PullStage` with
    /// `phase == "retry"`, increments `retries`, and carries the retry count on
    /// all subsequent stages. Also asserts `RowReceived` with a lower hash count
    /// (scratch-discard scenario) does NOT reduce `last_hashes` (monotonic clamp).
    #[test]
    fn sse_observer_window_retry_mapping() {
        use super::{PullMsg, SseObserver};
        use naiad_netproto::PullPhase::{
            ChunkReceived, Done, Merging, RequestSent, RowReceived, WindowRetry,
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PullMsg>();
        let obs = SseObserver {
            tx,
            repo: "retry-repo".to_string(),
            index: 1,
            total: 1,
            started: std::time::Instant::now(),
            bytes: std::cell::Cell::new(0),
            domain: std::cell::Cell::new(None),
            hashes_base: std::cell::Cell::new(0),
            tags_base: std::cell::Cell::new(0),
            last_hashes: std::cell::Cell::new(0),
            last_tags: std::cell::Cell::new(0),
            last_total: std::cell::Cell::new(0),
            retries: std::cell::Cell::new(0),
        };

        use naiad_netproto::PullObserver as _;

        obs.set_domain(Some("blake3"));

        // First RequestSent: no retries yet.
        obs.on_phase(RequestSent {
            done: 0,
            total: 10,
            window: 8,
        });

        // WindowRetry: first retry event.
        obs.on_phase(WindowRetry {
            done: 0,
            total: 10,
            old_window: 8,
            new_window: 4,
            attempt: 0,
            reason: naiad_netproto::RetryReason::Timeout,
        });

        // Simulate a RowReceived tick that arrived during the first window attempt
        // (e.g., partial stream rows before the truncation). last_hashes → 5.
        obs.on_phase(RowReceived {
            hashes: 5,
            tags: 12,
        });

        // Second RequestSent (the retry attempt): retries should be 1 now.
        // This stage reads last_hashes = 5 (set by the RowReceived above).
        obs.on_phase(RequestSent {
            done: 0,
            total: 10,
            window: 4,
        });

        // Simulate a lower RowReceived from the retry attempt (scratch was discarded,
        // so the retry starts from 0 rows — only 1 hash seen so far). The monotonic
        // clamp must keep last_hashes at 5, not reduce it to 1.
        obs.on_phase(RowReceived { hashes: 1, tags: 2 });

        // Second WindowRetry AFTER the lower RowReceived: its stage snapshots
        // last_hashes/last_tags with no intervening ChunkReceived overwrite, so
        // it only reads >= 5/12 if the monotonic clamp actually held.
        obs.on_phase(WindowRetry {
            done: 0,
            total: 10,
            old_window: 4,
            new_window: 4,
            attempt: 1,
            reason: naiad_netproto::RetryReason::Timeout,
        });

        // ChunkReceived after the retry.
        obs.on_phase(ChunkReceived {
            done: 4,
            total: 10,
            window: 4,
            chunk_bytes: 50,
            cumulative_bytes: 50,
            hashes: 2,
            tags: 5,
            request_ms: 100,
        });

        obs.set_domain(None);
        obs.on_phase(Merging);
        obs.on_phase(Done);

        // Drain all stages.
        let mut stages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let PullMsg::Stage(s) = msg {
                stages.push(s);
            }
        }

        // Expected sequence:
        //   0: request  (first RequestSent, retries=0)
        //   1: retry    (WindowRetry, retries=1)
        //   2: request  (second RequestSent, retries=1)
        //   3: retry    (second WindowRetry, retries=2 — clamp probe)
        //   4: chunk    (ChunkReceived, retries=2)
        //   5: merging  (retries=2)
        //   6: done     (retries=2)
        assert_eq!(
            stages.len(),
            7,
            "expected 7 stage events; got {}; phases={:?}",
            stages.len(),
            stages.iter().map(|s| s.phase.as_str()).collect::<Vec<_>>()
        );

        let phases: Vec<&str> = stages.iter().map(|s| s.phase.as_str()).collect();
        assert_eq!(
            phases,
            [
                "request", "retry", "request", "retry", "chunk", "merging", "done"
            ],
            "phase sequence must include both retries"
        );

        // retries=0 on the first request (before the retry).
        assert_eq!(stages[0].retries, 0, "first request retries must be 0");

        // retry stage: retries=1, phase="retry", window=new_window=4, chunk/chunk_total=done/total.
        let retry_stage = &stages[1];
        assert_eq!(retry_stage.phase, "retry");
        assert_eq!(retry_stage.retries, 1, "retry stage retries must be 1");
        assert_eq!(
            retry_stage.window, 4,
            "retry stage window must equal new_window"
        );
        assert_eq!(retry_stage.chunk, 0, "retry stage chunk == done == 0");
        assert_eq!(
            retry_stage.chunk_total, 10,
            "retry stage chunk_total == total == 10"
        );

        // stages[2] carries retries=1; second WindowRetry bumps to 2 and all
        // subsequent stages carry retries=2.
        assert_eq!(stages[2].retries, 1, "second request retries must be 1");
        for (i, stage) in stages[3..].iter().enumerate() {
            assert_eq!(
                stage.retries,
                2,
                "stage {} ({}) must carry retries=2 after the second retry; got {}",
                i + 3,
                stage.phase,
                stage.retries
            );
        }

        // Done carries final hashes/tags from ChunkReceived.
        assert_eq!(stages[6].hashes, 2, "done hashes");
        assert_eq!(stages[6].tags, 5, "done tags");

        // Monotonic clamp (#177 §6.2): stages[3] (second WindowRetry) is emitted
        // AFTER RowReceived { hashes: 1, tags: 2 } tried to reduce the counters
        // set by RowReceived { hashes: 5, tags: 12 }, with no ChunkReceived
        // overwrite in between. Without the clamp in the RowReceived arm this
        // stage would read 1/2 — it must still read >= 5/12.
        assert!(
            stages[3].hashes >= 5,
            "second retry stage hashes must be >= 5 (clamp must ignore the lower RowReceived); \
             got {}",
            stages[3].hashes
        );
        assert!(
            stages[3].tags >= 12,
            "second retry stage tags must be >= 12 (clamp must ignore the lower RowReceived); \
             got {}",
            stages[3].tags
        );
    }
}

#[cfg(test)]
mod repo_entries_tests {
    use super::current_repo_entries;

    #[test]
    fn repo_override_survives_reconcile() {
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let db = naiad_db::Db::open(&db_path).unwrap();
        db.subscribe_shared_service("a", "http://a", None).unwrap();
        db.subscribe_shared_service("b", "http://b", None).unwrap();

        let prev = vec![
            crate::settings::RepoEntry {
                name: "a".into(),
                url: "http://a".into(),
                max_query_bits: Some(14),
            },
            crate::settings::RepoEntry {
                name: "b".into(),
                url: "http://b".into(),
                max_query_bits: None,
            },
        ];

        let entries = current_repo_entries(&db, &prev).unwrap();
        assert_eq!(
            entries
                .iter()
                .find(|r| r.name == "a")
                .unwrap()
                .max_query_bits,
            Some(14),
            "override for repo 'a' must survive reconcile"
        );
        assert_eq!(
            entries
                .iter()
                .find(|r| r.name == "b")
                .unwrap()
                .max_query_bits,
            None,
            "repo 'b' has no override, must remain None"
        );
    }
}

#[cfg(test)]
mod query_bits_tests {
    use super::{repo_max_query_bits, repos_query_bits_handler};
    use axum::Json;
    use axum::extract::State;
    use axum::http::StatusCode;
    use naiad_api::RepoQueryBitsReq;

    fn test_store(dir: &tempfile::TempDir) -> crate::thumb_store::ThumbStore {
        crate::thumb_store::ThumbStore::open(&dir.path().join("thumbs.db")).unwrap()
    }

    #[tokio::test]
    async fn set_query_bits_persists_and_clears() {
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let thumbs = tempfile::tempdir().unwrap();
        let db = naiad_db::Db::open(&db_path).unwrap();
        db.subscribe_shared_service("a", "http://a", None).unwrap();

        let toml_path = db_dir.path().join("naiad.toml");
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_settings_path(toml_path);

        // Set max_query_bits = 14 → should persist.
        let res = repos_query_bits_handler(
            State(state.clone()),
            Json(RepoQueryBitsReq {
                name: "a".into(),
                max_query_bits: Some(14),
            }),
        )
        .await;
        assert!(res.is_ok(), "set to 14 must succeed");
        assert_eq!(
            repo_max_query_bits(&state, "a"),
            14,
            "max_query_bits should be 14 after set"
        );

        // Clear override (None) → falls back to global default (24).
        let res = repos_query_bits_handler(
            State(state.clone()),
            Json(RepoQueryBitsReq {
                name: "a".into(),
                max_query_bits: None,
            }),
        )
        .await;
        assert!(res.is_ok(), "clear must succeed");
        assert_eq!(
            repo_max_query_bits(&state, "a"),
            24,
            "max_query_bits should fall back to global default (24) after clear"
        );

        // Unknown repo → 404.
        let err = repos_query_bits_handler(
            State(state.clone()),
            Json(RepoQueryBitsReq {
                name: "unknown".into(),
                max_query_bits: Some(14),
            }),
        )
        .await
        .expect_err("unknown repo must fail");
        assert_eq!(err.0, StatusCode::NOT_FOUND, "unknown repo must return 404");

        // Out-of-range value (300) → 400.
        let err = repos_query_bits_handler(
            State(state.clone()),
            Json(RepoQueryBitsReq {
                name: "a".into(),
                max_query_bits: Some(300),
            }),
        )
        .await
        .expect_err("out-of-range must fail");
        assert_eq!(
            err.0,
            StatusCode::BAD_REQUEST,
            "out-of-range must return 400"
        );

        drop(db_dir);
    }

    /// Setting one repo's ceiling must not clobber another repo's existing
    /// override — `current_repo_entries` must carry all overrides through.
    #[tokio::test]
    async fn set_query_bits_preserves_other_repo_override() {
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let thumbs = tempfile::tempdir().unwrap();
        let db = naiad_db::Db::open(&db_path).unwrap();
        db.subscribe_shared_service("a", "http://a", None).unwrap();
        db.subscribe_shared_service("b", "http://b", None).unwrap();

        let toml_path = db_dir.path().join("naiad.toml");
        let state = crate::AppState::new(db, test_store(&thumbs), 64).with_settings_path(toml_path);

        // Set B's override to 12 first.
        let res = repos_query_bits_handler(
            State(state.clone()),
            Json(RepoQueryBitsReq {
                name: "b".into(),
                max_query_bits: Some(12),
            }),
        )
        .await;
        assert!(res.is_ok(), "set B to 12 must succeed");

        // Now set A's override to 20 — must not disturb B.
        let res = repos_query_bits_handler(
            State(state.clone()),
            Json(RepoQueryBitsReq {
                name: "a".into(),
                max_query_bits: Some(20),
            }),
        )
        .await;
        assert!(res.is_ok(), "set A to 20 must succeed");

        assert_eq!(
            repo_max_query_bits(&state, "b"),
            12,
            "B's override must survive after A is set"
        );
        assert_eq!(
            repo_max_query_bits(&state, "a"),
            20,
            "A's override must be 20"
        );

        drop(db_dir);
    }
}

#[cfg(test)]
mod repos_list_tests {
    use super::repos_list_handler;
    use axum::Json;
    use axum::extract::State;

    fn test_store(dir: &tempfile::TempDir) -> crate::thumb_store::ThumbStore {
        crate::thumb_store::ThumbStore::open(&dir.path().join("thumbs.db")).unwrap()
    }

    /// Verify that `repos_list_handler` populates `advertised_bits` and `count`
    /// from the session-cached caps (#bucket-crowd-control task 4).
    #[tokio::test]
    async fn repos_list_advertises_bits_and_count() {
        use std::collections::BTreeMap;

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let thumbs = tempfile::tempdir().unwrap();
        let db = naiad_db::Db::open(&db_path).unwrap();

        // Subscribe one repo and capture its service id.
        let svc_id = db.subscribe_shared_service("a", "http://a", None).unwrap();

        let state = crate::AppState::new(db, test_store(&thumbs), 64);

        // Seed the caps cache — bucketed mode, 18 bits, 94 317 hashes.
        let caps = naiad_netproto::Caps {
            version: naiad_netproto::PROTOCOL_VERSION,
            mode: naiad_netproto::PullMode::Bucketed { prefix_bits: 18 },
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: naiad_netproto::HashDomain::Blake3,
            hash_domains: vec![],
            incremental_domains: None,
            server_version: None,
            serve_hint: BTreeMap::new(),
            streaming: false,
            min_query_bits: None,
            count: Some(94_317),
            store_generation: None,
            name: None,
        };
        state.caps_cache.seed(svc_id, caps);

        let Json(repos) = repos_list_handler(State(state))
            .await
            .ok()
            .expect("repos_list_handler must succeed");
        let a = repos
            .iter()
            .find(|r| r.name == "a")
            .expect("repo 'a' must appear in the list");
        assert_eq!(
            a.advertised_bits,
            Some(18),
            "advertised_bits must come from the cached caps prefix_bits"
        );
        assert_eq!(
            a.count,
            Some(94_317),
            "count must come from the cached caps count field"
        );

        drop(db_dir);
    }
}
