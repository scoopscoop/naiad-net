import { describe, it, expect, vi, beforeEach } from 'vitest';
import * as api from './api';

function jsonResponse(data: unknown): Response {
  return new Response(JSON.stringify(data), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

describe('api client', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('search hits /api/search with the URL-encoded query', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(
        jsonResponse([
          {
            hash: 'a',
            name: 'a.png',
            size: 1,
            path: '/a.png',
            imported_at: 100,
            created_at: 80,
            modified_at: 90,
            mime: 'image/png',
          },
        ]),
      );
    const files = await api.search('character:samus');
    expect(fetchMock).toHaveBeenCalledWith('/api/search?q=character%3Asamus');
    expect(files).toHaveLength(1);
    expect(files[0].name).toBe('a.png');
  });

  it('fileTags hits /api/tags with the file hash', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse(['character:samus']));
    const tags = await api.fileTags('deadbeef');
    expect(fetchMock).toHaveBeenCalledWith('/api/tags?file=deadbeef');
    expect(tags).toEqual(['character:samus']);
  });

  it('search appends local_only when asked', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse([]));
    await api.search('character:samus', true);
    expect(fetchMock).toHaveBeenCalledWith('/api/search?q=character%3Asamus&local_only=true');
  });

  it('fileTags appends local_only when asked', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse([]));
    await api.fileTags('deadbeef', true);
    expect(fetchMock).toHaveBeenCalledWith('/api/tags?file=deadbeef&local_only=true');
  });

  it('addTags posts a JSON body to /api/tags/add', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(null, { status: 200 }));
    await api.addTags('deadbeef', ['character:samus']);
    expect(fetchMock).toHaveBeenCalledWith('/api/tags/add', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ file: 'deadbeef', tags: ['character:samus'] }),
    });
  });

  it('removeTags posts a JSON body to /api/tags/remove', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(null, { status: 200 }));
    await api.removeTags('deadbeef', ['character:samus']);
    expect(fetchMock).toHaveBeenCalledWith('/api/tags/remove', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ file: 'deadbeef', tags: ['character:samus'] }),
    });
  });

  it('throws the response body text on a non-ok response', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response('bad query: unexpected token', { status: 400 }),
    );
    await expect(api.search('*bad')).rejects.toThrow('bad query: unexpected token');
  });

  it('builds thumb and file URLs from a hash', () => {
    expect(api.thumbUrl('abc')).toBe('/thumb/abc');
    expect(api.fileUrl('abc')).toBe('/file/abc');
  });

  it('scan posts the folder to /api/scan and parses the summary', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse({ imported: 3, marked_missing: 1, errors: [] }));
    const summary = await api.scan('/photos');
    expect(fetchMock).toHaveBeenCalledWith('/api/scan', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ folder: '/photos' }),
    });
    expect(summary.imported).toBe(3);
    expect(summary.marked_missing).toBe(1);
    expect(summary.errors).toEqual([]);
  });

  it('scan throws the response body text on a non-ok response', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response('no such directory: /nope', { status: 400 }),
    );
    await expect(api.scan('/nope')).rejects.toThrow('no such directory: /nope');
  });

  it('listRoots GETs /api/roots and returns the array', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse(['/media/photos', '/media/art']));
    const roots = await api.listRoots();
    expect(fetchMock).toHaveBeenCalledWith('/api/roots');
    expect(roots).toEqual(['/media/photos', '/media/art']);
  });

  it('removeRoot DELETEs /api/roots with the URL-encoded path', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(null, { status: 200 }));
    await api.removeRoot('/media/my photos');
    expect(fetchMock).toHaveBeenCalledWith('/api/roots?path=%2Fmedia%2Fmy%20photos', {
      method: 'DELETE',
    });
  });

  it('removeRoot appends hide=true when asked', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(null, { status: 200 }));
    await api.removeRoot('/media/photos', true);
    expect(fetchMock).toHaveBeenCalledWith('/api/roots?path=%2Fmedia%2Fphotos&hide=true', {
      method: 'DELETE',
    });
  });

  it('removeRoot throws the response body text on a non-ok response', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response('not a watched root: /nope', { status: 404 }),
    );
    await expect(api.removeRoot('/nope')).rejects.toThrow('not a watched root: /nope');
  });

  it('health resolves ok:true with watch and scan payloads on a 200', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(
        new Response(
          JSON.stringify({
            status: 'ok',
            watch: null,
            scan: {
              running: true,
              imported: 12000,
              errors: 0,
              roots_total: 1,
              roots_done: 0,
              current: 'D:/img/newstuff',
              complete: false,
            },
          }),
          { status: 200 },
        ),
      );
    const result = await api.health();
    expect(result.ok).toBe(true);
    expect(result.watch).toBeNull();
    expect(result.scan?.imported).toBe(12000);
    expect(result.scan?.running).toBe(true);
    expect(fetchMock).toHaveBeenCalledWith('/api/health');
  });

  it('health tolerates a payload without a scan field', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(JSON.stringify({ status: 'ok', watch: null }), { status: 200 }),
    );
    const result = await api.health();
    expect(result.ok).toBe(true);
    expect(result.scan).toBeNull();
  });

  it('health resolves ok:false on a non-ok response', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response('', { status: 500 }));
    const result = await api.health();
    expect(result.ok).toBe(false);
    expect(result.watch).toBeNull();
    expect(result.scan).toBeNull();
  });

  it('health resolves ok:false when fetch rejects', async () => {
    vi.spyOn(globalThis, 'fetch').mockRejectedValue(new Error('offline'));
    const result = await api.health();
    expect(result.ok).toBe(false);
    expect(result.watch).toBeNull();
    expect(result.scan).toBeNull();
  });

  it('getGallerySort GETs the persisted view sort', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse({ key: 'name', direction: 'asc' }));
    const sort = await api.getGallerySort();
    expect(fetchMock).toHaveBeenCalledWith('/api/view/sort');
    expect(sort).toEqual({ key: 'name', direction: 'asc' });
  });

  it('setGallerySort POSTs the view sort', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(null, { status: 200 }));
    await api.setGallerySort({ key: 'size', direction: 'desc' });
    expect(fetchMock).toHaveBeenCalledWith('/api/view/sort', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ key: 'size', direction: 'desc' }),
    });
  });

  it('tagsDetailed hits /api/tags/detailed with the file hash', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse([{ tag: 'character:samus', presence: 'local', services: [] }]));
    const tags = await api.tagsDetailed('deadbeef');
    expect(fetchMock).toHaveBeenCalledWith('/api/tags/detailed?file=deadbeef');
    expect(tags[0].presence).toBe('local');
  });

  it('tagsDetailed appends local_only when asked', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(jsonResponse([]));
    await api.tagsDetailed('abc', true);
    expect(fetchMock).toHaveBeenCalledWith('/api/tags/detailed?file=abc&local_only=true');
  });

  it('listRepos reads /api/repos', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse([{ name: 'r', url: 'http://r/' }]));
    const repos = await api.listRepos();
    expect(fetchMock).toHaveBeenCalledWith('/api/repos');
    expect(repos[0].name).toBe('r');
  });

  it('hydrusConfig GETs the persisted config', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse({ dir: '/db', tag_services: [9] }));
    const cfg = await api.hydrusConfig();
    expect(fetchMock).toHaveBeenCalledWith('/api/hydrus/config');
    expect(cfg.dir).toBe('/db');
    expect(cfg.tag_services).toEqual([9]);
  });

  it('rejectTag POSTs to /api/reject and returns { reports }', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse({ reports: true }));
    const result = await api.rejectTag('deadbeef', 'series:metroid', 'my-repo');
    expect(fetchMock).toHaveBeenCalledWith('/api/reject', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ hash: 'deadbeef', tag: 'series:metroid', service: 'my-repo' }),
    });
    expect(result).toEqual({ reports: true });
  });

  it('undoReject DELETEs /api/reject with URL-encoded params', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(null, { status: 200 }));
    await api.undoReject('deadbeef', 'series:metroid', 'my-repo');
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/reject?hash=deadbeef&tag=series%3Ametroid&service=my-repo',
      { method: 'DELETE' },
    );
  });

  it('listRejections GETs /api/rejections with optional hash', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse([{ service: 'r', tag: 't', note: null, created_at: '2026-01-01' }]));
    const rows = await api.listRejections('deadbeef');
    expect(fetchMock).toHaveBeenCalledWith('/api/rejections?hash=deadbeef');
    expect(rows[0].service).toBe('r');
  });

  it('listRejections omits hash param when called without argument', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(jsonResponse([]));
    await api.listRejections();
    expect(fetchMock).toHaveBeenCalledWith('/api/rejections');
  });

  it('report POSTs the report body to /api/report', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(null, { status: 200 }));
    await api.report('deadbeef', 'series:metroid', 'my-repo', 'wrong tag');
    expect(fetchMock).toHaveBeenCalledWith('/api/report', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ hash: 'deadbeef', tag: 'series:metroid', service: 'my-repo', note: 'wrong tag' }),
    });
  });

  it('report sends null note when no reason given', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(null, { status: 200 }));
    await api.report('deadbeef', 'series:metroid', 'my-repo', null);
    expect(fetchMock).toHaveBeenCalledWith('/api/report', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ hash: 'deadbeef', tag: 'series:metroid', service: 'my-repo', note: null }),
    });
  });

  it('fetchTagRelations builds the URL and returns the parsed body', async () => {
    const body = {
      canonical: 'character:samus',
      count: 51,
      via_alias: true,
      aliases: { items: [{ tag: 'samus_aran', count: 7 }], total: 3 },
      parents: { items: [], total: 0 },
      children: { items: [], total: 0 },
    };
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse(body));
    const got = await api.fetchTagRelations('character:samus', 'abc', 10);
    expect(got.via_alias).toBe(true);
    expect(got.count).toBe(51);
    expect(got.aliases.total).toBe(3);
    expect(got.aliases.items[0]).toEqual({ tag: 'samus_aran', count: 7 });
    const calledUrl = fetchMock.mock.calls[0][0] as string;
    expect(calledUrl).toContain('/api/tags/relations');
    expect(calledUrl).toContain('tag=character%3Asamus');
    expect(calledUrl).toContain('file=abc');
    expect(calledUrl).toContain('cap=10');
  });

  it('fetchTagRelations omits file param when fileHash is undefined', async () => {
    const body = {
      canonical: 'character:samus',
      count: 0,
      via_alias: false,
      aliases: { items: [], total: 0 },
      parents: { items: [], total: 0 },
      children: { items: [], total: 0 },
    };
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(jsonResponse(body));
    await api.fetchTagRelations('character:samus');
    const calledUrl = fetchMock.mock.calls[0][0] as string;
    expect(calledUrl).not.toContain('file=');
  });
});

