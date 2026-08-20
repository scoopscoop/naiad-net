//! `naiad-api` — the wire contract shared by the daemon and its clients.
//!
//! Pure data: serde DTOs plus the route path constants, so the daemon's router
//! and every client build URLs from one source of truth. No HTTP, DB, or async
//! deps live here (mirrors `core`'s dependency-light philosophy).

use serde::{Deserialize, Serialize};

/// One file in a `list` or `search` response.
///
/// `hash` is the identity: reference a file by `hash` (e.g. `/file/{hash}`,
/// tag requests), never by round-tripping `path`. `name`/`path` are best-effort
/// *display* strings — a non-UTF-8 on-disk name is rendered lossily (`U+FFFD`),
/// so they must not be used as a key back into the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDto {
    pub hash: String,
    pub name: String,
    pub size: u64,
    pub path: String,
    pub imported_at: i64,
    pub created_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub mime: Option<String>,
}

/// Request body for `POST /api/scan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReq {
    pub folder: String,
}

/// Result of a scan. `errors` lists per-file failures that were skipped (not
/// fatal); `imported` and `marked_missing` mirror the indexer's summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanSummary {
    pub imported: usize,
    pub marked_missing: usize,
    pub errors: Vec<ScanError>,
}

/// One skipped file during a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanError {
    pub path: String,
    pub message: String,
}

/// A progress tick streamed during a scan over `GET /api/scan/stream`.
/// `total` is the determinate count of supported images that will be walked
/// (computed once before the scan via a cheap path-only pre-count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanProgress {
    pub imported: u64,
    pub skipped: u64,
    pub total: u64,
}

/// Request body for `POST /api/tags/add` and `/api/tags/remove`. `file` is a
/// path-or-hash reference resolved by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagsReq {
    pub file: String,
    pub tags: Vec<String>,
}

/// A sibling alias `bad -> ideal`. Used both as a list item (`GET /api/siblings`)
/// and as the `POST /api/siblings/add` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiblingDto {
    pub bad: String,
    pub ideal: String,
}

/// Request body for `POST /api/siblings/remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiblingRemoveReq {
    pub bad: String,
}

/// A parent implication `child -> parent`. Used as a list item
/// (`GET /api/parents`), the add body, and the remove body (both fields needed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentDto {
    pub child: String,
    pub parent: String,
}

/// A subscribed tag repository: `POST /api/repos` body, `GET /api/repos` item.
/// `url` is the repository's base URL (it serves `GET /repo/snapshot`).
///
/// The `origin` field was removed in v0.2.6x (#166): the `repo add --origin`
/// flag was inert (no local producer stamps a non-null origin). The Hydrus
/// bridge (#124) is the first intended producer; origin wiring is deferred
/// to that issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDto {
    pub name: String,
    pub url: String,
    /// The effective privacy ceiling (max bucket-query width, prefix bits) that
    /// governs pulls from this repo — the #169 per-repo override if set, else the
    /// global `[privacy].max_query_bits` (#179). Always resolvable, so always
    /// `Some` when the daemon populates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_query_bits: Option<u32>,
    /// This repo's advertised minimum query width (prefix bits), from its cached
    /// caps (#179). `Some` only once the repo has been handshaken this session and
    /// runs a snapshot backend; `None` otherwise (unknown until first pull, or the
    /// repo enforces no floor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_query_bits: Option<u32>,
    /// This repo's advertised bucket width (prefix bits) from its cached caps —
    /// `Some` once handshaken this session and serving in bucketed mode, else
    /// `None`. The UI shows the EFFECTIVE width `min(advertised, ceiling)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_bits: Option<u32>,
    /// This repo's distinct-hash count from its cached caps. `Some` once
    /// handshaken this session against a count-advertising server. Drives the
    /// crowd↔bits conversion and the download estimate; `None` → the UI shows a
    /// bits-only control with no size estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

