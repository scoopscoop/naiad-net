import { describe, it, expect, vi } from 'vitest';
import { createThumbQueue } from './thumb-queue';

/** A fetch stub whose responses are resolved manually via the returned controls. */
function deferredFetch() {
  const calls: {
    url: string;
    signal: AbortSignal;
    resolve: (blob: Blob) => void;
    reject: (err: unknown) => void;
    aborted: () => boolean;
  }[] = [];
  const fetchFn = ((url: string, init?: { signal?: AbortSignal }) => {
    const signal = init!.signal!;
    return new Promise<Response>((resolve, reject) => {
      const entry = {
        url,
        signal,
        resolve: (blob: Blob) => resolve({ ok: true, blob: async () => blob } as Response),
        reject,
        aborted: () => signal.aborted,
      };
      calls.push(entry);
      signal.addEventListener('abort', () => {
        const err = new Error('aborted');
        err.name = 'AbortError';
        reject(err);
      });
    });
  }) as unknown as typeof fetch;
  return { fetchFn, calls };
}

describe('createThumbQueue', () => {
  it('runs no more than maxConcurrent fetches at once', () => {
    const { fetchFn, calls } = deferredFetch();
    const q = createThumbQueue(2, fetchFn);
    q.request('/thumb/a', {});
    q.request('/thumb/b', {});
    q.request('/thumb/c', {});
    expect(calls.length).toBe(2);
    expect(q.activeCount()).toBe(2);
    expect(q.pendingCount()).toBe(1);
  });

  it('admits the newest pending job when a slot frees (LIFO)', async () => {
    const { fetchFn, calls } = deferredFetch();
    const q = createThumbQueue(1, fetchFn);
    q.request('/thumb/a', {});
    q.request('/thumb/b', {});
    q.request('/thumb/c', {});
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a']);
    calls[0].resolve(new Blob(['a']));
    await Promise.resolve();
    await Promise.resolve();
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a', '/thumb/c']);
  });

  it('cancelling a pending job never fetches it', () => {
    const { fetchFn, calls } = deferredFetch();
    const q = createThumbQueue(1, fetchFn);
    q.request('/thumb/a', {});
    const cancelB = q.request('/thumb/b', {});
    cancelB();
    expect(q.pendingCount()).toBe(0);
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a']);
  });

  it('cancelling a running job aborts it and admits the next', async () => {
    const { fetchFn, calls } = deferredFetch();
    const q = createThumbQueue(1, fetchFn);
    const cancelA = q.request('/thumb/a', {});
    q.request('/thumb/b', {});
    cancelA();
    expect(calls[0].aborted()).toBe(true);
    await Promise.resolve();
    await Promise.resolve();
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a', '/thumb/b']);
  });

  it('a rejected fetch frees the slot and keeps draining', async () => {
    const { fetchFn, calls } = deferredFetch();
    const onError = vi.fn();
    const q = createThumbQueue(1, fetchFn);
    q.request('/thumb/a', { onError });
    q.request('/thumb/b', {});
    calls[0].reject(new Error('boom'));
    await Promise.resolve();
    await Promise.resolve();
    expect(onError).toHaveBeenCalledOnce();
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a', '/thumb/b']);
  });

  it('delivers the blob on success and not after cancel', async () => {
    const { fetchFn, calls } = deferredFetch();
    const onBlob = vi.fn();
    const q = createThumbQueue(2, fetchFn);
    q.request('/thumb/a', { onBlob });
    const cancelB = q.request('/thumb/b', { onBlob });
    cancelB();
    calls[0].resolve(new Blob(['a']));
    await Promise.resolve();
    await Promise.resolve();
    expect(onBlob).toHaveBeenCalledOnce();
  });

  it('LIFO order is preserved when some pending jobs are cancelled', async () => {
    // With concurrency=1, /a is running; /b, /c, /d are pending.
    // Cancel /b and /c (middle items). When /a finishes, /d (newest) must run next.
    const { fetchFn, calls } = deferredFetch();
    const q = createThumbQueue(1, fetchFn);
    q.request('/thumb/a', {});
    const cancelB = q.request('/thumb/b', {});
    const cancelC = q.request('/thumb/c', {});
    q.request('/thumb/d', {});
    cancelB();
    cancelC();
    expect(q.pendingCount()).toBe(1);
    calls[0].resolve(new Blob(['a']));
    await Promise.resolve();
    await Promise.resolve();
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a', '/thumb/d']);
  });

  it('cancelling a running job does not affect pendingCount', () => {
    // A is admitted (running); B is pending.  Cancelling A aborts the fetch
    // but must not touch logicalPendingCount — B is still waiting.
    const { fetchFn } = deferredFetch();
    const q = createThumbQueue(1, fetchFn);
    const cancelA = q.request('/thumb/a', {}); // runs immediately (concurrency=1)
    q.request('/thumb/b', {}); // stays pending
    expect(q.pendingCount()).toBe(1);
    cancelA(); // A is running → abort, no pendingCount change
    expect(q.pendingCount()).toBe(1);
  });

  it('lowering maxConcurrent never aborts running jobs; freed slots stay empty until under the new cap', async () => {
    const { fetchFn, calls } = deferredFetch();
    const q = createThumbQueue(2, fetchFn);
    q.request('/thumb/a', {});
    q.request('/thumb/b', {});
    q.request('/thumb/c', {});
    expect(q.activeCount()).toBe(2);
    q.setMaxConcurrent(1);
    // Both in-flight fetches keep their sockets.
    expect(calls[0].aborted()).toBe(false);
    expect(calls[1].aborted()).toBe(false);
    // First slot frees: still at the new cap (1 active) — /c must wait.
    calls[0].resolve(new Blob(['a']));
    await Promise.resolve();
    await Promise.resolve();
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a', '/thumb/b']);
    expect(q.pendingCount()).toBe(1);
    // Second slot frees: now under the cap — /c is admitted.
    calls[1].resolve(new Blob(['b']));
    await Promise.resolve();
    await Promise.resolve();
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a', '/thumb/b', '/thumb/c']);
  });

  it('raising maxConcurrent immediately admits pending jobs, newest first', () => {
    const { fetchFn, calls } = deferredFetch();
    const q = createThumbQueue(1, fetchFn);
    q.request('/thumb/a', {});
    q.request('/thumb/b', {});
    q.request('/thumb/c', {});
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a']);
    q.setMaxConcurrent(3);
    expect(calls.map((c) => c.url)).toEqual(['/thumb/a', '/thumb/c', '/thumb/b']);
    expect(q.pendingCount()).toBe(0);
  });

  it('cancel decrements pendingCount immediately; phantom entry is drained lazily by pump', () => {
    // Enqueue many jobs, then cancel the middle one.
    // The array length stays the same (no splice); only logical count drops.
    const { fetchFn } = deferredFetch();
    const q = createThumbQueue(1, fetchFn);
    q.request('/thumb/a', {}); // runs immediately
    q.request('/thumb/b', {});
    q.request('/thumb/c', {});
    const cancelD = q.request('/thumb/d', {});
    q.request('/thumb/e', {});
    // 4 pending (b,c,d,e)
    expect(q.pendingCount()).toBe(4);
    cancelD();
    // d is cancelled in O(1); logical count drops by 1
    expect(q.pendingCount()).toBe(3);
  });
});
