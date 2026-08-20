//! `naiad-netproto` — the client/server wire contract plus the client that speaks
//! it. Dependency-light DTOs (like `api`) and a blocking [`RepoClient`] the
//! daemon drives inside `spawn_blocking`. No DB or async runtime here.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

/// Connect timeout for [`RepoClient`] HTTP requests.  Applies to the TCP
/// handshake only; does not bound the transfer phase.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-read idle timeout for [`RepoClient`] HTTP requests.  Fires if the
/// server stops sending data for this long mid-response (stalled socket), but
/// does NOT bound the total transfer time for a large body.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-request deadline for [`RepoClient`] HTTP requests.  Bounds one
/// complete request-response cycle (connect + transfer) so that a slow-drip
/// server (e.g. 1 byte every 29 s) cannot stall a single request indefinitely
/// even when each individual read satisfies [`READ_TIMEOUT`].  Set generously
/// enough to accommodate a 64 MiB response on a slow link.
///
/// **Scope post-#146:** this deadline applies *per request*, not per pull.
/// [`RepoClient::fetch_buckets_in`] and [`RepoClient::fetch_bucket_delta_in`]
/// split large key lists across many requests (up to ~111 for a 94 k-file
/// library at 24-bit prefix width); each request receives its own fresh budget,
/// so the worst-case wall time for a chunked pull is `chunks × OVERALL_TIMEOUT`
/// rather than `OVERALL_TIMEOUT` alone.  Each response is now server-bounded per
/// request (#145); streaming shipped in #176 (opt-in NDJSON); delta path still
/// materialized.
pub const OVERALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-request deadline override for *streaming* `POST /repo/buckets` requests
/// (#176).  The streaming path does not buffer the full response before sending,
/// so the ordinary [`OVERALL_TIMEOUT`] (120 s) would kill a legitimately long
/// but progressing stream.  This value is set generously (30 min) so that
/// progress — measured by [`READ_TIMEOUT`]'s 30 s idle guard — is the only
/// bound on a streaming response, not wall-clock duration.  A server that emits
/// no bytes for 30 s still trips [`READ_TIMEOUT`]; this constant prevents the
/// 120 s whole-request cap from interfering first.
pub const STREAM_OVERALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Maximum byte length of a single NDJSON line in a streamed `POST /repo/buckets`
/// response (#176).  `BufReader::read_line` / `read_until` into a `Vec` is
/// guarded by `take(MAX_STREAM_LINE_BYTES + 1)`: if a line runs over, the client
/// errors rather than buffering an unbounded allocation — the defence a
/// line-at-a-time parse needs that `RESPONSE_SIZE_CAP` provided for the
/// whole-body materialised path.
pub const MAX_STREAM_LINE_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Maximum response body size accepted from any pull/query endpoint.
/// Responses larger than this are rejected with an explicit error rather than
/// materialising an arbitrarily large allocation.
pub const RESPONSE_SIZE_CAP: usize = 64 * 1024 * 1024; // 64 MiB

/// Maximum number of bytes read from a server's HTTP error body when building
/// an error message (#150).  A hostile repo must not be able to inflate an
/// error string arbitrarily; 512 bytes is enough for every real rejection
/// message the server emits (the longest is roughly 80 characters) while
/// keeping diagnostic text human-readable.
pub const STATUS_ERR_BODY_CAP: usize = 512;

/// Maximum cumulative decoded-response bytes accepted across all chunks of one
/// [`RepoClient::fetch_buckets_in`] or [`RepoClient::fetch_bucket_delta_in`]
/// call (#154).  [`RESPONSE_SIZE_CAP`] bounds each individual chunk reply; this
/// constant bounds the merge accumulator so a hostile repo cannot OOM the daemon
/// by returning maximally-large valid chunks across hundreds of requests.
///
/// Sizing rationale: a 94 k-file library at 24-bit prefix width (111 chunks)
/// with 20 tags × 55 bytes each produces roughly 103 MiB of JSON.  A PTR-scale
/// mirror with 10× more content would reach ~1 GiB.  2 GiB gives ~20× headroom
/// over that ceiling while still converting the daemon-killing multi-GB scenario
/// into a recoverable error.  The per-chunk cap (64 MiB) remains the primary
/// defence against any single oversized response.
///
/// Counted in **raw JSON bytes**, not resident bytes: the deserialised
/// `BTreeMap<String, Vec<String>>` is materially larger than the wire form, so
/// this is a backstop that keeps a hostile repo from running away — not a
/// promise about peak RSS. Each response is bounded per request (#145); streaming
/// shipped in #176 (opt-in NDJSON); the delta path remains materialized.
pub const MERGED_RESPONSE_SIZE_CAP: usize = 2 * 1024 * 1024 * 1024; // 2 GiB

/// Target maximum JSON body size for one `POST /repo/buckets` request (#146).
///
/// `naiad-repo` puts a 64 KiB `DefaultBodyLimit` on its whole router, and the
/// bucket key list is the only request that can plausibly outgrow it: a bucket
/// key is a full 64-char hash hex, so it costs 67 body bytes, and a client asks
/// for one key per *distinct hash prefix* it owns. Measured on a 94,317-file
/// library: 268 KB of keys at 12 bits and 6.0 MB at the default 24-bit ceiling
/// — 4× and 96× over the limit. Every full pull from a real library would 413.
///
/// So [`RepoClient::fetch_buckets_in`] and [`RepoClient::fetch_bucket_delta_in`]
/// split the key list into requests that fit this budget and merge the replies.
/// Chunking client-side (rather than raising the server's limit) is what makes
/// a current client work against an **already-deployed** repo, and it bounds
/// each reply too — one 94k-bucket request against a PTR-scale mirror would
/// also blow [`RESPONSE_SIZE_CAP`].
///
/// Set below the server's 64 KiB so the estimate in [`bucket_chunks`] has room
/// to be wrong without turning into a 413.
pub const BUCKET_REQUEST_BODY_BUDGET: usize = 56 * 1024;

/// Body bytes a serialised [`BucketRequest`] costs before any bucket keys:
/// `{"version":6,"prefix_bits":24,"buckets":[],"since":[],"domain":"sha256"}`
/// is 74 bytes, rounded up generously since the budget has slack anyway.
const BUCKET_REQUEST_ENVELOPE: usize = 256;

/// Target wall time per adaptive bucket request window, in milliseconds (#174).
/// The adaptive walker aims each request at roughly this duration.
/// Re-exported so the daemon can reason about the bootstrap seed (#178).
pub const WINDOW_TARGET_MS: u64 = 5_000;

/// Slow threshold for AIMD shrink (#174): a window that took longer than this
/// triggers a multiplicative decrease (`W ← max(MIN_WINDOW, W / 2)`).
/// Equal to `WINDOW_TARGET_MS` so any over-target response shrinks.
const WINDOW_SLOW_MS: u64 = 5_000;

/// Fast threshold for AIMD growth (#174): a window that completed faster than
/// this triggers an additive increase (`W ← W + MIN_WINDOW`).
/// Set at target/2 so a comfortably under-budget window grows.
const WINDOW_FAST_MS: u64 = 2_500;

/// Minimum window size in buckets for the adaptive walker (#174). Prevents
/// per-request overhead from dominating when the server is very slow: even the
/// coldest repo gets at least 32 buckets per request, giving useful payload per
/// round-trip while still keeping each request relatively short.
/// Re-exported so the daemon's bootstrap seed can assert it lands here (#178).
pub const MIN_WINDOW: usize = 32;

/// Consecutive failures tolerated at the MIN_WINDOW floor before a window fetch
/// gives up and aborts the pull (#177). Above the floor a retryable failure
/// halves the window; once the window is already 32 buckets it cannot shrink
/// further, so this bounds patience at the floor. 3 is chosen so a single cold
/// region's warm-up is ridden out — #170 measured a 24-bit cold window that
/// served in ~51 s on the first manual retry, so 3 attempts (each up to a 30 s
/// READ_TIMEOUT plus backoff, ≈ 90 s of patience) comfortably covers one
/// cold-region warm-up — while a genuinely unserveable window (a dead server,
/// or the #178 single-oversized-bucket case) still fails in under two minutes
/// rather than hanging.
const FLOOR_RETRY_LIMIT: usize = 3;

/// Base backoff between window shrink-retries (#177). A timed-out window has
/// already burned up to READ_TIMEOUT (30 s) of wall time, so this is negligible
/// against that; it exists to (a) avoid a tight reconnect loop hammering a
/// server that just dropped the socket, and (b) give a just-touched cold disk
/// region a moment to settle in the page cache (#170 found cold-region stalls
/// that resolve once pages warm). Bounded because the retry count itself is
/// bounded (§3.2).
const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Cap on the per-attempt backoff so the bounded floor-retry stretch adds at
/// most a few seconds of deliberate wait on top of the transport timeouts
/// (#177).
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(3);

/// The largest `end > start` such that serialising `buckets[start..end]` (with
/// index-aligned `since`) stays under `budget`, capped at `start + max_window`.
/// Always returns at least `start + 1` (a lone oversized key gets its own
/// request rather than looping forever) unless `start == buckets.len()`.
///
/// The cost of each entry is measured, not assumed: a key contributes its own
/// length plus 3 (two quotes and a comma) and a cursor its decimal width plus 1,
/// so the estimate does not silently drift if a caller ever passes shorter keys
/// than [`bucket_key`] produces.
fn window_end(
    buckets: &[String],
    since: Option<&[u64]>,
    start: usize,
    budget: usize,
    max_window: usize,
) -> usize {
    let per_entry = |i: usize| -> usize {
        let key = buckets[i].len() + 3; // two quotes and a comma
        let cursor = since.map_or(0, |s| {
            // `s` is index-aligned with `buckets`; a caller that violates that
            // is rejected by the server, so fall back to the widest u64 rather
            // than panicking on a short slice here.
            s.get(i).map_or(21, |v| decimal_width(*v) + 1)
        });
        key + cursor
    };
    let available = budget.saturating_sub(BUCKET_REQUEST_ENVELOPE);
    let end_cap = start.saturating_add(max_window).min(buckets.len());
    let mut used = 0;
    for i in start..end_cap {
        let cost = per_entry(i);
        // Close the current window before `i` when adding it would overflow —
        // unless the window is still empty, in which case this one oversized
        // entry has to go somewhere.
        if used + cost > available && i > start {
            return i;
        }
        used += cost;
    }
    // All entries in [start, end_cap) fit; return end_cap, but guarantee at
    // least start+1 so a lone oversized key is not dropped (end_cap >= start+1
    // whenever start < buckets.len(), since max_window >= 1 in practice).
    end_cap.max(start + 1)
}

/// Split `buckets` (and its index-aligned `since`, when present) into index
/// ranges whose serialised JSON is expected to stay under `budget` bytes.
///
/// Implemented as a thin loop over [`window_end`] so there is one cost
/// estimator shared with the adaptive snapshot path.
///
/// Guarantees, in order of importance to callers:
/// - Ranges are contiguous and cover `0..buckets.len()` exactly once, so the
///   union of the chunks is the union the caller asked for.
/// - An empty `buckets` yields exactly one empty range, **not** zero ranges:
///   a pull from a library with no files must still perform one request, since
///   the reply carries the repo cursor the caller records.
/// - A single key larger than the whole budget still gets its own chunk rather
///   than producing an empty chunk (which would loop forever).
fn bucket_chunks(buckets: &[String], since: Option<&[u64]>, budget: usize) -> Vec<(usize, usize)> {
    if buckets.is_empty() {
        return vec![(0, 0)];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < buckets.len() {
        let end = window_end(buckets, since, start, budget, usize::MAX);
        chunks.push((start, end));
        start = end;
    }
    chunks
}

/// Decimal digits in `v` (1 for zero). Used to size `since` cursors exactly.
fn decimal_width(v: u64) -> usize {
    if v == 0 {
        return 1;
    }
    v.ilog10() as usize + 1
}

/// Error text for a failed bucket request, with position context that scales to
/// the calling path:
/// - `total > 1` (delta / legacy chunk path): `"(chunk N of total)"`.
/// - `total == 0` (adaptive snapshot path — total unknown upfront): `"(window N,
///   buckets lo..hi)"` so the failing window range is always named.
/// - `total == 1` (single unchunked request): bare error, no suffix.
fn chunk_err(
    what: &str,
    url: &str,
    n: usize,
    total: usize,
    lo: usize,
    hi: usize,
    e: &dyn std::fmt::Display,
) -> anyhow::Error {
    if total > 1 {
        anyhow!("{what} from {url} (chunk {} of {total}): {e}", n + 1)
    } else if total == 0 {
        anyhow!(
            "{what} from {url} (window {}, buckets {lo}..{hi}): {e}",
            n + 1
        )
    } else {
        anyhow!("{what} from {url}: {e}")
    }
}

/// Build an error for a failed GET/POST on the read path, reading the server's
/// rejection body when the failure is an HTTP `Status` error (#150).
///
/// For `ureq::Error::Status(code, resp)` the body is read and truncated to
/// [`STATUS_ERR_BODY_CAP`] bytes so a hostile server cannot balloon an error
/// string.  Non-`Status` transport errors keep their original text.
///
/// The four read-path sites (`fetch_snapshot_in`, `fetch_caps`,
/// `fetch_buckets_in`, `fetch_bucket_delta_in`) all previously surfaced only
/// `ureq`'s `"url: status code N"` string, hiding every actionable rejection
/// the server emitted (domain mismatch, prefix-bits floor, snapshot-mode
/// delta rejection, etc.).  The five write-path sites already read the body;
/// this helper brings parity without touching their distinct message shapes.
fn status_err(what: &str, url: &str, e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            // Read the body so the caller sees the server's actual rejection reason.
            // `into_string` itself refuses bodies over 10 MiB (ureq's
            // INTO_STRING_LIMIT), returning Err — which becomes an empty reason
            // rather than an unbounded allocation.
            let raw = resp.into_string().unwrap_or_default();
            // Truncate before formatting: a hostile repo must not inflate the
            // error string via an arbitrarily large body. Cut on a char
            // boundary — slicing mid-codepoint panics, and the body is
            // server-controlled bytes, so a crafted multibyte char straddling
            // the cap would otherwise take the daemon down.
            let body = if raw.len() > STATUS_ERR_BODY_CAP {
                let cut = (0..=STATUS_ERR_BODY_CAP)
                    .rev()
                    .find(|&i| raw.is_char_boundary(i))
                    .unwrap_or(0);
                format!("{} … (truncated)", &raw[..cut])
            } else {
                raw
            };
            anyhow!("{what} from {url} ({code}): {}", body.trim())
        }
        e => anyhow!("{what} from {url}: {e}"),
    }
}

mod auth;
mod bucket;
mod relation;
mod sign;
pub use auth::{
    AUTH_FRESHNESS_SECS, HDR_AUTH_KEY, HDR_AUTH_SIG, HDR_AUTH_TS, auth_canonical_bytes, verify_auth,
};
pub use bucket::{
    BucketRequest, Caps, DeltaMapping, DomainError, DomainParam, HINT_SHIFT_CLAMP, HashDomain,
    MappingDelta, MappingStatus, OriginTag, ParseHashDomainError, PullMode, ServeHint,
    StreamHeader, StreamRow, StreamTrailer, bucket_key, bucket_upper, effective_prefix_bits,
    effective_prefix_bits_floored, requested_domain, resolve_domain,
};
pub use relation::{
    AuthoredEdge, DeltaEdge, EdgeStatus, RelKind, RelationDelta, RelationGraph, RelationSubmission,
};
pub use sign::{
    Account, MAX_ORIGIN_LEN, canonical_bytes, relation_canonical_bytes, validate_key_hex, verify,
    verify_relation,
};

/// The client/server wire version this build speaks. A pulled snapshot whose
/// `version` differs is rejected (see [`ensure_supported`]). Negotiation is
/// deferred; this is a tripwire, not a handshake.
///
/// **Grammar policy (#77):** the leading-colon tag grammar change shipped inside
/// v6 WITHOUT a version bump (the parser is lenient and already canonical on
/// any real data). Any future tag-grammar change that would cause old/new builds
/// to DISAGREE on how a stored tag string is interpreted MUST bump
/// `PROTOCOL_VERSION` (and raise `MIN_SUPPORTED_VERSION`) so mismatches fail
/// loud instead of silently discarding valid tags.
pub const PROTOCOL_VERSION: u32 = 8;

/// Oldest wire version this build still accepts. v8 folds `origin` (the
/// generation source asserted by the signer) into the signed submission
/// canonical bytes (ADR 0026, #162): a v7 signer omitted it, so a v7 signature
/// does not verify against a v8 canonical frame and vice versa. The
/// `naiad-sub:v{PROTOCOL_VERSION}` prefix makes the mismatch a clean rejection
/// rather than a misinterpretation, but there is no compatibility window — both
/// constants move (hard cutover, pre-1.0 per ADR 0015).
pub const MIN_SUPPORTED_VERSION: u32 = 8;

/// Route path for the whole-repo snapshot read (below-`k` fallback + debug).
pub const REPO_SNAPSHOT: &str = "/repo/snapshot";

/// Route path for the capabilities handshake (advertised prefix length).
pub const REPO_CAPS: &str = "/repo/caps";

/// Route path for the server liveness probe.
pub const REPO_HEALTH: &str = "/health";

/// Route path for a batched bucket pull.
pub const REPO_BUCKETS: &str = "/repo/buckets";

/// Route path for submitting one signed tag operation.
pub const REPO_SUBMIT: &str = "/repo/submit";

/// Route path for submitting one signed relation operation.
pub const REPO_RELATIONS_SUBMIT: &str = "/repo/relations/submit";

/// Route path for the whole relation-graph bulk read.
pub const REPO_RELATIONS: &str = "/repo/relations";

/// Route path for a moderator's signed approve/reject/lift action.
pub const REPO_MODERATE: &str = "/repo/moderate";

/// Route path for filing an anonymous report against a `(hash, tag)`.
pub const REPO_REPORT: &str = "/repo/report";

/// Route path for the moderator report queue (GET) — authenticated.
pub const REPO_REPORTS: &str = "/repo/reports";

/// A repository's whole `hash → [tag]` set. Keys are 64-char lowercase BLAKE3
/// hex (or SHA-256 hex in the sha256 domain). Each entry is the currently-active
/// tags for that file, each carrying its asserted generation origin (ADR 0026).
/// `BTreeMap` makes the serialized output deterministic.
///
/// **v8 wire break:** v7 snapshots serialised `tags` as bare string arrays
/// (`["tag:a","tag:b"]`); v8 uses `[{"tag":"tag:a"}]` objects. A v7 snapshot
/// body fails at the serde layer rather than the version gate — acceptable under
/// the pre-1.0 hard cutover (ADR 0015).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    #[serde(default)]
    pub cursor: u64,
    pub tags: BTreeMap<String, Vec<bucket::OriginTag>>,
}

/// A tag operation: assert a tag (`Add`) or retract the author's own
/// assertion (`Remove`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Add,
    Remove,
}

impl Op {
    /// The canonical lowercase token used in the signed bytes and on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Add => "add",
            Op::Remove => "remove",
        }
    }
}

/// One signed tag operation submitted to a repository. `hash`/`tag` are the
/// normalized strings the signature covers; `author`/`signature` are hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    pub version: u32,
    pub op: Op,
    pub hash: String,
    pub tag: String,
    pub author: String,
    pub signature: String,
    /// Generation source asserted by the signer, or None = manual/unattested.
    /// FRAMED INTO the signed canonical bytes (unlike RelationSubmission.origin,
    /// which is carried unsigned). Asserted, not proven (ADR 0026): the signature
    /// binds authorship of the claim, not the claim's truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// An anonymous report filing a `(hash, tag)` for moderator review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub version: u32,
    pub hash: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One report entry in the moderator queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRow {
    pub id: u64,
    pub hash: String,
    pub tag: String,
    /// Reporter account hex, or a server-assigned anonymous token.
    pub reporter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: i64,
    /// Status string: `"open"` | `"resolved"` | `"dismissed"`. Plain `String`
    /// (no enum) so unknown future values are preserved — forward-compatible.
    pub status: String,
}

/// The moderator report-queue response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportList {
    pub version: u32,
    pub rows: Vec<ReportRow>,
}

/// A moderator action posted to `REPO_MODERATE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ModerateAction {
    /// Hard-delete the `(hash, tag)` mapping.
    DeleteMapping { hash: String, tag: String },
    /// Ban the account pubkey (hex) from the server.
    Ban { pubkey: String },
    /// Dismiss an open report without taking further action.
    Dismiss { report_id: u64 },
}

/// Accept a snapshot only if its wire version is within this build's
/// supported range.
///
/// # Errors
/// Returns an error if `version` is outside
/// `MIN_SUPPORTED_VERSION..=PROTOCOL_VERSION`.
pub fn ensure_supported(version: u32) -> Result<()> {
    if (MIN_SUPPORTED_VERSION..=PROTOCOL_VERSION).contains(&version) {
        Ok(())
    } else {
        Err(anyhow!(
            "unsupported repo protocol version {version} (this client speaks \
             {MIN_SUPPORTED_VERSION}..={PROTOCOL_VERSION})"
        ))
    }
}

