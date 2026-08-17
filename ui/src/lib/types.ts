/** Mirror of `naiad_api::FileDto` — one library file as returned by the API. */
export interface FileDto {
  hash: string;
  name: string;
  size: number;
  path: string;
  /** Unix seconds when this content first entered the Naiad library. */
  imported_at: number;
  /** Filesystem creation time for the displayed path, when the OS reports it. */
  created_at: number | null;
  /** Filesystem modification time for the displayed path, when the OS reports it. */
  modified_at: number | null;
  /** Detected MIME type, when content metadata extraction knows it. */
  mime: string | null;
}

/** Mirror of `naiad_api::ScanError` — one file skipped during a scan. */
export interface ScanError {
  path: string;
  message: string;
}

/** Mirror of `naiad_api::ScanSummary` — the result of a scan. */
export interface ScanSummary {
  imported: number;
  marked_missing: number;
  errors: ScanError[];
}

/** Mirror of `naiad_api::ScanProgress` — a progress tick during a scan. */
export interface ScanProgress {
  imported: number;
  skipped: number;
  /** Supported-image count from the pre-count walk; the bar's denominator.
   *  `0` (empty tree) → the UI shows an indeterminate bar. */
  total: number;
}

/** Mirror of `naiad_api::TagDetailDto` — a displayed tag with its presence and
 *  the shared service names that supply it for this file. `presence` is which of
 *  this client's services carry the tag; `services` is empty when presence is
 *  "local". Used by the ghost-reject flow. */
export interface TagDetail {
  tag: string;
  presence: 'local' | 'pulled' | 'both';
  /** Shared repo display names carrying this tag for the current file. */
  services: string[];
  /** Whether this tag has sibling/parent/child relations in the library. */
  relations: boolean;
  /** Generation source (ADR 0026): the tool that made this tag, e.g. 'hydrus'
   *  or 'wd14-tagger'. Absent = manual/local. Read-only display metadata. */
  origin?: string;
}

/** One tag in a relation section — tag text plus how many files share the relation. */
export interface RelationTag {
  tag: string;
  count: number;
}

/** A capped section of related tags. `total - items.length` = "… N more". */
export interface RelationSection {
  items: RelationTag[];
  total: number;
}

/** Mirror of `GET /api/tags/relations` response. */
export interface TagRelations {
  /** The canonical form of the queried tag (after sibling resolution). */
  canonical: string;
  /**
   * Merged display count for the canonical: raw(canonical) + Σ raw(aliases).
   * Consistent with tag-completion counts. 0 if unmapped.
   */
  count: number;
  /** True when the queried tag reached `canonical` via an alias hop. */
  via_alias: boolean;
  aliases: RelationSection;
  parents: RelationSection;
  children: RelationSection;
}

/** Mirror of `naiad_api::RepoDto` — a subscribed repository. */
export interface RepoDto {
  name: string;
  url: string;
  /** Effective privacy ceiling for pulls from this repo, in prefix bits (#179).
   *  Always `Some` when the daemon populates it; absent from older daemons. */
  max_query_bits?: number;
  /** Advertised repo minimum query width, from cached caps (#179). Some only once
   *  the repo has been handshaken and runs a snapshot backend. */
  min_query_bits?: number;
}

/** Mirror of `naiad_api::FilePullRepoResult` — one repo's per-file pull outcome. */
export interface FilePullRepoResult {
  repo: string;
  mappings_added: number;
  /** Requested files with no SHA-256 interop hash, so this repo was never asked
   *  about them. Aggregate across repos with max, not sum — see `PullSummary`.
   *  Optional on the wire: absent when the daemon omits the field (e.g. older
   *  builds), treat as 0. */
  missing_sha256?: number;
  error?: string;
  /** Advisory notice from the daemon, e.g. a floor clamp-up (#179). At most
   *  one per repo+domain per daemon session. */
  notice?: string;
}

/** Mirror of `naiad_api::PluginDto` — a registered plugin and its capabilities. */
export interface PluginDto {
  id: string;
  name: string;
  tagger: boolean;
  processor: boolean;
  source: boolean;
}

/** One file's tag candidates from a tagger plugin lookup. */
export interface TaggerLookupItem {
  file: string;
  tags: string[];
}

/** Summary returned after a source plugin bulk import. */
export interface SourceImportSummary {
  mappings_staged: number;
  mappings_resolved: number;
  siblings: number;
  parents: number;
  sha256_backfilled: number;
}

/** Mirror of `naiad_api::ImportProgress` — a tick during a streamed library import. */
export interface ImportProgress {
  files: number;
  total: number;
  mappings: number;
}

/** Result of a relations-only Hydrus import (issue #41). */
export interface RelationsImportSummary {
  siblings: number;
  parents: number;
}

/** Mirror of `naiad_api::RelationsProgress` — a tick during a streamed relations pull. */
export interface RelationsProgress {
  edges_done: number;
  edges_total: number;
  siblings: number;
  parents: number;
}

/** Mirror of `naiad-daemon` `WatchFailure` — a root that failed to register. */
export interface WatchFailure {
  path: string;
  error: string;
}

/** Mirror of `naiad-daemon` `WatchStatus` — background watch-registration. */
export interface WatchStatus {
  total: number;
  done: number;
  current: string | null;
  failed: WatchFailure[];
  complete: boolean;
}

