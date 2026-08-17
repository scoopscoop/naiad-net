//! Request middleware for per-minute in-memory aggregation (#235, Piece A).
//!
//! `StatsLayer` wraps the public router outermost, recording matched endpoint,
//! status class, latency (into a `FixedHistogram`), buffered-response bytes, and
//! a salted client-IP observation — all in memory, no `.await` on any DB.
//! The 60-second flush task (`spawn_flush`) swaps the live `MinuteBucket` for a
//! fresh one and writes the closed minute to `StatsDb` outside the lock.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::stats::store::MinuteWrite;
use crate::stats::users::UserCounter;

// ── Fixed histogram ────────────────────────────────────────────────────────────

/// Upper bounds for the 14 finite buckets. The 15th bucket (index 14) is `+inf`.
/// Boundaries: `[0ms, 1ms, 2ms, 5ms, 10ms, 20ms, 50ms, 100ms, 200ms, 500ms,
/// 1 s, 2 s, 5 s, 10 s, +inf]` — 15 buckets total (`[u32; 15]`).
const BOUNDS: [u64; 14] = [
    0, 1, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10000,
];

/// Fixed-bucket millisecond latency histogram used for per-endpoint
/// p50 / p95 / p99 estimation.
///
/// Bucket `i` (0 ≤ i ≤ 13) covers `(BOUNDS[i-1], BOUNDS[i]]`; bucket 14 is
/// `(10 000, +∞)`. All arithmetic is saturating — the type never panics.
#[derive(Clone, Copy, Default)]
pub(crate) struct FixedHistogram([u32; 15]);

impl FixedHistogram {
    /// Increment the bucket covering `ms` whole milliseconds.
    pub(crate) fn record_ms(&mut self, ms: u64) {
        let idx = BOUNDS.iter().position(|&b| ms <= b).unwrap_or(14);
        self.0[idx] = self.0[idx].saturating_add(1);
    }

    /// Estimate the `q`-th quantile (0.0–1.0) by linear interpolation within
    /// the covering bucket. Returns `0.0` for an empty histogram.
    pub(crate) fn percentile(&self, q: f64) -> f64 {
        let total: u64 = self.0.iter().map(|&c| c as u64).sum();
        if total == 0 {
            return 0.0;
        }
        let target = (q * total as f64).ceil() as u64;
        let mut cum = 0u64;
        for (i, &c) in self.0.iter().enumerate() {
            cum += c as u64;
            if cum >= target {
                // Lower bound of this bucket.
                let lo = if i == 0 { 0.0 } else { BOUNDS[i - 1] as f64 };
                // Upper bound: +inf bucket uses 2× the last finite bound.
                let hi = if i >= 14 {
                    BOUNDS[13] as f64 * 2.0
                } else {
                    BOUNDS[i] as f64
                };
                let in_bucket = c as f64;
                let rank_in = (target - (cum - c as u64)) as f64;
                let frac = if in_bucket > 0.0 {
                    rank_in / in_bucket
                } else {
                    1.0
                };
                return lo + (hi - lo) * frac;
            }
        }
        BOUNDS[13] as f64
    }
}

// ── Minute bucket ──────────────────────────────────────────────────────────────

/// In-memory aggregates for one wall-clock minute. Swapped out atomically; the
/// `Mutex` is held for microseconds — never across `.await` or I/O.
struct MinuteBucket {
    /// Request counts keyed by `(endpoint, status_class)`.
    requests: HashMap<(String, &'static str), u64>,
    /// Summed buffered-response bytes per endpoint (streaming excluded).
    bytes: HashMap<String, u64>,
    /// Per-endpoint latency histogram.
    latency: HashMap<String, FixedHistogram>,
    /// Number of streaming responses (no `Content-Length`) this minute.
    streamed: u64,
    /// Unix timestamp floored to the minute (`epoch_secs / 60 * 60`).
    started_minute: i64,
}

impl MinuteBucket {
    fn new(started_minute: i64) -> Self {
        Self {
            requests: HashMap::new(),
            bytes: HashMap::new(),
            latency: HashMap::new(),
            streamed: 0,
            started_minute,
        }
    }
}

// ── Accumulator ────────────────────────────────────────────────────────────────

/// Thread-safe in-memory accumulator for the current minute's request data.
///
/// One instance per server, shared via `Arc<MinuteAccumulator>` between the
/// request middleware and the flush task. Critical sections are microseconds.
pub(crate) struct MinuteAccumulator {
    bucket: Mutex<MinuteBucket>,
}

impl MinuteAccumulator {
    /// Create a new accumulator starting at `started_minute` (unix seconds floored
    /// to the minute: `epoch_secs / 60 * 60`).
    pub(crate) fn new(started_minute: i64) -> Self {
        Self {
            bucket: Mutex::new(MinuteBucket::new(started_minute)),
        }
    }