/// Request body for `POST /api/repos`. `name` is an optional fallback used
/// only when the repo's caps advertise no name (older servers); the
/// server-advertised name always wins. Absent both → the URL hostname.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoAddReq {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Request body for `POST /api/repos/pull`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPullReq {
    pub name: String,
}

/// Result of a pull: how many owned files matched the snapshot and how many
/// mapping rows the service holds afterward (each pull is authoritative).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPullSummary {
    pub matched_files: u64,
    pub mappings: u64,
    /// Advisory notice from the daemon, e.g. a floor clamp-up warning (#179).
    /// `Some` at most once per repo+domain per daemon session; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// Request body for `POST /api/files/pull-tags`: hex hashes of the files to
/// pull tags for (the Inspector sends the current selection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePullReq {
    pub hashes: Vec<String>,
}

/// One repo's outcome in a per-file pull. `error` present = that repo failed
/// (unreachable, protocol mismatch, ...); the others still merged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePullRepoResult {
    pub repo: String,
    pub mappings_added: u64,
    /// How many of the requested files this repo could not be asked about,
    /// because they have no SHA-256 interop hash. Without it the caller cannot
    /// tell "upstream has no tags for this file" from "we never asked" (#144),
    /// and so cannot prompt the backfill that would fix it. Always 0 for a repo
    /// that serves no SHA-256 domain. **Not additive across repos** — every
    /// SHA-256 repo reports the same un-resolvable files, so a caller
    /// aggregating several results wants the maximum, not the sum.
    #[serde(default)]
    pub missing_sha256: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Advisory notice from the daemon, e.g. a floor clamp-up warning (#179).
    /// `Some` at most once per repo+domain per daemon session; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// SSE `connecting` event: emitted once per repo, before its remote fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullConnecting {
    pub repo: String,
    pub index: usize,
    pub total: usize,
}

/// SSE `progress` event: emitted after each repo completes; cumulative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullProgress {
    pub repos_done: usize,
    pub repos_total: usize,
    /// The repo that just finished.
    pub repo: String,
    /// Cumulative matched files across repos so far.
    pub matched_files: u64,
    /// Cumulative new mappings across repos so far.
    pub mappings: u64,
}

/// One repo's outcome in the terminal `summary` event of a streamed pull.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRepoOutcome {
    pub repo: String,
    pub matched_files: u64,
    pub mappings: u64,
    /// Requested files with no SHA-256 interop hash, so this repo was never
    /// asked about them. See [`FilePullRepoResult::missing_sha256`] — same
    /// meaning, same "aggregate with max, not sum" caveat.
    #[serde(default)]
    pub missing_sha256: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Advisory notice from the daemon, e.g. a floor clamp-up warning (#179).
    /// `Some` at most once per repo+domain per daemon session; `None` otherwise.
    /// Mirrors [`FilePullRepoResult::notice`] for the streamed pull path (#192).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// SSE `summary` event: final, once. Cumulative totals plus per-repo results.
///
/// There is deliberately no cumulative `missing_sha256` here: the counts in
/// `results` describe the *same* set of un-resolvable files repo after repo, so
/// summing them would multiply one problem by the repo count. Take the maximum
/// over `results` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullSummary {
    pub results: Vec<PullRepoOutcome>,
    pub matched_files: u64,
    pub mappings: u64,
}

