import type { GallerySort } from './gallery-sort';
import type { BackupSummary, CatchupStatus, Completions, FileDto, FilePullRepoResult, HealthStatus, ImportProgress, NamespaceSuggestion, PluginDto, PullConnecting, PullError, PullProgress, PullStage, PullSummary, Rejection, RejectResponse, RelationsImportSummary, RelationsProgress, RepoDto, ScanProgress, ScanSummary, SourceImportSummary, TagDetail, TagRelations, TaggerLookupItem, WarmupStatus, WatchStatus } from './types';
export type { CatchupStatus, HealthStatus, WarmupPhase, WarmupStatus, WatchStatus } from './types';

/**
 * Resolve to the response when ok; otherwise throw an Error carrying the
 * daemon's plain-text body (it returns 400/404/500 with a human-readable
 * message).
 */
async function ensureOk(res: Response): Promise<Response> {
  if (!res.ok) {
    const body = (await res.text()).trim();
    throw new Error(body || `request failed (${res.status})`);
  }
  return res;
}

/** Run a search. An empty query returns all files. `localOnly` hides pulled
 *  (synced) tags from matching. */
export async function search(q: string, localOnly = false): Promise<FileDto[]> {
  const scope = localOnly ? '&local_only=true' : '';
  const res = await ensureOk(await fetch(`/api/search?q=${encodeURIComponent(q)}${scope}`));
  return (await res.json()) as FileDto[];
}

/** A file's effective (sibling/parent-expanded) tags, by content hash.
 *  `localOnly` excludes pulled tags from the merge. */
export async function fileTags(hash: string, localOnly = false): Promise<string[]> {
  const scope = localOnly ? '&local_only=true' : '';
  const res = await ensureOk(await fetch(`/api/tags?file=${encodeURIComponent(hash)}${scope}`));
  return (await res.json()) as string[];
}

/** A file's effective tags with provenance and supporting-author info. */
export async function tagsDetailed(hash: string, localOnly = false): Promise<TagDetail[]> {
  const scope = localOnly ? '&local_only=true' : '';
  const res = await ensureOk(
    await fetch(`/api/tags/detailed?file=${encodeURIComponent(hash)}${scope}`),
  );
  return (await res.json()) as TagDetail[];
}

/** Add tags to a file. */
export async function addTags(hash: string, tags: string[]): Promise<void> {
  await ensureOk(
    await fetch('/api/tags/add', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ file: hash, tags }),
    }),
  );
}

/** Remove tags from a file. */
export async function removeTags(hash: string, tags: string[]): Promise<void> {
  await ensureOk(
    await fetch('/api/tags/remove', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ file: hash, tags }),
    }),
  );
}

/** Scan (import/index) a server-side folder path; returns the indexer summary. */
export async function scan(folder: string): Promise<ScanSummary> {
  const res = await ensureOk(
    await fetch('/api/scan', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ folder }),
    }),
  );
  return (await res.json()) as ScanSummary;
}

export interface ScanStreamHandlers {
  onProgress?: (p: ScanProgress) => void;
  onSummary: (s: ScanSummary) => void;
  onError: (message: string) => void;
}

/**
 * Start a streaming scan over SSE. Returns a function that closes the stream.
 * `onSummary`/`onError` are terminal — the stream is closed after either fires.
 */
export function scanStream(folder: string, handlers: ScanStreamHandlers): () => void {
  const es = new EventSource(`/api/scan/stream?folder=${encodeURIComponent(folder)}`);
  const close = () => es.close();
  es.addEventListener('progress', (e) =>
    handlers.onProgress?.(JSON.parse((e as MessageEvent).data) as ScanProgress),
  );
  es.addEventListener('summary', (e) => {
    handlers.onSummary(JSON.parse((e as MessageEvent).data) as ScanSummary);
    close();
  });
  es.addEventListener('error', (e) => {
    const data = (e as MessageEvent).data;
    handlers.onError(typeof data === 'string' && data ? data : 'scan connection lost');
    close();
  });
  return close;
}

