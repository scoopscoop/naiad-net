import { fileUrl } from './api';
import { thumbStream } from './thumb-stream';
import type { CancelFn, ThumbCallbacks } from './thumb-queue';

const SETTLE_MS = 150;
/**
 * Delay (from cycle start) before an arrived thumbnail is painted as a preview.
 * On a warm cache the full /file image usually lands first, so the thumb is
 * never painted and navigation swaps sharp→sharp with no blur flash. Only a
 * genuine stall (full image slower than this window) surfaces the preview.
 * Measured from cycle start, so with SETTLE_MS = 150 this leaves ~150 ms of
 * real fetch/decode latency before the thumb shows.
 */
const PREVIEW_MS = 300;

/**
 * Decode a blob URL off-screen so it can be swapped onto the visible <img>
 * without a decode-window blank. Assigning a fresh blob straight to the shown
 * element makes the browser decode it on the hot path; in the WebView2 runtime
 * the element blanks for that window instead of holding the previous frame
 * (the same "revoke/replace before decode blanks the tile" hazard load-thumb
 * guards against). Pre-decoding means the later `img.src = url` paints from
 * cache instantly. A no-op where `Image`/`decode()` is unavailable (jsdom).
 */
async function decodeOffscreen(url: string): Promise<void> {
  const ImageCtor = (globalThis as unknown as { Image?: typeof Image }).Image;
  if (typeof ImageCtor !== 'function') return;
  const pre = new ImageCtor();
  pre.src = url;
  if (typeof pre.decode === 'function') {
    // A decode failure here is non-fatal: fall through and let the visible
    // element's own load/error settle the cycle.
    try {
      await pre.decode();
    } catch {
      /* ignore — visible load/error handles it */
    }
  }
}

export interface StageLoaderParams {
  hash: string;
  /** Full-size fetch cycle begins — drives the spinner. */
  onLoadStart: () => void;
  /** Full-size settled: decoded, errored, or superseded. */
  onLoadEnd: () => void;
}

export interface StageLoaderDeps {
  /** Default: `globalThis.fetch` (resolved lazily so vi.stubGlobal works in tests). */
  fetchFn?: typeof fetch;
  /** Default: `thumbStream.request` (resolved via the import binding so vi.mock works). */
  requestThumb?: (hash: string, cbs: ThumbCallbacks) => CancelFn;
  /** Default: `globalThis.setTimeout` (resolved lazily so vi.useFakeTimers works). */
  setTimer?: typeof setTimeout;
  /** Default: `globalThis.clearTimeout` (resolved lazily). */
  clearTimer?: typeof clearTimeout;
  /** Debounce window before the full fetch starts.  Default: 150 ms. */
  settleMs?: number;
  /** Window (from cycle start) before an arrived thumb is painted.  Default: 300 ms. */
  previewMs?: number;
  /** Off-screen decode of a full-image URL before it's swapped onto the visible
   *  <img>. Default: `decodeOffscreen`. Injectable so tests stay deterministic. */
  decodeImg?: (url: string) => Promise<void>;
}

/**
 * Factory that returns a Svelte action `(img, params) => { update, destroy }`.
 *
 * Single-flight full-image fetch with:
 *  - WebSocket thumbnail preview, gated behind a ~300 ms timer so it only
 *    paints during a genuine stall (no blur flash when the full image is fast)
 *  - ~150 ms debounce before the expensive /file request (spares the disk during
 *    rapid key presses while the thumb already shows on every keypress)
 *  - Off-screen decode of the full image before the visible swap, so a fast
 *    switch never blanks for the decode window (the previous frame stays up)
 *  - AbortController abort on supersession (at most one /file request is alive)
 *  - Full object-URL lifecycle: both thumb and full URLs are revoked after use
 *  - Balanced onLoadStart/onLoadEnd refcount even when cycles are superseded
 */
