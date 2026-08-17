//! Axum router for the stats subsystem (#235): `GET /` (embedded page) and
//! `GET /api/stats?range=…` (JSON payload). Binding to a loopback address and
//! the bind-failure warn are handled by the caller (`stats::spawn_stats`).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use crate::stats::store::{Range, query_payload_conn};

// ── State ──────────────────────────────────────────────────────────────────────

/// Listener state: a dedicated read-only `stats.db` connection held for the
/// lifetime of the stats HTTP listener, plus the server start instant for
/// computing `uptime_secs`. Never touches the writer connection in `StatsDb`.
pub struct StatsHttpState {
    /// Read-only connection to `stats.db`. Wrapped in a `Mutex` because
    /// `SQLITE_OPEN_NO_MUTEX` makes `Connection: !Sync`, so the `Arc` would
    /// not be `Send + Sync` without the guard.
    pub(crate) ro_conn: Mutex<Connection>,
    pub(crate) started: Instant,
}

impl StatsHttpState {
    /// Open `path` as a read-only SQLite connection and create the state.
    ///
    /// # Errors
    /// Returns `Err` if the SQLite open fails (e.g. file not found, permission).
    pub fn open(path: &std::path::Path, started: Instant) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening stats.db read-only at {}", path.display()))?;
        Ok(Self {
            ro_conn: Mutex::new(conn),
            started,
        })
    }
}

// ── Router ─────────────────────────────────────────────────────────────────────

/// Build the stats-listener axum `Router`.
///
/// Routes:
/// - `GET /`            → embedded `page.html`, `text/html; charset=utf-8`
/// - `GET /api/stats`   → JSON payload (`?range=24h|7d|90d|all`, default `24h`)
pub fn app(state: Arc<StatsHttpState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/stats", get(api_stats))
        .with_state(state)
}

// ── Handlers ───────────────────────────────────────────────────────────────────

/// Serve the embedded dashboard HTML. Static — no store access.
async fn index() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("page.html"),
    )
}

#[derive(Deserialize)]
struct RangeQ {
    range: Option<String>,
}

/// Serve the JSON stats payload.
///
/// `range` defaults to `24h` when absent or unrecognised — no 400.
/// Reads `stats.db` through the dedicated read-only connection inside
/// `spawn_blocking`. A query error yields `500` + `{"error":"stats query
/// failed"}` and never touches the main repo server.
async fn api_stats(State(st): State<Arc<StatsHttpState>>, Query(q): Query<RangeQ>) -> Response {
    let range = Range::parse(q.range.as_deref());
    let uptime = st.started.elapsed().as_secs();
    let generated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let st2 = Arc::clone(&st);
    let result = tokio::task::spawn_blocking(move || {
        let conn = st2.ro_conn.lock().unwrap_or_else(|e| e.into_inner());
        query_payload_conn(
            &conn,
            range,
            env!("CARGO_PKG_VERSION"),
            uptime,
            generated_at,
        )
    })
    .await;

    match result {
        Ok(Ok(payload)) => Json(payload).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "stats", error = %e, "stats query failed");
            err_500()
        }
        Err(e) => {
            tracing::warn!(target: "stats", error = %e, "stats query task join failed");
            err_500()
        }
    }
}

/// Canonical 500 response for any stats query failure.
fn err_500() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "stats query failed"})),
    )
        .into_response()
}
