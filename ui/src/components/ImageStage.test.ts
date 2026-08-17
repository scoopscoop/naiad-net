import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import { flushSync } from 'svelte';
import ImageStage from './ImageStage.svelte';

const file = {
  hash: 'abc',
  name: 'a.png',
  size: 1,
  path: '/a.png',
  imported_at: 100,
  created_at: 80,
  modified_at: 90,
  mime: 'image/png',
};
const noop = () => {};

// jsdom has no ResizeObserver; stub with a no-op so the component mounts
// cleanly. Individual tests that need to simulate resizes override this stub
// before they call render().
class NoopRO {
  constructor(_cb: ResizeObserverCallback) {}
  observe() {}
  unobserve() {}
  disconnect() {}
}

// jsdom (at the version used here) has no PointerEvent; provide a minimal
// polyfill so the component's `onpointerdown` handlers receive events with
// a correctly set `button` property (MouseEvent sets it; bare Event does not).
class PointerEventPolyfill extends MouseEvent {
  pointerId: number;
  constructor(type: string, init: PointerEventInit & MouseEventInit = {}) {
    super(type, init);
    this.pointerId = init.pointerId ?? 0;
  }
}

beforeEach(() => {
  vi.stubGlobal('ResizeObserver', NoopRO);
  vi.stubGlobal('PointerEvent', PointerEventPolyfill);
  // Stub URL blob methods so the stage-loader action can run in jsdom.
  URL.createObjectURL = vi.fn(() => 'blob:fake') as unknown as typeof URL.createObjectURL;
  URL.revokeObjectURL = vi.fn() as unknown as typeof URL.revokeObjectURL;
});

describe('ImageStage', () => {
  it('shows the image with the file name as alt text', () => {
    render(ImageStage, { file, hasPrev: false, hasNext: false, onprev: noop, onnext: noop });
    expect(screen.getByAltText('a.png')).toBeInTheDocument();
  });

  it('renders the next arrow only when hasNext', () => {
    render(ImageStage, { file, hasPrev: false, hasNext: true, onprev: noop, onnext: noop });
    expect(screen.getByLabelText('next image')).toBeInTheDocument();
    expect(screen.queryByLabelText('previous image')).toBeNull();
  });

  it('renders an optional one-based position pill', () => {
    render(ImageStage, {
      file,
      hasPrev: false,
      hasNext: false,
      onprev: noop,
      onnext: noop,
      position: { index: 1, total: 12 },
    });
    expect(screen.getByText('2 / 12')).toBeInTheDocument();
  });

  it('renders the prev arrow only when hasPrev', () => {
    render(ImageStage, { file, hasPrev: true, hasNext: false, onprev: noop, onnext: noop });
    expect(screen.getByLabelText('previous image')).toBeInTheDocument();
    expect(screen.queryByLabelText('next image')).toBeNull();
  });

  it('fires onnext / onprev when the arrows are clicked', async () => {
    const onnext = vi.fn();
    const onprev = vi.fn();
    render(ImageStage, { file, hasPrev: true, hasNext: true, onprev, onnext });
    await fireEvent.click(screen.getByLabelText('next image'));
    await fireEvent.click(screen.getByLabelText('previous image'));
    expect(onnext).toHaveBeenCalledTimes(1);
    expect(onprev).toHaveBeenCalledTimes(1);
  });

  it('swaps the image when the file prop changes', async () => {
    const { rerender } = render(ImageStage, {
      file, hasPrev: false, hasNext: false, onprev: noop, onnext: noop,
    });
    expect(screen.getByAltText('a.png')).toBeInTheDocument();
    await rerender({
      file: {
        hash: 'xyz',
        name: 'b.png',
        size: 1,
        path: '/b.png',
        imported_at: 101,
        created_at: 81,
        modified_at: 91,
        mime: 'image/png',
      },
      hasPrev: false, hasNext: false, onprev: noop, onnext: noop,
    });
    expect(screen.getByAltText('b.png')).toBeInTheDocument();
  });
});