/// SSE `stage` event: sub-repo progress within one repo's fetch. Additive
/// (#172); a client that does not recognise the event ignores it and falls back
/// to per-repo `connecting`/`progress` granularity. Emitted many times per repo
/// (bounded by the request chunk count), so it carries no cumulative cross-repo
/// totals — only this repo's position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullStage {
    /// The repo currently being fetched.
    pub repo: String,
    /// 1-based repo index within the pull, and the repo count — identical to
    /// `PullConnecting.index`/`.total`, so the UI can place the sub-progress
    /// inside the correct repo slice without tracking `connecting` separately.
    pub index: usize,
    pub total: usize,
    /// `"request"` | `"chunk"` | `"merging"` | `"done"` — the `PullPhase` mapped
    /// to a stable string. Stringly-typed so the wire stays additive if a phase
    /// is ever added.
    pub phase: String,
    /// 1-based bucket index and bucket count for the active fetch; `0`/`0` when
    /// the phase carries no bucket (`merging`, `done`). Previously carried the
    /// chunk index; now carries buckets done / buckets total (#174). The ratio
    /// `chunk / chunk_total` remains a valid 0→1 fraction so old UIs render a
    /// correct progress fraction without any changes.
    #[serde(default)]
    pub chunk: usize,
    /// See `chunk`. Bucket count for the active fetch window; `0` when the
    /// phase carries no bucket (`merging`, `done`, WholeRepo single request).
    #[serde(default)]
    pub chunk_total: usize,
    /// Cumulative bytes received for THIS repo so far (across both hash
    /// domains). The label figure; monotonic within a repo.
    #[serde(default)]
    pub bytes: u64,
    /// Hash domain of the active fetch (`"blake3"`/`"sha256"`), for the log and
    /// to explain a mid-repo chunk-count reset. Omitted for `merging`/`done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Cumulative distinct hashes merged for THIS repo so far. Label figure.
    #[serde(default)]
    pub hashes: u64,
    /// Cumulative tag entries merged for THIS repo so far. Label figure.
    #[serde(default)]
    pub tags: u64,
    /// Wall time since this repo's fetch began, ms. Informational.
    #[serde(default)]
    pub elapsed_ms: u64,
    /// Current adaptive window size in buckets (informational; 0 for
    /// merging/done and the WholeRepo single request). Lets a debugger see the window adapting.
    #[serde(default)]
    pub window: usize,
    /// Cumulative window shrink-retries for THIS repo so far (#177). A non-zero
    /// value tells the UI the pull hit transport stalls and recovered by
    /// shrinking windows. Additive; `#[serde(default)]` keeps old
    /// daemons/UIs interoperable.
    #[serde(default)]
    pub retries: u64,
}

/// SSE `error` event: stream-fatal only (no repos, bad request).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullError {
    pub message: String,
}

/// Request body for `POST /api/repos/submit`. `file` is a path-or-hash reference;
/// `op` is `"add"` or `"remove"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitReq {
    pub name: String,
    pub file: String,
    pub tag: String,
    pub op: String,
}

/// Request body for `POST /api/relations/submit`. `kind` is `"sibling"` or
/// `"parent"`; `op` is `"add"` or `"remove"`. For a sibling, `from` is the alias
/// and `to` the ideal; for a parent, `from` is the child and `to` the parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationSubmitReq {
    pub name: String,
    pub kind: String,
    pub from: String,
    pub to: String,
    pub op: String,
}

/// Request body for `POST /api/relations/pull`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationPullReq {
    pub name: String,
}

/// Result of a relation pull: how many sibling and parent edges the service holds
/// afterward (each pull is authoritative).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationPullSummary {
    pub siblings: u64,
    pub parents: u64,
}

/// A relation edge with provenance: `GET /api/relations` list item. `kind` is
/// `"sibling"` or `"parent"`; `author` is the submitter's public-key hex for a
/// pulled edge, `None` for a locally-created one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationEdgeDto {
    pub kind: String,
    pub from: String,
    pub to: String,
    pub service: String,
    pub author: Option<String>,
}

/// Per-service relation summary: `GET /api/relations/status` list item.
/// `last_pull` is unix-seconds of the last relation pull, or `None` if never.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationStatusDto {
    pub service: String,
    pub siblings: u64,
    pub parents: u64,
    pub last_pull: Option<i64>,
}

/// The local account, as seen by `GET /api/account`. `public_key` is `None` until
/// the key is created (on first submit); `key_path` is always the file location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDto {
    pub public_key: Option<String>,
    pub key_path: String,
}

/// One registered plugin: `GET /api/plugins` item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDto {
    pub id: String,
    pub name: String,
    pub tagger: bool,
    pub processor: bool,
    pub source: bool,
}