/// Current Unix timestamp in seconds — used to stamp auth headers.
fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Deserialise a JSON response body, rejecting bodies larger than `cap` bytes.
///
/// Reads up to `cap + 1` bytes through a [`std::io::Take`] guard; if the body
/// hits the cap the response is considered malformed and an error is returned
/// instead of allocating an unbounded buffer.  Under the cap the bytes are
/// decoded with `serde_json`.
///
/// # Errors
/// Returns an error if the body exceeds `cap`, JSON decoding fails, or an I/O
/// error occurs while reading.
fn read_capped<T: serde::de::DeserializeOwned>(resp: ureq::Response, cap: usize) -> Result<T> {
    let mut buf = Vec::new();
    // Read at most cap+1 bytes so we can detect the over-cap case.
    resp.into_reader()
        .take((cap + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| anyhow!("reading response body: {e}"))?;
    if buf.len() > cap {
        let mib = cap / (1024 * 1024);
        return Err(anyhow!("response exceeds {mib} MiB cap"));
    }
    serde_json::from_slice(&buf).map_err(|e| anyhow!("decoding JSON response: {e}"))
}

/// Like [`read_capped`] but also returns the number of raw response bytes read,
/// so the chunk-loop accumulator can enforce [`MERGED_RESPONSE_SIZE_CAP`].
///
/// # Errors
/// Same as [`read_capped`].
fn read_capped_counted<T: serde::de::DeserializeOwned>(
    resp: ureq::Response,
    cap: usize,
) -> Result<(T, usize)> {
    let mut buf = Vec::new();
    resp.into_reader()
        .take((cap + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| anyhow!("reading response body: {e}"))?;
    if buf.len() > cap {
        let mib = cap / (1024 * 1024);
        return Err(anyhow!("response exceeds {mib} MiB cap"));
    }
    let raw_len = buf.len();
    let value = serde_json::from_slice(&buf).map_err(|e| anyhow!("decoding JSON response: {e}"))?;
    Ok((value, raw_len))
}

/// The `domain` field every authenticated [`RepoClient`] request signs.
///
/// None of the four authenticated routes takes a `?domain=` today: submit only
/// targets the repo's native domain, and report/moderate/reports reject every
/// non-native one outright (#160). Signing `None` states that on the wire, so a
/// proxy that appends `?domain=` gets a 401 rather than a redirect (#161). A
/// phase-3 client that submits to an added domain must send the parameter *and*
/// pass the matching `Some(domain)` here — the two must agree.
const NO_DOMAIN: Option<HashDomain> = None;

/// Why a window fetch is being retried (#177). `Copy` so [`PullPhase`] stays
/// `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryReason {
    /// Transport-level timeout or connect failure (e.g. os error 10060).
    Timeout,
    /// Connection reset or refused mid-transfer.
    Disconnect,
    /// Stream truncated — EOF received without a `done` trailer line.
    Truncation,
}

/// Outcome of one window-fetch attempt. `Retryable` carries the [`RetryReason`]
/// so the outer loop can propagate it to the observer without string-matching;
/// `Fatal` propagates unchanged (the pull aborts, exactly as today). (#177)
#[derive(Debug)]
enum WindowError {
    Retryable(RetryReason, anyhow::Error),
    Fatal(anyhow::Error),
}

/// Classify a `ureq::Error` into [`WindowError`]: `Transport` variants are
/// retryable (the #170 timeout/connection-reset case); `Status` variants are
/// fatal (a deterministic server refusal — retrying only burns the floor
/// budget). The error message is produced by [`status_err`] in both arms so
/// the text is unchanged. The `RetryReason` is determined from the transport
/// error message: "timed out" / os error 10060 → `Timeout`, otherwise
/// `Disconnect`. (#177)
fn classify_ureq(what: &str, url: &str, e: ureq::Error) -> WindowError {
    let reason = if let ureq::Error::Transport(ref t) = e {
        let msg = format!("{t}").to_lowercase();
        if msg.contains("timed out") || msg.contains("10060") {
            RetryReason::Timeout
        } else {
            RetryReason::Disconnect
        }
    } else {
        // Not a transport error — will become Fatal below; reason unused.
        RetryReason::Disconnect
    };
    match e {
        ureq::Error::Transport(_) => WindowError::Retryable(reason, status_err(what, url, e)),
        _ => WindowError::Fatal(status_err(what, url, e)),
    }
}

/// Linear retry backoff capped at [`RETRY_BACKOFF_MAX`]. `attempt` is
/// 1-based (the first retry is attempt 1). (#177)
fn retry_backoff(attempt: usize) -> Duration {
    RETRY_BACKOFF_BASE
        .saturating_mul(attempt as u32)
        .min(RETRY_BACKOFF_MAX)
}

/// A phase of one repo's network fetch, reported to a [`PullObserver`] so a
/// caller can surface sub-repo progress. The core shape is one `RequestSent` +
/// one `ChunkReceived` per adaptive request window plus the caller-driven
/// `Merging`/`Done` bookends. When streaming is active (#176) additional
/// `RowReceived` events are emitted within each window — one per streamed row
/// — providing within-window progress ticks at finer granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullPhase {
    /// About to POST a window of `window` buckets. `done` buckets precede it;
    /// `total` is the whole fetch's bucket count (fixed). WholeRepo reports
    /// `done: 0, total: 1, window: 1` — one indivisible request.
    RequestSent {
        done: usize,
        total: usize,
        window: usize,
    },
    /// A window returned. `done` now includes this window; `total` unchanged.
    /// `chunk_bytes` is this window's raw body length (summed across any 413
    /// bisection or streaming continuation); `cumulative_bytes` is the running
    /// total for THIS fetch call. `hashes`/`tags` are the merged running totals
    /// so far in this fetch; `request_ms` is this window's wall time (drives
    /// adaptation).
    ChunkReceived {
        done: usize,
        total: usize,
        window: usize,
        chunk_bytes: usize,
        cumulative_bytes: usize,
        hashes: usize,
        tags: usize,
        request_ms: u64,
    },
    /// One NDJSON row arrived within a streaming window (#176). `hashes`/`tags`
    /// are the merged running totals after this row lands. Emitted per row by
    /// `fetch_buckets_streaming`; the enclosing window's `ChunkReceived` is
    /// still emitted once the window completes, so `RowReceived` events are
    /// additive within-window ticks.
    RowReceived { hashes: usize, tags: usize },
    /// A window fetch failed with a retryable transport error and is being
    /// retried at a shrunk size (#177). `old_window`/`new_window` are the
    /// issued window sizes before and after the shrink (equal when already at
    /// the `MIN_WINDOW` floor); `attempt` is 0-based; `done`/`total` mirror
    /// `RequestSent` so a UI can place the retry within the walk.
    WindowRetry {
        done: usize,
        total: usize,
        old_window: usize,
        new_window: usize,
        attempt: usize,
        reason: RetryReason,
    },
    /// Network done for this repo; the local merge is starting (caller-emitted).
    Merging,
    /// This repo is fully done (caller-emitted, after the merge).
    Done,
}

/// Observes a pull's phases. `on_phase` runs synchronously on the calling
/// (blocking) thread, so an implementation must be cheap and non-blocking — the
/// SSE impl just does a non-blocking `UnboundedSender::send`. Object-safe;
/// callers take `&dyn PullObserver`.
pub trait PullObserver {
    fn on_phase(&self, phase: PullPhase);
    /// Called by the per-file pull orchestrator before each hash-domain leg so
    /// the observer can annotate subsequent phases with the active domain
    /// (`"blake3"`/`"sha256"`), and with `None` around the merge. Default no-op:
    /// `netproto` never calls this — it has no "leg" concept — only the daemon
    /// does. This realizes the spec's "domain set around each leg" without a
    /// concrete downcast through `&dyn PullObserver`.
    fn set_domain(&self, _domain: Option<&'static str>) {}
}

/// No-op observer for every caller that does not want progress (full-repo
/// pulls, tests). A shared unit impl keeps call sites free of `Option`.
pub struct NoopObserver;
impl PullObserver for NoopObserver {
    fn on_phase(&self, _: PullPhase) {}
}

/// Client-side single-pass decode target for a streamed NDJSON body line.
/// A line is either a row or the trailer; the
/// header is parsed separately (`StreamHeader`) before the loop, so it is not
/// a variant here. `untagged` matches the existing on-wire shapes with no
/// discriminator field, exactly as `StreamTrailer` already does.
///
/// Accepted divergence from the old key-sniffing dispatch: a hybrid line
/// carrying both trailer keys and a spurious `"h"` (e.g.
/// `{"done":true,"h":"x"}`) now parses as a trailer instead of failing Fatal.
/// No conforming server emits such lines, and rejecting them via
/// `deny_unknown_fields` would break forward compatibility with future
/// trailer fields.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StreamLine {
    /// A hash→tags row: `{"h":"…","t":[…]}`. Tried first because rows dominate
    /// the stream; `untagged` tries variants top-down.
    Row(StreamRow),
    /// A budget/done/error trailer: `{"done":…}` | `{"more":"…"}` | `{"err":"…"}`.
    Trailer(StreamTrailer),
}

/// Produce the same Fatal diagnostic the two-pass decoder produced, used only
/// on the parse-error path (always fatal, always aborts — cost irrelevant).
fn classify_bad_stream_line(url: &str, line: &str) -> WindowError {
    match serde_json::from_str::<serde_json::Value>(line) {
        Err(e) => WindowError::Fatal(anyhow!(
            "streaming buckets from {url}: bad NDJSON line: {e}"
        )),
        Ok(v) if v.get("h").is_some() => match serde_json::from_value::<StreamRow>(v) {
            Err(e) => {
                WindowError::Fatal(anyhow!("streaming buckets from {url}: bad row line: {e}"))
            }
            Ok(_) => WindowError::Fatal(anyhow!(
                "streaming buckets from {url}: bad row line: untagged parse failed"
            )),
        },
        Ok(v) => match serde_json::from_value::<StreamTrailer>(v) {
            Err(e) => WindowError::Fatal(anyhow!(
                "streaming buckets from {url}: bad trailer line: {e}"
            )),
            Ok(_) => WindowError::Fatal(anyhow!(
                "streaming buckets from {url}: bad trailer line: untagged parse failed"
            )),
        },
    }
}

/// A blocking client for one repository's base URL.
pub struct RepoClient {
    base: String,
    /// Regular (non-streaming) agent: `OVERALL_TIMEOUT` (120 s) whole-request
    /// deadline, `READ_TIMEOUT` (30 s) idle guard. Used for all non-streaming
    /// requests.
    agent: ureq::Agent,
    /// Streaming agent: `STREAM_OVERALL_TIMEOUT` (30 min) whole-request
    /// deadline, same `READ_TIMEOUT` (30 s) idle guard (#176). The longer
    /// deadline allows a legitimately long streamed response to complete without
    /// the 120 s cap interfering; the idle guard still fires on true stalls.
    streaming_agent: ureq::Agent,
}