    /// Record one request's contribution entirely in memory.
    ///
    /// - `content_len = None` with `streamed = true` → 0 bytes, `streamed` counter
    ///   incremented.
    /// - `content_len = Some(n)` → `n` bytes added; `streamed` unchanged.
    ///
    /// Never `.await`s; never touches the DB. Panic-free by construction
    /// (saturating arithmetic; `HashMap` growth is the only allocation).
    pub(crate) fn record(
        &self,
        endpoint: &str,
        status: u16,
        latency_ms: u64,
        content_len: Option<u64>,
        streamed: bool,
    ) {
        let class = status_class(status);
        let mut g = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
        *g.requests.entry((endpoint.to_owned(), class)).or_insert(0) += 1;
        // Only count bytes for 2xx/3xx responses — error-response bodies are
        // not "bytes served" in the shipped-content sense.
        if let Some(bytes) = content_len {
            if matches!(status / 100, 2 | 3) {
                *g.bytes.entry(endpoint.to_owned()).or_insert(0) += bytes;
            }
        }
        if streamed {
            g.streamed += 1;
        }
        g.latency
            .entry(endpoint.to_owned())
            .or_default()
            .record_ms(latency_ms);
    }

    /// Atomically swap out the live `MinuteBucket`, replacing it with a fresh
    /// empty one keyed to `now_minute`.
    ///
    /// Returns a `MinuteWrite` with pre-computed latency percentiles, ready for
    /// `StatsDb::write_minute`. The swap itself holds the lock for < 1 µs; all
    /// percentile computation happens outside the lock.
    pub(crate) fn swap(&self, now_minute: i64) -> MinuteWrite {
        // Swap under the lock — microseconds.
        let old = {
            let mut g = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *g, MinuteBucket::new(now_minute))
        };

        // Flatten outside the lock.
        let total_requests: u64 = old.requests.values().sum();
        let total_bytes: u64 = old.bytes.values().sum();

        let requests: Vec<(String, &'static str, u64)> = old
            .requests
            .into_iter()
            .map(|((ep, class), count)| (ep, class, count))
            .collect();

        let bytes: Vec<(String, u64)> = old.bytes.into_iter().collect();

        let latency: Vec<(String, f64, f64, f64)> = old
            .latency
            .into_iter()
            .map(|(ep, h)| {
                (
                    ep,
                    h.percentile(0.50),
                    h.percentile(0.95),
                    h.percentile(0.99),
                )
            })
            .collect();

        MinuteWrite {
            ts_minute: old.started_minute,
            requests,
            bytes,
            latency,
            total_requests,
            total_bytes,
            streamed: old.streamed,
        }
    }
}

// ── Status class ───────────────────────────────────────────────────────────────

/// Map an HTTP status code to its `"2xx"` / `"3xx"` / `"4xx"` / `"5xx"` label.
/// Anything outside the standard range is `"other"`.
pub(crate) fn status_class(code: u16) -> &'static str {
    match code / 100 {
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    }
}

// ── Tower Layer / Service ──────────────────────────────────────────────────────

/// Tower layer that records per-request stats into a [`MinuteAccumulator`].
///
/// Apply *outermost* in `build_app` so it observes every response, including
/// those short-circuited by `DefaultBodyLimit`, compression, and 4xx guards.
/// Present only when a `StatsHandle` is configured; existing tests pass `None`
/// and are unaffected.
#[derive(Clone)]
pub struct StatsLayer {
    accum: Arc<MinuteAccumulator>,
    users: Arc<UserCounter>,
}

impl StatsLayer {
    /// Construct a layer from shared accumulator and user-counter handles.
    pub(crate) fn new(accum: Arc<MinuteAccumulator>, users: Arc<UserCounter>) -> Self {
        Self { accum, users }
    }
}

impl<S> Layer<S> for StatsLayer {
    type Service = StatsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        StatsService {
            inner,
            accum: Arc::clone(&self.accum),
            users: Arc::clone(&self.users),
        }
    }
}

/// Wraps an inner service, recording one observation per completed response.
pub struct StatsService<S> {
    inner: S,
    accum: Arc<MinuteAccumulator>,
    users: Arc<UserCounter>,
}

impl<S: Clone> Clone for StatsService<S> {
    fn clone(&self) -> Self {
        StatsService {
            inner: self.inner.clone(),
            accum: Arc::clone(&self.accum),
            users: Arc::clone(&self.users),
        }
    }
}