/// Request body for `POST /api/hydrus/configure`. `tag_services` empty = all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HydrusConfigReq {
    pub dir: String,
    pub tag_services: Vec<i64>,
}

/// Response body for `GET /api/hydrus/config`. `dir: None` = not yet configured;
/// `tag_services` empty = all services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HydrusConfigDto {
    pub dir: Option<String>,
    pub tag_services: Vec<i64>,
}

/// Request body for `POST /api/tagger/lookup`. `files` are hash references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggerLookupReq {
    pub plugin_id: String,
    pub files: Vec<String>,
    pub apply: bool,
}

/// Candidate tags for one file: `POST /api/tagger/lookup` response item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggerLookupItem {
    pub file: String,
    pub tags: Vec<String>,
}

/// Request body for `POST /api/source/import`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceImportReq {
    pub plugin_id: String,
    /// When true, import tags only for files already in the library (matched by
    /// SHA-256) rather than every file the source owns. Defaults to false (full
    /// import) for older clients.
    #[serde(default)]
    pub library_only: bool,
}

/// A progress tick streamed during a library import over
/// `GET /api/source/import/stream`. `files`/`total` count files processed;
/// `mappings` is the running total of tags applied so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportProgress {
    pub files: u64,
    pub total: u64,
    pub mappings: u64,
}

/// Result of a bulk import: `POST /api/source/import` response.
/// `siblings`/`parents` count applied (non-deleted, non-self) relations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceImportSummary {
    pub mappings_staged: u64,
    pub mappings_resolved: u64,
    pub siblings: u64,
    pub parents: u64,
    pub sha256_backfilled: u64,
}

/// Result of a relations-only Hydrus import: `POST /api/hydrus/relations`
/// response and the terminal `summary` event of its SSE stream (issue #41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationsImportSummary {
    pub siblings: u64,
    pub parents: u64,
}

/// One `progress` SSE event of `GET /api/hydrus/relations/stream`. Determinate:
/// `edges_total` is known up front from the Hydrus relation tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationsProgress {
    pub edges_done: u64,
    pub edges_total: u64,
    pub siblings: u64,
    pub parents: u64,
}

/// Request body for `POST /api/backup`. `dest: None` (or absent) triggers the
/// default destination: `<db_dir>/backups/naiad-YYYYMMDD-HHMMSS.db`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupReq {
    pub dest: Option<String>,
}

/// Result of a `POST /api/backup`: the path written, file size, and elapsed time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSummary {
    pub dest: String,
    pub bytes: u64,
    pub duration_ms: u64,
}

/// Gallery sort preference persisted as internal UI state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GallerySortDto {
    pub key: String,
    pub direction: String,
}