/** List the folders currently being watched (display strings). */
export async function listRoots(): Promise<string[]> {
  const res = await ensureOk(await fetch('/api/roots'));
  return (await res.json()) as string[];
}

/** Stop watching a folder. Indexed files are kept unless `hide` is set, which
 *  marks them missing so they drop out of the gallery (reversible via re-scan). */
export async function removeRoot(path: string, hide = false): Promise<void> {
  const url = `/api/roots?path=${encodeURIComponent(path)}${hide ? '&hide=true' : ''}`;
  await ensureOk(await fetch(url, { method: 'DELETE' }));
}

/** Read the DB-backed gallery sort preference. */
export async function getGallerySort(): Promise<GallerySort> {
  const res = await ensureOk(await fetch('/api/view/sort'));
  return (await res.json()) as GallerySort;
}

/** Persist the gallery sort preference in the library DB. */
export async function setGallerySort(sort: GallerySort): Promise<void> {
  await ensureOk(
    await fetch('/api/view/sort', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(sort),
    }),
  );
}

/** List subscribed repositories. */
export async function listRepos(): Promise<RepoDto[]> {
  const res = await ensureOk(await fetch('/api/repos'));
  return (await res.json()) as RepoDto[];
}

/** Subscribe to a repository. The daemon validates the caps handshake first
 *  and takes the repo's advertised name (URL host for older servers). */
export async function addRepo(url: string): Promise<void> {
  await ensureOk(
    await fetch('/api/repos', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ url }),
    }),
  );
}

/** Unsubscribe. Pulled tags are KEPT unless `purge` is set. */
export async function removeRepo(name: string, purge = false): Promise<void> {
  const url = `/api/repos?name=${encodeURIComponent(name)}${purge ? '&purge=true' : ''}`;
  await ensureOk(await fetch(url, { method: 'DELETE' }));
}

/** Pull tags for specific files from every subscribed repo. Per-repo errors
 *  come back in the entries; the call itself only throws on transport/4xx.
 *
 *  @deprecated The UI pulls through {@link pullFileTagsStream} so progress
 *  reaches the activity indicator. This buffered form is kept only to match
 *  the daemon's retained `POST /api/files/pull-tags` endpoint. */
export async function pullFileTags(hashes: string[]): Promise<FilePullRepoResult[]> {
  const res = await ensureOk(
    await fetch('/api/files/pull-tags', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ hashes }),
    }),
  );
  return (await res.json()) as FilePullRepoResult[];
}

/** URL for a file's cached thumbnail. */
export function thumbUrl(hash: string): string {
  return `/thumb/${hash}`;
}

/** URL for a file's original bytes. */
export function fileUrl(hash: string): string {
  return `/file/${hash}`;
}

/** Poll daemon liveness plus background watch-registration, catch-up scan, and
 *  startup cache-warmup status. A network failure or non-200 resolves to
 *  offline with no info. `warmup` is absent on a pre-#130 daemon, so it falls
 *  back to null and the caller simply shows no warmup job. */
export async function health(): Promise<HealthStatus> {
  try {
    const res = await fetch('/api/health');
    if (!res.ok) return { ok: false, watch: null, scan: null, warmup: null };
    const body = (await res.json()) as {
      status?: string;
      watch?: WatchStatus;
      scan?: CatchupStatus;
      warmup?: WarmupStatus;
    };
    return {
      ok: true,
      watch: body.watch ?? null,
      scan: body.scan ?? null,
      warmup: body.warmup ?? null,
    };
  } catch {
    return { ok: false, watch: null, scan: null, warmup: null };
  }
}

/** List registered plugins and their capabilities. */
export async function listPlugins(): Promise<PluginDto[]> {
  const res = await ensureOk(await fetch('/api/plugins'));
  return (await res.json()) as PluginDto[];
}