impl<S, ReqBody, ResBody> Service<axum::http::Request<ReqBody>> for StatsService<S>
where
    S: Service<axum::http::Request<ReqBody>, Response = axum::http::Response<ResBody>>
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: http_body::Body + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<ReqBody>) -> Self::Future {
        let start = std::time::Instant::now();

        // Extract route template from extensions before consuming the request.
        let endpoint: String = req
            .extensions()
            .get::<axum::extract::MatchedPath>()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_else(|| "<unmatched>".to_owned());

        // Client IP for distinct-user counting — may be absent (tests, Unix sockets).
        let client_ip: Option<IpAddr> = req
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip());

        let accum = Arc::clone(&self.accum);
        let users = Arc::clone(&self.users);
        let future = self.inner.call(req);

        Box::pin(async move {
            let result = future.await;
            let elapsed_ms = start.elapsed().as_millis() as u64;

            if let Ok(ref response) = result {
                let status = response.status().as_u16();
                // Determine body size. axum/hyper adds Content-Length BELOW the
                // outermost service layer, so it is absent at observation time.
                // Try size_hint().exact() first — axum buffers sized bodies
                // (Json, String, Bytes) and exposes an exact hint before hyper
                // sees the response. Fall back to the Content-Length header for
                // any response that already carries it (e.g. a proxied upstream).
                // A response with neither is streaming (NDJSON, chunked). Note:
                // 204/304 bodiless responses also lack both and count as
                // "streamed" here — byte-count is unavailable for those too.
                // http_body::Body is in scope via the ResBody: http_body::Body bound.
                let content_len = response.body().size_hint().exact().or_else(|| {
                    response
                        .headers()
                        .get(axum::http::header::CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                });
                let streamed = content_len.is_none();

                // Panic-contained: covers both record() and users.observe() so
                // no panic can escape to axum's handler layer or poison the
                // accumulator mutex (mutex poisoning is recovered via
                // unwrap_or_else inside record, but belt-and-suspenders here).
                let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    accum.record(&endpoint, status, elapsed_ms, content_len, streamed);
                    if let Some(ip) = client_ip {
                        users.observe(ip);
                    }
                }));
            }

            result
        })
    }
}

// ── Flush task ─────────────────────────────────────────────────────────────────