describe('scanStream', () => {
  class FakeEventSource {
    static last: FakeEventSource | undefined;
    url: string;
    listeners: Record<string, (e: MessageEvent) => void> = {};
    closed = false;
    constructor(url: string) {
      this.url = url;
      FakeEventSource.last = this;
    }
    addEventListener(type: string, fn: (e: MessageEvent) => void) {
      this.listeners[type] = fn;
    }
    close() {
      this.closed = true;
    }
    emit(type: string, data: string) {
      this.listeners[type]?.({ data } as MessageEvent);
    }
  }

  beforeEach(() => {
    (globalThis as unknown as Record<string, unknown>).EventSource = FakeEventSource;
  });

  it('opens an EventSource with the URL-encoded folder and routes events', () => {
    const onProgress = vi.fn();
    const onSummary = vi.fn();
    const onError = vi.fn();
    api.scanStream('/my photos', { onProgress, onSummary, onError });
    const es = FakeEventSource.last!;
    expect(es.url).toBe('/api/scan/stream?folder=%2Fmy%20photos');

    es.emit('progress', JSON.stringify({ imported: 3, skipped: 1 }));
    expect(onProgress).toHaveBeenCalledWith({ imported: 3, skipped: 1 });

    es.emit('summary', JSON.stringify({ imported: 5, marked_missing: 0, errors: [] }));
    expect(onSummary).toHaveBeenCalledWith({ imported: 5, marked_missing: 0, errors: [] });
    expect(es.closed).toBe(true);
  });

  it('reports a generic message on a dataless error event', () => {
    const onError = vi.fn();
    api.scanStream('/x', { onSummary: vi.fn(), onError });
    const es = FakeEventSource.last!;
    es.emit('error', '');
    expect(onError).toHaveBeenCalledWith('scan connection lost');
    expect(es.closed).toBe(true);
  });
});