/** Mirror of `naiad-daemon` `CatchupStatus` — startup catch-up rescan progress. */
export interface CatchupStatus {
  running: boolean;
  imported: number;
  errors: number;
  roots_total: number;
  roots_done: number;
  current: string | null;
  complete: boolean;
}

/** Which step of the startup cache warmup is in flight (`naiad-daemon`
 *  `WarmupPhase`). `idle` means no warmup was spawned — reported alongside
 *  `complete: true`, so it never grows an activity job. `queued` means spawned
 *  but parked on the startup gate: incomplete, yet nothing is being read, so the
 *  UI must not claim work has begun. */
export type WarmupPhase = 'idle' | 'queued' | 'graph' | 'completion' | 'done';

/** Mirror of `naiad-daemon` `WarmupStatus` — startup cache-warmup progress.
 *  The catch-up scan defers behind this (#126), so during the warmup the scan
 *  counters are all zero and this is the only signal that work is happening. */
export interface WarmupStatus {
  phase: WarmupPhase;
  complete: boolean;
}

/** Parsed `GET /api/health`: liveness, watch-registration, catch-up scan, and
 *  startup cache warmup. */
export interface HealthStatus {
  ok: boolean;
  watch: WatchStatus | null;
  scan: CatchupStatus | null;
  warmup: WarmupStatus | null;
}

export interface TagSuggestion {
  namespace: string;
  subtag: string;
  count: number;
  /** Pre-formatted surfacing alias (`badtag` / `character:badtag`), display-only.
   *  Present (from the daemon) whenever the row came via an alias; the UI gates
   *  rendering on the `view.showAliasSource` pref. */
  alias_source?: string | null;
}

export interface NamespaceSuggestion {
  namespace: string;
  tag_count: number;
}

export interface Completions {
  namespaces: NamespaceSuggestion[];
  tags: TagSuggestion[];
}

/** Mirror of `naiad_api::RejectResponse` — returned by POST /api/reject. */
export interface RejectResponse {
  /** Whether the source repo accepts fire-and-forget reports, so the caller can offer escalation. */
  reports: boolean;
}

/** Mirror of `naiad_api::RejectionDto` — one rejected pulled mapping. */
export interface Rejection {
  /** Blake3 hex hash of the rejected file. */
  hash: string;
  service: string;
  tag: string;
  note: string | null;
  created_at: string;
}

/** Mirror of `naiad_api::PullConnecting` — one repo, before its fetch. */
export interface PullConnecting {
  repo: string;
  index: number;
  total: number;
}

/** Mirror of `naiad_api::PullProgress` — cumulative, after each repo. */
export interface PullProgress {
  repos_done: number;
  repos_total: number;
  repo: string;
  matched_files: number;
  mappings: number;
}

/** Mirror of `naiad_api::PullRepoOutcome` — one repo's row in the summary. */
export interface PullRepoOutcome {
  repo: string;
  matched_files: number;
  mappings: number;
  /** Requested files this repo was never asked about, for want of a SHA-256
   *  interop hash. Optional on the wire; absent means 0. */
  missing_sha256?: number;
  error?: string;
  /** Advisory notice from the daemon, e.g. a floor clamp-up (#179). At most one
   *  per repo+domain per daemon session; absent otherwise.
   *
   *  The streamed pull path populates this (#192): `naiad_api::PullRepoOutcome`
   *  carries `notice`, and the SSE `summary` handler in `daemon/src/server.rs`
   *  drains the pending clamp notice into it — mirroring the non-streamed
   *  `FilePullRepoResult` path. Optional because older daemons omit the field. */
  notice?: string;
}

/** Mirror of `naiad_api::PullSummary` — terminal event of a streamed pull.
 *
 *  No cumulative `missing_sha256`: every SHA-256 repo reports the *same*
 *  un-resolvable files, so summing would multiply one problem by the repo
 *  count. Take the maximum over `results`. */
export interface PullSummary {
  results: PullRepoOutcome[];
  matched_files: number;
  mappings: number;
}

/** Mirror of `naiad_api::PullError` — stream-fatal error payload. */
export interface PullError {
  message: string;
}

/** Mirror of `naiad_api::PullStage` — sub-repo progress within one repo's
 *  fetch. Additive (#172): absent on old daemons, ignored by old UIs. `chunk`/
 *  `chunk_total`/`bytes` are always serialized (0 when N/A); `domain` is omitted
 *  for `merging`/`done`. `hashes`/`tags`/`elapsed_ms`/`window` are additive
 *  (#174): absent on older daemons, default to 0. */
export interface PullStage {
  repo: string;
  index: number;
  total: number;
  /** "request" | "chunk" | "merging" | "done". */
  phase: string;
  chunk: number;
  chunk_total: number;
  bytes: number;
  domain?: string;
  /** Cumulative distinct file hashes seen so far in this pull (#174). */
  hashes?: number;
  /** Cumulative tag mappings staged so far in this pull (#174). */
  tags?: number;
  /** Wall-clock milliseconds elapsed in this pull (#174). */
  elapsed_ms?: number;
  /** Sliding-window size used for rate estimates (#174). */
  window?: number;
}

/** Mirror of `naiad_api::BackupSummary` — result of a successful VACUUM INTO backup. */
export interface BackupSummary {
  /** Absolute path of the written snapshot file. */
  dest: string;
  /** Size of the snapshot in bytes. */
  bytes: number;
  /** Wall-clock duration of the backup in milliseconds. */
  duration_ms: number;
}