impl RepoClient {
    /// Build a client for the repository at `base_url` (trailing slash trimmed).
    ///
    /// Two agents are constructed internally:
    /// - `agent` — non-streaming requests: `CONNECT_TIMEOUT` (5 s) handshake,
    ///   `READ_TIMEOUT` (30 s) idle guard, `OVERALL_TIMEOUT` (120 s) deadline.
    /// - `streaming_agent` — streaming `POST /repo/buckets` (#176):
    ///   `CONNECT_TIMEOUT` (5 s) handshake + `READ_TIMEOUT` (30 s) idle guard.
    ///   No agent-level `.timeout()` is set here: ureq's `DeadlineStream::fill_buf`
    ///   would collapse `timeout_read` into the remaining deadline, defeating the
    ///   idle guard. The overall bound is enforced by a per-response deadline
    ///   checked in the client loop (`fetch_buckets_streaming`).
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .timeout(OVERALL_TIMEOUT)
            .build();
        // No agent-level `.timeout()`: see doc comment above.
        let streaming_agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .build();
        Self {
            base,
            agent,
            streaming_agent,
        }
    }

    /// Build a client with a custom streaming-path read timeout.
    ///
    /// Intended for tests that need to exercise the idle-timeout guard without
    /// waiting 30 s wall-clock. Builds the same production configuration as
    /// [`RepoClient::new`] but substitutes the injected value for `READ_TIMEOUT`.
    /// No agent-level `.timeout()` is set, matching production behaviour.
    /// Production code uses [`RepoClient::new`].
    #[cfg(test)]
    fn with_streaming_read_timeout(
        base_url: &str,
        streaming_read_timeout: std::time::Duration,
    ) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .timeout(OVERALL_TIMEOUT)
            .build();
        // Production config: timeout_connect + timeout_read only, no agent-level timeout.
        let streaming_agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(streaming_read_timeout)
            .build();
        Self {
            base,
            agent,
            streaming_agent,
        }
    }

    /// Fetch the repository's whole snapshot in its default hash domain.
    ///
    /// # Errors
    /// Returns an error if the request fails, the body is not decodable, or the
    /// wire version is unsupported.
    pub fn fetch_snapshot(&self) -> Result<Snapshot> {
        self.fetch_snapshot_in(None, &NoopObserver)
    }

    /// Fetch the repository's whole snapshot in `domain`.
    ///
    /// `None` sends no `domain=` at all, producing a request byte-identical to
    /// the one a pre-dual-domain client sends — use it against any repo whose
    /// caps omitted `hash_domains` (see [`Caps::wire_domain`]).
    ///
    /// # Errors
    /// Returns an error if the request fails, the body is not decodable, or the
    /// wire version is unsupported.
    pub fn fetch_snapshot_in(
        &self,
        domain: Option<HashDomain>,
        observer: &dyn PullObserver,
    ) -> Result<Snapshot> {
        let url = match domain {
            Some(d) => format!("{}{}?domain={d}", self.base, REPO_SNAPSHOT),
            None => format!("{}{}", self.base, REPO_SNAPSHOT),
        };
        let started = std::time::Instant::now();
        observer.on_phase(PullPhase::RequestSent {
            done: 0,
            total: 1,
            window: 1,
        });
        let resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| status_err("fetching snapshot", &url, e))?;
        let (snapshot, body_len): (Snapshot, usize) = read_capped_counted(resp, RESPONSE_SIZE_CAP)
            .map_err(|e| anyhow!("decoding snapshot from {url}: {e}"))?;
        ensure_supported(snapshot.version)?;
        let request_ms = started.elapsed().as_millis() as u64;
        let tags: usize = snapshot.tags.values().map(Vec::len).sum();
        observer.on_phase(PullPhase::ChunkReceived {
            done: 1,
            total: 1,
            window: 1,
            chunk_bytes: body_len,
            cumulative_bytes: body_len,
            hashes: snapshot.tags.len(),
            tags,
            request_ms,
        });
        tracing::debug!(target: "sync", %url, hashes = snapshot.tags.len(), tags, body_len, elapsed_ms = request_ms, "fetched snapshot");
        Ok(snapshot)
    }

    /// Fetch the repo's pull capabilities (the prefix-length handshake),
    /// rejecting an unsupported version.
    ///
    /// # Errors
    /// Returns an error if the request fails, the body is undecodable, or the
    /// wire version is unsupported.
    pub fn fetch_caps(&self) -> Result<Caps> {
        let url = format!("{}{}", self.base, REPO_CAPS);
        let started = std::time::Instant::now();
        let resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| status_err("fetching caps", &url, e))?;
        let caps: Caps = read_capped(resp, RESPONSE_SIZE_CAP)
            .map_err(|e| anyhow!("decoding caps from {url}: {e}"))?;
        ensure_supported(caps.version)?;
        tracing::debug!(target: "sync", %url, mode = ?caps.mode, elapsed_ms = started.elapsed().as_millis() as u64, "fetched caps");
        Ok(caps)
    }

    /// Fetch the union of `buckets` (lo-bound hash hex) at `prefix_bits` in the
    /// repo's default hash domain. The response reuses the snapshot shape (a
    /// partial snapshot).
    ///
    /// # Errors
    /// Returns an error if the request fails, the body is undecodable, or the
    /// wire version is unsupported.
    pub fn fetch_buckets(&self, prefix_bits: u32, buckets: &[String]) -> Result<Snapshot> {
        self.fetch_buckets_in(prefix_bits, buckets, None, None, &NoopObserver, false)
    }

    /// Fetch the union of `buckets` at `prefix_bits` in `domain`. `None` omits
    /// the `domain` field from the request body entirely.
    ///
    /// The key list is split across as many requests as it takes to stay under
    /// [`BUCKET_REQUEST_BODY_BUDGET`] (#146) and the replies are merged, so the
    /// caller still receives exactly one [`Snapshot`] covering every requested
    /// bucket — the one-merge rule in the daemon's pull path is untouched.
    ///
    /// The merged `cursor` is the **minimum** across the replies. A repo that
    /// advances mid-pull hands later chunks a higher cursor, and recording the
    /// maximum would skip the changes the earlier chunks never saw; the minimum
    /// re-reads a little on the next pull, which the merge is idempotent under.
    ///
    /// # Response size / streaming
    /// When `stream` is `false` (or the server does not advertise `streaming`),
    /// the server bounds each response and returns **413** on budget overflow;
    /// the client bisects (#145). When `stream` is `true` the server streams
    /// NDJSON rows with a budget-cutoff continuation cursor, so each window
    /// completes across several bounded responses rather than 413-ing (#176).
    ///
    /// # Errors
    /// Returns an error if any request fails, a body is undecodable, or a wire
    /// version is unsupported. A failed chunk aborts the whole fetch — the
    /// caller must not merge a partial union as if it were authoritative.
    pub fn fetch_buckets_in(
        &self,
        prefix_bits: u32,
        buckets: &[String],
        domain: Option<HashDomain>,
        hint: Option<f64>,
        observer: &dyn PullObserver,
        stream: bool,
    ) -> Result<Snapshot> {
        self.fetch_buckets_inner(
            prefix_bits,
            buckets,
            domain,
            MERGED_RESPONSE_SIZE_CAP,
            hint,
            observer,
            stream,
        )
    }

    /// Inner implementation of [`fetch_buckets_in`] with an explicit
    /// `merged_cap` so unit tests can exercise the aggregate-size guard without
    /// generating gigabytes of fake response data.
    #[allow(clippy::too_many_arguments)]
    fn fetch_buckets_inner(
        &self,
        prefix_bits: u32,
        buckets: &[String],
        domain: Option<HashDomain>,
        merged_cap: usize,
        hint: Option<f64>,
        observer: &dyn PullObserver,
        stream: bool,
    ) -> Result<Snapshot> {
        let url = format!("{}{}", self.base, REPO_BUCKETS);
        let started = std::time::Instant::now();
        let total = buckets.len();
        let mut merged = Snapshot {
            version: PROTOCOL_VERSION,
            cursor: u64::MAX,
            tags: BTreeMap::new(),
        };
        // Committed bytes from successfully completed windows (used as read-only
        // base when computing the aggregate cap for the in-flight scratch).
        let mut cumulative_bytes: usize = 0;

        // Initial adaptive window: hint seeds the first guess; no hint → start
        // as wide as the body budget allows (today's behaviour, then adapt from
        // the second window onward after observing the first latency).
        let mut w: usize = match hint {
            Some(ms) if ms > 0.0 && ms.is_finite() => {
                ((WINDOW_TARGET_MS as f64) / ms).round() as usize
            }
            _ => usize::MAX,
        };

        let mut start = 0usize;
        let mut win_count = 0usize;

        // Invariant: loop runs at least once even for an empty key list (#146).
        loop {
            // ── Compute the initial window for this outer iteration ──────────
            let (initial_end, initial_window_size) = if total == 0 {
                // Empty list: one request with an empty range so the reply
                // carries the repo cursor.
                (0, 0)
            } else {
                let clamped_w = w.max(MIN_WINDOW);
                let end = window_end(buckets, None, start, BUCKET_REQUEST_BODY_BUDGET, clamped_w);
                (end, end - start)
            };

            // ── Per-window shrink-retry state machine (#177) ─────────────────
            // `attempt` = 0-based retry counter for THIS window.
            // `floor_failures` = consecutive failures at MIN_WINDOW floor.
            // `end` / `window_size` = range for the current attempt (may shrink).
            let mut attempt: usize = 0;
            let mut floor_failures: usize = 0;
            let mut end = initial_end;
            let mut window_size = initial_window_size;

            // Variables set on a successful attempt; used after the loop.
            let request_ms;
            let chunk_bytes;
            let retried;
            let eff_window; // effective window size that succeeded

            loop {
                // ── Emit RequestSent before each attempt ─────────────────
                let done = start; // buckets processed before this window
                observer.on_phase(PullPhase::RequestSent {
                    done,
                    total,
                    window: window_size,
                });

                // ── Per-attempt scratch buffer (§3.3) ────────────────────
                // Discarded on a retryable failure to prevent double-merge.
                let mut scratch = Snapshot {
                    version: PROTOCOL_VERSION,
                    cursor: u64::MAX,
                    tags: BTreeMap::new(),
                };
                let mut scratch_bytes: usize = 0;
                let merged_hashes = merged.tags.len();
                let merged_tags: usize = merged.tags.values().map(Vec::len).sum();

                let req_start = std::time::Instant::now();
                let result = if stream {
                    self.fetch_buckets_streaming(
                        &url,
                        prefix_bits,
                        buckets,
                        start,
                        end,
                        domain,
                        win_count,
                        &mut scratch,
                        &mut scratch_bytes,
                        cumulative_bytes,
                        merged_cap,
                        merged_hashes,
                        merged_tags,
                        observer,
                    )
                } else {
                    self.fetch_buckets_chunk_bisecting(
                        &url,
                        prefix_bits,
                        buckets,
                        start,
                        end,
                        domain,
                        win_count,
                        0, // total unknown upfront; 0 gives clean single-request error format
                        &mut scratch,
                        &mut scratch_bytes,
                        cumulative_bytes,
                        merged_cap,
                    )
                };

                match result {
                    Ok(()) => {
                        // Success: fold scratch into merged.
                        merged.cursor = merged.cursor.min(scratch.cursor);
                        for (hash, tags) in scratch.tags {
                            merged.tags.entry(hash).or_default().extend(tags);
                        }
                        cumulative_bytes = cumulative_bytes.saturating_add(scratch_bytes);
                        request_ms = req_start.elapsed().as_millis() as u64;
                        chunk_bytes = scratch_bytes;
                        retried = attempt > 0;
                        eff_window = window_size;
                        break; // → AIMD update then advance start
                    }
                    Err(WindowError::Fatal(e)) => {
                        // Fatal error: propagate immediately, abort the pull.
                        return Err(e);
                    }
                    Err(WindowError::Retryable(reason, e)) => {
                        if window_size <= MIN_WINDOW {
                            // Already at the floor; count floor-level failures.
                            floor_failures += 1;
                            observer.on_phase(PullPhase::WindowRetry {
                                done: start,
                                total,
                                old_window: window_size,
                                new_window: window_size,
                                attempt,
                                reason,
                            });
                            if floor_failures >= FLOOR_RETRY_LIMIT {
                                // Give up: name how far the pull got and the actual
                                // issued window (may be < MIN_WINDOW if budget-forced).
                                let tags_so_far = merged.tags.len();
                                return Err(anyhow!(
                                    "repo bucket window at prefix_bits {prefix_bits}: still \
                                     failing at the {window_size}-bucket floor after \
                                     {FLOOR_RETRY_LIMIT} attempts (pulled {start} of {total} \
                                     buckets, {tags_so_far} hashes so far); last error: {e}"
                                ));
                            }
                            // Retry the same floor-sized window after backoff.
                        } else {
                            // Shrink the window by half (down to MIN_WINDOW floor).
                            let w_new = (window_size / 2).max(MIN_WINDOW);
                            let old = window_size;
                            end =
                                window_end(buckets, None, start, BUCKET_REQUEST_BODY_BUDGET, w_new);
                            window_size = end - start;
                            observer.on_phase(PullPhase::WindowRetry {
                                done: start,
                                total,
                                old_window: old,
                                new_window: window_size,
                                attempt,
                                reason,
                            });
                        }
                        attempt += 1;
                        std::thread::sleep(retry_backoff(attempt));
                        // continue inner retry loop
                    }
                }
            }

            // ── ChunkReceived ────────────────────────────────────────────────
            let done_after = start + eff_window;
            let cumulative_hashes = merged.tags.len();
            let cumulative_tags: usize = merged.tags.values().map(Vec::len).sum();

            observer.on_phase(PullPhase::ChunkReceived {
                done: done_after,
                total,
                window: eff_window,
                chunk_bytes,
                cumulative_bytes,
                hashes: cumulative_hashes,
                tags: cumulative_tags,
                request_ms,
            });

            // ── AIMD feedback (#177 §3.4) ────────────────────────────────────
            // When a window succeeded only after shrinking (retried == true), pin
            // the next window at eff_window and suppress additive growth — the
            // next window starts at the size that worked, avoiding an immediate
            // re-issue of the original wide window that triggered the failure.
            if total > 0 {
                w = if retried {
                    eff_window.max(MIN_WINDOW)
                } else if request_ms > WINDOW_SLOW_MS {
                    // Multiplicative decrease from the ISSUED window, not from w.
                    (eff_window / 2).max(MIN_WINDOW)
                } else if request_ms < WINDOW_FAST_MS {
                    // Additive increase; saturating_add keeps usize::MAX stable.
                    w.saturating_add(MIN_WINDOW)
                } else {
                    w // in-band: hold
                };
            }

            win_count += 1;
            start = end; // `end` is the shrunk end if the window was retried

            if total == 0 || start >= total {
                break;
            }
        }

        let elapsed_ms = started.elapsed().as_millis() as u64;
        let tags: usize = merged.tags.values().map(Vec::len).sum();
        if win_count > 1 {
            tracing::info!(
                target: "sync",
                %url,
                prefix_bits,
                requested = buckets.len(),
                windows = win_count,
                hashes = merged.tags.len(),
                tags,
                elapsed_ms,
                "fetched buckets (chunked)"
            );
        } else {
            tracing::debug!(
                target: "sync",
                %url,
                prefix_bits,
                requested = buckets.len(),
                hashes = merged.tags.len(),
                tags,
                elapsed_ms,
                "fetched buckets"
            );
        }
        Ok(merged)
    }

    /// Read one NDJSON line from `reader`, guarded by [`MAX_STREAM_LINE_BYTES`].
    ///
    /// Returns `Ok(None)` on clean EOF (zero bytes read before any data),
    /// `Ok(Some(line))` for a complete line (newline stripped), or
    /// `Err(WindowError)` on failure:
    /// - `io::Error` from `read_until` → `Retryable` (`Timeout` when the io
    ///   error kind is `TimedOut`, `Disconnect` otherwise) — the connection
    ///   stalled mid-stream.
    /// - Line exceeds `MAX_STREAM_LINE_BYTES` → `Fatal` (protocol violation).
    /// - Non-UTF-8 bytes → `Fatal` (protocol violation).
    fn read_stream_line<'b>(
        reader: &mut impl std::io::BufRead,
        buf: &'b mut Vec<u8>,
    ) -> Result<Option<&'b str>, WindowError> {
        use std::io::BufRead as _;
        buf.clear();
        let n = reader
            .by_ref()
            .take((MAX_STREAM_LINE_BYTES + 1) as u64)
            .read_until(b'\n', buf)
            .map_err(|e| {
                let reason = if e.kind() == std::io::ErrorKind::TimedOut {
                    RetryReason::Timeout
                } else {
                    RetryReason::Disconnect
                };
                WindowError::Retryable(reason, anyhow!("reading stream line: {e}"))
            })?;
        if n == 0 {
            return Ok(None); // clean EOF before any bytes
        }
        // If we read MAX_STREAM_LINE_BYTES + 1 bytes and the last byte is not \n,
        // the line exceeded the cap.
        if n > MAX_STREAM_LINE_BYTES && !buf.ends_with(b"\n") {
            return Err(WindowError::Fatal(anyhow!(
                "stream line exceeds MAX_STREAM_LINE_BYTES ({} MiB); possible hostile server",
                MAX_STREAM_LINE_BYTES / (1024 * 1024)
            )));
        }
        // Strip trailing CRLF/LF.
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        std::str::from_utf8(buf)
            .map(Some)
            .map_err(|e| WindowError::Fatal(anyhow!("non-UTF-8 stream line: {e}")))
    }

    /// Fetch `buckets[lo..hi]` as a streaming NDJSON response (#176), looping
    /// over budget-cutoff continuation cursors until the server emits `done`.
    ///
    /// Each continuation request carries the same key slice plus the `resume_at`
    /// cursor echoed from the previous response's `more` trailer. Rows are merged
    /// into `scratch` as they arrive; a `RowReceived` phase is emitted per row.
    /// The outer [`fetch_buckets_inner`] still emits `ChunkReceived` once the
    /// window completes, so these are *additive* within-window ticks.
    ///
    /// `committed_bytes` is the already-committed running total from prior windows
    /// (read-only base for the aggregate-cap check); `scratch_bytes` accumulates
    /// this window's bytes in the scratch buffer. `merged_hashes`/`merged_tags`
    /// are the already-committed counts used to seed cumulative `RowReceived`
    /// ticks so they remain monotonic across the retry boundary (#177).
    ///
    /// [`MERGED_RESPONSE_SIZE_CAP`] is enforced cumulatively across all rows in
    /// all continuations. A stream that ends without a trailer line returns
    /// `Err(WindowError::Retryable)` so the outer retry loop can shrink and retry.
    ///
    /// Row/trailer decode is single-pass via [`StreamLine`]; precise
    /// diagnostics are reconstructed on the (always-fatal) error path via
    /// [`classify_bad_stream_line`].
    ///
    /// Error classification (§3.1):
    /// - `ureq::Error::Transport` → `Retryable`; `Status` → `Fatal`
    /// - io error from `read_stream_line` → `Retryable`; line-too-long/non-UTF8 → `Fatal`
    /// - stream truncated (EOF without trailer) / overall-deadline → `Retryable`
    /// - parse/protocol/cap/version errors → `Fatal`
    /// - in-band `{"err":…}` trailer → `Fatal`
    #[allow(clippy::too_many_arguments)]
    fn fetch_buckets_streaming(
        &self,
        url: &str,
        prefix_bits: u32,
        buckets: &[String],
        lo: usize,
        hi: usize,
        domain: Option<HashDomain>,
        win_count: usize,
        scratch: &mut Snapshot,
        scratch_bytes: &mut usize,
        committed_bytes: usize,
        merged_cap: usize,
        merged_hashes: usize,
        merged_tags: usize,
        observer: &dyn PullObserver,
    ) -> Result<(), WindowError> {
        let mut resume_at: Option<String> = None;
        // Seed running tags from scratch so within-attempt continuations are
        // cumulative; the outer wrapper resets scratch between attempts.
        let mut running_tags: usize =
            merged_tags + scratch.tags.values().map(Vec::len).sum::<usize>();
        // Overall deadline enforced in the loop (agent-level timeout is not set on
        // streaming_agent to avoid ureq's DeadlineStream collapsing timeout_read).
        let stream_deadline = std::time::Instant::now() + STREAM_OVERALL_TIMEOUT;

        loop {
            let req = BucketRequest {
                version: PROTOCOL_VERSION,
                prefix_bits,
                buckets: buckets[lo..hi].to_vec(),
                since: None,
                domain: domain.map(|d| d.to_string()),
                stream: true,
                resume_at: resume_at.clone(),
            };

            // §3.1: Transport error → Retryable; Status error → Fatal.
            let resp = self
                .streaming_agent
                .post(url)
                .send_json(&req)
                .map_err(|e| {
                    let we = classify_ureq("streaming buckets", url, e);
                    // Append window context; preserve the RetryReason.
                    match we {
                        WindowError::Retryable(r, e) => WindowError::Retryable(
                            r,
                            anyhow!("{e} (window {}, buckets {lo}..{hi})", win_count + 1),
                        ),
                        WindowError::Fatal(e) => WindowError::Fatal(anyhow!(
                            "{e} (window {}, buckets {lo}..{hi})",
                            win_count + 1
                        )),
                    }
                })?;

            // 64 KiB buffer: ~8× fewer read() syscalls / refills on gzip-inflated NDJSON
            // vs the default 8 KiB (perf-bench win).
            let mut reader = std::io::BufReader::with_capacity(64 * 1024, resp.into_reader());
            // Scratch buffer reused across all read_stream_line calls this window.
            let mut line_buf: Vec<u8> = Vec::with_capacity(4096);

            // ── Parse header ────────────────────────────────────────────────
            // io error reading header → Retryable; header absent (Ok(None)) → Retryable
            // (truncation before any response body).
            let header_line =
                Self::read_stream_line(&mut reader, &mut line_buf)?.ok_or_else(|| {
                    WindowError::Retryable(
                        RetryReason::Truncation,
                        anyhow!(
                            "streaming buckets from {url}: stream ended without header \
                             (window {}, buckets {lo}..{hi})",
                            win_count + 1
                        ),
                    )
                })?;
            // Bad header / server-error-as-first-line → Fatal (protocol violation).
            let header: StreamHeader = serde_json::from_str(header_line).map_err(|_| {
                // The server may have emitted an error trailer as its first line
                // (e.g. mapping_cursor() failure before the header was sent).
                if let Ok(StreamTrailer::Err { err: msg }) =
                    serde_json::from_str::<StreamTrailer>(header_line)
                {
                    WindowError::Fatal(anyhow!("streaming buckets from {url}: server error: {msg}"))
                } else {
                    WindowError::Fatal(anyhow!(
                        "streaming buckets from {url}: bad header line: {header_line:?}"
                    ))
                }
            })?;
            // Version mismatch → Fatal.
            ensure_supported(header.version).map_err(WindowError::Fatal)?;
            scratch.cursor = scratch.cursor.min(header.cursor);

            // ── Parse rows and exactly one trailer ──────────────────────────
            let mut found_trailer = false;
            loop {
                // io error reading a row line → Retryable; EOF at row position
                // is checked via found_trailer after the loop.
                let line = match Self::read_stream_line(&mut reader, &mut line_buf)? {
                    Some(l) => l,
                    None => break, // EOF — check found_trailer below
                };

                // Single-pass dispatch via untagged enum.
                // On parse failure reconstruct the precise "bad NDJSON / row / trailer"
                // message via classify_bad_stream_line (error path only — always Fatal).
                let sl: StreamLine = match serde_json::from_str::<StreamLine>(line) {
                    Ok(sl) => sl,
                    Err(_) => return Err(classify_bad_stream_line(url, line)),
                };

                match sl {
                    StreamLine::Row(row) => {
                        let row_bytes = line.len();
                        *scratch_bytes = scratch_bytes.saturating_add(row_bytes);
                        // Aggregate cap check uses committed base + scratch so far.
                        if committed_bytes.saturating_add(*scratch_bytes) > merged_cap {
                            let gib = merged_cap / (1024 * 1024 * 1024);
                            return Err(WindowError::Fatal(anyhow!(
                                "streaming buckets from {url}: merged response exceeds \
                                 MERGED_RESPONSE_SIZE_CAP ({gib} GiB)"
                            )));
                        }
                        let tag_count = row.t.len();
                        scratch.tags.entry(row.h).or_default().extend(row.t);
                        let hashes = merged_hashes + scratch.tags.len();
                        running_tags += tag_count;
                        observer.on_phase(PullPhase::RowReceived {
                            hashes,
                            tags: running_tags,
                        });
                        // Overall deadline exceeded → Retryable(Timeout).
                        if std::time::Instant::now() > stream_deadline {
                            return Err(WindowError::Retryable(
                                RetryReason::Timeout,
                                anyhow!(
                                    "streaming buckets from {url}: stream exceeded overall deadline"
                                ),
                            ));
                        }
                    }
                    StreamLine::Trailer(trailer) => {
                        found_trailer = true;
                        match trailer {
                            StreamTrailer::Done { done: true } => return Ok(()),
                            StreamTrailer::Done { done: false } => {
                                return Err(WindowError::Fatal(anyhow!(
                                    "streaming buckets from {url}: server sent done:false \
                                     (protocol error)"
                                )));
                            }
                            StreamTrailer::More { more: k } => {
                                resume_at = Some(k);
                                // Overall deadline exceeded after a More → Retryable(Timeout).
                                if std::time::Instant::now() > stream_deadline {
                                    return Err(WindowError::Retryable(
                                        RetryReason::Timeout,
                                        anyhow!(
                                            "streaming buckets from {url}: stream exceeded overall deadline"
                                        ),
                                    ));
                                }
                                break; // break inner loop; outer loop sends continuation
                            }
                            // In-band server error → Fatal (#178 single-oversized-bucket
                            // case; shrink cannot help).
                            StreamTrailer::Err { err: m } => {
                                return Err(WindowError::Fatal(anyhow!(
                                    "repo stream error from {url}: {m}"
                                )));
                            }
                        }
                    }
                }
            }

            if !found_trailer {
                // EOF without a trailer → stream truncation → Retryable(Truncation).
                return Err(WindowError::Retryable(
                    RetryReason::Truncation,
                    anyhow!(
                        "streaming buckets from {url}: stream truncated — EOF without trailer \
                         (window {}, buckets {lo}..{hi})",
                        win_count + 1
                    ),
                ));
            }
            // found_trailer=true and it was More → continue outer loop.
        }
    }

    /// Fetch `buckets[lo..hi]` in one request; on a `413 Payload Too Large`
    /// (the server's #145 response-budget refusal) split the range at its
    /// midpoint and fetch the halves serially, recursing until each piece fits.
    /// A single key that still 413s is a hard error — that one bucket is larger
    /// than the server will ever emit in one response. `prefix_bits` is never
    /// touched: it is the caller's k-anonymity ceiling. Every fetched piece is
    /// merged into `scratch` and counted against `merged_cap` (#154).
    ///
    /// `committed_bytes` is the already-committed running total from prior
    /// windows (read-only base); `scratch_bytes` accumulates this window's bytes.
    ///
    /// Error classification (§3.1):
    /// - `ureq::Error::Transport` → `Retryable`; `Status(non-413)` → `Fatal`
    /// - 413 → handled in-place by bisection; lone-key 413 → `Fatal`
    /// - decode / version / cap errors → `Fatal`
    #[allow(clippy::too_many_arguments)]
    fn fetch_buckets_chunk_bisecting(
        &self,
        url: &str,
        prefix_bits: u32,
        buckets: &[String],
        lo: usize,
        hi: usize,
        domain: Option<HashDomain>,
        n: usize,
        total: usize,
        scratch: &mut Snapshot,
        scratch_bytes: &mut usize,
        committed_bytes: usize,
        merged_cap: usize,
    ) -> Result<(), WindowError> {
        let req = BucketRequest {
            version: PROTOCOL_VERSION,
            prefix_bits,
            buckets: buckets[lo..hi].to_vec(),
            since: None,
            domain: domain.map(|d| d.to_string()),
            stream: false,
            resume_at: None,
        };
        match self.agent.post(url).send_json(&req) {
            Ok(resp) => {
                // Decode and version-check → Fatal on parse error.
                let (snapshot, body_len): (Snapshot, usize) =
                    read_capped_counted(resp, RESPONSE_SIZE_CAP).map_err(|e| {
                        WindowError::Fatal(chunk_err("decoding buckets", url, n, total, lo, hi, &e))
                    })?;
                // Version mismatch → Fatal.
                ensure_supported(snapshot.version).map_err(WindowError::Fatal)?;
                *scratch_bytes = scratch_bytes.saturating_add(body_len);
                // Aggregate cap check uses committed base + scratch so far.
                if committed_bytes.saturating_add(*scratch_bytes) > merged_cap {
                    let gib = merged_cap / (1024 * 1024 * 1024);
                    return Err(WindowError::Fatal(if total > 1 {
                        anyhow!(
                            "fetching buckets from {url}: merged response exceeds \
                             MERGED_RESPONSE_SIZE_CAP ({gib} GiB) after chunk {} of {}",
                            n + 1,
                            total
                        )
                    } else if total == 0 {
                        anyhow!(
                            "fetching buckets from {url}: merged response exceeds \
                             MERGED_RESPONSE_SIZE_CAP ({gib} GiB) \
                             (window {}, buckets {lo}..{hi})",
                            n + 1
                        )
                    } else {
                        anyhow!(
                            "fetching buckets from {url}: merged response exceeds \
                             MERGED_RESPONSE_SIZE_CAP ({gib} GiB)"
                        )
                    }));
                }
                let chunk_tags: usize = snapshot.tags.values().map(Vec::len).sum();
                tracing::debug!(
                    target: "sync",
                    %url,
                    prefix_bits,
                    chunk = n + 1,
                    total,
                    requested = hi - lo,
                    hashes = snapshot.tags.len(),
                    tags = chunk_tags,
                    body_len,
                    cumulative_bytes = committed_bytes.saturating_add(*scratch_bytes),
                    "fetched bucket chunk"
                );
                scratch.cursor = scratch.cursor.min(snapshot.cursor);
                for (hash, tags) in snapshot.tags {
                    scratch.tags.entry(hash).or_default().extend(tags);
                }
                Ok(())
            }
            // 413 → handled in-place by bisection; never surfaced as Retryable
            // to the outer retry loop — a 413 is a budget refusal, not a stall.
            Err(ureq::Error::Status(413, _)) => {
                if hi - lo <= 1 {
                    let key = buckets.get(lo).map_or("<none>", |k| k.as_str());
                    return Err(WindowError::Fatal(anyhow!(
                        "repo bucket {key:?} at {prefix_bits} bits exceeds the server's \
                         per-request response budget; the repo operator must raise the \
                         query precision floor or the client must query at finer \
                         prefix_bits (privacy ceiling permitting)"
                    )));
                }
                let mid = lo + (hi - lo) / 2;
                self.fetch_buckets_chunk_bisecting(
                    url,
                    prefix_bits,
                    buckets,
                    lo,
                    mid,
                    domain,
                    n,
                    total,
                    scratch,
                    scratch_bytes,
                    committed_bytes,
                    merged_cap,
                )?;
                self.fetch_buckets_chunk_bisecting(
                    url,
                    prefix_bits,
                    buckets,
                    mid,
                    hi,
                    domain,
                    n,
                    total,
                    scratch,
                    scratch_bytes,
                    committed_bytes,
                    merged_cap,
                )
            }
            // Non-413 status errors → Fatal; Transport errors → Retryable.
            Err(e) => {
                let we = classify_ureq("fetching buckets", url, e);
                Err(match we {
                    WindowError::Retryable(r, base) => WindowError::Retryable(
                        r,
                        if total > 1 {
                            anyhow!("{base} (chunk {} of {})", n + 1, total)
                        } else if total == 0 {
                            anyhow!("{base} (window {}, buckets {lo}..{hi})", n + 1)
                        } else {
                            base
                        },
                    ),
                    WindowError::Fatal(base) => WindowError::Fatal(if total > 1 {
                        anyhow!("{base} (chunk {} of {})", n + 1, total)
                    } else if total == 0 {
                        anyhow!("{base} (window {}, buckets {lo}..{hi})", n + 1)
                    } else {
                        base
                    }),
                })
            }
        }
    }

    /// Fetch incremental mapping deltas for `buckets`, keyed by index-aligned
    /// cursors, in the repo's default hash domain.
    ///
    /// # Errors
    /// Returns an error if the request fails, the body is undecodable, or the
    /// wire version is unsupported.
    pub fn fetch_bucket_delta(
        &self,
        prefix_bits: u32,
        buckets: &[String],
        since: &[u64],
    ) -> Result<MappingDelta> {
        self.fetch_bucket_delta_in(prefix_bits, buckets, since, None)
    }

    /// Fetch incremental mapping deltas for `buckets` in `domain`.
    ///
    /// `None` sends no `domain` field at all, producing a request body
    /// byte-identical to the one a pre-dual-domain client sends — use it against
    /// any repo whose caps omitted `hash_domains` (see [`Caps::wire_domain`]).
    ///
    /// Chunked exactly like [`RepoClient::fetch_buckets_in`] (#146): `buckets`
    /// and `since` are split in lockstep so every request keeps the
    /// index-aligned pairing the server validates, the `changes` are
    /// concatenated, and the merged `cursor` is the minimum across replies for
    /// the same reason.  The cumulative aggregate is additionally bounded by
    /// [`MERGED_RESPONSE_SIZE_CAP`] (#154).
    /// On a 413 the client bisects the chunk's key list — slicing `since` by the
    /// same range so the index-aligned pairing survives — and fetches the halves
    /// serially; a single bucket that alone 413s is a hard error (#145).
    ///
    /// # Errors
    /// Returns an error if the request fails, the body is undecodable, or the
    /// wire version is unsupported.
    pub fn fetch_bucket_delta_in(
        &self,
        prefix_bits: u32,
        buckets: &[String],
        since: &[u64],
        domain: Option<HashDomain>,
    ) -> Result<MappingDelta> {
        self.fetch_bucket_delta_inner(
            prefix_bits,
            buckets,
            since,
            domain,
            MERGED_RESPONSE_SIZE_CAP,
        )
    }

    /// Inner implementation of [`fetch_bucket_delta_in`] with an explicit
    /// `merged_cap` so unit tests can exercise the aggregate-size guard without
    /// generating gigabytes of fake response data.
    fn fetch_bucket_delta_inner(
        &self,
        prefix_bits: u32,
        buckets: &[String],
        since: &[u64],
        domain: Option<HashDomain>,
        merged_cap: usize,
    ) -> Result<MappingDelta> {
        let url = format!("{}{}", self.base, REPO_BUCKETS);
        let started = std::time::Instant::now();
        let chunks = bucket_chunks(buckets, Some(since), BUCKET_REQUEST_BODY_BUDGET);
        let mut merged = MappingDelta {
            version: PROTOCOL_VERSION,
            cursor: u64::MAX,
            changes: Vec::new(),
        };
        let mut cumulative_bytes: usize = 0;
        for (n, (lo, hi)) in chunks.iter().enumerate() {
            self.fetch_bucket_delta_chunk_bisecting(
                &url,
                prefix_bits,
                buckets,
                since,
                *lo,
                *hi,
                domain,
                n,
                chunks.len(),
                &mut merged,
                &mut cumulative_bytes,
                merged_cap,
            )?;
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if chunks.len() > 1 {
            tracing::info!(target: "sync", %url, prefix_bits, since = since.len(), chunks = chunks.len(), changes = merged.changes.len(), cursor = merged.cursor, elapsed_ms, "fetched bucket delta (chunked)");
        } else {
            tracing::debug!(target: "sync", %url, prefix_bits, since = since.len(), changes = merged.changes.len(), cursor = merged.cursor, elapsed_ms, "fetched bucket delta");
        }
        Ok(merged)
    }

    /// Delta twin of [`RepoClient::fetch_buckets_chunk_bisecting`]. Slices
    /// `since` by the same `[lo, hi)` it slices `buckets`, so the index-aligned
    /// pairing the server validates survives every split. Same 413 → bisect,
    /// same single-key hard error, same aggregate accounting (#145 / #154).
    #[allow(clippy::too_many_arguments)]
    fn fetch_bucket_delta_chunk_bisecting(
        &self,
        url: &str,
        prefix_bits: u32,
        buckets: &[String],
        since: &[u64],
        lo: usize,
        hi: usize,
        domain: Option<HashDomain>,
        n: usize,
        total: usize,
        merged: &mut MappingDelta,
        cumulative_bytes: &mut usize,
        merged_cap: usize,
    ) -> Result<()> {
        let req = BucketRequest {
            version: PROTOCOL_VERSION,
            prefix_bits,
            buckets: buckets[lo..hi].to_vec(),
            // Slice `since` by the same range. A caller that passed a mismatched
            // length keeps getting the server's 400 rather than panicking here.
            since: Some(since.get(lo..hi).unwrap_or(since).to_vec()),
            domain: domain.map(|d| d.to_string()),
            stream: false,
            resume_at: None,
        };
        match self.agent.post(url).send_json(&req) {
            Ok(resp) => {
                let (delta, body_len): (MappingDelta, usize) =
                    read_capped_counted(resp, RESPONSE_SIZE_CAP).map_err(|e| {
                        chunk_err("decoding bucket delta", url, n, total, lo, hi, &e)
                    })?;
                ensure_supported(delta.version)?;
                *cumulative_bytes = cumulative_bytes.saturating_add(body_len);
                if *cumulative_bytes > merged_cap {
                    let gib = merged_cap / (1024 * 1024 * 1024);
                    return Err(anyhow!(
                        "fetching bucket delta from {url}: merged response exceeds \
                         MERGED_RESPONSE_SIZE_CAP ({gib} GiB) after chunk {} of {}",
                        n + 1,
                        total
                    ));
                }
                tracing::debug!(target: "sync", %url, prefix_bits, chunk = n + 1, total, requested = hi - lo, changes = delta.changes.len(), cursor = delta.cursor, body_len, cumulative_bytes, "fetched bucket delta chunk");
                merged.cursor = merged.cursor.min(delta.cursor);
                merged.changes.extend(delta.changes);
                Ok(())
            }
            Err(ureq::Error::Status(413, _)) => {
                if hi - lo <= 1 {
                    let key = buckets.get(lo).map_or("<none>", |k| k.as_str());
                    return Err(anyhow!(
                        "repo bucket {key:?} at {prefix_bits} bits exceeds the server's \
                         per-request response budget; the repo operator must raise the \
                         query precision floor or the client must query at finer \
                         prefix_bits (privacy ceiling permitting)"
                    ));
                }
                let mid = lo + (hi - lo) / 2;
                self.fetch_bucket_delta_chunk_bisecting(
                    url,
                    prefix_bits,
                    buckets,
                    since,
                    lo,
                    mid,
                    domain,
                    n,
                    total,
                    merged,
                    cumulative_bytes,
                    merged_cap,
                )?;
                self.fetch_bucket_delta_chunk_bisecting(
                    url,
                    prefix_bits,
                    buckets,
                    since,
                    mid,
                    hi,
                    domain,
                    n,
                    total,
                    merged,
                    cumulative_bytes,
                    merged_cap,
                )
            }
            Err(e) => {
                let base = status_err("fetching bucket delta", url, e);
                if total > 1 {
                    Err(anyhow!("{base} (chunk {} of {})", n + 1, total))
                } else {
                    Err(base)
                }
            }
        }
    }

    /// Submit one signed operation to the repository. The request is
    /// additionally authenticated via the `x-naiad-*` auth headers so the
    /// server can auto-create the account (v6 requirement).
    ///
    /// # Errors
    /// Returns an error if the request fails or the repo rejects it (non-2xx);
    /// the repo's plain-text reason is included.
    pub fn submit(&self, account: &Account, sub: &Submission) -> Result<()> {
        let url = format!("{}{}", self.base, REPO_SUBMIT);
        let ts = now_secs();
        let body = serde_json::to_vec(sub).map_err(|e| anyhow!("serialising submission: {e}"))?;
        let sig = account.sign_auth("POST", REPO_SUBMIT, NO_DOMAIN, ts, &body);
        match self
            .agent
            .post(&url)
            .set(HDR_AUTH_KEY, &account.public_hex())
            .set(HDR_AUTH_TS, &ts.to_string())
            .set(HDR_AUTH_SIG, &sig)
            .set("content-type", "application/json")
            .send_bytes(&body)
        {
            Ok(_) => {
                tracing::debug!(target: "sync", %url, "submission accepted");
                Ok(())
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                tracing::warn!(target: "sync", %url, code, reason = %body.trim(), "repo rejected submission");
                Err(anyhow!(
                    "repo rejected submission ({code}): {}",
                    body.trim()
                ))
            }
            Err(e) => {
                tracing::warn!(target: "sync", %url, error = %e, "repo unreachable: submission not sent");
                Err(anyhow!("submitting to {url}: {e}"))
            }
        }
    }

    /// Submit one signed relation operation to the repository.
    ///
    /// # Errors
    /// Returns an error if the request fails or the repo rejects it (non-2xx);
    /// the repo's plain-text reason is included.
    pub fn submit_relation(&self, sub: &RelationSubmission) -> Result<()> {
        let url = format!("{}{}", self.base, REPO_RELATIONS_SUBMIT);
        match self.agent.post(&url).send_json(sub) {
            Ok(_) => {
                tracing::debug!(target: "sync", %url, "relation submission accepted");
                Ok(())
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                tracing::warn!(target: "sync", %url, code, reason = %body.trim(), "repo rejected relation");
                Err(anyhow!("repo rejected relation ({code}): {}", body.trim()))
            }
            Err(e) => {
                tracing::warn!(target: "sync", %url, error = %e, "repo unreachable: relation submission not sent");
                Err(anyhow!("submitting relation to {url}: {e}"))
            }
        }
    }

    /// Fetch the repository's whole relation graph. A `404` (a pre-relations
    /// repo) maps to an **empty graph**, not an error — graceful degradation.
    ///
    /// # Errors
    /// Returns an error if the request fails (other than 404), the body is
    /// undecodable, or the wire version is unsupported.
    pub fn fetch_relations(&self) -> Result<RelationGraph> {
        let url = format!("{}{}", self.base, REPO_RELATIONS);
        let started = std::time::Instant::now();
        match self.agent.get(&url).call() {
            Ok(resp) => {
                let graph: RelationGraph = read_capped(resp, RESPONSE_SIZE_CAP)
                    .map_err(|e| anyhow!("decoding relations from {url}: {e}"))?;
                ensure_supported(graph.version)?;
                tracing::debug!(target: "sync", %url, siblings = graph.siblings.len(), parents = graph.parents.len(), elapsed_ms = started.elapsed().as_millis() as u64, "fetched relation graph");
                Ok(graph)
            }
            Err(ureq::Error::Status(404, _)) => {
                tracing::debug!(target: "sync", %url, "relations endpoint 404 → empty graph (pre-relations repo)");
                Ok(RelationGraph {
                    version: PROTOCOL_VERSION,
                    cursor: 0,
                    siblings: Vec::new(),
                    parents: Vec::new(),
                })
            }
            Err(e) => {
                tracing::warn!(target: "sync", %url, error = %e, "repo unreachable: relation graph fetch failed");
                Err(anyhow!("fetching relations from {url}: {e}"))
            }
        }
    }

    /// Fetch the incremental relation delta since cursor `since` (`?since=N`):
    /// every edge with `seq > since`, tombstones included, plus the repo's new
    /// high-watermark. `since = 0` returns the full set. A `404` (a pre-relations
    /// repo) maps to an **empty delta** with cursor 0, not an error.
    ///
    /// # Errors
    /// Returns an error if the request fails (other than 404), the body is
    /// undecodable, or the wire version is unsupported.
    pub fn fetch_relations_since(&self, since: u64) -> Result<RelationDelta> {
        let url = format!("{}{}?since={since}", self.base, REPO_RELATIONS);
        let started = std::time::Instant::now();
        match self.agent.get(&url).call() {
            Ok(resp) => {
                let delta: RelationDelta = read_capped(resp, RESPONSE_SIZE_CAP)
                    .map_err(|e| anyhow!("decoding relation delta from {url}: {e}"))?;
                ensure_supported(delta.version)?;
                tracing::debug!(target: "sync", %url, since, edges = delta.edges.len(), cursor = delta.cursor, elapsed_ms = started.elapsed().as_millis() as u64, "fetched relation delta");
                Ok(delta)
            }
            Err(ureq::Error::Status(404, _)) => {
                tracing::debug!(target: "sync", %url, "relation delta 404 → empty delta (pre-relations repo)");
                Ok(RelationDelta {
                    version: PROTOCOL_VERSION,
                    cursor: 0,
                    edges: Vec::new(),
                })
            }
            Err(e) => {
                tracing::warn!(target: "sync", %url, error = %e, "repo unreachable: relation delta fetch failed");
                Err(anyhow!("fetching relation delta from {url}: {e}"))
            }
        }
    }

    /// File a report against `(hash, tag)`. The request is authenticated via the
    /// `x-naiad-*` auth headers so the server can identify (and possibly ban)
    /// the reporter.
    ///
    /// # Errors
    /// Returns an error if the request fails or the repo rejects it (non-2xx).
    pub fn report(&self, account: &Account, r: &Report) -> Result<()> {
        let url = format!("{}{}", self.base, REPO_REPORT);
        let ts = now_secs();
        let body = serde_json::to_vec(r).map_err(|e| anyhow!("serialising report: {e}"))?;
        let sig = account.sign_auth("POST", REPO_REPORT, NO_DOMAIN, ts, &body);
        match self
            .agent
            .post(&url)
            .set(HDR_AUTH_KEY, &account.public_hex())
            .set(HDR_AUTH_TS, &ts.to_string())
            .set(HDR_AUTH_SIG, &sig)
            .set("content-type", "application/json")
            .send_bytes(&body)
        {
            Ok(_) => {
                tracing::debug!(target: "sync", %url, "report accepted");
                Ok(())
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                tracing::warn!(target: "sync", %url, code, reason = %body.trim(), "repo rejected report");
                Err(anyhow!("repo rejected report ({code}): {}", body.trim()))
            }
            Err(e) => {
                tracing::warn!(target: "sync", %url, error = %e, "repo unreachable: report not sent");
                Err(anyhow!("submitting report to {url}: {e}"))
            }
        }
    }

    /// Fetch the open report queue (moderator-only endpoint). The request is
    /// authenticated via the `x-naiad-*` auth headers; GET uses an empty body.
    ///
    /// # Errors
    /// Returns an error if the request fails, the body is undecodable, or the
    /// wire version is unsupported.
    pub fn fetch_reports(&self, account: &Account) -> Result<ReportList> {
        let url = format!("{}{}", self.base, REPO_REPORTS);
        let started = std::time::Instant::now();
        let ts = now_secs();
        let sig = account.sign_auth("GET", REPO_REPORTS, NO_DOMAIN, ts, b"");
        let resp = match self
            .agent
            .get(&url)
            .set(HDR_AUTH_KEY, &account.public_hex())
            .set(HDR_AUTH_TS, &ts.to_string())
            .set(HDR_AUTH_SIG, &sig)
            .call()
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, resp)) => {
                let reason = resp.into_string().unwrap_or_default();
                tracing::warn!(target: "sync", %url, code, reason = %reason.trim(), "repo rejected report fetch");
                return Err(anyhow!(
                    "repo rejected report fetch ({code}): {}",
                    reason.trim()
                ));
            }
            Err(e) => {
                tracing::warn!(target: "sync", %url, error = %e, "repo unreachable: report queue fetch failed");
                return Err(anyhow!("fetching reports from {url}: {e}"));
            }
        };
        let list: ReportList = read_capped(resp, RESPONSE_SIZE_CAP)
            .map_err(|e| anyhow!("decoding reports from {url}: {e}"))?;
        ensure_supported(list.version)?;
        tracing::debug!(target: "sync", %url, rows = list.rows.len(), elapsed_ms = started.elapsed().as_millis() as u64, "fetched report queue");
        Ok(list)
    }

    /// Post a moderator action (`DeleteMapping`, `Ban`, or `Dismiss`). The
    /// request is authenticated via the `x-naiad-*` auth headers.
    ///
    /// # Errors
    /// Returns an error if the request fails or the repo rejects it (non-2xx).
    pub fn moderate(&self, account: &Account, action: &ModerateAction) -> Result<()> {
        let url = format!("{}{}", self.base, REPO_MODERATE);
        let ts = now_secs();
        let body = serde_json::to_vec(action).map_err(|e| anyhow!("serialising action: {e}"))?;
        let sig = account.sign_auth("POST", REPO_MODERATE, NO_DOMAIN, ts, &body);
        match self
            .agent
            .post(&url)
            .set(HDR_AUTH_KEY, &account.public_hex())
            .set(HDR_AUTH_TS, &ts.to_string())
            .set(HDR_AUTH_SIG, &sig)
            .set("content-type", "application/json")
            .send_bytes(&body)
        {
            Ok(_) => {
                tracing::debug!(target: "sync", %url, "moderation accepted");
                Ok(())
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                tracing::warn!(target: "sync", %url, code, reason = %body.trim(), "repo rejected moderation");
                Err(anyhow!(
                    "repo rejected moderation ({code}): {}",
                    body.trim()
                ))
            }
            Err(e) => {
                tracing::warn!(target: "sync", %url, error = %e, "repo unreachable: moderation action not sent");
                Err(anyhow!("moderating at {url}: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v8_protocol_version_and_v7_rejected() {
        // Hard cutover: both constants must be 8.
        // v7 signed a submission frame without the origin field; v8 rejects it.
        assert_eq!(PROTOCOL_VERSION, 8);
        assert_eq!(MIN_SUPPORTED_VERSION, 8);
        assert!(ensure_supported(7).is_err());
        assert!(ensure_supported(6).is_err());
        assert!(ensure_supported(8).is_ok());
    }

    #[test]
    fn ensure_supported_accepts_current_rejects_other() {
        assert!(ensure_supported(PROTOCOL_VERSION).is_ok());
        assert!(ensure_supported(PROTOCOL_VERSION + 1).is_err());
    }

    #[test]
    fn ensure_supported_accepts_the_supported_range_and_rejects_outside() {
        assert!(ensure_supported(PROTOCOL_VERSION).is_ok());
        assert!(ensure_supported(MIN_SUPPORTED_VERSION).is_ok());
        assert!(ensure_supported(PROTOCOL_VERSION + 1).is_err());
        if MIN_SUPPORTED_VERSION > 0 {
            assert!(ensure_supported(MIN_SUPPORTED_VERSION - 1).is_err());
        }
    }

    #[test]
    fn snapshot_plain_tags_round_trip() {
        // hash → list of OriginTag; origin omitted when manual (the common case).
        let mut tags = BTreeMap::new();
        tags.insert(
            "0a".repeat(32),
            vec![
                OriginTag {
                    tag: "character:samus".to_string(),
                    origin: None,
                },
                OriginTag {
                    tag: "series:metroid".to_string(),
                    origin: None,
                },
            ],
        );
        let s = Snapshot {
            version: PROTOCOL_VERSION,
            cursor: 42,
            tags,
        };
        let json = serde_json::to_string(&s).unwrap();
        // Must not contain any supporter-summary fields from the old (pre-pivot) v5 shape.
        assert!(!json.contains("supporters"));
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn snapshot_cursor_defaults_zero() {
        let json = format!(
            r#"{{"version":6,"tags":{{"{}":[ {{"tag":"a:b"}} ]}}}}"#,
            "ab".repeat(32)
        );
        let s: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn op_serde_matches_as_str() {
        // The signed-bytes token (as_str) and the JSON form must stay identical.
        for op in [Op::Add, Op::Remove] {
            assert_eq!(
                serde_json::to_value(op).unwrap(),
                serde_json::json!(op.as_str())
            );
        }
    }

    #[test]
    fn submission_frames_origin_and_round_trips() {
        let acct = Account::generate();
        let sub = acct.sign_with_origin(
            Op::Add,
            &naiad_core::hash_bytes(b"f"),
            &naiad_core::Tag::parse("a:b").unwrap(),
            Some("wd14-tagger"),
        );
        let json = serde_json::to_string(&sub).unwrap();
        assert!(
            json.contains("\"origin\""),
            "origin present when Some: {json}"
        );
        let back: Submission = serde_json::from_str(&json).unwrap();
        assert_eq!(sub, back);
        verify(&back).unwrap();
        // Tampering the asserted origin breaks verification (the signature
        // commits to origin).
        let mut tampered = back.clone();
        tampered.origin = Some("gelbooru".to_string());
        assert!(
            verify(&tampered).is_err(),
            "tampered origin (Some→different) must fail verify"
        );
        // Some→None must also fail: removing the origin changes the canonical frame.
        let mut tampered_none = back.clone();
        tampered_none.origin = None;
        assert!(
            verify(&tampered_none).is_err(),
            "tampered origin (Some→None) must fail verify"
        );
    }

    #[test]
    fn submission_manual_omits_origin() {
        let acct = Account::generate();
        let sub = acct.sign(
            Op::Add,
            &naiad_core::hash_bytes(b"f"),
            &naiad_core::Tag::parse("a:b").unwrap(),
        );
        assert_eq!(sub.origin, None);
        let json = serde_json::to_string(&sub).unwrap();
        assert!(
            !json.contains("\"origin\""),
            "origin omitted when None: {json}"
        );
        let back: Submission = serde_json::from_str(&json).unwrap();
        assert_eq!(sub, back);
        verify(&back).unwrap();
        // None→Some must fail: injecting an origin changes the canonical frame.
        let mut tampered = back.clone();
        tampered.origin = Some("wd14-tagger".to_string());
        assert!(
            verify(&tampered).is_err(),
            "tampered origin (None→Some) must fail verify"
        );
    }

    #[test]
    fn caps_reports_field_round_trips() {
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: bucket::PullMode::WholeRepo,
            relation_incremental: false,
            mapping_incremental: false,
            reports: true,
            repo_key: Some("ab".repeat(32)),
            hash_domain: HashDomain::Blake3,
            hash_domains: Vec::new(),
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains("reports"));
        let back: Caps = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);
    }

    #[test]
    fn caps_name_round_trips_and_absent_means_none() {
        // Fixture copied from caps_reports_field_round_trips, with name: Some("NOS".into()).
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: bucket::PullMode::WholeRepo,
            relation_incremental: false,
            mapping_incremental: false,
            reports: true,
            repo_key: Some("ab".repeat(32)),
            hash_domain: HashDomain::Blake3,
            hash_domains: Vec::new(),
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: Some("NOS".into()),
        };
        // The field must appear in the serialised JSON and round-trip.
        let json = serde_json::to_string(&caps).unwrap();
        assert!(
            json.contains("\"name\":\"NOS\""),
            "name must be present in JSON when Some: {json}"
        );
        let back: Caps = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);

        // Absent on the wire (e.g. an older server) → None.
        let no_name: Caps = serde_json::from_str(r#"{"version":6,"mode":"wholerepo"}"#).unwrap();
        assert_eq!(no_name.name, None, "absent name must parse as None");

        // name: None must not appear in the serialised output at all.
        let caps_no_name = Caps {
            name: None,
            ..caps.clone()
        };
        let json_no_name = serde_json::to_string(&caps_no_name).unwrap();
        assert!(
            !json_no_name.contains("\"name\""),
            "name must be absent from JSON when None: {json_no_name}"
        );
    }

    #[test]
    fn caps_reports_defaults_false_when_absent() {
        let c: Caps = serde_json::from_str(r#"{"version":6,"mode":"wholerepo"}"#).unwrap();
        assert!(!c.reports);
    }

    #[test]
    fn report_round_trips() {
        let r = Report {
            version: PROTOCOL_VERSION,
            hash: "ab".repeat(32),
            tag: "character:samus".to_string(),
            note: Some("spam".to_string()),
        };
        let back: Report = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);

        // note is optional — omitted when None.
        let r2 = Report {
            version: PROTOCOL_VERSION,
            hash: "ab".repeat(32),
            tag: "character:samus".to_string(),
            note: None,
        };
        assert!(!serde_json::to_string(&r2).unwrap().contains("note"));
        let back2: Report = serde_json::from_str(&serde_json::to_string(&r2).unwrap()).unwrap();
        assert_eq!(r2, back2);
    }

    #[test]
    fn report_list_round_trips() {
        let list = ReportList {
            version: PROTOCOL_VERSION,
            rows: vec![ReportRow {
                id: 1,
                hash: "ab".repeat(32),
                tag: "character:samus".to_string(),
                reporter: "cd".repeat(32),
                note: None,
                created_at: 1_700_000_000,
                status: "open".to_string(),
            }],
        };
        let back: ReportList =
            serde_json::from_str(&serde_json::to_string(&list).unwrap()).unwrap();
        assert_eq!(list, back);

        // note: Some(...) round-trip — the field must survive serialization.
        let list_with_note = ReportList {
            version: PROTOCOL_VERSION,
            rows: vec![ReportRow {
                id: 2,
                hash: "cd".repeat(32),
                tag: "series:metroid".to_string(),
                reporter: "ef".repeat(32),
                note: Some("contains spoilers".to_string()),
                created_at: 1_700_000_001,
                status: "resolved".to_string(),
            }],
        };
        let json = serde_json::to_string(&list_with_note).unwrap();
        assert!(
            json.contains("note"),
            "note field must be present when Some"
        );
        let back_note: ReportList = serde_json::from_str(&json).unwrap();
        assert_eq!(list_with_note, back_note);
    }

    #[test]
    fn moderate_action_serde_round_trips() {
        // DeleteMapping
        let a = ModerateAction::DeleteMapping {
            hash: "ab".repeat(32),
            tag: "a:b".to_string(),
        };
        let back: ModerateAction =
            serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(a, back);

        // Ban
        let b = ModerateAction::Ban {
            pubkey: "cd".repeat(32),
        };
        let back: ModerateAction =
            serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(b, back);

        // Dismiss
        let d = ModerateAction::Dismiss { report_id: 42 };
        let back: ModerateAction =
            serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn moderate_action_tag_is_snake_case() {
        // The wire `action` tag must be snake_case (delete_mapping, not DeleteMapping).
        let a = ModerateAction::DeleteMapping {
            hash: "ab".repeat(32),
            tag: "a:b".to_string(),
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains(r#""action":"delete_mapping""#));

        let b = ModerateAction::Ban {
            pubkey: "cd".repeat(32),
        };
        assert!(
            serde_json::to_string(&b)
                .unwrap()
                .contains(r#""action":"ban""#)
        );

        let d = ModerateAction::Dismiss { report_id: 1 };
        assert!(
            serde_json::to_string(&d)
                .unwrap()
                .contains(r#""action":"dismiss""#)
        );
    }

    // ── response-size cap tests ───────────────────────────────────────────────

    /// Build a synthetic `ureq::Response` from raw bytes so we can drive
    /// `read_capped` without spinning up an HTTP server.
    fn fake_response(body: &[u8]) -> ureq::Response {
        // ureq::Response::new(status, status_text, body_str) is the public
        // constructor available in ureq 2.x for testing.
        ureq::Response::new(200, "OK", std::str::from_utf8(body).unwrap()).unwrap()
    }

    #[test]
    fn read_capped_accepts_under_cap() {
        let json = br#"{"version":6,"cursor":0,"tags":{}}"#;
        // Cap is much larger than the body — must succeed.
        let result: Result<Snapshot> = read_capped(fake_response(json), 1024);
        assert!(result.is_ok(), "expected ok, got {result:?}");
    }

    #[test]
    fn read_capped_rejects_over_cap() {
        // Body larger than cap of 10 bytes.
        let json = br#"{"version":6,"cursor":0,"tags":{}}"#;
        let result: Result<Snapshot> = read_capped(fake_response(json), 10);
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("cap"),
            "expected 'cap' in error, got: {err}"
        );
    }

    /// Body of exactly `cap` bytes must succeed: the `Take` guard reads `cap`
    /// bytes, the buffer is not over-cap, and JSON decodes to `String`.
    ///
    /// Layout: `"` + 62 × `x` + `"` = 64 bytes raw.  Deserialises to `String`.
    #[test]
    fn read_capped_accepts_body_at_exact_cap() {
        const CAP: usize = 64;
        // 1 (opening quote) + 62 content bytes + 1 (closing quote) = 64 bytes.
        let body = format!("\"{}\"", "x".repeat(CAP - 2));
        assert_eq!(body.len(), CAP, "test body must be exactly cap bytes");
        let result: Result<String> = read_capped(fake_response(body.as_bytes()), CAP);
        assert!(
            result.is_ok(),
            "expected Ok for body == cap, got {result:?}"
        );
    }

    /// Body of `cap + 1` bytes must be rejected with an error mentioning "cap".
    ///
    /// Layout: `"` + 63 × `x` + `"` = 65 bytes raw (one byte over CAP = 64).
    #[test]
    fn read_capped_rejects_body_one_over_cap() {
        const CAP: usize = 64;
        // 1 + 63 + 1 = 65 bytes.
        let body = format!("\"{}\"", "x".repeat(CAP - 1));
        assert_eq!(body.len(), CAP + 1, "test body must be cap+1 bytes");
        let result: Result<String> = read_capped(fake_response(body.as_bytes()), CAP);
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("cap"),
            "expected 'cap' in error message, got: {err}"
        );
    }

    #[test]
    fn repo_client_constructs_with_timeouts() {
        // Construction must not panic even when timeouts are applied.
        let _client = RepoClient::new("http://localhost:9999");
    }

    /// An HTTP/1.1 stub that serves one connection per element of `bodies`, in
    /// order. For each it reads the request line, headers and (if
    /// `content-length` says so) the body, replies with that element as JSON,
    /// and publishes the raw request text on the returned channel. Deliberately
    /// dependency-free: `naiad-netproto` has no tokio/axum dev-dependency and
    /// must not grow one for this.
    ///
    /// The serving thread is not joined, so a test that makes *fewer* requests
    /// than it queued bodies simply leaves the thread parked in `accept` for
    /// the rest of the process — assert on the request count instead of relying
    /// on the stub to notice.
    fn stub_bodies(bodies: Vec<String>) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("stub local_addr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for response_body in bodies {
                let (mut sock, _peer) = listener.accept().expect("accept stub connection");
                let mut reader = BufReader::new(sock.try_clone().expect("clone stub socket"));
                let mut request = String::new();
                let mut body_len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).expect("read stub request line") == 0 {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        body_len = v.trim().parse().unwrap_or(0);
                    }
                    let done = line == "\r\n" || line == "\n";
                    request.push_str(&line);
                    if done {
                        break;
                    }
                }
                let mut body = vec![0u8; body_len];
                reader
                    .read_exact(&mut body)
                    .expect("read stub request body");
                request.push_str(&String::from_utf8_lossy(&body));
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                sock.write_all(resp.as_bytes())
                    .expect("write stub response");
                sock.flush().expect("flush stub response");
                tx.send(request).expect("publish stub request");
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// A single-shot stub — the shape most tests want.
    fn stub_once(response_body: String) -> (String, std::sync::mpsc::Receiver<String>) {
        stub_bodies(vec![response_body])
    }

    /// The JSON body of a request captured by [`stub_bodies`], parsed back into
    /// a [`BucketRequest`] so tests can assert on what actually went on the wire.
    fn captured_request(raw: &str) -> BucketRequest {
        let body = raw
            .split_once("\r\n\r\n")
            .map_or(raw, |(_, b)| b)
            .trim_start();
        serde_json::from_str(body).unwrap_or_else(|e| panic!("parse captured body {body:?}: {e}"))
    }

    /// Body bytes the captured request occupied — what the server's
    /// `DefaultBodyLimit` actually measures.
    fn captured_body_len(raw: &str) -> usize {
        raw.split_once("\r\n\r\n")
            .map_or(0, |(_, b)| b.trim_start().len())
    }

    fn empty_snapshot_json() -> String {
        format!(r#"{{"version":{PROTOCOL_VERSION},"cursor":0,"tags":{{}}}}"#)
    }

    #[test]
    fn fetch_buckets_in_sends_the_domain_and_omits_it_when_none() {
        let timeout = std::time::Duration::from_secs(10);
        let bucket = "00".repeat(32);

        let (url, rx) = stub_once(empty_snapshot_json());
        RepoClient::new(&url)
            .fetch_buckets_in(
                12,
                std::slice::from_ref(&bucket),
                Some(HashDomain::Sha256),
                None,
                &NoopObserver,
                false,
            )
            .expect("domain-carrying bucket fetch");
        let req = rx.recv_timeout(timeout).expect("stub captured request");
        assert!(req.contains("POST /repo/buckets"), "wrong route: {req}");
        assert!(
            req.contains(r#""domain":"sha256""#),
            "domain must ride in the request body: {req}"
        );

        let (url, rx) = stub_once(empty_snapshot_json());
        RepoClient::new(&url)
            .fetch_buckets(12, std::slice::from_ref(&bucket))
            .expect("legacy bucket fetch");
        let req = rx.recv_timeout(timeout).expect("stub captured request");
        assert!(
            !req.contains("domain"),
            "a domain-less call must send byte-identical requests to today's: {req}"
        );
    }

    #[test]
    fn fetch_snapshot_in_puts_the_domain_in_the_query_string() {
        let timeout = std::time::Duration::from_secs(10);

        let (url, rx) = stub_once(empty_snapshot_json());
        RepoClient::new(&url)
            .fetch_snapshot_in(Some(HashDomain::Sha256), &NoopObserver)
            .expect("domain-carrying snapshot fetch");
        let req = rx.recv_timeout(timeout).expect("stub captured request");
        assert!(
            req.contains("GET /repo/snapshot?domain=sha256"),
            "domain must ride in the query string: {req}"
        );

        let (url, rx) = stub_once(empty_snapshot_json());
        RepoClient::new(&url)
            .fetch_snapshot()
            .expect("legacy snapshot fetch");
        let req = rx.recv_timeout(timeout).expect("stub captured request");
        assert!(
            req.contains("GET /repo/snapshot "),
            "legacy call must have no query string: {req}"
        );
    }

    fn empty_delta_json() -> String {
        format!(r#"{{"version":{PROTOCOL_VERSION},"cursor":0,"changes":[]}}"#)
    }

    #[test]
    fn fetch_bucket_delta_in_sends_the_domain_and_omits_it_when_none() {
        let timeout = std::time::Duration::from_secs(10);
        let bucket = "00".repeat(32);
        let since = [0u64];

        // With domain: the body must contain both "domain":"sha256" and "since".
        let (url, rx) = stub_once(empty_delta_json());
        RepoClient::new(&url)
            .fetch_bucket_delta_in(
                12,
                std::slice::from_ref(&bucket),
                &since,
                Some(HashDomain::Sha256),
            )
            .expect("domain-carrying delta fetch");
        let req = rx.recv_timeout(timeout).expect("stub captured request");
        assert!(req.contains("POST /repo/buckets"), "wrong route: {req}");
        assert!(
            req.contains(r#""domain":"sha256""#),
            "domain must ride in the request body: {req}"
        );
        assert!(
            req.contains("\"since\""),
            "since must be present in the request body: {req}"
        );

        // Without domain: the raw text must not contain "domain" anywhere.
        let (url, rx) = stub_once(empty_delta_json());
        RepoClient::new(&url)
            .fetch_bucket_delta(12, std::slice::from_ref(&bucket), &since)
            .expect("legacy delta fetch");
        let req = rx.recv_timeout(timeout).expect("stub captured request");
        assert!(
            !req.contains("domain"),
            "a domain-less call must not include domain in the body: {req}"
        );
    }

    /// The server's whole-router limit, which the client's chunk budget exists
    /// to stay under (`http.rs`: `DefaultBodyLimit::max(64 * 1024)`).
    const SERVER_BODY_LIMIT: usize = 64 * 1024;

    /// 64-char hex bucket keys, the only shape `bucket_key` ever produces.
    fn fake_keys(n: u32) -> Vec<String> {
        (0..n).map(|i| format!("{i:064x}")).collect()
    }

    #[test]
    fn bucket_chunks_cover_every_key_and_each_chunk_fits_the_server_limit() {
        let keys = fake_keys(2500);
        let chunks = bucket_chunks(&keys, None, BUCKET_REQUEST_BODY_BUDGET);
        assert!(
            chunks.len() > 1,
            "2500 64-char keys cannot fit one {BUCKET_REQUEST_BODY_BUDGET}-byte body"
        );

        // Contiguous and complete: the union of the chunks is the union asked for.
        let mut next = 0;
        for (lo, hi) in &chunks {
            assert_eq!(*lo, next, "chunks must not skip or overlap: {chunks:?}");
            assert!(hi > lo, "no empty chunk: {chunks:?}");
            next = *hi;
        }
        assert_eq!(next, keys.len(), "chunks must cover every key");

        // The estimate is only useful if the *real* serialisation fits. Include
        // the longest optional field (`domain`) so this is the worst case.
        for (lo, hi) in &chunks {
            let req = BucketRequest {
                version: PROTOCOL_VERSION,
                prefix_bits: 24,
                buckets: keys[*lo..*hi].to_vec(),
                since: None,
                domain: Some(HashDomain::Sha256.to_string()),
                stream: false,
                resume_at: None,
            };
            let len = serde_json::to_vec(&req).expect("serialise chunk").len();
            assert!(
                len <= SERVER_BODY_LIMIT,
                "chunk {lo}..{hi} serialises to {len} bytes, over the server's {SERVER_BODY_LIMIT}"
            );
        }
    }

    #[test]
    fn bucket_chunks_edge_cases() {
        assert_eq!(
            bucket_chunks(&[], None, BUCKET_REQUEST_BODY_BUDGET),
            vec![(0, 0)],
            "an empty pull still makes one request — the reply carries the cursor"
        );
        let one = vec!["ab".repeat(32)];
        assert_eq!(
            bucket_chunks(&one, None, BUCKET_REQUEST_BODY_BUDGET),
            vec![(0, 1)],
            "a normal key list is a single unchunked request"
        );
        // A key that alone blows the budget still gets a chunk of its own
        // instead of producing an empty chunk (which would never advance).
        let huge = vec!["a".repeat(1000), "b".repeat(1000)];
        assert_eq!(bucket_chunks(&huge, None, 300), vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn bucket_chunks_are_smaller_when_since_cursors_ride_along() {
        let keys = fake_keys(2000);
        let since = vec![u64::MAX; keys.len()];
        let plain = bucket_chunks(&keys, None, BUCKET_REQUEST_BODY_BUDGET);
        let with_since = bucket_chunks(&keys, Some(&since), BUCKET_REQUEST_BODY_BUDGET);
        assert!(
            with_since.len() > plain.len(),
            "cursors cost body bytes too: {} chunks with since vs {} without",
            with_since.len(),
            plain.len()
        );
    }

    #[test]
    fn fetch_buckets_in_chunks_a_large_key_list_and_merges_the_replies() {
        let timeout = std::time::Duration::from_secs(10);
        let keys = fake_keys(2000);
        let expected = bucket_chunks(&keys, None, BUCKET_REQUEST_BODY_BUDGET);
        assert!(expected.len() > 1, "test needs a key list that chunks");

        // One distinct hash per chunk, and deliberately out-of-order cursors so
        // the min-not-max rule is actually exercised.
        let cursors = [7u64, 5, 9];
        let bodies: Vec<String> = (0..expected.len())
            .map(|i| {
                format!(
                    r#"{{"version":{PROTOCOL_VERSION},"cursor":{},"tags":{{"{}":[ {{"tag":"chunk:{i}"}} ]}}}}"#,
                    cursors[i % cursors.len()],
                    format_args!("{:064x}", 0xF000 + i)
                )
            })
            .collect();
        let min_cursor = (0..expected.len())
            .map(|i| cursors[i % cursors.len()])
            .min()
            .expect("at least one chunk");

        let (url, rx) = stub_bodies(bodies);
        let snap = RepoClient::new(&url)
            .fetch_buckets_in(
                24,
                &keys,
                Some(HashDomain::Sha256),
                None,
                &NoopObserver,
                false,
            )
            .expect("chunked bucket fetch");

        let mut seen: Vec<String> = Vec::new();
        for _ in 0..expected.len() {
            let raw = rx.recv_timeout(timeout).expect("stub captured request");
            assert!(
                captured_body_len(&raw) <= SERVER_BODY_LIMIT,
                "a chunk went out at {} bytes, over the server's {SERVER_BODY_LIMIT}",
                captured_body_len(&raw)
            );
            let req = captured_request(&raw);
            assert_eq!(req.prefix_bits, 24, "every chunk restates the same width");
            assert_eq!(req.domain.as_deref(), Some("sha256"));
            assert!(req.since.is_none(), "a full fetch never sends cursors");
            seen.extend(req.buckets);
        }
        assert_eq!(seen, keys, "the chunks must request exactly the input keys");
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "no request beyond the expected {} chunks",
            expected.len()
        );

        assert_eq!(
            snap.tags.len(),
            expected.len(),
            "every chunk's tags land in the merged snapshot"
        );
        assert_eq!(
            snap.cursor, min_cursor,
            "the merged cursor is the minimum, so no chunk's changes get skipped"
        );
    }

    #[test]
    fn fetch_bucket_delta_in_chunks_buckets_and_since_in_lockstep() {
        let timeout = std::time::Duration::from_secs(10);
        let keys = fake_keys(2000);
        let since: Vec<u64> = (0..keys.len() as u64).collect();
        let expected = bucket_chunks(&keys, Some(&since), BUCKET_REQUEST_BODY_BUDGET);
        assert!(expected.len() > 1, "test needs a key list that chunks");

        let cursors = [11u64, 4, 30];
        let bodies: Vec<String> = (0..expected.len())
            .map(|i| {
                format!(
                    r#"{{"version":{PROTOCOL_VERSION},"cursor":{},"changes":[{{"hash":"{}","tag":"chunk:{i}","status":"current","seq":1}}]}}"#,
                    cursors[i % cursors.len()],
                    format_args!("{:064x}", 0xF000 + i)
                )
            })
            .collect();
        let min_cursor = (0..expected.len())
            .map(|i| cursors[i % cursors.len()])
            .min()
            .expect("at least one chunk");

        let (url, rx) = stub_bodies(bodies);
        let delta = RepoClient::new(&url)
            .fetch_bucket_delta_in(24, &keys, &since, None)
            .expect("chunked delta fetch");

        let mut seen_keys: Vec<String> = Vec::new();
        let mut seen_since: Vec<u64> = Vec::new();
        for _ in 0..expected.len() {
            let raw = rx.recv_timeout(timeout).expect("stub captured request");
            assert!(
                captured_body_len(&raw) <= SERVER_BODY_LIMIT,
                "a chunk went out at {} bytes, over the server's {SERVER_BODY_LIMIT}",
                captured_body_len(&raw)
            );
            let req = captured_request(&raw);
            let cursors = req.since.expect("a delta fetch always sends cursors");
            assert_eq!(
                req.buckets.len(),
                cursors.len(),
                "the server rejects a chunk whose since length differs from its buckets"
            );
            seen_keys.extend(req.buckets);
            seen_since.extend(cursors);
        }
        assert_eq!(seen_keys, keys, "the chunks request exactly the input keys");
        assert_eq!(
            seen_since, since,
            "each key keeps its own cursor across the split"
        );

        assert_eq!(
            delta.changes.len(),
            expected.len(),
            "every chunk's changes are concatenated"
        );
        assert_eq!(delta.cursor, min_cursor, "the merged cursor is the minimum");
    }

    // ── stub_status: error-response variant of stub_bodies ────────────────────

    /// Like [`stub_bodies`] but serves one connection with `status` and
    /// `body` so tests can exercise the error path without a real server.
    /// Reads the full request (so ureq does not see a broken pipe before it
    /// finishes sending) and then sends the error response.
    fn stub_status(status: u32, status_text: &str, body: String) -> String {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub_status");
        let addr = listener.local_addr().expect("stub_status local_addr");
        let text = status_text.to_string();
        std::thread::spawn(move || {
            let (mut sock, _peer) = listener.accept().expect("accept stub_status");
            let mut reader = BufReader::new(sock.try_clone().expect("clone stub_status socket"));
            let mut body_len = 0usize;
            loop {
                let mut line = String::new();
                if reader
                    .read_line(&mut line)
                    .expect("read stub_status request line")
                    == 0
                {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    body_len = v.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            // Drain the request body so ureq can finish the write before we reply.
            let mut drain = vec![0u8; body_len];
            let _ = reader.read_exact(&mut drain);
            let resp = format!(
                "HTTP/1.1 {status} {text}\r\ncontent-type: text/plain\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes())
                .expect("write stub_status response");
            sock.flush().expect("flush stub_status response");
        });
        format!("http://{addr}")
    }

    // ── #150: pull-path error body surfacing ──────────────────────────────────

    /// A 400 from `GET /repo/snapshot` must carry the server's body text in the
    /// error, not just the status code.  Pre-#150 the message was
    /// `"fetching snapshot from URL: URL: status code 400"` with the body silently
    /// discarded; after the fix it reads `"fetching snapshot from URL (400): <body>"`.
    #[test]
    fn status_err_surfaces_body_from_fetch_snapshot() {
        let reason = "this repo does not serve the sha256 hash domain; it serves: blake3";
        let url = stub_status(400, "Bad Request", reason.to_string());
        let err = RepoClient::new(&url)
            .fetch_snapshot()
            .expect_err("400 must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains(reason),
            "error must include the server's rejection body; got: {msg}"
        );
        assert!(
            msg.contains("400"),
            "error must include the HTTP status code; got: {msg}"
        );
    }

    /// Same as above but for `GET /repo/caps`.
    #[test]
    fn status_err_surfaces_body_from_fetch_caps() {
        let reason = "minimum prefix_bits is 12; you requested 4";
        let url = stub_status(400, "Bad Request", reason.to_string());
        let err = RepoClient::new(&url)
            .fetch_caps()
            .expect_err("400 must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains(reason),
            "fetch_caps error must include server rejection body; got: {msg}"
        );
    }

    /// A body longer than [`STATUS_ERR_BODY_CAP`] must be truncated: the error
    /// string may not grow without bound when a hostile server sends a huge body.
    #[test]
    fn status_err_truncates_oversized_error_body() {
        // Build a body exactly 3× the cap so truncation is certain.
        let huge_body = "x".repeat(STATUS_ERR_BODY_CAP * 3);
        let url = stub_status(400, "Bad Request", huge_body.clone());
        let err = RepoClient::new(&url)
            .fetch_snapshot()
            .expect_err("400 must be an error");
        let msg = err.to_string();
        // The raw body must NOT appear verbatim — the error must be shorter.
        assert!(
            !msg.contains(&huge_body),
            "oversized body must be truncated; message length: {}",
            msg.len()
        );
        // The sentinel suffix added by the truncation branch must be present.
        assert!(
            msg.contains("truncated"),
            "truncated error must say so; got: {msg}"
        );
    }

    /// The truncation cut must land on a UTF-8 char boundary. The body is
    /// server-controlled, so a multibyte codepoint straddling the cap would
    /// panic a naive `&raw[..CAP]` slice and take the daemon down with it.
    #[test]
    fn status_err_truncates_multibyte_body_without_panicking() {
        // "é" is two bytes. An odd number of leading ASCII bytes puts a
        // codepoint boundary *inside* the cap, so byte STATUS_ERR_BODY_CAP
        // lands mid-character.
        let mut body = "a".repeat(STATUS_ERR_BODY_CAP - 1);
        body.push_str(&"é".repeat(64));
        assert!(
            !body.is_char_boundary(STATUS_ERR_BODY_CAP),
            "test is only meaningful if the cap splits a codepoint"
        );

        let url = stub_status(400, "Bad Request", body);
        let err = RepoClient::new(&url)
            .fetch_snapshot()
            .expect_err("400 must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains("truncated"),
            "multibyte body must still truncate cleanly; got: {msg}"
        );
    }

    /// A 400 from `POST /repo/buckets` (single chunk) must include the body in
    /// the error, and a multi-chunk rejection must also name the chunk.
    #[test]
    fn status_err_surfaces_body_from_fetch_buckets_in() {
        let reason = "the sha256 domain has no incremental deltas in snapshot mode";

        // Single-chunk case: one bucket, one 400 response.
        let url = stub_status(400, "Bad Request", reason.to_string());
        let err = RepoClient::new(&url)
            .fetch_buckets_in(
                12,
                &["00".repeat(32)],
                Some(HashDomain::Sha256),
                None,
                &NoopObserver,
                false,
            )
            .expect_err("400 must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains(reason),
            "single-chunk error must include server body; got: {msg}"
        );

        // Multi-window case: enough keys to split across multiple adaptive windows.
        // The error must include both the server's body AND a positional suffix
        // identifying the failing window (total=0 path → "window N, buckets lo..hi").
        let keys = fake_keys(2000);
        let n_chunks = bucket_chunks(&keys, None, BUCKET_REQUEST_BODY_BUDGET).len();
        assert!(
            n_chunks > 1,
            "need a multi-window split for this part of the test"
        );
        let url = stub_status(400, "Bad Request", reason.to_string());
        let err = RepoClient::new(&url)
            .fetch_buckets_in(24, &keys, None, None, &NoopObserver, false)
            .expect_err("400 must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains(reason),
            "multi-window error must include server body; got: {msg}"
        );
        // Adaptive path emits "(window 1, buckets 0..N)" for the first window that
        // fails, replacing the old "chunk 1 of N" suffix.
        assert!(
            msg.contains("window 1, buckets 0.."),
            "multi-window error must name the failing window range; got: {msg}"
        );
    }

    // ── #154: merged-response-size cap ───────────────────────────────────────

    /// A small `merged_cap` must be enforced even when each individual chunk
    /// passes the per-chunk [`RESPONSE_SIZE_CAP`].  This exercises the
    /// accumulator path without generating gigabytes of fake responses.
    #[test]
    fn fetch_buckets_inner_rejects_when_merged_cap_exceeded() {
        // Use a key list that produces at least 2 chunks.
        let keys = fake_keys(2000);
        let n_chunks = bucket_chunks(&keys, None, BUCKET_REQUEST_BODY_BUDGET).len();
        assert!(n_chunks > 1, "need chunks for this test");

        // Each reply is a small but valid snapshot. A merged_cap of 1 byte means
        // even the first chunk reply exceeds the cap.
        let bodies: Vec<String> = (0..n_chunks).map(|_| empty_snapshot_json()).collect();
        let (url, _rx) = stub_bodies(bodies);
        let err = RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, 1, None, &NoopObserver, false)
            .expect_err("merged cap of 1 byte must always be exceeded");
        let msg = err.to_string();
        assert!(
            msg.contains("MERGED_RESPONSE_SIZE_CAP"),
            "error must name the constant; got: {msg}"
        );
    }

    /// A normal multi-chunk pull must succeed when the accumulated size is under
    /// `merged_cap`.  Regression: the cap check must not be triggered on a
    /// legitimate pull.
    #[test]
    fn fetch_buckets_inner_succeeds_under_merged_cap() {
        let keys = fake_keys(2000);
        let n_chunks = bucket_chunks(&keys, None, BUCKET_REQUEST_BODY_BUDGET).len();
        assert!(n_chunks > 1, "need chunks for this test");

        let bodies: Vec<String> = (0..n_chunks).map(|_| empty_snapshot_json()).collect();
        let (url, _rx) = stub_bodies(bodies);
        // usize::MAX is effectively unlimited — must succeed.
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &NoopObserver, false)
            .expect("multi-chunk pull under merged_cap must succeed");
    }

    /// Same guard on the delta path: small `merged_cap` must reject the first
    /// chunk that pushes the accumulator over.
    #[test]
    fn fetch_bucket_delta_inner_rejects_when_merged_cap_exceeded() {
        let keys = fake_keys(2000);
        let since: Vec<u64> = vec![0u64; keys.len()];
        let n_chunks = bucket_chunks(&keys, Some(&since), BUCKET_REQUEST_BODY_BUDGET).len();
        assert!(n_chunks > 1, "need chunks for this test");

        let bodies: Vec<String> = (0..n_chunks).map(|_| empty_delta_json()).collect();
        let (url, _rx) = stub_bodies(bodies);
        let err = RepoClient::new(&url)
            .fetch_bucket_delta_inner(24, &keys, &since, None, 1)
            .expect_err("merged cap of 1 byte must always be exceeded");
        let msg = err.to_string();
        assert!(
            msg.contains("MERGED_RESPONSE_SIZE_CAP"),
            "delta error must name the constant; got: {msg}"
        );
    }

    /// Normal delta pull succeeds under the cap.
    #[test]
    fn fetch_bucket_delta_inner_succeeds_under_merged_cap() {
        let keys = fake_keys(2000);
        let since: Vec<u64> = vec![0u64; keys.len()];
        let n_chunks = bucket_chunks(&keys, Some(&since), BUCKET_REQUEST_BODY_BUDGET).len();
        assert!(n_chunks > 1, "need chunks for this test");

        let bodies: Vec<String> = (0..n_chunks).map(|_| empty_delta_json()).collect();
        let (url, _rx) = stub_bodies(bodies);
        RepoClient::new(&url)
            .fetch_bucket_delta_inner(24, &keys, &since, None, usize::MAX)
            .expect("delta pull under merged_cap must succeed");
    }

    // ── #145: bisect-on-413 ───────────────────────────────────────────────────

    /// A stub that returns **413** for any request carrying more than `threshold`
    /// keys and, otherwise, a 200 body built by `make_ok_body(&req)`. Serves an
    /// unbounded number of connections (bisecting makes many). Every parsed
    /// request is published on the channel so a test can assert on the split.
    fn stub_413_over(
        threshold: usize,
        make_ok_body: impl Fn(&BucketRequest) -> String + Send + 'static,
    ) -> (String, std::sync::mpsc::Receiver<BucketRequest>) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub_413");
        let addr = listener.local_addr().expect("stub_413 local_addr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut sock = match conn {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut reader = BufReader::new(sock.try_clone().expect("clone stub_413 socket"));
                let mut body_len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).expect("read stub_413 line") == 0 {
                        return;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        body_len = v.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; body_len];
                reader.read_exact(&mut body).expect("read stub_413 body");
                let req: BucketRequest =
                    serde_json::from_slice(&body).expect("parse stub_413 request body");
                let resp = if req.buckets.len() > threshold {
                    "HTTP/1.1 413 Payload Too Large\r\ncontent-type: text/plain\r\n\
                     content-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                } else {
                    let ok = make_ok_body(&req);
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{}",
                        ok.len(),
                        ok
                    )
                };
                sock.write_all(resp.as_bytes())
                    .expect("write stub_413 response");
                sock.flush().expect("flush stub_413 response");
                tx.send(req).expect("publish stub_413 request");
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// A 200 snapshot body whose tags cover exactly the request's keys (one
    /// tag each), so a merged union can be asserted to cover every key.
    fn snapshot_body_covering(req: &BucketRequest) -> String {
        let tags: BTreeMap<String, Vec<OriginTag>> = req
            .buckets
            .iter()
            .map(|k| {
                (
                    k.clone(),
                    vec![OriginTag {
                        tag: "t".to_string(),
                        origin: None,
                    }],
                )
            })
            .collect();
        let snap = Snapshot {
            version: PROTOCOL_VERSION,
            cursor: 0,
            tags,
        };
        serde_json::to_string(&snap).expect("serialise stub snapshot")
    }

    #[test]
    fn fetch_buckets_in_bisects_on_413_until_each_piece_fits() {
        let keys = fake_keys(8);
        // The 8 keys fit one request-body chunk, so the SPLITTING here is the
        // bisect, not the body chunker. Server 413s any request over 2 keys.
        let (url, rx) = stub_413_over(2, snapshot_body_covering);
        let snap = RepoClient::new(&url)
            .fetch_buckets_in(24, &keys, None, None, &NoopObserver, false)
            .expect("bisected pull completes");
        for k in &keys {
            assert!(
                snap.tags.contains_key(k),
                "key {k} missing from merged union"
            );
        }
        let mut requests = 0;
        while rx.try_recv().is_ok() {
            requests += 1;
        }
        assert!(
            requests > 1,
            "bisecting must make multiple requests; got {requests}"
        );
    }

    #[test]
    fn fetch_buckets_in_single_key_413_is_a_hard_error() {
        let keys = fake_keys(3);
        // 413 for EVERY request, even a single key: no split can rescue it.
        let (url, _rx) = stub_413_over(0, snapshot_body_covering);
        let err = RepoClient::new(&url)
            .fetch_buckets_in(24, &keys, None, None, &NoopObserver, false)
            .expect_err("an always-413 server must hard-error, not hang");
        let msg = err.to_string();
        assert!(
            msg.contains("per-request response budget"),
            "must name the cause: {msg}"
        );
        assert!(
            keys.iter().any(|k| msg.contains(k.as_str())),
            "must name the offending bucket key: {msg}"
        );
    }

    #[test]
    fn fetch_bucket_delta_in_bisects_and_keeps_since_aligned() {
        let keys = fake_keys(8);
        let since: Vec<u64> = (0..8u64).collect();
        let (url, rx) = stub_413_over(2, |_req| empty_delta_json());
        RepoClient::new(&url)
            .fetch_bucket_delta_in(24, &keys, &since, None)
            .expect("bisected delta pull completes");

        // key i must always ride with cursor i, in every request, after every split.
        let expected: std::collections::HashMap<String, u64> =
            keys.iter().cloned().zip(since.iter().copied()).collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut requests = 0;
        while let Ok(req) = rx.try_recv() {
            requests += 1;
            let s = req.since.expect("a delta fetch always sends cursors");
            assert_eq!(
                req.buckets.len(),
                s.len(),
                "since stays length-aligned per request"
            );
            for (k, v) in req.buckets.iter().zip(s) {
                let exp = *expected.get(k).expect("key must be in expected map");
                assert_eq!(exp, v, "key {k} kept its own cursor through the split");
                seen.insert(k.clone());
            }
        }
        assert!(
            requests > 1,
            "bisecting must make multiple requests; got {requests}"
        );
        for k in &keys {
            assert!(seen.contains(k), "key {k} was never requested");
        }
    }

    #[test]
    fn bisected_pieces_still_count_against_merged_cap() {
        let keys = fake_keys(8);
        // Server 413s >2 keys, else a non-empty snapshot. merged_cap of 1 byte
        // must trip on the first successful (bisected) 200 piece.
        let (url, _rx) = stub_413_over(2, snapshot_body_covering);
        let err = RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, 1, None, &NoopObserver, false)
            .expect_err("bisected 200 pieces must still accrue against merged_cap");
        assert!(
            err.to_string().contains("MERGED_RESPONSE_SIZE_CAP"),
            "must name the aggregate cap: {err}"
        );
    }

    // ── #172: PullObserver emission ──────────────────────────────────────────

    /// Records every phase. `RefCell` because `on_phase` takes `&self`; tests
    /// are single-threaded.
    #[derive(Default)]
    struct Recorder {
        phases: std::cell::RefCell<Vec<PullPhase>>,
    }
    impl PullObserver for Recorder {
        fn on_phase(&self, p: PullPhase) {
            self.phases.borrow_mut().push(p);
        }
    }

    #[test]
    fn observer_fires_one_pair_per_top_level_chunk() {
        // Build a key list that bucket_chunks splits into >= 2 adaptive windows.
        let keys = fake_keys(2000);
        let expected = bucket_chunks(&keys, None, BUCKET_REQUEST_BODY_BUDGET);
        assert!(
            expected.len() >= 2,
            "test needs a key list that spans multiple windows"
        );

        let bodies: Vec<String> = (0..expected.len()).map(|_| empty_snapshot_json()).collect();
        let (url, _rx) = stub_bodies(bodies);
        let rec = Recorder::default();
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &rec, false)
            .unwrap();
        let phases = rec.phases.borrow();
        // `total` is the bucket count (fixed), not the window count.
        let totals: Vec<usize> = phases
            .iter()
            .map(|p| match p {
                PullPhase::RequestSent { total, .. } | PullPhase::ChunkReceived { total, .. } => {
                    *total
                }
                _ => panic!("netproto must not emit bookend phases"),
            })
            .collect();
        assert!(totals.iter().all(|t| *t == totals[0]), "total is constant");
        assert!(totals[0] == keys.len(), "total equals the bucket count");
        // Verify done is monotonically non-decreasing.
        let mut last_done = 0usize;
        let mut last_cum = 0usize;
        let mut summed = 0usize;
        for p in phases.iter() {
            match p {
                PullPhase::RequestSent { done, .. } => {
                    assert!(*done >= last_done, "done must be monotonic in RequestSent");
                }
                PullPhase::ChunkReceived {
                    done,
                    chunk_bytes,
                    cumulative_bytes,
                    ..
                } => {
                    assert!(
                        *done >= last_done,
                        "done must be monotonic in ChunkReceived"
                    );
                    last_done = *done;
                    assert!(*cumulative_bytes >= last_cum);
                    last_cum = *cumulative_bytes;
                    summed += *chunk_bytes;
                }
                _ => {}
            }
        }
        assert_eq!(summed, last_cum, "chunk_bytes sum == final cumulative");
        assert_eq!(last_done, keys.len(), "done reaches total");
    }

    #[test]
    fn observer_413_bisection_keeps_total_and_one_chunk_received() {
        // 8 keys fit one body-budget window; the split is 413 bisection.
        // Server 413s any request over 2 keys.
        let keys = fake_keys(8);
        let (url, _rx) = stub_413_over(2, snapshot_body_covering);
        let rec = Recorder::default();
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &rec, false)
            .unwrap();
        let received: Vec<PullPhase> = rec
            .phases
            .borrow()
            .iter()
            .copied()
            .filter(|p| matches!(p, PullPhase::ChunkReceived { .. }))
            .collect();
        assert_eq!(
            received.len(),
            1,
            "one ChunkReceived for the bisected window"
        );
        if let PullPhase::ChunkReceived {
            total,
            done,
            chunk_bytes,
            ..
        } = received[0]
        {
            assert_eq!(
                total,
                keys.len(),
                "total is the bucket count, unchanged by bisection"
            );
            assert_eq!(
                done,
                keys.len(),
                "done = total after the only window completes"
            );
            assert!(chunk_bytes > 0, "chunk_bytes sums the two bisected leaves");
        }
    }

    #[test]
    fn observer_wholerepo_fires_single_pair() {
        let (url, _rx) = stub_once(empty_snapshot_json());
        let rec = Recorder::default();
        RepoClient::new(&url).fetch_snapshot_in(None, &rec).unwrap();
        assert!(matches!(
            rec.phases.borrow().as_slice(),
            [
                PullPhase::RequestSent {
                    done: 0,
                    total: 1,
                    window: 1
                },
                PullPhase::ChunkReceived {
                    done: 1,
                    total: 1,
                    window: 1,
                    ..
                }
            ]
        ));
    }

    // ── #174: window_end unit tests ──────────────────────────────────────────

    #[test]
    fn window_end_unit_tests() {
        let keys = fake_keys(200);

        // max_window cap: must not exceed start + max_window.
        let end = window_end(&keys, None, 0, BUCKET_REQUEST_BODY_BUDGET, 10);
        assert_eq!(end, 10, "max_window=10 must cap the window to exactly 10");

        // max_window = 1: always returns start+1.
        let end = window_end(&keys, None, 5, BUCKET_REQUEST_BODY_BUDGET, 1);
        assert_eq!(end, 6, "max_window=1 must return start+1");

        // Lone oversized key: must still advance by 1 (not get dropped).
        // key len=57086, cost=57089 > available=57088; but i==start so it cannot
        // be omitted.
        let huge_keys = vec!["h".repeat(57086)];
        let end = window_end(&huge_keys, None, 0, BUCKET_REQUEST_BODY_BUDGET, usize::MAX);
        assert_eq!(end, 1, "lone oversized key must get its own window");

        // Contiguous cover: every key covered, no gaps, no overlaps.
        let mut start = 0;
        while start < keys.len() {
            let end = window_end(&keys, None, start, BUCKET_REQUEST_BODY_BUDGET, usize::MAX);
            assert!(end > start, "window_end must advance");
            start = end;
        }
        assert_eq!(
            start,
            keys.len(),
            "window_end must provide contiguous cover"
        );
    }

    // ── #174: adaptive windowing tests ───────────────────────────────────────

    /// Helper: stub that serves `n_responses` connections, sleeping `first_delay`
    /// before responding to the first one (to trigger AIMD shrink) and responding
    /// immediately afterward.
    fn stub_adaptive(
        n_responses: usize,
        first_delay: std::time::Duration,
    ) -> (String, std::sync::mpsc::Receiver<BucketRequest>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind adaptive stub");
        let addr = listener.local_addr().expect("adaptive stub local_addr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for i in 0..n_responses {
                let mut sock = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(_) => break,
                };
                let mut reader =
                    BufReader::new(sock.try_clone().expect("clone adaptive stub socket"));
                let mut body_len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader
                        .read_line(&mut line)
                        .expect("adaptive stub read line")
                        == 0
                    {
                        return;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        body_len = v.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; body_len];
                reader
                    .read_exact(&mut body)
                    .expect("adaptive stub read body");
                let req: BucketRequest =
                    serde_json::from_slice(&body).expect("adaptive stub parse request");
                if i == 0 {
                    std::thread::sleep(first_delay);
                }
                let resp_body = empty_snapshot_json();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                sock.write_all(resp.as_bytes())
                    .expect("adaptive stub write");
                sock.flush().expect("adaptive stub flush");
                tx.send(req).ok();
            }
        });
        (format!("http://{addr}"), rx)
    }

    #[test]
    fn adaptive_floor_respected_with_huge_hint() {
        // Huge ms/bucket hint → W0 ≈ 0, clamped to MIN_WINDOW.
        // All windows must be >= MIN_WINDOW.
        let keys = fake_keys(64); // >= 2 * MIN_WINDOW
        // n_windows = ceil(64 / MIN_WINDOW) = 2
        let n_windows = keys.len().div_ceil(MIN_WINDOW);
        let bodies: Vec<String> = (0..n_windows).map(|_| empty_snapshot_json()).collect();
        let (url, _rx) = stub_bodies(bodies);
        let rec = Recorder::default();
        let huge_hint = Some(10_000.0f64); // 10 000 ms/bucket → W0 = 1, clamp to 32
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, huge_hint, &rec, false)
            .unwrap();
        let phases = rec.phases.borrow();
        let total = keys.len();
        let mut done_vals: Vec<usize> = Vec::new();
        for p in phases.iter() {
            match p {
                PullPhase::RequestSent { window, .. } => {
                    assert!(
                        *window >= MIN_WINDOW,
                        "window {window} < MIN_WINDOW {MIN_WINDOW}"
                    );
                }
                PullPhase::ChunkReceived { done, total: t, .. } => {
                    assert_eq!(*t, total, "total must be constant");
                    done_vals.push(*done);
                }
                _ => {}
            }
        }
        // done is monotonically increasing and ends at total.
        let mut prev = 0;
        for d in &done_vals {
            assert!(*d >= prev, "done must be monotonic: {done_vals:?}");
            prev = *d;
        }
        assert_eq!(*done_vals.last().unwrap(), total, "done must reach total");
    }

    #[test]
    fn adaptive_no_hint_never_coarser_than_budget() {
        // No hint → W0 = usize::MAX → every window clamped to budget_fit.
        // No request body must exceed BUCKET_REQUEST_BODY_BUDGET.
        let keys = fake_keys(2000);
        let n_chunks = bucket_chunks(&keys, None, BUCKET_REQUEST_BODY_BUDGET).len();
        assert!(n_chunks >= 2, "need multiple windows for this test");
        let bodies: Vec<String> = (0..n_chunks).map(|_| empty_snapshot_json()).collect();
        let (url, rx) = stub_bodies(bodies);
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &NoopObserver, false)
            .expect("no-hint pull must succeed");
        while let Ok(req_text) = rx.try_recv() {
            let body_len = captured_body_len(&req_text);
            assert!(
                body_len <= BUCKET_REQUEST_BODY_BUDGET,
                "request body {body_len} must not exceed BUCKET_REQUEST_BODY_BUDGET \
                 {BUCKET_REQUEST_BODY_BUDGET}"
            );
        }
    }

    /// AIMD test: first response is slow (>WINDOW_SLOW_MS), rest are fast.
    /// Uses hint=50ms/bucket so W0=100 — the halve lands at 50 (well above the
    /// MIN_WINDOW floor of 32), pinning the multiplicative math itself rather than
    /// just proving "some shrink reached the floor".
    /// Wall time: ~5.1 s (the delay on the first stub response).
    #[test]
    fn adaptive_aimd_shrinks_then_grows() {
        // hint = 50 ms/bucket → W0 = round(5000/50) = 100.
        // After slow: (100/2).max(32) = 50  (ABOVE floor — proves actual halving).
        // After fast: 50+32=82. After fast: 82+32=114 (capped by remaining).
        // 300 keys: windows [100, 50, 82, 68].
        let keys = fake_keys(300);
        let hint_ms = 50.0f64; // ms per bucket
        let w0 = (WINDOW_TARGET_MS as f64 / hint_ms).round() as usize; // 100
        assert!(
            w0 / 2 > MIN_WINDOW,
            "test requires W0/2 > MIN_WINDOW so halving is visible above the floor"
        );

        // 4 windows: 100+50+82+68=300. Allow up to 6 for timing variance.
        let n_responses = 6usize;
        let delay = std::time::Duration::from_millis(WINDOW_SLOW_MS + 200); // 5200 ms
        let (url, _rx) = stub_adaptive(n_responses, delay);
        let rec = Recorder::default();
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, Some(hint_ms), &rec, false)
            .unwrap();

        let phases = rec.phases.borrow();
        let windows: Vec<usize> = phases
            .iter()
            .filter_map(|p| {
                if let PullPhase::RequestSent { window, .. } = p {
                    Some(*window)
                } else {
                    None
                }
            })
            .collect();

        // First window seeded by hint.
        assert_eq!(windows[0], w0, "first window must be seeded by hint ({w0})");
        // Second window: W0/2 = 50, above floor — proves multiplicative decrease.
        let w1_expected = (w0 / 2).max(MIN_WINDOW);
        assert_eq!(
            windows[1], w1_expected,
            "second window must be W0/2={w1_expected}, above the floor"
        );
        assert!(
            w1_expected > MIN_WINDOW,
            "w1 must be above floor to prove actual halving, not just clamping"
        );
        // Third window: grew by MIN_WINDOW after fast response.
        assert_eq!(
            windows[2],
            w1_expected + MIN_WINDOW,
            "third window must grow by MIN_WINDOW from w1"
        );

        // done must be monotonic and end at total.
        let mut last_done = 0usize;
        for p in phases.iter() {
            if let PullPhase::ChunkReceived { done, total, .. } = p {
                assert!(*done >= last_done, "done must be monotonic");
                assert_eq!(*total, keys.len(), "total must be constant");
                last_done = *done;
            }
        }
        assert_eq!(last_done, keys.len(), "done must reach total");
    }

    /// AIMD no-hint slow-path: with no hint, W₀ = usize::MAX clamped to budget_fit.
    /// After a slow first response the window should halve from the ISSUED window
    /// (budget_fit), not from usize::MAX — without Fix 1 this test would fail because
    /// usize::MAX/2 >> budget_fit and the effective second window stays unchanged.
    /// Wall time: ~5.1 s.
    #[test]
    fn adaptive_no_hint_slow_response_shrinks_from_effective_window() {
        // Use enough keys to span >2 budget windows.
        let keys = fake_keys(1500);
        let budget_fit = window_end(&keys, None, 0, BUCKET_REQUEST_BODY_BUDGET, usize::MAX);
        assert!(
            budget_fit >= MIN_WINDOW,
            "budget_fit must be at least MIN_WINDOW"
        );
        assert!(
            budget_fit < keys.len(),
            "need more than one window for this test"
        );

        // Allow up to 5 responses (budget_fit + budget_fit/2 + remainder ≤ 1500).
        let n_responses = 5usize;
        let delay = std::time::Duration::from_millis(WINDOW_SLOW_MS + 200);
        let (url, _rx) = stub_adaptive(n_responses, delay);
        let rec = Recorder::default();
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &rec, false)
            .unwrap();

        let phases = rec.phases.borrow();
        let windows: Vec<usize> = phases
            .iter()
            .filter_map(|p| {
                if let PullPhase::RequestSent { window, .. } = p {
                    Some(*window)
                } else {
                    None
                }
            })
            .collect();

        // First window: budget_fit (no hint → usize::MAX clamped to budget).
        assert_eq!(
            windows[0], budget_fit,
            "no-hint first window must equal budget_fit"
        );
        // Second window: (budget_fit/2).max(MIN_WINDOW) — must be less than budget_fit.
        let expected_w1 = (budget_fit / 2).max(MIN_WINDOW);
        assert_eq!(
            windows[1], expected_w1,
            "no-hint second window must shrink from budget_fit to budget_fit/2 (floored)"
        );
        assert!(
            windows[1] < windows[0],
            "second window must be smaller than first after slow response"
        );
    }

    #[test]
    fn adaptive_413_bisection_stable_window_and_single_chunk_received() {
        // 8 keys fit one adaptive window; server 413s > 2 keys → bisects within
        // the window. total unchanged, exactly one ChunkReceived, chunk_bytes = sum.
        let keys = fake_keys(8);
        let (url, _rx) = stub_413_over(2, snapshot_body_covering);
        let rec = Recorder::default();
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &rec, false)
            .unwrap();

        let received: Vec<PullPhase> = rec
            .phases
            .borrow()
            .iter()
            .copied()
            .filter(|p| matches!(p, PullPhase::ChunkReceived { .. }))
            .collect();
        assert_eq!(
            received.len(),
            1,
            "one ChunkReceived per window even when bisected"
        );

        if let PullPhase::ChunkReceived {
            total,
            done,
            chunk_bytes,
            request_ms,
            ..
        } = received[0]
        {
            assert_eq!(
                total,
                keys.len(),
                "total is bucket count, not sub-request count"
            );
            assert_eq!(
                done,
                keys.len(),
                "done equals total after the single window"
            );
            assert!(chunk_bytes > 0, "chunk_bytes must sum the bisected leaves");
            // request_ms spans the bisection (multiple HTTP sub-requests).
            // We can only assert it is non-zero (timing is non-deterministic).
            let _ = request_ms; // present in the struct; its non-zero nature is a runtime property
        }
    }

    // ── #176: streaming NDJSON client tests (tests 1–6, 8) ───────────────────

    /// Handle one stub HTTP connection: read the request, send back `rows_with_delay`
    /// as chunked NDJSON, report the parsed request via `tx`.
    ///
    /// Returns `true` if the connection completed normally; `false` if the thread
    /// should exit early (client disconnected or read error).
    fn handle_stub_connection(
        mut sock: std::net::TcpStream,
        rows_with_delay: &[(std::time::Duration, String)],
        tx: &std::sync::mpsc::Sender<BucketRequest>,
    ) -> bool {
        use std::io::{BufRead, BufReader, Read, Write};
        let mut reader = BufReader::new(sock.try_clone().expect("clone stub socket"));
        let mut body_len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return false;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                body_len = v.trim().parse().unwrap_or(0);
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
        }
        let mut body = vec![0u8; body_len];
        reader.read_exact(&mut body).expect("read stub body");
        let req: BucketRequest = serde_json::from_slice(&body).expect("parse stub request");
        sock.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\n\
              transfer-encoding: chunked\r\n\r\n",
        )
        .expect("write stub headers");
        for (delay, line) in rows_with_delay {
            std::thread::sleep(*delay);
            let data = format!("{line}\n");
            let chunk = format!("{:x}\r\n{data}\r\n", data.len());
            if sock.write_all(chunk.as_bytes()).is_err() {
                return false;
            }
            sock.flush().ok();
        }
        sock.write_all(b"0\r\n\r\n").ok();
        sock.flush().ok();
        tx.send(req).ok();
        true
    }

    /// Build a one-shot NDJSON streaming stub.
    ///
    /// Serves exactly ONE connection with `content-type: application/x-ndjson`
    /// and chunked transfer encoding. `rows_with_delay` is a list of
    /// `(pre_delay, line)` pairs: the stub sleeps `pre_delay` before sending
    /// each line. Lines are sent WITHOUT a trailing newline (the stub appends
    /// one); the caller can pass whatever JSON it likes.
    ///
    /// Returns the base URL. The channel receives the raw captured request body
    /// after the request has been fully read.
    fn stub_streaming_ndjson(
        rows_with_delay: Vec<(std::time::Duration, String)>,
    ) -> (String, std::sync::mpsc::Receiver<BucketRequest>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind streaming stub");
        let addr = listener.local_addr().expect("streaming stub addr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Accept at most 2 connections (for continuation tests).
            for _ in 0..2 {
                let (sock, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => break,
                };
                if !handle_stub_connection(sock, &rows_with_delay, &tx) {
                    return;
                }
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// A multi-connection streaming stub for continuation tests: each
    /// `Vec<(delay, line)>` element is one connection's response.
    fn stub_streaming_ndjson_multi(
        responses: Vec<Vec<(std::time::Duration, String)>>,
    ) -> (String, std::sync::mpsc::Receiver<BucketRequest>) {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind streaming multi stub");
        let addr = listener.local_addr().expect("streaming multi stub addr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for rows_with_delay in &responses {
                let (sock, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => break,
                };
                if !handle_stub_connection(sock, rows_with_delay, &tx) {
                    return;
                }
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// Helper: build a StreamHeader line for use in stub responses.
    fn stream_header_line(cursor: u64) -> String {
        serde_json::to_string(&StreamHeader {
            version: PROTOCOL_VERSION,
            cursor,
        })
        .expect("serialize header")
    }

    /// Helper: build a StreamRow line for a hash with a single tag.
    fn stream_row_line(hash_hex: &str, tag: &str) -> String {
        serde_json::to_string(&StreamRow {
            h: hash_hex.to_string(),
            t: vec![OriginTag {
                tag: tag.to_string(),
                origin: None,
            }],
        })
        .expect("serialize row")
    }

    /// Helper: build a trailer line.
    fn stream_done_line() -> String {
        serde_json::to_string(&StreamTrailer::Done { done: true }).expect("serialize done")
    }
    fn stream_more_line(key: &str) -> String {
        serde_json::to_string(&StreamTrailer::More {
            more: key.to_string(),
        })
        .expect("serialize more")
    }
    fn stream_err_line(msg: &str) -> String {
        serde_json::to_string(&StreamTrailer::Err {
            err: msg.to_string(),
        })
        .expect("serialize err")
    }

    const NO_DELAY: std::time::Duration = std::time::Duration::ZERO;

    /// Test 1 — happy path, single response: header + N rows + done.
    /// The merged snapshot covers all N hashes, cursor equals the header cursor,
    /// and the observer saw exactly N RowReceived ticks.
    #[test]
    fn streaming_happy_path_single_response() {
        let n_rows = 3usize;
        let hashes: Vec<String> = (0..n_rows).map(|i| format!("{i:064x}")).collect();
        let cursor_val = 42u64;

        let mut lines = vec![(NO_DELAY, stream_header_line(cursor_val))];
        for h in &hashes {
            lines.push((NO_DELAY, stream_row_line(h, "test:tag")));
        }
        lines.push((NO_DELAY, stream_done_line()));

        let (url, rx) = stub_streaming_ndjson(lines);

        let rec = Recorder::default();
        let snap = RepoClient::new(&url)
            .fetch_buckets_in(24, &hashes, None, None, &rec, true)
            .expect("streaming happy path must succeed");

        // All N hashes merged.
        assert_eq!(snap.tags.len(), n_rows, "all rows must be merged");
        for h in &hashes {
            assert!(
                snap.tags.contains_key(h),
                "hash {h} must be in merged snapshot"
            );
        }
        // Cursor equals the header.
        assert_eq!(
            snap.cursor, cursor_val,
            "cursor must equal the header cursor"
        );

        // Observer: at least N RowReceived ticks (one per row).
        let row_ticks = rec
            .phases
            .borrow()
            .iter()
            .filter(|p| matches!(p, PullPhase::RowReceived { .. }))
            .count();
        assert!(
            row_ticks >= n_rows,
            "expected >= {n_rows} RowReceived, got {row_ticks}"
        );

        // The stub must have received one request.
        let req = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("stub must have captured request");
        assert!(req.stream, "request must have stream=true");
        assert!(
            req.resume_at.is_none(),
            "first request must not carry resume_at"
        );
    }

    /// Test 2 — continuation loop: first response returns `more`, second returns `done`.
    /// Asserts the second request echoed `resume_at`, the merged union covers both,
    /// and the cursor is the min of both headers.
    #[test]
    fn streaming_continuation_loop() {
        let h1 = format!("{:064x}", 1u64); // first response
        let h2 = format!("{:064x}", 2u64); // second response
        let cursor_a = 10u64;
        let cursor_b = 5u64; // lower → merged cursor must be 5
        let more_key = "resumekey";

        let response1 = vec![
            (NO_DELAY, stream_header_line(cursor_a)),
            (NO_DELAY, stream_row_line(&h1, "a:tag")),
            (NO_DELAY, stream_more_line(more_key)),
        ];
        let response2 = vec![
            (NO_DELAY, stream_header_line(cursor_b)),
            (NO_DELAY, stream_row_line(&h2, "b:tag")),
            (NO_DELAY, stream_done_line()),
        ];

        let (url, rx) = stub_streaming_ndjson_multi(vec![response1, response2]);

        let snap = RepoClient::new(&url)
            .fetch_buckets_in(
                24,
                &[h1.clone(), h2.clone()],
                None,
                None,
                &NoopObserver,
                true,
            )
            .expect("continuation must succeed");

        // Both hashes in union.
        assert!(snap.tags.contains_key(&h1), "h1 must be in merged snapshot");
        assert!(snap.tags.contains_key(&h2), "h2 must be in merged snapshot");

        // Cursor must be the minimum.
        assert_eq!(
            snap.cursor,
            cursor_b.min(cursor_a),
            "cursor must be min of both headers"
        );

        // First request: no resume_at. Second: resume_at = more_key.
        let req1 = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("first request must be captured");
        let req2 = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("second request must be captured");
        assert!(
            req1.resume_at.is_none(),
            "first request must not carry resume_at"
        );
        assert_eq!(
            req2.resume_at.as_deref(),
            Some(more_key),
            "second request must echo the more key"
        );
        // Both carry the same key list.
        assert_eq!(
            req1.buckets, req2.buckets,
            "both requests must carry the same key list"
        );
    }

    /// Test 3 — in-band `err` trailer: returns a hard error with the server message.
    #[test]
    fn streaming_err_trailer_is_hard_error() {
        let msg = "bucket oversized";
        let h = format!("{:064x}", 99u64);

        let lines = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_err_line(msg)),
        ];
        let (url, _rx) = stub_streaming_ndjson(lines);

        let err = RepoClient::new(&url)
            .fetch_buckets_in(24, &[h], None, None, &NoopObserver, true)
            .expect_err("err trailer must be a hard error");
        assert!(
            err.to_string().contains(msg),
            "error must include the server message; got: {err}"
        );
    }

    /// Test 4 — truncated stream: EOF without a trailer line eventually fails.
    ///
    /// With the #177 retry loop, a truncation no longer aborts immediately —
    /// it retries FLOOR_RETRY_LIMIT times. We supply FLOOR_RETRY_LIMIT truncating
    /// connections and assert the fetch fails with the give-up error (which names
    /// the floor and the retry count).
    #[test]
    fn streaming_truncated_stream_is_error() {
        let h = format!("{:064x}", 77u64);

        // No trailer on any connection — all FLOOR_RETRY_LIMIT connections truncate.
        // Use one key so the window is at the floor (MIN_WINDOW) immediately.
        let truncated = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_row_line(&h, "t:tag")),
            // no trailer
        ];
        // Provide FLOOR_RETRY_LIMIT + 1 connections for safety; the implementation
        // gives up after FLOOR_RETRY_LIMIT floor failures.
        let responses = vec![truncated; FLOOR_RETRY_LIMIT + 1];
        let (url, _rx) = stub_streaming_ndjson_multi(responses);

        let err = RepoClient::new(&url)
            .fetch_buckets_in(24, &[h], None, None, &NoopObserver, true)
            .expect_err("repeated truncation must eventually fail");
        // After #177, the give-up error names the floor retry limit.
        let msg = err.to_string();
        assert!(
            msg.contains("floor") || msg.to_ascii_lowercase().contains("truncat"),
            "error must mention floor retry or truncation; got: {msg}"
        );
    }

    /// Test 5 — `MAX_STREAM_LINE_BYTES` guard: a line longer than the cap errors.
    #[test]
    fn streaming_oversized_line_is_error() {
        // Build a single line larger than MAX_STREAM_LINE_BYTES (8 MiB).
        // We send it BEFORE the header to trigger the guard as early as possible
        // without needing to allocate 8 MiB in the stub; instead just check that
        // the client rejects a well-known-oversized line. We use a much smaller
        // injected cap by subclassing: actually we can't inject the cap easily,
        // so let's fake it: create a line that the client cannot parse as JSON
        // AND is longer than MAX_STREAM_LINE_BYTES + 1. To do that without
        // actually sending 8 MiB over a loopback socket (which would be slow),
        // we instead directly test `read_stream_line` with a synthetic reader.
        //
        // Direct unit test of read_stream_line helper:
        let cap = MAX_STREAM_LINE_BYTES;
        let oversized = vec![b'x'; cap + 2]; // cap+2 bytes, no newline
        let mut reader = std::io::BufReader::new(oversized.as_slice());
        // An oversized line is a Fatal protocol violation.
        let mut buf = Vec::new();
        let we = RepoClient::read_stream_line(&mut reader, &mut buf)
            .expect_err("oversized line must be an error");
        let msg = match we {
            WindowError::Fatal(e) => e.to_string(),
            other => panic!("expected Fatal for oversized line; got {other:?}"),
        };
        assert!(
            msg.contains("MAX_STREAM_LINE_BYTES"),
            "error must name the constant; got: {msg}"
        );

        // Also verify a line exactly at the cap (with trailing newline) succeeds.
        let exactly_at_cap: Vec<u8> = {
            let mut v = vec![b'x'; cap]; // exactly cap bytes of content
            v.push(b'\n'); // plus newline = cap+1 bytes total
            v
        };
        let mut reader2 = std::io::BufReader::new(exactly_at_cap.as_slice());
        let mut buf2 = Vec::new();
        let line = RepoClient::read_stream_line(&mut reader2, &mut buf2)
            .expect("line exactly at cap must succeed")
            .expect("must return Some");
        assert_eq!(line.len(), cap, "content should be exactly cap bytes");
    }

    /// Test 6 — aggregate cap still holds across continuations.
    /// Drive enough rows across two continuation responses to exceed a small
    /// injected `merged_cap`; the `MERGED_RESPONSE_SIZE_CAP` guard must trip.
    #[test]
    fn streaming_aggregate_cap_enforced_across_continuations() {
        // Two responses; each has one row. merged_cap=1 guarantees the first row trips it.
        let h1 = format!("{:064x}", 1u64);
        let h2 = format!("{:064x}", 2u64);
        let more_key = "mk";

        let response1 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_row_line(&h1, "t:tag")),
            (NO_DELAY, stream_more_line(more_key)),
        ];
        let response2 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_row_line(&h2, "t:tag")),
            (NO_DELAY, stream_done_line()),
        ];

        let (url, _rx) = stub_streaming_ndjson_multi(vec![response1, response2]);

        // Use fetch_buckets_inner with merged_cap=1 to trip immediately.
        let err = RepoClient::new(&url)
            .fetch_buckets_inner(
                24,
                &[h1.clone(), h2.clone()],
                None,
                1, // tiny merged_cap
                None,
                &NoopObserver,
                true, // stream=true
            )
            .expect_err("tiny merged_cap must be exceeded");
        assert!(
            err.to_string().contains("MERGED_RESPONSE_SIZE_CAP"),
            "error must name the aggregate cap; got: {err}"
        );
    }

    /// Test 8 — idle-timeout override: streaming uses READ_TIMEOUT (30 s) as the
    /// idle guard, not OVERALL_TIMEOUT (120 s). We shrink the streaming read timeout
    /// via `with_streaming_read_timeout` so the test completes in well under a second.
    ///
    /// Case A: pauses under the timeout → succeeds.
    /// Case B: pauses over the timeout → errors (idle stall detected).
    #[test]
    fn streaming_idle_timeout_override() {
        // Use a very short streaming read timeout so the test is fast.
        let test_read_timeout = std::time::Duration::from_millis(400);
        let short = std::time::Duration::from_millis(50); // well under 400 ms
        let long = std::time::Duration::from_millis(1500); // well over  400 ms

        let h1 = format!("{:064x}", 1u64);
        let h2 = format!("{:064x}", 2u64);

        // ── Case A: all delays are short → should succeed ────────────────────
        let lines_ok = vec![
            (NO_DELAY, stream_header_line(0)),
            (short, stream_row_line(&h1, "a:tag")),
            (short, stream_row_line(&h2, "b:tag")),
            (NO_DELAY, stream_done_line()),
        ];
        let (url_ok, _) = stub_streaming_ndjson(lines_ok);

        RepoClient::with_streaming_read_timeout(&url_ok, test_read_timeout)
            .fetch_buckets_in(
                24,
                &[h1.clone(), h2.clone()],
                None,
                None,
                &NoopObserver,
                true,
            )
            .expect("short delays must not trip the streaming idle timeout");

        // ── Case B: one delay exceeds the timeout → must fail ────────────────
        let lines_fail = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_row_line(&h1, "a:tag")),
            (long, stream_row_line(&h2, "b:tag")), // this delay trips the timeout
            (NO_DELAY, stream_done_line()),
        ];
        let (url_fail, _) = stub_streaming_ndjson(lines_fail);

        RepoClient::with_streaming_read_timeout(&url_fail, test_read_timeout)
            .fetch_buckets_in(
                24,
                &[h1.clone(), h2.clone()],
                None,
                None,
                &NoopObserver,
                true,
            )
            .expect_err("long delay must trip the streaming idle timeout");
    }

    // ── #177: shrink-retry tests ─────────────────────────────────────────────

    /// Alias for [`stub_streaming_ndjson_multi`]: build a multi-connection
    /// streaming stub that accepts exactly as many connections as there are
    /// response vectors. Each element of `responses` is served to one incoming
    /// connection in order.
    fn stub_streaming_multi_n(
        responses: Vec<Vec<(std::time::Duration, String)>>,
    ) -> (String, std::sync::mpsc::Receiver<BucketRequest>) {
        stub_streaming_ndjson_multi(responses)
    }

    /// #177 Test 1 — streaming truncation → shrink → success.
    ///
    /// Uses 64 keys so the initial window (64 buckets, above the MIN_WINDOW floor)
    /// covers all keys. Connection 1 sends a header + zero rows and NO trailer
    /// (truncation, retryable). The wrapper shrinks to 32 keys and connection 2
    /// serves them successfully. A third connection covers the remaining 32 keys.
    ///
    /// Asserts:
    /// - Fetch succeeds.
    /// - Merged snapshot covers connection-2 rows exactly once (no duplicate tags).
    /// - Connection 2's request carries 32 buckets = connection-1's 64 / 2 (shrink).
    /// - Recorder saw exactly 1 WindowRetry with old_window=64, new_window=32.
    #[test]
    fn shrink_retry_streaming_truncation_halve_then_success() {
        let keys = fake_keys(64); // 64 keys > MIN_WINDOW=32, so window can shrink
        let h = format!("{:064x}", 42u64); // a hash we use to test no-double-merge

        // Connection 1: header + no rows + NO trailer → truncated
        let conn1 = vec![(NO_DELAY, stream_header_line(0))];
        // No trailer: fetch_buckets_streaming returns WindowError::Retryable

        // Connection 2: serves keys[0..32] with a row for `h`
        let mut conn2 = vec![(NO_DELAY, stream_header_line(0))];
        // Only emit row for h if h is in the first 32 keys (it is, as it's key 42)
        conn2.push((NO_DELAY, stream_row_line(&h, "test:tag")));
        conn2.push((NO_DELAY, stream_done_line()));

        // Connection 3: serves keys[32..64] with done (no rows for h)
        let conn3 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_done_line()),
        ];

        let (url, rx) = stub_streaming_multi_n(vec![conn1, conn2, conn3]);

        let rec = Recorder::default();
        let snap = RepoClient::new(&url)
            .fetch_buckets_inner(
                24,
                &keys,
                None,
                usize::MAX,
                None,
                &rec,
                true, // stream=true
            )
            .expect("shrink-then-succeed must complete ok");

        // Merged snapshot must contain h exactly once.
        let h_entry = snap
            .tags
            .get(&h)
            .expect("hash h must be in merged snapshot");
        assert_eq!(
            h_entry.len(),
            1,
            "hash h must appear exactly once (no double-merge): got {} tags",
            h_entry.len()
        );

        // Connection 1 (the truncated attempt) sends its request first.
        // Consume it so we can check connection 2 separately.
        let req1 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("connection 1 request must be captured");
        assert_eq!(
            req1.buckets.len(),
            64,
            "connection 1 must have 64 buckets (before shrink)"
        );

        // Connection 2 must have received 32 buckets = connection-1's 64 / 2.
        let req2 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("connection 2 request must be captured");
        assert_eq!(
            req2.buckets.len(),
            32,
            "connection 2 must carry the shrunk window (32 = 64/2); got {}",
            req2.buckets.len()
        );

        // Recorder must have seen exactly 1 WindowRetry.
        let retries: Vec<PullPhase> = rec
            .phases
            .borrow()
            .iter()
            .copied()
            .filter(|p| matches!(p, PullPhase::WindowRetry { .. }))
            .collect();
        assert_eq!(
            retries.len(),
            1,
            "expected exactly 1 WindowRetry; got {}",
            retries.len()
        );

        // The retry must shrink from 64 to 32 and carry RetryReason::Truncation.
        if let PullPhase::WindowRetry {
            old_window,
            new_window,
            attempt,
            reason,
            ..
        } = retries[0]
        {
            assert_eq!(old_window, 64, "old_window must be 64; got {old_window}");
            assert_eq!(new_window, 32, "new_window must be 32; got {new_window}");
            assert_eq!(attempt, 0, "first retry is attempt 0; got {attempt}");
            assert_eq!(
                reason,
                RetryReason::Truncation,
                "streaming truncation must set reason=Truncation; got {reason:?}"
            );
        }
    }

    /// #177 Test 2 — no double-merge on retry.
    ///
    /// Connection 1 sends a row for hash H then truncates.
    /// Connection 2 sends a row for the SAME hash H then done.
    ///
    /// After the retry succeeds, merged.tags[H] must contain exactly ONE tag —
    /// the scratch discard (§3.3) prevents the partial row from being folded.
    #[test]
    fn shrink_retry_no_double_merge() {
        let h = format!("{:064x}", 99u64);
        // 64 keys so the window is above the floor and a shrink happens.
        let keys = fake_keys(64);

        // Connection 1: header + row for H + no trailer → truncated
        let conn1 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_row_line(&h, "first:tag")),
            // NO done/more — truncated
        ];

        // Connection 2: header + row for same H + done
        let conn2 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_row_line(&h, "second:tag")),
            (NO_DELAY, stream_done_line()),
        ];

        // Connection 3: remaining keys after shrink, done
        let conn3 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_done_line()),
        ];

        let (url, _rx) = stub_streaming_multi_n(vec![conn1, conn2, conn3]);

        let snap = RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &NoopObserver, true)
            .expect("retry after truncation must succeed");

        let tags = snap
            .tags
            .get(&h)
            .expect("hash H must be present in merged snapshot");
        assert_eq!(
            tags.len(),
            1,
            "hash H must have exactly one tag entry (no double-merge from scratch discard); \
             got {} entries: {tags:?}",
            tags.len()
        );
        assert_eq!(
            tags[0].tag, "second:tag",
            "the surviving tag must be from connection 2 (the successful retry)"
        );
    }

    /// #177 Test 3 — floor-level exhaustion.
    ///
    /// Keys are exactly MIN_WINDOW=32 so the initial window is at the floor;
    /// all FLOOR_RETRY_LIMIT=3 connections truncate. The fetch must fail with
    /// the give-up error naming the floor and the retry count. The Recorder
    /// must have seen exactly FLOOR_RETRY_LIMIT=3 WindowRetry events.
    #[test]
    fn shrink_retry_floor_exhaustion() {
        let keys = fake_keys(MIN_WINDOW as u32); // exactly MIN_WINDOW keys

        // FLOOR_RETRY_LIMIT = 3 truncating connections; the stub accepts 4 for safety
        // in case an implementation counts differently, but we expect 3.
        let truncated = |_: usize| -> Vec<(std::time::Duration, String)> {
            // header only, no trailer
            vec![(NO_DELAY, stream_header_line(0))]
        };
        let responses: Vec<_> = (0..FLOOR_RETRY_LIMIT + 1).map(truncated).collect();
        let (url, _rx) = stub_streaming_multi_n(responses);

        let rec = Recorder::default();
        let err = RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &rec, true)
            .expect_err("floor exhaustion must return Err");

        let msg = err.to_string();
        assert!(msg.contains("floor"), "error must mention 'floor': {msg}");
        assert!(
            msg.contains(&FLOOR_RETRY_LIMIT.to_string()),
            "error must name FLOOR_RETRY_LIMIT={FLOOR_RETRY_LIMIT}: {msg}"
        );
        assert!(
            msg.contains("0 of") || msg.contains("of 32"),
            "error must name the pull progress: {msg}"
        );

        let retries: Vec<PullPhase> = rec
            .phases
            .borrow()
            .iter()
            .copied()
            .filter(|p| matches!(p, PullPhase::WindowRetry { .. }))
            .collect();
        assert_eq!(
            retries.len(),
            FLOOR_RETRY_LIMIT,
            "must see exactly FLOOR_RETRY_LIMIT={FLOOR_RETRY_LIMIT} WindowRetry events; \
             got {}",
            retries.len()
        );

        // All retry events must report new_window == MIN_WINDOW.
        for p in &retries {
            if let PullPhase::WindowRetry { new_window, .. } = p {
                assert_eq!(
                    *new_window, MIN_WINDOW,
                    "each floor retry must keep new_window == MIN_WINDOW; got {new_window}"
                );
            }
        }
    }

    /// #177 Test 4 — non-retryable HTTP status (500) passes through unchanged
    /// with zero retries, and an in-band {"err":…} trailer also fires no retry.
    #[test]
    fn shrink_retry_fatal_status_no_retry() {
        let keys = fake_keys(64);

        // Case A: HTTP 500 status → fatal, zero retries.
        {
            let url = stub_status(500, "Internal Server Error", "server error".to_string());
            let rec = Recorder::default();
            let err = RepoClient::new(&url)
                .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &rec, true)
                .expect_err("HTTP 500 must fail");
            assert!(
                !err.to_string().is_empty(),
                "must produce an error message: {err}"
            );
            let retry_count = rec
                .phases
                .borrow()
                .iter()
                .filter(|p| matches!(p, PullPhase::WindowRetry { .. }))
                .count();
            assert_eq!(
                retry_count, 0,
                "HTTP 500 must not retry; got {retry_count} retries"
            );
        }

        // Case B: in-band {"err":…} trailer → fatal, zero retries.
        {
            let conn = vec![
                (NO_DELAY, stream_header_line(0)),
                (NO_DELAY, stream_err_line("bucket too large")),
            ];
            let (url, _rx) = stub_streaming_ndjson(conn);
            let rec = Recorder::default();
            let err = RepoClient::new(&url)
                .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &rec, true)
                .expect_err("in-band err trailer must fail");
            assert!(
                err.to_string().contains("bucket too large"),
                "error must propagate server message: {err}"
            );
            let retry_count = rec
                .phases
                .borrow()
                .iter()
                .filter(|p| matches!(p, PullPhase::WindowRetry { .. }))
                .count();
            assert_eq!(
                retry_count, 0,
                "in-band err trailer must not retry; got {retry_count} retries"
            );
        }
    }

    /// #177 Test 5 — retry feeds AIMD.
    ///
    /// First window (64 keys) truncates; second connection serves the shrunk
    /// window (32 keys) successfully. The NEXT window's RequestSent must carry
    /// window == eff_window == 32 (not the original 64), proving §3.4 pins the
    /// next window to the size that worked.
    #[test]
    fn shrink_retry_feeds_aimd() {
        let keys = fake_keys(64);

        // Connection 1: truncated (64-key window)
        let conn1 = vec![(NO_DELAY, stream_header_line(0))]; // no trailer

        // Connection 2: serves keys[0..32] — the shrunk window
        let conn2 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_done_line()),
        ];

        // Connection 3: serves keys[32..64] — the NEXT window (must be 32 keys)
        let conn3 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_done_line()),
        ];

        let (url, rx) = stub_streaming_multi_n(vec![conn1, conn2, conn3]);

        let rec = Recorder::default();
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &rec, true)
            .expect("fetch must succeed after shrink-retry");

        // Drain captured requests. Requests are sent after each connection serves.
        // Order: conn1 (truncated, 64 keys), conn2 (success, 32 keys), conn3 (next window, 32 keys).
        let req_conn1 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("must capture req from conn 1");
        let req_conn2 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("must capture req from conn 2");
        let req_conn3 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("must capture req from conn 3");

        assert_eq!(
            req_conn1.buckets.len(),
            64,
            "connection 1 must have 64 buckets"
        );
        assert_eq!(
            req_conn2.buckets.len(),
            32,
            "connection 2 must have 32 buckets"
        );
        assert_eq!(
            req_conn3.buckets.len(),
            32,
            "connection 3 (next window) must start at eff_window=32, not 64; got {}",
            req_conn3.buckets.len()
        );

        // Also verify the RequestSent phases: the second outer window's RequestSent
        // must carry window=32, not 64.
        let request_sents: Vec<usize> = rec
            .phases
            .borrow()
            .iter()
            .filter_map(|p| {
                if let PullPhase::RequestSent { window, .. } = p {
                    Some(*window)
                } else {
                    None
                }
            })
            .collect();
        // We expect: [64, 32 (shrunk retry), 32 (next window after AIMD pin)]
        // The shrunk retry and the next window both have window=32.
        assert!(
            request_sents.len() >= 2,
            "must see at least 2 RequestSent events"
        );
        // The last RequestSent (the next outer window) must be 32, not 64.
        let last_window = *request_sents.last().unwrap();
        assert_eq!(
            last_window, 32,
            "next window after retry must be eff_window=32 (AIMD pin); got {last_window}"
        );
    }

    /// #177 Test 6 — streaming truncation after a `more` continuation resets
    /// resume_at to None on the retry (§3.5, no intra-window resume in v1).
    ///
    /// Scenario:
    /// - Connection 1: header + rows + more (success within the attempt)
    /// - Connection 2 (continuation within attempt 0): header + rows + NO trailer
    ///   → truncated mid-continuation
    /// - Connection 3 (retry — shrunk window): must carry resume_at=None
    ///   and re-fetch from start of the (shrunk) window.
    #[test]
    fn shrink_retry_streaming_resume_at_reset_on_retry() {
        // Use a small key list so the window stays above the floor.
        let h1 = format!("{:064x}", 1u64);
        let h2 = format!("{:064x}", 2u64);
        let keys = fake_keys(64);

        // Connection 1: serves h1 + more (budget cutoff within attempt 0)
        let more_key = "cursor:k1".to_string();
        let conn1 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_row_line(&h1, "a:tag")),
            (NO_DELAY, stream_more_line(&more_key)),
        ];

        // Connection 2: the continuation of attempt 0; truncates (no trailer)
        let conn2 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_row_line(&h2, "b:tag")),
            // NO done/more — truncated → whole scratch discarded, retry from start
        ];

        // Connection 3: the retry (attempt 1), shrunk window, must have resume_at=None
        let conn3 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_row_line(&h1, "a:tag")), // re-fetches h1 (v1 re-scan)
            (NO_DELAY, stream_done_line()),
        ];

        // Connection 4: remaining keys after shrink (the second outer window)
        let conn4 = vec![
            (NO_DELAY, stream_header_line(0)),
            (NO_DELAY, stream_done_line()),
        ];

        let (url, rx) = stub_streaming_multi_n(vec![conn1, conn2, conn3, conn4]);

        let snap = RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &NoopObserver, true)
            .expect("retry after mid-continuation truncation must succeed");

        // Drain: conn1 and conn2 send requests; conn3 and conn4 send requests.
        // Total: 4 captured requests (one per connection).
        let req1 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("req 1");
        let req2 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("req 2");
        let req3 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("req 3 (retry)");

        // req1: no resume_at (first attempt, first continuation)
        assert!(req1.resume_at.is_none(), "req1 must not carry resume_at");
        // req2: carries resume_at = more_key (continuation within attempt 0)
        assert_eq!(
            req2.resume_at.as_deref(),
            Some(more_key.as_str()),
            "req2 must carry resume_at from the 'more' trailer"
        );
        // req3 (the retry): must NOT carry resume_at (v1 always re-fetches from start)
        assert!(
            req3.resume_at.is_none(),
            "retry request must reset resume_at to None (v1 §3.5); got {:?}",
            req3.resume_at
        );

        // h1 must appear exactly once (scratch discard prevents double-merge).
        let h1_tags = snap.tags.get(&h1).expect("h1 must be in merged snapshot");
        assert_eq!(
            h1_tags.len(),
            1,
            "h1 must appear once after discard+retry (no double-merge); got {}",
            h1_tags.len()
        );
    }

    /// #177 Test 7 — non-streaming (materialized) path retries on transport error.
    ///
    /// First connection: server closes without sending any HTTP response → Transport
    /// error → Retryable. Second connection: serves a normal materialized snapshot.
    /// The fetch must succeed and the Recorder must see a WindowRetry with
    /// reason matching Disconnect.
    #[test]
    fn shrink_retry_non_streaming_transport_error() {
        let keys = fake_keys(64);

        let ok_body = {
            // A snapshot covering keys[0..32] (the shrunk window).
            // We don't control which keys exactly, so just use empty_snapshot_json.
            empty_snapshot_json()
        };

        // Inline stub: connection 1 resets immediately (transport error); connections
        // 2 and 3 serve ok_body as a materialized JSON response. After the reset,
        // fetch_buckets_inner shrinks the window and retries; the remaining keys
        // require one further connection.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind non-stream retry stub");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = std::sync::mpsc::channel::<BucketRequest>();
        let ok_body_clone = ok_body.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Read, Write};
            // Connection 1: accept and immediately close (transport error)
            let (sock1, _) = listener.accept().expect("accept 1");
            drop(sock1);

            // Connections 2 and 3: read request, serve ok_body
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().expect("accept ok conn");
                let mut reader = BufReader::new(sock.try_clone().expect("clone"));
                let mut body_len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        body_len = v.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; body_len];
                reader.read_exact(&mut body).unwrap_or(());
                let req: BucketRequest = serde_json::from_slice(&body).expect("parse request");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    ok_body_clone.len(),
                    ok_body_clone
                );
                sock.write_all(resp.as_bytes()).expect("write response");
                sock.flush().expect("flush");
                tx.send(req).ok();
            }
        });

        let url = format!("http://{addr}");
        let rec = Recorder::default();
        RepoClient::new(&url)
            .fetch_buckets_inner(24, &keys, None, usize::MAX, None, &rec, false) // stream=false
            .expect("non-streaming retry after transport error must succeed");

        // Must have seen at least 1 WindowRetry.
        let retries: Vec<PullPhase> = rec
            .phases
            .borrow()
            .iter()
            .copied()
            .filter(|p| matches!(p, PullPhase::WindowRetry { .. }))
            .collect();
        assert!(
            !retries.is_empty(),
            "non-streaming path must fire WindowRetry on transport error"
        );

        // A connection-close-without-response is not a timeout, so the reason
        // must be Disconnect.
        if let PullPhase::WindowRetry { reason, .. } = retries[0] {
            assert_eq!(
                reason,
                RetryReason::Disconnect,
                "connection reset must set reason=Disconnect; got {reason:?}"
            );
        }

        // Connection 2 must have been issued (we got a request from it).
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("connection 2 must be reached after retry");
    }

    // ── StreamLine untagged disambiguation tests ──

    /// `StreamLine` must route each wire shape to the correct variant (#5.2 §1).
    #[test]
    fn stream_line_untagged_disambiguation() {
        // Row line
        let row_json = r#"{"h":"aabbcc","t":[{"tag":"character:samus","origin":null}]}"#;
        let sl: StreamLine = serde_json::from_str(row_json).expect("row must parse");
        assert!(
            matches!(sl, StreamLine::Row(_)),
            "row JSON must map to StreamLine::Row"
        );

        // Done trailer
        let done_json = r#"{"done":true}"#;
        let sl: StreamLine = serde_json::from_str(done_json).expect("done must parse");
        assert!(
            matches!(sl, StreamLine::Trailer(StreamTrailer::Done { done: true })),
            "done trailer must map to StreamLine::Trailer(Done)"
        );

        // More trailer
        let more_json = r#"{"more":"aabbccdd"}"#;
        let sl: StreamLine = serde_json::from_str(more_json).expect("more must parse");
        assert!(
            matches!(sl, StreamLine::Trailer(StreamTrailer::More { .. })),
            "more trailer must map to StreamLine::Trailer(More)"
        );

        // Err trailer
        let err_json = r#"{"err":"something broke"}"#;
        let sl: StreamLine = serde_json::from_str(err_json).expect("err must parse");
        assert!(
            matches!(sl, StreamLine::Trailer(StreamTrailer::Err { .. })),
            "err trailer must map to StreamLine::Trailer(Err)"
        );

        // Garbage line — must fail (feeds classify_bad_stream_line).
        let bad_json = r#"{"zz":1}"#;
        assert!(
            serde_json::from_str::<StreamLine>(bad_json).is_err(),
            "garbage line must fail untagged parse"
        );
    }

    /// `classify_bad_stream_line` must emit the same three distinct Fatal message
    /// prefixes as the old two-pass decoder (#5.2 §2).
    #[test]
    fn bad_line_error_message_parity() {
        let url = "http://example.com";

        // Non-JSON input → "bad NDJSON line"
        let we = classify_bad_stream_line(url, "not json at all {{{");
        let msg = match we {
            WindowError::Fatal(e) => e.to_string(),
            other => panic!("expected Fatal; got {other:?}"),
        };
        assert!(
            msg.contains("bad NDJSON line"),
            "non-JSON must say 'bad NDJSON line'; got: {msg}"
        );

        // Valid JSON with "h" but invalid row (t is not an array) → "bad row line"
        let we = classify_bad_stream_line(url, r#"{"h":"aa","t":"notarray"}"#);
        let msg = match we {
            WindowError::Fatal(e) => e.to_string(),
            other => panic!("expected Fatal; got {other:?}"),
        };
        assert!(
            msg.contains("bad row line"),
            "malformed row must say 'bad row line'; got: {msg}"
        );

        // Valid JSON without "h" but invalid trailer (more is not a string) → "bad trailer line"
        let we = classify_bad_stream_line(url, r#"{"more":123}"#);
        let msg = match we {
            WindowError::Fatal(e) => e.to_string(),
            other => panic!("expected Fatal; got {other:?}"),
        };
        assert!(
            msg.contains("bad trailer line"),
            "malformed trailer must say 'bad trailer line'; got: {msg}"
        );
    }
}