describe('pullFileTagsStream', () => {
  function sseResponse(frames: string[]): Response {
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        const enc = new TextEncoder();
        for (const f of frames) controller.enqueue(enc.encode(f));
        controller.close();
      },
    });
    return new Response(body, {
      status: 200,
      headers: { 'content-type': 'text/event-stream' },
    });
  }

  it('POSTs the hashes and dispatches connecting/progress/summary', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      sseResponse([
        'event: connecting\ndata: {"repo":"r1","index":1,"total":1}\n\n',
        'event: progress\ndata: {"repos_done":1,"repos_total":1,"repo":"r1","matched_files":2,"mappings":5}\n\n',
        'event: summary\ndata: {"results":[{"repo":"r1","matched_files":2,"mappings":5,"missing_sha256":0}],"matched_files":2,"mappings":5}\n\n',
      ]),
    );
    const onConnecting = vi.fn();
    const onProgress = vi.fn();
    const summary = await new Promise<unknown>((resolve) => {
      api.pullFileTagsStream(['deadbeef'], {
        onConnecting,
        onProgress,
        onSummary: (s) => resolve(s),
        onError: (m) => resolve(new Error(m)),
      });
    });

    expect(fetchMock).toHaveBeenCalledWith(
      '/api/files/pull-tags/stream',
      expect.objectContaining({
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ hashes: ['deadbeef'] }),
      }),
    );
    expect(onConnecting).toHaveBeenCalledWith({ repo: 'r1', index: 1, total: 1 });
    expect(onProgress).toHaveBeenCalledWith(
      expect.objectContaining({ repos_done: 1, matched_files: 2, mappings: 5 }),
    );
    expect(summary).toMatchObject({ matched_files: 2, mappings: 5 });
  });

  it('routes an error event to onError', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      sseResponse(['event: error\ndata: {"message":"no subscribed repositories"}\n\n']),
    );
    const message = await new Promise<string>((resolve) => {
      api.pullFileTagsStream(['x'], {
        onSummary: () => resolve('unexpected summary'),
        onError: (m) => resolve(m),
      });
    });
    expect(message).toBe('no subscribed repositories');
  });

  it('handles a frame split across two stream chunks', async () => {
    // The summary frame is split: first chunk ends mid-frame, second completes it.
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        const enc = new TextEncoder();
        controller.enqueue(enc.encode('event: summary\ndata: {"results":[],"matched'));
        controller.enqueue(enc.encode('_files":0,"mappings":0}\n\n'));
        controller.close();
      },
    });
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(body, { status: 200, headers: { 'content-type': 'text/event-stream' } }),
    );
    const summary = await new Promise<unknown>((resolve) => {
      api.pullFileTagsStream(['x'], {
        onSummary: (s) => resolve(s),
        onError: (m) => resolve(new Error(m)),
      });
    });
    expect(summary).toMatchObject({ matched_files: 0, mappings: 0 });
  });

  it('handles CRLF-terminated SSE frames', async () => {
    // Use CRLF line endings and \r\n\r\n frame separator.
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      sseResponse([
        'event: summary\r\ndata: {"results":[],"matched_files":0,"mappings":0}\r\n\r\n',
      ]),
    );
    const summary = await new Promise<unknown>((resolve) => {
      api.pullFileTagsStream(['x'], {
        onSummary: (s) => resolve(s),
        onError: (m) => resolve(new Error(m)),
      });
    });
    expect(summary).toMatchObject({ matched_files: 0, mappings: 0 });
  });

  it('silently ignores SSE keep-alive comment lines', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      sseResponse([
        ': heartbeat\n\n',
        'event: summary\ndata: {"results":[],"matched_files":0,"mappings":0}\n\n',
      ]),
    );
    const onError = vi.fn();
    const summary = await new Promise<unknown>((resolve) => {
      api.pullFileTagsStream(['x'], {
        onSummary: (s) => resolve(s),
        onError,
      });
    });
    expect(onError).not.toHaveBeenCalled();
    expect(summary).toMatchObject({ matched_files: 0, mappings: 0 });
  });

  it('does not fire onError when the caller aborts the stream', async () => {
    // Simulate a long-running stream (no frames ever arrive) and abort it.
    const body = new ReadableStream<Uint8Array>({ start() { /* never enqueues */ } });
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(body, { status: 200, headers: { 'content-type': 'text/event-stream' } }),
    );
    const onError = vi.fn();
    const close = api.pullFileTagsStream(['x'], {
      onSummary: vi.fn(),
      onError,
    });
    // Abort synchronously before any microtask reads from the stream.
    close();
    // Drain the microtask queue so the async IIFE can run to completion.
    await new Promise((r) => setTimeout(r, 20));
    expect(onError).not.toHaveBeenCalled();
  });

  it('dispatches a stage event and ignores an unknown event', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      sseResponse([
        'event: stage\ndata: {"repo":"r1","index":1,"total":1,"phase":"chunk","chunk":2,"chunk_total":3,"bytes":1234567,"domain":"blake3"}\n\n',
        'event: bogus\ndata: {"nope":true}\n\n',
        'event: summary\ndata: {"results":[],"matched_files":0,"mappings":0}\n\n',
      ]),
    );
    const onStage = vi.fn();
    await new Promise<void>((resolve) => {
      api.pullFileTagsStream(['x'], {
        onStage,
        onSummary: () => resolve(),
        onError: () => resolve(),
      });
    });
    expect(onStage).toHaveBeenCalledTimes(1);
    expect(onStage).toHaveBeenCalledWith(
      expect.objectContaining({ repo: 'r1', phase: 'chunk', chunk: 2, chunk_total: 3, bytes: 1234567 }),
    );
  });

  it('stage frame WITH #174 fields passes all four to onStage', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      sseResponse([
        'event: stage\ndata: {"repo":"r1","index":1,"total":1,"phase":"chunk","chunk":1,"chunk_total":3,"bytes":500000,"hashes":2000,"tags":150,"elapsed_ms":400,"window":5}\n\n',
        'event: summary\ndata: {"results":[],"matched_files":0,"mappings":0}\n\n',
      ]),
    );
    const onStage = vi.fn();
    await new Promise<void>((resolve) => {
      api.pullFileTagsStream(['x'], {
        onStage,
        onSummary: () => resolve(),
        onError: () => resolve(),
      });
    });
    expect(onStage).toHaveBeenCalledTimes(1);
    expect(onStage).toHaveBeenCalledWith(
      expect.objectContaining({ hashes: 2000, tags: 150, elapsed_ms: 400, window: 5 }),
    );
  });

  it('stage frame WITHOUT #174 fields still parses cleanly (old-daemon compat)', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      sseResponse([
        'event: stage\ndata: {"repo":"r1","index":1,"total":1,"phase":"chunk","chunk":1,"chunk_total":2,"bytes":300000}\n\n',
        'event: summary\ndata: {"results":[],"matched_files":0,"mappings":0}\n\n',
      ]),
    );
    const onStage = vi.fn();
    await new Promise<void>((resolve) => {
      api.pullFileTagsStream(['x'], {
        onStage,
        onSummary: () => resolve(),
        onError: () => resolve(),
      });
    });
    expect(onStage).toHaveBeenCalledTimes(1);
    // New fields absent: TypeScript leaves them undefined — caller must handle ?? 0.
    const arg = onStage.mock.calls[0][0] as { hashes?: number; tags?: number; elapsed_ms?: number; window?: number };
    expect(arg.hashes).toBeUndefined();
    expect(arg.tags).toBeUndefined();
    expect(arg.elapsed_ms).toBeUndefined();
    expect(arg.window).toBeUndefined();
    // Core fields still intact.
    expect(arg).toMatchObject({ repo: 'r1', phase: 'chunk', bytes: 300000 });
  });
});