export function createStageLoader(deps: StageLoaderDeps = {}) {
  return function stageLoaderAction(img: HTMLImageElement, params: StageLoaderParams) {
    // Deps resolved at action-call time (not factory time) so that vitest's
    // vi.stubGlobal('fetch') and vi.useFakeTimers() are visible to the defaults.
    const _fetchFn: typeof fetch =
      deps.fetchFn ?? (globalThis as unknown as { fetch: typeof fetch }).fetch;
    const _requestThumb: (hash: string, cbs: ThumbCallbacks) => CancelFn =
      deps.requestThumb ?? ((h, cbs) => thumbStream.request(h, cbs));
    const _setTimer: typeof setTimeout =
      deps.setTimer ?? (globalThis as unknown as { setTimeout: typeof setTimeout }).setTimeout;
    const _clearTimer: typeof clearTimeout =
      deps.clearTimer ??
      (globalThis as unknown as { clearTimeout: typeof clearTimeout }).clearTimeout;
    const _settleMs = deps.settleMs ?? SETTLE_MS;
    const _previewMs = deps.previewMs ?? PREVIEW_MS;
    const _decodeImg: (url: string) => Promise<void> = deps.decodeImg ?? decodeOffscreen;

    // ── Per-instance state ────────────────────────────────────────────────────
    let onLoadStart = params.onLoadStart;
    let onLoadEnd = params.onLoadEnd;
    let currentHash = '';
    /** Monotonic counter; guards all async callbacks against stale cycles. */
    let seq = 0;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let debounceTimer: any = null;
    /** Timer that gates painting the preview thumb; fires PREVIEW_MS after start. */
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let previewTimer: any = null;
    /** True once the preview window has elapsed with the full image still absent. */
    let previewReady = false;
    /** A decoded thumb URL held back until the preview window elapses. */
    let stashedThumbUrl: string | null = null;
    let controller: AbortController | null = null;
    let cancelThumb: CancelFn = () => {};
    let objThumbUrl: string | null = null;
    let objFullUrl: string | null = null;
    let imgLoadHandler: ((e: Event) => void) | null = null;
    let imgErrorHandler: ((e: Event) => void) | null = null;
    /** True while a load cycle is in flight and has not yet called onLoadEnd. */
    let pending = false;
    /**
     * True once the full image src has been set for the current cycle.
     * Prevents a late thumb blob from clobbering the full image.
     */
    let fullShown = false;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /** Call onLoadEnd exactly once per cycle.  Idempotent. */
    function settle() {
      if (pending) {
        pending = false;
        onLoadEnd();
      }
    }

    function removeImgListeners() {
      if (imgLoadHandler) {
        img.removeEventListener('load', imgLoadHandler);
        imgLoadHandler = null;
      }
      if (imgErrorHandler) {
        img.removeEventListener('error', imgErrorHandler);
        imgErrorHandler = null;
      }
    }

    function revokeThumb() {
      if (objThumbUrl) {
        URL.revokeObjectURL(objThumbUrl);
        objThumbUrl = null;
      }
    }

    function revokeFull() {
      if (objFullUrl) {
        URL.revokeObjectURL(objFullUrl);
        objFullUrl = null;
      }
    }

    /** Revoke a stashed-but-never-painted thumb URL. */
    function revokeStash() {
      if (stashedThumbUrl) {
        URL.revokeObjectURL(stashedThumbUrl);
        stashedThumbUrl = null;
      }
    }

    function cancelPreview() {
      if (previewTimer !== null) {
        _clearTimer(previewTimer);
        previewTimer = null;
      }
    }

    /** Reveal the <img> once it holds a real src. Only ever adds the class —
     *  never removed on a new cycle — so an in-place navigation keeps the
     *  previous frame visible (no fade-out) while the next image loads. On a
     *  *fresh* mount the class is absent, so the srcless element stays hidden
     *  (opacity 0) instead of painting the browser's broken-image icon. */
    function reveal() {
      img.classList.add('ready');
    }

    /** Paint the given thumb URL as a blurred preview. */
    function paintThumb(url: string) {
      revokeThumb();
      objThumbUrl = url;
      img.classList.add('preview');
      img.src = url;
      reveal();
    }

    /**
     * Tear down the current cycle without starting a new one.
     * If the cycle was still pending (no settle yet), calls onLoadEnd so that
     * createPending's refcount stays at 0/1 — supersession counts as a settle.
     */
    function clear() {
      if (debounceTimer !== null) {
        _clearTimer(debounceTimer);
        debounceTimer = null;
      }
      cancelPreview();
      if (controller) {
        controller.abort();
        controller = null;
      }
      cancelThumb();
      cancelThumb = () => {};
      removeImgListeners();
      settle(); // balanced: closes the pending cycle if still open
      revokeThumb();
      revokeStash();
      revokeFull();
    }

    // ── Async full-image fetch ────────────────────────────────────────────────

    async function runFull(mySeq: number, hash: string) {
      const ctrl = new AbortController();
      controller = ctrl;

      try {
        const res = await _fetchFn(fileUrl(hash), {
          signal: ctrl.signal,
          // `priority` is a non-standard hint; cast to silence TS.
          ...(({ priority: 'high' }) as Record<string, unknown>),
        });
        if (mySeq !== seq) return;
        if (!res.ok) throw new Error(`fetch failed: ${res.status}`);

        const blob = await res.blob();
        if (mySeq !== seq) return;

        const url = URL.createObjectURL(blob);

        // Decode off-screen BEFORE touching the visible element. The previous
        // image (thumb or full) stays painted throughout, so a fast switch goes
        // straight to the sharp image with no decode-window blank. Only after
        // the bitmap is ready do we swap it in below.
        await _decodeImg(url);
        if (mySeq !== seq) {
          URL.revokeObjectURL(url);
          return;
        }

        // Revoke any previous full URL before replacing it.
        revokeFull();
        objFullUrl = url;

        function onLoad() {
          removeImgListeners();
          if (mySeq === seq) {
            settle();
            revokeThumb();
            // Revoke after load: the browser retains the decoded bitmap.
            URL.revokeObjectURL(url);
            if (objFullUrl === url) objFullUrl = null;
          }
        }
        function onError() {
          removeImgListeners();
          if (mySeq === seq) {
            settle();
            revokeThumb();
            URL.revokeObjectURL(url);
            if (objFullUrl === url) objFullUrl = null;
          }
        }
        imgLoadHandler = onLoad;
        imgErrorHandler = onError;
        img.addEventListener('load', onLoad);
        img.addEventListener('error', onError);

        // Full image wins the race: kill the preview gate and drop any stashed
        // thumb so it never flashes in behind the sharp image.
        cancelPreview();
        revokeStash();
        fullShown = true;
        img.classList.remove('preview');
        img.src = url;
        reveal();
      } catch (err) {
        // AbortError = intentional abort from clear() — silent.
        // Stale seq = superseded — silent (the guard above already returned).
        if (mySeq !== seq) return;
        if ((err as { name?: string })?.name === 'AbortError') return;
        // Network or HTTP error: settle the spinner; leave the thumb preview
        // visible so the stage isn't blank.
        settle();
      }
    }

    // ── Cycle start ───────────────────────────────────────────────────────────

    function start(hash: string) {
      currentHash = hash;
      const mySeq = ++seq;
      pending = true;
      fullShown = false;
      previewReady = false;
      onLoadStart();

      // Request the thumb immediately via the WebSocket transport, but DON'T
      // paint it right away — stash it and let the preview timer decide. On a
      // warm cache the full image usually lands first, so the thumb is dropped
      // unpainted and there's no blur flash on every keypress.
      cancelThumb = _requestThumb(hash, {
        onBlob(blob) {
          const url = URL.createObjectURL(blob);
          if (mySeq !== seq) {
            // Stale — dispose immediately.
            URL.revokeObjectURL(url);
            return;
          }
          if (fullShown) {
            // Full image already shown; a late thumb must not clobber it.
            URL.revokeObjectURL(url);
            return;
          }
          if (previewReady) {
            // Preview window already elapsed (confirmed stall): paint now.
            paintThumb(url);
            return;
          }
          // Hold it back until the preview window elapses.
          revokeStash();
          stashedThumbUrl = url;
        },
        onError() {
          // Thumb failure is silent; the full fetch will still arrive.
        },
      });

      // Preview gate: if the full image hasn't shown within _previewMs, paint
      // the stashed thumb (if any) so a genuine stall isn't a blank stage.
      previewTimer = _setTimer(() => {
        previewTimer = null;
        if (mySeq !== seq || fullShown) return;
        previewReady = true;
        if (stashedThumbUrl) {
          const url = stashedThumbUrl;
          stashedThumbUrl = null;
          paintThumb(url);
        }
      }, _previewMs);

      // Debounced full fetch: only start the expensive /file request after the
      // keyboard settles.  Registered last so the newest timer is the debounce.
      debounceTimer = _setTimer(() => {
        debounceTimer = null;
        if (mySeq !== seq) return;
        void runFull(mySeq, hash);
      }, _settleMs);
    }

    // ── Svelte action interface ───────────────────────────────────────────────

    start(params.hash);

    return {
      update(newParams: StageLoaderParams) {
        if (newParams.hash === currentHash) {
          // Same hash: just refresh the callbacks (new closures, same semantics).
          // No reload — matches load-thumb's no-op-on-same-hash contract and
          // prevents reloading when zoom/pan-driven re-renders re-run the action.
          onLoadStart = newParams.onLoadStart;
          onLoadEnd = newParams.onLoadEnd;
          return;
        }

        // Hash changed: close the current cycle with the *current* callbacks
        // (so the spinner refcount stays balanced), then refresh and start.
        clear();
        onLoadStart = newParams.onLoadStart;
        onLoadEnd = newParams.onLoadEnd;
        start(newParams.hash);
      },

      destroy() {
        // Bump seq first so any in-flight runFull callback sees the stale guard.
        seq += 1;
        clear();
      },
    };
  };
}

/** Default action bound to the real transports.  Used in ImageStage via `use:loadStageImage`. */
export const loadStageImage = createStageLoader();