/** Configure the Hydrus importer: DB directory + which tag services to pull
 *  (empty array = all). */
export async function hydrusConfigure(dir: string, tagServices: number[]): Promise<void> {
  await ensureOk(
    await fetch('/api/hydrus/configure', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ dir, tag_services: tagServices }),
    }),
  );
}

/** Response shape of GET /api/hydrus/config. */
export interface HydrusConfigDto {
  dir: string | null;
  tag_services: number[];
}

/** Read the persisted Hydrus importer config (DB dir + tag services). */
export async function hydrusConfig(): Promise<HydrusConfigDto> {
  const res = await ensureOk(await fetch('/api/hydrus/config'));
  return (await res.json()) as HydrusConfigDto;
}

/** Per-file tag lookup via a tagger plugin. When `apply` is true, also writes the tags. */
export async function taggerLookup(
  pluginId: string,
  files: string[],
  apply: boolean,
): Promise<TaggerLookupItem[]> {
  const res = await ensureOk(
    await fetch('/api/tagger/lookup', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ plugin_id: pluginId, files, apply }),
    }),
  );
  return (await res.json()) as TaggerLookupItem[];
}

/** Run a bulk import from a source plugin. `libraryOnly` pulls tags just for the
 *  files already in the library (matched by SHA-256); otherwise every file the
 *  source owns plus its relation graph. */
export async function sourceImport(
  pluginId: string,
  libraryOnly = false,
): Promise<SourceImportSummary> {
  const res = await ensureOk(
    await fetch('/api/source/import', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ plugin_id: pluginId, library_only: libraryOnly }),
    }),
  );
  return (await res.json()) as SourceImportSummary;
}

export interface ImportStreamHandlers {
  onProgress?: (p: ImportProgress) => void;
  onSummary: (s: SourceImportSummary) => void;
  onError: (message: string) => void;
}

/**
 * Start a streaming library import over SSE: tags are committed and reported
 * file-by-file. Returns a function that closes the stream. `onSummary`/`onError`
 * are terminal — the stream is closed after either fires.
 */
export function sourceImportStream(pluginId: string, handlers: ImportStreamHandlers): () => void {
  const es = new EventSource(`/api/source/import/stream?plugin_id=${encodeURIComponent(pluginId)}`);
  const close = () => es.close();
  es.addEventListener('progress', (e) =>
    handlers.onProgress?.(JSON.parse((e as MessageEvent).data) as ImportProgress),
  );
  es.addEventListener('summary', (e) => {
    handlers.onSummary(JSON.parse((e as MessageEvent).data) as SourceImportSummary);
    close();
  });
  es.addEventListener('error', (e) => {
    const data = (e as MessageEvent).data;
    handlers.onError(typeof data === 'string' && data ? data : 'import connection lost');
    close();
  });
  return close;
}

/** Pull the full Hydrus sibling/parent graph (no mappings). Synchronous variant;
 *  the UI uses `hydrusRelationsStream` for progress. */
export async function hydrusRelationsImport(): Promise<RelationsImportSummary> {
  const res = await ensureOk(await fetch('/api/hydrus/relations', { method: 'POST' }));
  return (await res.json()) as RelationsImportSummary;
}

export interface RelationsStreamHandlers {
  onProgress?: (p: RelationsProgress) => void;
  onSummary: (s: RelationsImportSummary) => void;
  onError: (message: string) => void;
}

/**
 * Start a streaming relations pull over SSE. Determinate: every progress tick
 * carries edges_done/edges_total. Returns a function that closes the stream;
 * `onSummary`/`onError` are terminal — the stream is closed after either fires.
 */