// Route paths — the single source of truth for daemon router and clients.
pub const THUMB_STREAM: &str = "/thumb-stream";
pub const API_SCAN: &str = "/api/scan";
pub const API_SCAN_STREAM: &str = "/api/scan/stream";
pub const API_FILES: &str = "/api/files";
pub const API_SEARCH: &str = "/api/search";
pub const API_TAGS: &str = "/api/tags";
pub const API_TAGS_DETAILED: &str = "/api/tags/detailed";
pub const API_TAGS_RELATIONS: &str = "/api/tags/relations";
pub const API_TAGS_ADD: &str = "/api/tags/add";
pub const API_TAGS_REMOVE: &str = "/api/tags/remove";
pub const API_TAGS_COMPLETE: &str = "/api/tags/complete";
pub const API_NAMESPACES: &str = "/api/namespaces";
pub const API_SIBLINGS: &str = "/api/siblings";
pub const API_SIBLINGS_ADD: &str = "/api/siblings/add";
pub const API_SIBLINGS_REMOVE: &str = "/api/siblings/remove";
pub const API_PARENTS: &str = "/api/parents";
pub const API_PARENTS_ADD: &str = "/api/parents/add";
pub const API_PARENTS_REMOVE: &str = "/api/parents/remove";
pub const API_ROOTS: &str = "/api/roots";
/// `GET /api/repos` — list subscribed repositories.
/// `POST /api/repos` — subscribe; body is `{"url":"…","name":"…(optional)"}`.
/// Validates the `/repo/caps` handshake first; uses the server-advertised name
/// if present, else the client-supplied `name`, else the URL hostname. Suffixes
/// (`-2`, `-3`, …) resolve name collisions. 400 if the URL is already
/// subscribed. `DELETE /api/repos?name=X` — detach (tags kept). Add
/// `&purge=true` to delete every tag the repo contributed (irreversible). 404
/// if not subscribed.
pub const API_REPOS: &str = "/api/repos";
pub const API_REPOS_PULL: &str = "/api/repos/pull";
pub const API_REPOS_SUBMIT: &str = "/api/repos/submit";
pub const API_RELATIONS_SUBMIT: &str = "/api/relations/submit";
pub const API_RELATIONS_PULL: &str = "/api/relations/pull";
pub const API_RELATIONS: &str = "/api/relations";
pub const API_RELATIONS_STATUS: &str = "/api/relations/status";
pub const API_REPOS_PRIORITY: &str = "/api/repos/priority";
pub const API_REPOS_QUERY_BITS: &str = "/api/repos/query-bits";
pub const API_FILES_PULL_TAGS: &str = "/api/files/pull-tags";
pub const API_FILES_PULL_TAGS_STREAM: &str = "/api/files/pull-tags/stream";
pub const API_ACCOUNT: &str = "/api/account";
pub const API_BLOCKS: &str = "/api/blocks";
pub const API_REJECT: &str = "/api/reject";
pub const API_REJECTIONS: &str = "/api/rejections";
pub const API_HEALTH: &str = "/api/health";
pub const API_VIEW_SORT: &str = "/api/view/sort";

/// Request body for `POST /api/repos/priority`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPriorityReq {
    pub name: String,
    pub priority: i64,
}

/// Request body for `POST /api/repos/query-bits`. `max_query_bits: None`
/// clears the per-repo override so the repo falls back to the global ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoQueryBitsReq {
    pub name: String,
    #[serde(default)]
    pub max_query_bits: Option<u32>,
}

/// A block rule: `GET /api/blocks` list item. `kind` is `"tag"`,
/// `"tag_pattern"`, or `"author"`; `target` is the matched value; `id` is used
/// by `DELETE /api/blocks?id=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRuleDto {
    pub id: i64,
    pub kind: String,
    pub target: String,
    pub note: Option<String>,
    pub created_at: i64,
}

/// Request body for `POST /api/blocks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockAddReq {
    pub kind: String,
    pub target: String,
    pub note: Option<String>,
}

/// Body of `POST /api/reject` — reject one pulled mapping (ADR 0020 §6).
#[derive(Debug, Serialize, Deserialize)]
pub struct RejectRequest {
    pub hash: String,
    pub tag: String,
    pub service: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// Response of `POST /api/reject`: whether the repo advertises the reports
/// capability, so the UI can offer escalation without a second round-trip.
#[derive(Debug, Serialize, Deserialize)]
pub struct RejectResponse {
    pub reports: bool,
}

/// Body of `POST /api/report` — file an anonymous report against a pulled
/// mapping, forwarded to the originating repository.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReportRequest {
    pub hash: String,
    pub tag: String,
    pub service: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// One rejection row for the per-file "Rejected" disclosure.
#[derive(Debug, Serialize, Deserialize)]
pub struct RejectionDto {
    /// Blake3 hex hash of the rejected file. Always present so unscoped
    /// `GET /api/rejections` rows are distinguishable across files.
    pub hash: String,
    pub service: String,
    pub tag: String,
    pub note: Option<String>,
    pub created_at: i64,
}

/// A tag completion suggestion (`GET /api/tags/complete` → `tags[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSuggestionDto {
    pub namespace: String,
    pub subtag: String,
    pub count: i64,
    /// The surfacing alias, pre-formatted (`character:badtag` / bare `badtag`),
    /// display-only. `Some` on rows surfaced via an alias, `None` otherwise.
    /// Absent from the wire when `None` so non-alias rows stay byte-identical
    /// to the pre-#116 shape; old clients that see no key deserialise to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_source: Option<String>,
}

