/** Callbacks for one thumbnail request. Both are optional; neither fires after
 *  the request is cancelled. */
export interface ThumbCallbacks {
  /** The decoded response body. */
  onBlob?: (blob: Blob) => void;
  /** A non-abort failure (network error, or a non-ok HTTP status). */
  onError?: (err: unknown) => void;
}

/** Cancel a request: drops it from the queue if still pending, or aborts the
 *  in-flight fetch if already running. Idempotent. */
export type CancelFn = () => void;

interface Job {
  url: string;
  cbs: ThumbCallbacks;
  controller: AbortController;
  cancelled: boolean;
  running: boolean;
}

/**
 * A bounded, newest-first (LIFO) fetch scheduler for thumbnails.
 *
 * At most `maxConcurrent` fetches run at once; the rest wait in a stack so the
 * most-recently-requested (≈ currently on screen) is admitted first — matching
 * the daemon's LIFO generation ordering (#54). Cancelling a request aborts its
 * fetch and frees the slot, so tiles scrolled off screen release their sockets
 * immediately instead of pinning them (#56).
 *
 * `fetchFn` is injectable for tests; defaults to the global `fetch`.
 */
/** Lane budget for the shared queue. WebView2 caps HTTP/1.1 at ~6 connections
 *  per origin, shared with SSE streams and the detail `/file` request. While a
 *  detail tab or quick-look covers the grid, cold-cache thumb *generations*
 *  (seconds each) would pin 4 of those sockets and starve the detail image, so
 *  the covered grid drops to 2 lanes — thumbnails aren't urgent behind an
 *  overlay. In-flight fetches are never aborted; slots drain naturally. */
export const THUMB_LANES = 4;
export const THUMB_LANES_COVERED = 2;

export function createThumbQueue(maxConcurrent = THUMB_LANES, fetchFn: typeof fetch = fetch) {
  const pending: Job[] = []; // treated as a stack: newest at the end
  let active = 0;
  // Lifecycle: incremented when a job is pushed by request(); decremented by
  // pump() when it dequeues a non-cancelled job, OR by the cancel closure
  // immediately when the job hasn't been picked up yet (phantom entry stays in
  // the array but is skipped by pump() when it reaches the top).
  let logicalPendingCount = 0;

  function pump() {
    while (active < maxConcurrent && pending.length > 0) {
      const job = pending.pop()!; // newest-first
      if (job.cancelled) continue; // already decremented when cancelled
      logicalPendingCount--;
      run(job);
    }
  }

  async function run(job: Job) {
    job.running = true;
    active++;
    try {
      const res = await fetchFn(job.url, { signal: job.controller.signal });
      if (!res.ok) throw new Error(`thumb request failed (${res.status})`);
      const blob = await res.blob();
      if (!job.cancelled) job.cbs.onBlob?.(blob);
    } catch (err) {
      if (job.cancelled || (err as { name?: string })?.name === 'AbortError') return;
      job.cbs.onError?.(err);
    } finally {
      active--;
      pump();
    }
  }

  /** Enqueue a fetch for `url`. Returns a cancel function. */
  function request(url: string, cbs: ThumbCallbacks): CancelFn {
    const job: Job = {
      url,
      cbs,
      controller: new AbortController(),
      cancelled: false,
      running: false,
    };
    pending.push(job);
    logicalPendingCount++;
    pump();
    return () => {
      if (job.cancelled) return;
      job.cancelled = true;
      if (job.running) {
        job.controller.abort();
      } else {
        // O(1): just mark cancelled; pump() skips it when it reaches the top.
        logicalPendingCount--;
      }
    };
  }

  /** Change the concurrency cap. Raising it admits pending jobs immediately
   *  (newest first); lowering it never aborts running fetches — slots simply
   *  aren't refilled until the active count drains under the new cap. */
  function setMaxConcurrent(n: number) {
    maxConcurrent = n;
    pump();
  }

  return {
    request,
    setMaxConcurrent,
    activeCount: () => active,
    pendingCount: () => logicalPendingCount,
  };
}

/** Shared instance used by every gallery tile (single daemon origin). */
export const thumbQueue = createThumbQueue();