export function hydrusRelationsStream(handlers: RelationsStreamHandlers): () => void {
  const es = new EventSource('/api/hydrus/relations/stream');
  const close = () => es.close();
  es.addEventListener('progress', (e) =>
    handlers.onProgress?.(JSON.parse((e as MessageEvent).data) as RelationsProgress),
  );
  es.addEventListener('summary', (e) => {
    handlers.onSummary(JSON.parse((e as MessageEvent).data) as RelationsImportSummary);
    close();
  });
  es.addEventListener('error', (e) => {
    const data = (e as MessageEvent).data;
    handlers.onError(typeof data === 'string' && data ? data : 'relations connection lost');
    close();
  });
  return close;
}

export interface PullStreamHandlers {
  onConnecting?: (c: PullConnecting) => void;
  onProgress?: (p: PullProgress) => void;
  /** Sub-repo progress (#172). Optional; an old daemon never sends `stage`. */
  onStage?: (s: PullStage) => void;
  /** Terminal on success — the stream is closed after it fires. */
  onSummary: (s: PullSummary) => void;
  /** Terminal on failure (no repos, bad request, transport dead). */
  onError: (message: string) => void;
}

/**
 * Start a streaming tag pull over SSE. Unlike the relations stream this POSTs
 * a body (the hash list), so it cannot use `EventSource` (GET-only); it reads
 * the SSE frames off the fetch response instead. Same event names and terminal
 * semantics as `hydrusRelationsStream`. Returns a function that aborts the
 * request; `onSummary`/`onError` are terminal.
 */
export function pullFileTagsStream(hashes: string[], handlers: PullStreamHandlers): () => void {
  const controller = new AbortController();
  let settled = false;
  const fail = (m: string) => {
    if (settled) return;
    settled = true;
    handlers.onError(m);
  };

  const handleFrame = (frame: string) => {
    if (settled) return;
    let event = 'message';
    const dataLines: string[] = [];
    for (const raw of frame.split('\n')) {
      const line = raw.replace(/\r$/, '');
      if (line.startsWith('event:')) event = line.slice(6).trim();
      else if (line.startsWith('data:')) dataLines.push(line.slice(5).replace(/^ /, ''));
    }
    if (dataLines.length === 0) return; // keep-alive comment or blank frame
    const data = dataLines.join('\n');
    try {
      if (event === 'connecting') handlers.onConnecting?.(JSON.parse(data) as PullConnecting);
      else if (event === 'progress') handlers.onProgress?.(JSON.parse(data) as PullProgress);
      else if (event === 'stage') handlers.onStage?.(JSON.parse(data) as PullStage);
      else if (event === 'summary') {
        const s = JSON.parse(data) as PullSummary; // parse before marking settled
        settled = true;
        handlers.onSummary(s);
      } else if (event === 'error') {
        let msg = data;
        try {
          msg = (JSON.parse(data) as PullError).message;
        } catch {
          /* not JSON — use the raw data line */
        }
        fail(msg || 'pull failed');
      }
    } catch {
      fail('pull failed');
    }
  };

  (async () => {
    let res: Response;
    try {
      res = await fetch('/api/files/pull-tags/stream', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ hashes }),
        signal: controller.signal,
      });
    } catch {
      fail('pull connection lost');
      return;
    }
    if (!res.ok || !res.body) {
      const body = (await res.text().catch(() => '')).trim();
      fail(body || 'pull failed');
      return;
    }
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = '';
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        buf = buf.replace(/\r\n/g, '\n'); // normalize CRLF so \r\n\r\n separators work
        let sep: number;
        while ((sep = buf.indexOf('\n\n')) !== -1) {
          handleFrame(buf.slice(0, sep));
          buf = buf.slice(sep + 2);
          if (settled) {
            controller.abort();
            return;
          }
        }
      }
    } catch {
      fail('pull connection lost');
      return;
    }
    // Stream ended without a terminal event → treat as a dropped connection.
    if (!settled) fail('pull connection lost');
  })();

  return () => { settled = true; controller.abort(); };
}

/** Tag completion suggestions for the current search token. Empty token → empty
 *  result (no request). Pass an AbortSignal to cancel an in-flight lookup.
 *  `mode` controls whether suggestions are prefix-anchored (default) or
 *  substring-matched; it is forwarded to the daemon as `?mode=prefix|substring`. */