/// A namespace completion suggestion (`GET /api/tags/complete` → `namespaces[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceSuggestionDto {
    pub namespace: String,
    pub tag_count: i64,
}

/// Response body for `GET /api/tags/complete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteResponse {
    pub namespaces: Vec<NamespaceSuggestionDto>,
    pub tags: Vec<TagSuggestionDto>,
}

pub const API_BACKUP: &str = "/api/backup";
pub const API_REPORT: &str = "/api/report";
pub const API_PLUGINS: &str = "/api/plugins";
pub const API_HYDRUS_CONFIGURE: &str = "/api/hydrus/configure";
pub const API_HYDRUS_CONFIG: &str = "/api/hydrus/config";
pub const API_TAGGER_LOOKUP: &str = "/api/tagger/lookup";
pub const API_SOURCE_IMPORT: &str = "/api/source/import";
pub const API_SOURCE_IMPORT_STREAM: &str = "/api/source/import/stream";
pub const API_HYDRUS_RELATIONS: &str = "/api/hydrus/relations";
pub const API_HYDRUS_RELATIONS_STREAM: &str = "/api/hydrus/relations/stream";

/// A displayed tag with its presence and the shared service names that carry it
/// for this file. `presence` is `"local" | "pulled" | "both"` — which of this
/// client's services supply the tag. `services` lists the display names of
/// shared repos supplying the tag; empty when `presence == "local"`. Used by the
/// client to call `rejectTag` per service. `relations` is true iff the tag has at
/// least one relation (alias/parent/child); drives the detail-chip relations glyph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagDetailDto {
    pub tag: String,
    pub presence: String,
    pub services: Vec<String>,
    /// True iff the tag has at least one relation (alias/parent/child); drives
    /// the detail-chip relations glyph. Additive — old clients tolerate absence.
    #[serde(default)]
    pub relations: bool,
    /// Generation source (ADR 0026): the tool that produced this tag, or None =
    /// manual/local. Distinct from `presence` (which of my services carry it).
    /// Display/filter metadata only — asserted, not proven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// One tag in a relation section: the tag string and the number of files
/// carrying it in the local library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationTagDto {
    pub tag: String,
    pub count: i64,
}

/// A paginated section of related tags (aliases, parents, or children).
/// `total` is the full relation count from the DB; `items.len()` is the
/// truncated display set — `total - items.len()` gives the "… N more" row
/// the UI shows when there are more relations than the display limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationSectionDto {
    pub items: Vec<RelationTagDto>,
    pub total: usize,
}