describe('ImageStage load feedback', () => {
  /** Stub fetch to resolve immediately with a throwaway Blob. */
  function stubFetch() {
    const blob = new Blob(['x'], { type: 'image/jpeg' });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true,
      blob: () => Promise.resolve(blob),
    } as unknown as Response));
  }

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  function renderStage() {
    return render(ImageStage, {
      file, hasPrev: false, hasNext: false, onprev: noop, onnext: noop,
    });
  }

  it('shows a spinner once a load outlives the grace period, then clears it on load', async () => {
    vi.useFakeTimers();
    stubFetch();

    renderStage();

    // Advance past both the 150 ms createPending delay (spinner) AND the
    // 150 ms stage-loader debounce (full fetch).  Fetch resolves as a
    // microtask so img.src is set and listeners attached.
    await vi.advanceTimersByTimeAsync(200);
    flushSync();
    expect(screen.getByRole('status')).toBeInTheDocument();

    // Fire the full-image load event — action calls onLoadEnd → spinner settles.
    screen.getByAltText('a.png').dispatchEvent(new Event('load'));
    await vi.advanceTimersByTimeAsync(400); // past HOLD_MS (300 ms)
    flushSync();
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('clears the spinner on a failed load too', async () => {
    vi.useFakeTimers();
    stubFetch();

    renderStage();
    await vi.advanceTimersByTimeAsync(200);
    flushSync();
    expect(screen.getByRole('status')).toBeInTheDocument();

    screen.getByAltText('a.png').dispatchEvent(new Event('error'));
    await vi.advanceTimersByTimeAsync(400);
    flushSync();
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('keeps one pending load across rapid file swaps and settles on the final load', async () => {
    vi.useFakeTimers();
    stubFetch();

    const { rerender } = renderStage();

    // File A's cycle starts; spinner shows after 150 ms.
    await vi.advanceTimersByTimeAsync(200);
    flushSync();
    expect(screen.getByRole('status')).toBeInTheDocument();

    // Swap to file B while A is in flight.
    // The action: clear(A) [calls onLoadEnd → loadPending.end] + start(B) [calls
    // onLoadStart → loadPending.start].  Spinner stays (hold-window re-arms).
    await rerender({
      file: { ...file, hash: 'xyz', name: 'b.png' },
      hasPrev: false, hasNext: false, onprev: noop, onnext: noop,
    });
    flushSync();
    expect(screen.getByRole('status')).toBeInTheDocument();

    // Let B's debounce fire and fetch resolve so its load listener is wired up.
    await vi.advanceTimersByTimeAsync(200);
    flushSync();

    // Settle on B's load event.
    screen.getByAltText('b.png').dispatchEvent(new Event('load'));
    await vi.advanceTimersByTimeAsync(400); // past HOLD_MS
    flushSync();
    expect(screen.queryByRole('status')).toBeNull();
  });
});

describe('ImageStage rect caching (F5)', () => {
  // Svelte 5 $state wraps the bound element in a Proxy, so spying on the raw
  // DOM instance's getBoundingClientRect doesn't intercept calls made through
  // the component's `stage` variable. Spy on Element.prototype instead so all
  // getBoundingClientRect calls are captured regardless of proxy wrapping.

  afterEach(() => vi.restoreAllMocks());

  it('captures rect at pointerdown and does not call getBoundingClientRect during pointermove', async () => {
    const getBCR = vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({
      left: 0, top: 0, width: 800, height: 600,
      right: 800, bottom: 600, x: 0, y: 0,
      toJSON: () => ({}),
    } as DOMRect);

    const { container } = render(ImageStage, { file, hasPrev: false, hasNext: false, onprev: noop, onnext: noop });
    flushSync();
    const stage = container.querySelector('.stage') as HTMLDivElement;

    // jsdom lacks pointer-capture support; stub to avoid errors.
    stage.setPointerCapture = vi.fn();
    stage.releasePointerCapture = vi.fn();

    // Reset count after mount ($effect's initial measurement).
    getBCR.mockClear();

    // Drag start: one capture. Use a raw PointerEvent so jsdom doesn't
    // silently drop the button property via a testing-library shim.
    stage.dispatchEvent(new PointerEvent('pointerdown', {
      button: 0, bubbles: true, cancelable: true,
      clientX: 100, clientY: 100, pointerId: 1,
    }));
    flushSync();
    expect(getBCR).toHaveBeenCalledTimes(1);

    // Drag moves: zero additional layout reads.
    stage.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, clientX: 110, clientY: 110, pointerId: 1 }));
    stage.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, clientX: 120, clientY: 120, pointerId: 1 }));
    stage.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, clientX: 130, clientY: 130, pointerId: 1 }));
    flushSync();
    expect(getBCR).toHaveBeenCalledTimes(1); // still exactly 1
  });

  it('pan clamping uses the rect captured at pointerdown (cached vs live equivalence)', async () => {
    // Return a known 400×300 stage so clampPan limit = 400*1/2 = 200.
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({
      left: 0, top: 0, width: 400, height: 300,
      right: 400, bottom: 300, x: 0, y: 0,
      toJSON: () => ({}),
    } as DOMRect);

    const { container } = render(ImageStage, { file, hasPrev: false, hasNext: false, onprev: noop, onnext: noop });
    flushSync();
    const stage = container.querySelector('.stage') as HTMLDivElement;

    stage.setPointerCapture = vi.fn();
    stage.releasePointerCapture = vi.fn();

    // Pan far right — clampPan(9999, 400, 1) = 200.
    stage.dispatchEvent(new PointerEvent('pointerdown', {
      button: 0, bubbles: true, cancelable: true, clientX: 0, clientY: 0, pointerId: 1,
    }));
    stage.dispatchEvent(new PointerEvent('pointermove', {
      bubbles: true, clientX: 9999, clientY: 0, pointerId: 1,
    }));
    flushSync();

    const img = screen.getByAltText('a.png') as HTMLImageElement;
    // The transform is "translate(Xpx, Ypx) scale(1)"; extract the X value.
    const match = img.style.transform.match(/translate\(([^p]+)px/);
    expect(match).not.toBeNull();
    expect(parseFloat(match![1])).toBeCloseTo(200);
  });

  it('does not call getBoundingClientRect during wheel events (uses cached stageRect)', async () => {
    const getBCR = vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({
      left: 0, top: 0, width: 800, height: 600,
      right: 800, bottom: 600, x: 0, y: 0,
      toJSON: () => ({}),
    } as DOMRect);

    const { container } = render(ImageStage, { file, hasPrev: false, hasNext: false, onprev: noop, onnext: noop });
    flushSync(); // ensure $effect runs so stageRect is populated
    const stage = container.querySelector('.stage') as HTMLDivElement;

    // Reset after mount ($effect's initial measurement) — only catch leaks into wheel.
    getBCR.mockClear();

    await fireEvent.wheel(stage, { deltaY: -100, clientX: 400, clientY: 300 });
    await fireEvent.wheel(stage, { deltaY: 100, clientX: 400, clientY: 300 });

    expect(getBCR).not.toHaveBeenCalled();
  });

  it('refreshes stageRect when ResizeObserver fires', async () => {
    // Override the no-op RO with a capturing one before this render.
    let roCallback: ResizeObserverCallback | null = null;
    class CapturingRO {
      constructor(cb: ResizeObserverCallback) {
        roCallback = cb;
        CapturingRO.instance = this;
      }
      static instance: CapturingRO;
      observe(_el: Element) {}
      disconnect() {}
      fire() {
        roCallback!([], this as unknown as ResizeObserver);
      }
    }
    vi.stubGlobal('ResizeObserver', CapturingRO);

    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({
      left: 50, top: 50, width: 1200, height: 900,
      right: 1250, bottom: 950, x: 50, y: 50,
      toJSON: () => ({}),
    } as DOMRect);

    const { container: _ } = render(ImageStage, { file, hasPrev: false, hasNext: false, onprev: noop, onnext: noop });
    flushSync();

    // Clear mount-time calls; the RO fire should add exactly one more.
    const getBCR = vi.spyOn(Element.prototype, 'getBoundingClientRect');
    getBCR.mockClear();

    expect(roCallback).not.toBeNull();
    CapturingRO.instance.fire();

    expect(getBCR).toHaveBeenCalledTimes(1);
  });

  it('scroll marks rect dirty (zero BCR); first wheel recomputes lazily (one BCR)', async () => {
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({
      left: 0, top: 0, width: 800, height: 600,
      right: 800, bottom: 600, x: 0, y: 0,
      toJSON: () => ({}),
    } as DOMRect);

    const { container } = render(ImageStage, { file, hasPrev: false, hasNext: false, onprev: noop, onnext: noop });
    flushSync();
    const stage = container.querySelector('.stage') as HTMLDivElement;

    // Clear mount-time BCR calls.
    const getBCR = vi.spyOn(Element.prototype, 'getBoundingClientRect');
    getBCR.mockClear();

    // Scroll event: only marks dirty, never reads layout.
    window.dispatchEvent(new Event('scroll', { bubbles: false }));
    expect(getBCR).not.toHaveBeenCalled();

    // First wheel after scroll: one lazy BCR recompute.
    await fireEvent.wheel(stage, { deltaY: -100, clientX: 400, clientY: 300 });
    expect(getBCR).toHaveBeenCalledTimes(1);

    // Subsequent wheel (no intervening scroll): zero additional BCR reads.
    getBCR.mockClear();
    await fireEvent.wheel(stage, { deltaY: 100, clientX: 400, clientY: 300 });
    expect(getBCR).not.toHaveBeenCalled();
  });
});
