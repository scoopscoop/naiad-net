import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { createStageLoader } from './stage-loader';
import type { ThumbCallbacks, CancelFn } from './thumb-queue';

// ── URL stubs (jsdom may not have createObjectURL/revokeObjectURL) ─────────────
let urlSeq = 0;
const createObjectURL = vi.fn(() => `blob:fake-${++urlSeq}`);
const revokeObjectURL = vi.fn();

// ── Helpers ───────────────────────────────────────────────────────────────────

function makeImg(): HTMLImageElement {
  return document.createElement('img');
}

interface FakeDeps {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  fetchFn: ReturnType<typeof vi.fn<any>>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  requestThumb: ReturnType<typeof vi.fn<any>>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  setTimer: ReturnType<typeof vi.fn<any>>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  clearTimer: ReturnType<typeof vi.fn<any>>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  cancelThumb: ReturnType<typeof vi.fn<any>>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  decodeImg: ReturnType<typeof vi.fn<any>>;
  thumbCallbacks: ThumbCallbacks;
  settleMs: number;
  previewMs: number;
  pendingFetches: Array<{ resolve: (r: Response) => void; reject: (e: Error) => void }>;
}

function makeDeps(settleMs = 150, previewMs = 300): FakeDeps {
  const cancelThumb = vi.fn<() => void>();
  let thumbCallbacks: ThumbCallbacks = {};
  const pendingFetches: Array<{ resolve: (r: Response) => void; reject: (e: Error) => void }> = [];

  const fetchFn = vi.fn().mockImplementation(() =>
    new Promise<Response>((resolve, reject) => {
      pendingFetches.push({ resolve, reject });
    }),
  );

  const requestThumb = vi.fn<(hash: string, cbs: ThumbCallbacks) => CancelFn>().mockImplementation(
    (_hash, cbs) => {
      thumbCallbacks = cbs;
      return cancelThumb;
    },
  );

  // Distinct, non-zero ids so tests can assert clearTimer(id) precisely.
  let timerSeq = 0;
  const setTimer = vi.fn<(cb: () => void, ms: number) => number>().mockImplementation(
    (_cb, _ms) => {
      // Don't auto-call; tests control when timers fire
      return ++timerSeq;
    },
  );
  const clearTimer = vi.fn<(id: number) => void>();

  // Deterministic decode: resolves immediately so the swap proceeds in-test
  // without an off-screen Image()/decode() round-trip.
  const decodeImg = vi.fn<(url: string) => Promise<void>>().mockResolvedValue(undefined);

  // Expose mutable thumbCallbacks via getter so tests pick up updates
  const deps = {
    fetchFn,
    requestThumb,
    setTimer,
    clearTimer,
    cancelThumb,
    decodeImg,
    get thumbCallbacks(): ThumbCallbacks {
      return thumbCallbacks;
    },
    settleMs,
    previewMs,
    pendingFetches,
  };
  return deps;
}

/** Fire the most recent timer registered with the given ms window. */
function fireTimerWithMs(deps: FakeDeps, ms: number) {
  const call = [...deps.setTimer.mock.calls].reverse().find((c) => c[1] === ms);
  expect(call, `no setTimer registered with ms=${ms}`).toBeTruthy();
  (call![0] as () => void)();
}

/** Fire the debounce (full-fetch) timer. */
function fireDebounce(deps: FakeDeps) {
  fireTimerWithMs(deps, deps.settleMs);
}

/** Fire the preview (thumb-paint) timer. */
function firePreview(deps: FakeDeps) {
  fireTimerWithMs(deps, deps.previewMs);
}

/** Resolve the latest pending fetch with a blob-returning response */
async function resolveFetch(deps: FakeDeps, blob = new Blob(['data'], { type: 'image/jpeg' })): Promise<void> {
  expect(deps.pendingFetches.length).toBeGreaterThan(0);
  const { resolve } = deps.pendingFetches.pop()!;
  resolve({
    ok: true,
    blob: () => Promise.resolve(blob),
  } as unknown as Response);
  // Flush microtasks so the async chain progresses. The chain now awaits the
  // response, the blob, AND the off-screen decode, so flush generously.
  for (let i = 0; i < 6; i++) await Promise.resolve();
}

async function rejectFetch(deps: FakeDeps, err: Error): Promise<void> {
  expect(deps.pendingFetches.length).toBeGreaterThan(0);
  const { reject } = deps.pendingFetches.pop()!;
  reject(err);
  await Promise.resolve();
  await Promise.resolve();
}