/// Full relation graph for one tag: response body of `GET /api/tags/relations`.
/// `canonical` is the resolved canonical form after alias-following; `count` is
/// the merged display count for the canonical (raw of canonical + Σ raw of its
/// aliases); `via_alias` is true when the queried tag was itself an alias (so
/// the UI can show a "redirected from …" notice). `aliases`, `parents`, and
/// `children` are each a capped section — see `RelationSectionDto.total` for
/// the "… N more" row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRelationsDto {
    pub canonical: String,
    /// Merged display count: raw(canonical) + Σ raw(aliases). 0 if unmapped.
    pub count: i64,
    pub via_alias: bool,
    pub aliases: RelationSectionDto,
    pub parents: RelationSectionDto,
    pub children: RelationSectionDto,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_dto_round_trips() {
        let f = FileDto {
            hash: "a".repeat(64),
            name: "pic.png".into(),
            size: 1234,
            path: "/lib/pic.png".into(),
            imported_at: 100,
            created_at: Some(80),
            modified_at: Some(90),
            mime: Some("image/png".into()),
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: FileDto = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
        // Field names are the wire contract the gallery JS reads.
        assert!(json.contains("\"hash\""));
        assert!(json.contains("\"name\""));
        assert!(json.contains("\"size\""));
        assert!(json.contains("\"imported_at\""));
        assert!(json.contains("\"created_at\""));
        assert!(json.contains("\"modified_at\""));
        assert!(json.contains("\"mime\""));
    }

    #[test]
    fn repo_add_req_name_absent() {
        // A body with no `name` field must deserialise cleanly with name = None.
        let req: RepoAddReq = serde_json::from_str(r#"{"url":"http://x"}"#).unwrap();
        assert_eq!(req.url, "http://x");
        assert!(req.name.is_none(), "absent name must deserialise as None");

        // A body with an explicit name must deserialise as Some.
        let req: RepoAddReq = serde_json::from_str(r#"{"url":"http://x","name":"mine"}"#).unwrap();
        assert_eq!(req.name, Some("mine".to_string()));
    }

    #[test]
    fn repo_dtos_round_trip() {
        let r = RepoDto {
            name: "ptr".into(),
            url: "http://127.0.0.1:9090".into(),
            max_query_bits: None,
            min_query_bits: None,
            advertised_bits: None,
            count: None,
        };
        let back: RepoDto = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);

        let s = RepoPullSummary {
            matched_files: 3,
            mappings: 7,
            notice: None,
        };
        let back: RepoPullSummary =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);

        let p = RepoPullReq { name: "ptr".into() };
        let back: RepoPullReq = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(p, back);
    }

    /// #144: the per-file pull DTOs must carry `missing_sha256` on the wire —
    /// it is the only thing that lets a caller tell "upstream has no tags for
    /// this file" from "we never asked". It is also the newest field, so the
    /// absent case has to keep deserializing for older payloads.
    #[test]
    fn per_file_pull_dtos_carry_missing_sha256() {
        let r = FilePullRepoResult {
            repo: "ptr".into(),
            mappings_added: 7,
            missing_sha256: 2,
            error: None,
            notice: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            json.contains("\"missing_sha256\":2"),
            "the count must reach the client, not die in a WARN log: {json}"
        );
        assert_eq!(
            r,
            serde_json::from_str::<FilePullRepoResult>(&json).unwrap()
        );

        let o = PullRepoOutcome {
            repo: "ptr".into(),
            matched_files: 3,
            mappings: 7,
            missing_sha256: 2,
            error: None,
            notice: None,
        };
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains("\"missing_sha256\":2"), "{json}");
        // notice is skip_serializing_if None, so a plain outcome omits it.
        assert!(!json.contains("notice"), "{json}");
        assert_eq!(o, serde_json::from_str::<PullRepoOutcome>(&json).unwrap());

        // Additive: a payload predating the field reads back as "none missing".
        let old: PullRepoOutcome =
            serde_json::from_str(r#"{"repo":"ptr","matched_files":3,"mappings":7}"#).unwrap();
        assert_eq!(old.missing_sha256, 0);
        let old: FilePullRepoResult =
            serde_json::from_str(r#"{"repo":"ptr","mappings_added":7}"#).unwrap();
        assert_eq!(old.missing_sha256, 0);
    }

    /// #192: the streamed summary's per-repo rows carry the #179 floor-clamp
    /// notice, mirroring `FilePullRepoResult`. It is additive, so a payload
    /// predating the field must still deserialize (`notice` = `None`).
    #[test]
    fn pull_repo_outcome_carries_notice() {
        let o = PullRepoOutcome {
            repo: "ptr".into(),
            matched_files: 3,
            mappings: 7,
            missing_sha256: 0,
            error: None,
            notice: Some("repo ptr: privacy ceiling below floor; querying at 16 bits".into()),
        };
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains("\"notice\":\"repo ptr:"), "{json}");
        assert_eq!(o, serde_json::from_str::<PullRepoOutcome>(&json).unwrap());

        // Additive: an older daemon's row (no notice key) reads back as None.
        let old: PullRepoOutcome = serde_json::from_str(
            r#"{"repo":"ptr","matched_files":3,"mappings":7,"missing_sha256":0}"#,
        )
        .unwrap();
        assert_eq!(old.notice, None);
    }

    #[test]
    fn scan_summary_round_trips() {
        let s = ScanSummary {
            imported: 3,
            marked_missing: 1,
            errors: vec![ScanError {
                path: "/x".into(),
                message: "boom".into(),
            }],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ScanSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn submit_and_account_dtos_round_trip() {
        let r = SubmitReq {
            name: "ptr".into(),
            file: "a".repeat(64),
            tag: "character:samus".into(),
            op: "add".into(),
        };
        let back: SubmitReq = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn relation_submit_req_round_trips() {
        let r = RelationSubmitReq {
            name: "ptr".into(),
            kind: "sibling".into(),
            from: "character:samus_aran".into(),
            to: "character:samus".into(),
            op: "add".into(),
        };
        let back: RelationSubmitReq =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn relation_pull_summary_round_trips() {
        let s = RelationPullSummary {
            siblings: 3,
            parents: 2,
        };
        let back: RelationPullSummary =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn tag_relations_dto_round_trips() {
        let r = TagRelationsDto {
            canonical: "character:samus".into(),
            count: 51,
            via_alias: true,
            aliases: RelationSectionDto {
                items: vec![RelationTagDto {
                    tag: "samus_aran".into(),
                    count: 7,
                }],
                total: 3,
            },
            parents: RelationSectionDto {
                items: vec![RelationTagDto {
                    tag: "series:metroid".into(),
                    count: 40,
                }],
                total: 1,
            },
            children: RelationSectionDto {
                items: vec![],
                total: 0,
            },
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: TagRelationsDto = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        // Verify `relations` default works on TagDetailDto (serde(default)).
        let partial = r#"{"tag":"x","presence":"local","services":[]}"#;
        let d: TagDetailDto = serde_json::from_str(partial).unwrap();
        assert!(!d.relations);
    }

    #[test]
    fn relation_edge_dto_round_trips() {
        let e = RelationEdgeDto {
            kind: "sibling".into(),
            from: "samus".into(),
            to: "character:samus".into(),
            service: "ptr".into(),
            author: Some("aa".repeat(32)),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: RelationEdgeDto = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn relation_status_dto_round_trips() {
        let s = RelationStatusDto {
            service: "local".into(),
            siblings: 3,
            parents: 1,
            last_pull: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: RelationStatusDto = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn account_dto_round_trips() {
        let a = AccountDto {
            public_key: Some("ab".repeat(32)),
            key_path: "/lib/naiad.key".into(),
        };
        let back: AccountDto = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn block_dtos_round_trip() {
        let r = BlockRuleDto {
            id: 3,
            kind: "tag_pattern".into(),
            target: "meme:*".into(),
            note: Some("noise".into()),
            created_at: 1234,
        };
        let back: BlockRuleDto = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);

        let a = BlockAddReq {
            kind: "author".into(),
            target: "ab".repeat(32),
            note: None,
        };
        let back: BlockAddReq = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn plugin_dtos_round_trip() {
        let p = PluginDto {
            id: "hydrus".into(),
            name: "Hydrus importer".into(),
            tagger: true,
            processor: false,
            source: true,
        };
        let back: PluginDto = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(p, back);
        let s = SourceImportSummary {
            mappings_staged: 3,
            mappings_resolved: 2,
            siblings: 1,
            parents: 0,
            sha256_backfilled: 5,
        };
        let back: SourceImportSummary =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
        let r = RelationsImportSummary {
            siblings: 2,
            parents: 1,
        };
        let back: RelationsImportSummary =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
        let p = RelationsProgress {
            edges_done: 4096,
            edges_total: 614_000,
            siblings: 4000,
            parents: 96,
        };
        let back: RelationsProgress =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn hydrus_config_dto_round_trips() {
        let h = HydrusConfigDto {
            dir: Some("/db".into()),
            tag_services: vec![1, 2],
        };
        let back: HydrusConfigDto =
            serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(h, back);
    }
}