/// Spawn the 60-second wall-aligned flush task.
///
/// Each tick:
/// 1. Swaps the live `MinuteBucket` for a fresh empty one (lock held < 1 µs).
/// 2. Writes the closed minute to `StatsDb` **outside** the accumulator lock.
/// 3. Rolls `UserCounter` and persists any completed-window counts as
///    `users_hour` / `users_day` samples.
///
/// A write failure logs `warn target: "stats"` and drops the datum; the repo
/// keeps serving.
pub(crate) fn spawn_flush(
    accum: Arc<MinuteAccumulator>,
    db: Arc<crate::stats::store::StatsDb>,
    users: Arc<UserCounter>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Align the first tick to the next wall-clock minute boundary.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let secs_into_minute = now_secs % 60;
        let secs_to_next = if secs_into_minute == 0 {
            60
        } else {
            60 - secs_into_minute
        };
        tokio::time::sleep(tokio::time::Duration::from_secs(secs_to_next)).await;

        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let now_minute = now_secs / 60 * 60;

            // Swap-then-write: accumulator lock is released before DB I/O.
            let mw = accum.swap(now_minute);
            // Persist streamed-response count before write_minute (skip when 0
            // to avoid noise rows in quiet minutes).
            if mw.streamed > 0 {
                if let Err(e) =
                    db.write_sample(mw.ts_minute, "streamed_responses", "", mw.streamed as f64)
                {
                    tracing::warn!(
                        target: "stats",
                        error = %e,
                        "flush streamed_responses write failed; datum dropped"
                    );
                }
            }
            if let Err(e) = db.write_minute(&mw) {
                tracing::warn!(
                    target: "stats",
                    error = %e,
                    "flush write_minute failed; minute of request data dropped"
                );
            }

            // Roll user-counter windows and persist completed counts.
            let rows = users.roll(now_secs);
            for (metric, ts, count) in rows {
                if let Err(e) = db.write_sample(ts, metric, "", count as f64) {
                    tracing::warn!(
                        target: "stats",
                        error = %e,
                        metric,
                        "flush user sample write failed; datum dropped"
                    );
                }
            }
        }
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_percentiles_on_known_distribution() {
        let mut h = FixedHistogram::default();
        // 100 samples: 90 at 10 ms, 9 at 150 ms, 1 at 1200 ms.
        for _ in 0..90 {
            h.record_ms(10);
        }
        for _ in 0..9 {
            h.record_ms(150);
        }
        h.record_ms(1200);
        assert!(h.percentile(0.50) <= 20.0, "p50 within the 10ms mass");
        let p95 = h.percentile(0.95);
        assert!(
            (100.0..=200.0).contains(&p95),
            "p95 in the 100–200 bucket, got {p95}"
        );
        let p99 = h.percentile(0.99);
        assert!(p99 >= 200.0, "p99 reflects the tail, got {p99}");
    }

    /// Regression test for Defect 2: buffered response bytes always 0.
    ///
    /// Drives a real `StatsService` (tower layer) wrapping an axum `Json` handler.
    /// axum/hyper adds `Content-Length` BELOW the outermost layer, so the header
    /// is absent at observation time.  The fix reads `size_hint().exact()` from
    /// the body instead.  Without the fix this test fails because `total_bytes`
    /// stays 0 and `streamed` is 1.
    #[tokio::test(flavor = "current_thread")]
    async fn stats_service_sized_body_records_bytes_not_streamed() {
        use axum::response::IntoResponse;
        use tower::ServiceExt;

        let now_minute = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            / 60
            * 60;
        let accum = Arc::new(MinuteAccumulator::new(now_minute));
        let users = Arc::new(crate::stats::users::UserCounter::production());
        let layer = StatsLayer::new(Arc::clone(&accum), Arc::clone(&users));

        // Handler returning axum::Json — sized body; no explicit CL header set.
        let handler = tower::service_fn(|_req: axum::http::Request<axum::body::Body>| async {
            let resp = axum::Json(serde_json::json!({"ok": true})).into_response();
            Ok::<_, std::convert::Infallible>(resp)
        });
        let svc = layer.layer(handler);

        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/test")
            .body(axum::body::Body::empty())
            .unwrap();

        let _resp = svc.oneshot(req).await.unwrap();
        let mw = accum.swap(now_minute);

        assert!(
            mw.total_bytes > 0,
            "bytes_shipped_total must be > 0 for a sized Json response (got {})",
            mw.total_bytes
        );
        assert_eq!(
            mw.streamed, 0,
            "a sized Json response must not be counted as streamed"
        );
    }

    /// Regression test for Defect 2 (streaming branch): a body with no exact
    /// size hint must be counted as streamed with 0 bytes.
    #[tokio::test(flavor = "current_thread")]
    async fn stats_service_stream_body_records_streamed_not_bytes() {
        use tower::ServiceExt;

        let now_minute = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            / 60
            * 60;
        let accum = Arc::new(MinuteAccumulator::new(now_minute));
        let users = Arc::new(crate::stats::users::UserCounter::production());
        let layer = StatsLayer::new(Arc::clone(&accum), Arc::clone(&users));

        // Handler returning a streaming body: Body::from_stream has no exact
        // size_hint, so the middleware must count it as streamed.
        let handler = tower::service_fn(|_req: axum::http::Request<axum::body::Body>| async {
            // from_stream takes a TryStream; use iter(once(Ok(...))) to create one.
            let stream = tokio_stream::iter(std::iter::once(Ok::<Vec<u8>, axum::Error>(
                b"streaming chunk".to_vec(),
            )));
            let body = axum::body::Body::from_stream(stream);
            let resp = axum::http::Response::builder()
                .status(200)
                .body(body)
                .unwrap();
            Ok::<_, std::convert::Infallible>(resp)
        });
        let svc = layer.layer(handler);

        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/stream")
            .body(axum::body::Body::empty())
            .unwrap();

        let _resp = svc.oneshot(req).await.unwrap();
        let mw = accum.swap(now_minute);

        assert_eq!(
            mw.streamed, 1,
            "a streaming body must be counted as streamed (got {})",
            mw.streamed
        );
        assert_eq!(
            mw.total_bytes, 0,
            "a streaming body must record 0 bytes (got {})",
            mw.total_bytes
        );
    }

    #[test]
    fn accumulator_counts_bytes_and_swaps_clean() {
        let acc = MinuteAccumulator::new(1000);
        acc.record("/repo/buckets", 200, 21, Some(1000), false);
        acc.record("/repo/buckets", 200, 25, Some(500), false);
        acc.record("/repo/buckets", 404, 2, Some(30), false);
        acc.record("/repo/snapshot", 200, 5, None, true); // streamed → 0 bytes, streamed++

        let mw = acc.swap(1000);

        let bk = mw
            .requests
            .iter()
            .find(|(e, c, _)| e == "/repo/buckets" && *c == "2xx")
            .unwrap();
        assert_eq!(bk.2, 2);

        let bytes = mw.bytes.iter().find(|(e, _)| e == "/repo/buckets").unwrap();
        assert_eq!(bytes.1, 1500, "buffered bytes summed; streamed excluded");

        assert_eq!(
            mw.streamed, 1,
            "one streamed response counted in MinuteWrite"
        );

        // Post-swap the live bucket must be empty.
        let mw2 = acc.swap(1060);
        assert!(mw2.requests.is_empty(), "swap leaves a fresh empty bucket");
        assert_eq!(mw2.streamed, 0, "fresh bucket has zero streamed count");
    }
}