export async function completeTags(
  token: string,
  limit = 20,
  signal?: AbortSignal,
  mode: 'prefix' | 'substring' = 'prefix',
): Promise<Completions> {
  const q = token.trim();
  if (q === '') return { namespaces: [], tags: [] };
  const res = await ensureOk(
    await fetch(
      `/api/tags/complete?q=${encodeURIComponent(q)}&limit=${limit}&mode=${mode}`,
      { signal },
    ),
  );
  return (await res.json()) as Completions;
}

/** Every non-empty namespace in the library with its distinct-tag count, descending. */
export async function listNamespaces(): Promise<NamespaceSuggestion[]> {
  const res = await ensureOk(await fetch('/api/namespaces'));
  return (await res.json()) as NamespaceSuggestion[];
}

/** Reject one pulled mapping: this tag, on this file, from this repo.
 *  Reversible and purely local. Returns whether the repo accepts reports. */
export async function rejectTag(hash: string, tag: string, service: string): Promise<RejectResponse> {
  const res = await ensureOk(await fetch('/api/reject', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ hash, tag, service }),
  }));
  return (await res.json()) as RejectResponse;
}

/** Undo a rejection — the mapping reappears on the next refresh. */
export async function undoReject(hash: string, tag: string, service: string): Promise<void> {
  await ensureOk(await fetch(
    `/api/reject?hash=${encodeURIComponent(hash)}&tag=${encodeURIComponent(tag)}&service=${encodeURIComponent(service)}`,
    { method: 'DELETE' },
  ));
}

/** List rejections for a file (pass `hash`) or all rejections (omit `hash`). */
export async function listRejections(hash?: string): Promise<Rejection[]> {
  const q = hash != null ? `?hash=${encodeURIComponent(hash)}` : '';
  const res = await ensureOk(await fetch(`/api/rejections${q}`));
  return (await res.json()) as Rejection[];
}

/** Trigger a VACUUM INTO backup of the library DB. Posts an empty body, so the
 *  daemon uses its default destination (`<db_dir>/backups/naiad-YYYYMMDD-HHMMSS.db`).
 *  The request is synchronous — it resolves once the backup file is fully written. */
export async function backup(): Promise<BackupSummary> {
  const res = await ensureOk(
    await fetch('/api/backup', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({}),
    }),
  );
  return (await res.json()) as BackupSummary;
}

/** Send a fire-and-forget report to a repo: ask moderators to remove this
 *  tag from the file. This reveals the file's hash to the repo. */
export async function report(
  hash: string,
  tag: string,
  service: string,
  note: string | null,
): Promise<void> {
  await ensureOk(await fetch('/api/report', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ hash, tag, service, note }),
  }));
}

/** Fetch sibling/parent/child relations for a tag. When `fileHash` is provided
 *  the daemon uses the file's effective tags to resolve context; omitting it
 *  performs a library-wide lookup. `cap` limits each returned section.
 *  Pass an `AbortSignal` to cancel an in-flight request. */
export async function fetchTagRelations(
  tag: string,
  fileHash?: string,
  cap = 10,
  signal?: AbortSignal,
): Promise<TagRelations> {
  let url = `/api/tags/relations?tag=${encodeURIComponent(tag)}&cap=${cap}`;
  if (fileHash !== undefined) url += `&file=${encodeURIComponent(fileHash)}`;
  const res = await ensureOk(await fetch(url, { signal }));
  return (await res.json()) as TagRelations;
}

/** The Tauri app version string (e.g. "0.2.18"), read from the bundle manifest.
 *  Returns null outside a Tauri context (browser dev, vitest) or on any error. */
export async function getAppVersion(): Promise<string | null> {
  try {
    const { getVersion } = await import('@tauri-apps/api/app');
    return await getVersion();
  } catch {
    return null;
  }
}