// ── Setup / teardown ──────────────────────────────────────────────────────────

beforeEach(() => {
  urlSeq = 0;
  createObjectURL.mockClear();
  revokeObjectURL.mockClear();
  createObjectURL.mockImplementation(() => `blob:fake-${++urlSeq}`);

  // Assign directly — jsdom doesn't have these; vi.spyOn would fail.
  URL.createObjectURL = createObjectURL as unknown as typeof URL.createObjectURL;
  URL.revokeObjectURL = revokeObjectURL as unknown as typeof URL.revokeObjectURL;
});

afterEach(() => {
  vi.restoreAllMocks();
});

// ── Tests ─────────────────────────────────────────────────────────────────────

describe('createStageLoader', () => {
  describe('debounce', () => {
    it('does NOT call fetchFn before the debounce timer fires', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadStart = vi.fn();
      const onLoadEnd = vi.fn();
      action(img, { hash: 'a'.repeat(64), onLoadStart, onLoadEnd });

      // Both timers (preview + debounce) registered, neither fired
      expect(deps.setTimer).toHaveBeenCalledTimes(2);
      expect(deps.fetchFn).not.toHaveBeenCalled();
    });

    it('calls fetchFn once after the timer fires', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      fireDebounce(deps);

      expect(deps.fetchFn).toHaveBeenCalledOnce();
      expect(deps.fetchFn).toHaveBeenCalledWith(`/file/${'a'.repeat(64)}`, expect.objectContaining({ signal: expect.any(AbortSignal) }));
    });

    it('coalesces rapid updates: only fetches for the final hash', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Update twice more before the timer fires
      handle.update({ hash: 'b'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });
      handle.update({ hash: 'c'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Fire the last debounce timer
      fireDebounce(deps);

      expect(deps.fetchFn).toHaveBeenCalledOnce();
      expect(deps.fetchFn.mock.calls[0][0]).toBe(`/file/${'c'.repeat(64)}`);
    });
  });

  describe('abort-on-supersede', () => {
    it('aborts the in-flight controller when update fires with a new hash', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      fireDebounce(deps);
      expect(deps.fetchFn).toHaveBeenCalledOnce();

      // While fetch is in flight, update to a new hash
      const abortSpy = vi.spyOn(AbortController.prototype, 'abort');
      handle.update({ hash: 'b'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      expect(abortSpy).toHaveBeenCalledOnce();
    });

    it('only applies the blob for the latest hash after update', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadEnd = vi.fn();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd });

      fireDebounce(deps);

      // Supersede before fetch resolves
      handle.update({ hash: 'b'.repeat(64), onLoadStart: vi.fn(), onLoadEnd });

      // Resolve the OLD fetch (for hash A) — should be ignored
      await resolveFetch(deps);

      // img.src should NOT be set to hash A's blob URL
      expect(img.src).toBe('');
    });
  });

  describe('thumb preview', () => {
    it('calls requestThumb immediately on start (before debounce timer fires)', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      expect(deps.requestThumb).toHaveBeenCalledOnce();
      expect(deps.requestThumb.mock.calls[0][0]).toBe('a'.repeat(64));
    });

    it('does NOT paint the thumb on arrival — holds it until the preview window elapses', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      const thumbBlob = new Blob(['thumb'], { type: 'image/jpeg' });
      deps.thumbCallbacks.onBlob?.(thumbBlob);

      // URL created (stashed) but nothing painted yet
      expect(createObjectURL).toHaveBeenCalledWith(thumbBlob);
      expect(img.src).toBe('');
      expect(img.classList.contains('preview')).toBe(false);
    });

    it('paints the stashed thumb with .preview once the preview timer fires (stall)', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      deps.thumbCallbacks.onBlob?.(new Blob(['thumb'], { type: 'image/jpeg' }));
      expect(img.src).toBe('');

      // Preview window elapses with the full image still absent
      firePreview(deps);

      expect(img.src).toContain('blob:fake-');
      expect(img.classList.contains('preview')).toBe(true);
    });

    it('paints a thumb that arrives AFTER the preview window immediately (stall already confirmed)', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Window elapses before the thumb even arrives
      firePreview(deps);
      expect(img.src).toBe('');

      deps.thumbCallbacks.onBlob?.(new Blob(['thumb'], { type: 'image/jpeg' }));

      expect(img.src).toContain('blob:fake-');
      expect(img.classList.contains('preview')).toBe(true);
    });

    it('never paints the thumb when the full image wins the race (fast load)', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Thumb arrives and is stashed
      deps.thumbCallbacks.onBlob?.(new Blob(['thumb']));

      // Full lands before the preview timer ever fires
      fireDebounce(deps);
      await resolveFetch(deps);

      // Preview class never applied; src is the full image, not the thumb
      expect(img.classList.contains('preview')).toBe(false);
      expect(img.src).toContain('blob:fake-');

      // The preview timer was cancelled; firing it now is a no-op
      const srcBefore = img.src;
      firePreview(deps);
      expect(img.src).toBe(srcBefore);
      expect(img.classList.contains('preview')).toBe(false);
    });

    it('removes .preview and swaps src to full URL when full blob lands and load fires', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Show thumb first (stall path)
      deps.thumbCallbacks.onBlob?.(new Blob(['thumb']));
      firePreview(deps);
      expect(img.classList.contains('preview')).toBe(true);
      const thumbSrc = img.src;

      // Fire debounce, resolve full fetch
      fireDebounce(deps);
      await resolveFetch(deps);

      // Full URL should be set; .preview removed
      expect(img.classList.contains('preview')).toBe(false);
      expect(img.src).not.toBe(thumbSrc);
      expect(img.src).toContain('blob:fake-');
    });

    it('ignores a late thumb that arrives after the full image is shown', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Full fetch lands first (no thumb yet)
      fireDebounce(deps);
      await resolveFetch(deps);
      const fullSrc = img.src;
      expect(img.classList.contains('preview')).toBe(false);

      // Late thumb arrives
      deps.thumbCallbacks.onBlob?.(new Blob(['late-thumb']));

      // Full src must not be clobbered
      expect(img.src).toBe(fullSrc);
      expect(img.classList.contains('preview')).toBe(false);
    });

    it('cancels the thumb request when superseded', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      handle.update({ hash: 'b'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      expect(deps.cancelThumb).toHaveBeenCalled();
    });

    it('revokes a stale thumb blob URL created in a superseded callback', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Capture the A-cycle thumbCallbacks before superseding
      const aThumbCbs = { ...deps.thumbCallbacks };

      // Supersede
      handle.update({ hash: 'b'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Late blob arrives for the superseded seq
      aThumbCbs.onBlob?.(new Blob(['stale-thumb']));

      // A blob URL was created but then immediately revoked
      expect(createObjectURL).toHaveBeenCalled();
      expect(revokeObjectURL).toHaveBeenCalled();
    });

    it('revokes a stashed (unpainted) thumb and clears the preview timer on supersession', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Thumb arrives and is stashed (not painted — preview window not elapsed)
      deps.thumbCallbacks.onBlob?.(new Blob(['thumb']));
      const stashedUrl = createObjectURL.mock.results[0].value as string;
      expect(img.src).toBe(''); // nothing painted

      revokeObjectURL.mockClear();
      const previewIdx = deps.setTimer.mock.calls.findIndex((c) => c[1] === deps.previewMs);
      const previewId = deps.setTimer.mock.results[previewIdx].value as number;

      // Supersede before the preview window elapses
      handle.update({ hash: 'b'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // The stashed thumb is revoked and the preview timer cleared
      expect(revokeObjectURL).toHaveBeenCalledWith(stashedUrl);
      expect(deps.clearTimer).toHaveBeenCalledWith(previewId);
      // Nothing was ever painted for the superseded cycle
      expect(img.classList.contains('preview')).toBe(false);
    });
  });

  describe('reveal on first paint (no broken-image flash on fresh mount)', () => {
    it('does NOT add .ready before anything is painted', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Fresh mount: no src yet, so the element must stay hidden (no .ready).
      expect(img.src).toBe('');
      expect(img.classList.contains('ready')).toBe(false);
    });

    it('adds .ready when the preview thumb is painted (stall path)', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      deps.thumbCallbacks.onBlob?.(new Blob(['thumb']));
      expect(img.classList.contains('ready')).toBe(false); // stashed, not painted
      firePreview(deps);

      expect(img.src).toContain('blob:fake-');
      expect(img.classList.contains('ready')).toBe(true);
    });

    it('adds .ready when the full image is swapped in (fast path)', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      fireDebounce(deps);
      await resolveFetch(deps);

      expect(img.src).toContain('blob:fake-');
      expect(img.classList.contains('ready')).toBe(true);
    });
  });

  describe('off-screen decode before swap', () => {
    it('decodes the full image off-screen and does not touch the visible img until decode resolves', async () => {
      const deps = makeDeps();
      let releaseDecode!: () => void;
      const decodeGate = new Promise<void>((r) => {
        releaseDecode = r;
      });
      deps.decodeImg.mockReturnValueOnce(decodeGate);

      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      fireDebounce(deps);
      await resolveFetch(deps); // fetch + blob done; now awaiting the decode gate

      // Decode was requested for the created blob URL, but nothing is painted yet.
      expect(deps.decodeImg).toHaveBeenCalledOnce();
      const decodedUrl = deps.decodeImg.mock.calls[0][0] as string;
      expect(img.src).toBe('');

      // Decode resolves → the (already-decoded) URL is swapped onto the visible img.
      releaseDecode();
      for (let i = 0; i < 6; i++) await Promise.resolve();

      expect(img.src).toBe(decodedUrl);
      expect(img.classList.contains('preview')).toBe(false);
    });

    it('discards a full image superseded mid-decode without painting it', async () => {
      const deps = makeDeps();
      let releaseDecode!: () => void;
      const decodeGate = new Promise<void>((r) => {
        releaseDecode = r;
      });
      deps.decodeImg.mockReturnValueOnce(decodeGate);

      const action = createStageLoader(deps);
      const img = makeImg();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      fireDebounce(deps);
      await resolveFetch(deps); // awaiting decode

      // Supersede while the A image is still decoding.
      handle.update({ hash: 'b'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      revokeObjectURL.mockClear();
      releaseDecode();
      for (let i = 0; i < 6; i++) await Promise.resolve();

      // The stale, decoded URL is revoked and never painted onto the visible img.
      const decodedUrl = deps.decodeImg.mock.calls[0][0] as string;
      expect(revokeObjectURL).toHaveBeenCalledWith(decodedUrl);
      expect(img.src).not.toBe(decodedUrl);
    });
  });

  describe('onLoadStart / onLoadEnd pairing', () => {
    it('calls onLoadStart once when start() is called', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadStart = vi.fn();
      action(img, { hash: 'a'.repeat(64), onLoadStart, onLoadEnd: vi.fn() });
      expect(onLoadStart).toHaveBeenCalledOnce();
    });

    it('calls onLoadEnd once when the full load event fires', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadEnd = vi.fn();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd });

      fireDebounce(deps);
      await resolveFetch(deps);

      // img.src set; fire load event
      img.dispatchEvent(new Event('load'));
      expect(onLoadEnd).toHaveBeenCalledOnce();
    });

    it('calls onLoadEnd once on img error event', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadEnd = vi.fn();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd });

      fireDebounce(deps);
      await resolveFetch(deps);

      img.dispatchEvent(new Event('error'));
      expect(onLoadEnd).toHaveBeenCalledOnce();
    });

    it('calls onLoadEnd once on fetch network error', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadEnd = vi.fn();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd });

      fireDebounce(deps);
      await rejectFetch(deps, new Error('network failure'));

      expect(onLoadEnd).toHaveBeenCalledOnce();
    });

    it('calls onLoadEnd on supersession (balanced refcount)', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadEndA = vi.fn();
      const onLoadStartB = vi.fn();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: onLoadEndA });

      // Before A settles, supersede with B
      handle.update({ hash: 'b'.repeat(64), onLoadStart: onLoadStartB, onLoadEnd: vi.fn() });

      // Supersession must close A's cycle
      expect(onLoadEndA).toHaveBeenCalledOnce();
      // And open B's
      expect(onLoadStartB).toHaveBeenCalledOnce();
    });

    it('does NOT call onLoadEnd on supersession when cycle was already settled', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadEnd = vi.fn();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd });

      // Let A settle via load event
      fireDebounce(deps);
      await resolveFetch(deps);
      img.dispatchEvent(new Event('load'));
      expect(onLoadEnd).toHaveBeenCalledOnce();

      // Now supersede — should NOT double-call onLoadEnd
      handle.update({ hash: 'b'.repeat(64), onLoadStart: vi.fn(), onLoadEnd });
      expect(onLoadEnd).toHaveBeenCalledOnce(); // still once
    });

    it('does not call onLoadEnd on AbortError (silent)', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadEnd = vi.fn();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd });

      fireDebounce(deps);

      // Inject an AbortError
      const abortErr = new DOMException('aborted', 'AbortError');
      await rejectFetch(deps, abortErr as unknown as Error);

      // AbortError is silent — onLoadEnd called by clear() since cycle was pending...
      // Actually: the AbortError path returns early from runFull, so pending is still true.
      // clear() was NOT called here (no update/destroy). So onLoadEnd is NOT called.
      // (The cycle remains "pending" until clear/destroy, which the test confirms by
      //  NOT seeing onLoadEnd here.)
      expect(onLoadEnd).not.toHaveBeenCalled();
    });
  });

  describe('destroy', () => {
    it('aborts the in-flight fetch controller', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      fireDebounce(deps);

      const abortSpy = vi.spyOn(AbortController.prototype, 'abort');
      handle.destroy();
      expect(abortSpy).toHaveBeenCalled();
    });

    it('clears the debounce timer', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Both the preview and debounce timers were registered
      expect(deps.setTimer).toHaveBeenCalledTimes(2);
      const debounceIdx = deps.setTimer.mock.calls.findIndex((c) => c[1] === deps.settleMs);
      const previewIdx = deps.setTimer.mock.calls.findIndex((c) => c[1] === deps.previewMs);
      const debounceId = deps.setTimer.mock.results[debounceIdx].value as number;
      const previewId = deps.setTimer.mock.results[previewIdx].value as number;

      handle.destroy();
      // destroy() must clear both timers
      expect(deps.clearTimer).toHaveBeenCalledWith(debounceId);
      expect(deps.clearTimer).toHaveBeenCalledWith(previewId);
    });

    it('revokes both thumb and full object URLs on destroy', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      // Load thumb (stall path so it actually paints)
      deps.thumbCallbacks.onBlob?.(new Blob(['thumb']));
      firePreview(deps);
      const thumbUrl = img.src;

      // Load full (but don't fire load event — let destroy revoke it)
      fireDebounce(deps);
      await resolveFetch(deps);
      const fullUrl = img.src;

      revokeObjectURL.mockClear();
      handle.destroy();

      // Both blob URLs should be revoked
      const revokedUrls = revokeObjectURL.mock.calls.map(c => c[0] as string);
      expect(revokedUrls).toContain(thumbUrl);
      expect(revokedUrls).toContain(fullUrl);
    });

    it('neutralises in-flight callbacks after destroy (stale-seq guard)', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadEnd = vi.fn();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd });

      fireDebounce(deps);
      handle.destroy(); // bumps seq

      // Late fetch resolution should not call onLoadEnd again
      await resolveFetch(deps);
      expect(onLoadEnd).toHaveBeenCalledOnce(); // once from destroy's settle(), not from runFull
    });
  });

  describe('object-URL lifecycle', () => {
    it('revokes the thumb URL when the full image load fires', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      deps.thumbCallbacks.onBlob?.(new Blob(['thumb']));
      firePreview(deps); // paint the thumb (stall path)
      const thumbBlobUrl = img.src; // blob:fake-1

      fireDebounce(deps);
      await resolveFetch(deps);
      // img.src is now the full URL

      revokeObjectURL.mockClear();
      img.dispatchEvent(new Event('load'));

      const revokedUrls = revokeObjectURL.mock.calls.map(c => c[0] as string);
      expect(revokedUrls).toContain(thumbBlobUrl);
    });

    it('revokes the full URL after the load event (browser retains decoded bitmap)', async () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      action(img, { hash: 'a'.repeat(64), onLoadStart: vi.fn(), onLoadEnd: vi.fn() });

      fireDebounce(deps);
      await resolveFetch(deps);
      const fullUrl = img.src;

      revokeObjectURL.mockClear();
      img.dispatchEvent(new Event('load'));

      const revokedUrls = revokeObjectURL.mock.calls.map(c => c[0] as string);
      expect(revokedUrls).toContain(fullUrl);
    });
  });

  describe('no-op update on same hash', () => {
    it('does not restart or call onLoadStart again when hash is unchanged', () => {
      const deps = makeDeps();
      const action = createStageLoader(deps);
      const img = makeImg();
      const onLoadStart = vi.fn();
      const handle = action(img, { hash: 'a'.repeat(64), onLoadStart, onLoadEnd: vi.fn() });

      handle.update({ hash: 'a'.repeat(64), onLoadStart, onLoadEnd: vi.fn() });

      // Only the initial start() call
      expect(onLoadStart).toHaveBeenCalledOnce();
      // requestThumb also called only once
      expect(deps.requestThumb).toHaveBeenCalledOnce();
    });
  });
});
