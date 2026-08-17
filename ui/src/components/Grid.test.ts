import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render } from '@testing-library/svelte';
import { flushSync, tick } from 'svelte';
import Grid from './Grid.svelte';
import type { FileDto } from '../lib/types';
import { thumbStream } from '../lib/thumb-stream';
import { computeGrid, computeWindow, scrollTargetForIndex, anchorForViewport, scrollTopForAnchor } from '../lib/grid-window';

// jsdom has no ResizeObserver; a no-op stub is enough since Grid also reads
// geometry directly via an initial measure().
class RO {
  constructor(_cb: ResizeObserverCallback) {}
  observe() {}
  unobserve() {}
  disconnect() {}
}

beforeEach(() => {
  vi.stubGlobal('ResizeObserver', RO);
  // Run rAF synchronously so scroll-driven recompute is deterministic.
  vi.stubGlobal('requestAnimationFrame', (fn: FrameRequestCallback) => {
    fn(0);
    return 0;
  });
  vi.stubGlobal('cancelAnimationFrame', () => {});
  vi.spyOn(thumbStream, 'request').mockImplementation(() => () => {});
  vi.stubGlobal('URL', {
    ...URL,
    createObjectURL: vi.fn(() => 'blob:stub'),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function makeFiles(n: number): FileDto[] {
  return Array.from({ length: n }, (_, i) => ({
    hash: i.toString(16).padStart(64, '0'),
    name: `f${i}.png`,
    size: 1,
    path: `/f${i}.png`,
    imported_at: 100 + i,
    created_at: 80 + i,
    modified_at: 90 + i,
    mime: 'image/png',
  }));
}

function makeScrollParent(width: number, height: number): HTMLElement {
  const el = document.createElement('div');
  Object.defineProperty(el, 'clientWidth', { value: width, configurable: true });
  Object.defineProperty(el, 'clientHeight', { value: height, configurable: true });
  Object.defineProperty(el, 'scrollTop', { value: 0, writable: true, configurable: true });
  document.body.appendChild(el);
  return el;
}

describe('Grid virtualization', () => {
  it('renders only a windowed slice, not the whole set', () => {
    const files = makeFiles(1000);
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, { files, columns: 3, scrollParent: parent, onselect: () => {} });
    const cells = container.querySelectorAll('.cell');
    expect(cells.length).toBeGreaterThan(0);
    expect(cells.length).toBeLessThan(50); // a few rows, not 1000
  });

  it('sizes the spacer to the full scroll height of all rows', () => {
    const files = makeFiles(1000);
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, { files, columns: 3, scrollParent: parent, onselect: () => {} });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    // ~334 rows * ~226px row height -> tens of thousands of px.
    expect(parseFloat(viewport.style.height)).toBeGreaterThan(50000);
  });

  it('passes the global index to onfocus for a clicked cell', () => {
    const files = makeFiles(1000);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, { files, columns: 3, scrollParent: parent, onfocus });
    (container.querySelector('.cell') as HTMLButtonElement).dispatchEvent(
      new MouseEvent('click', { bubbles: true, detail: 1 }),
    );
    expect(onfocus).toHaveBeenCalledWith(files[0], 0); // top of grid -> global index 0
  });

  it('plain click focuses a cell without opening it', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const onopen = vi.fn();
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      onselect: vi.fn(),
      onfocus,
      onopen,
    });
    (container.querySelector('.cell') as HTMLButtonElement).dispatchEvent(
      new MouseEvent('click', { bubbles: true, detail: 1 }),
    );
    expect(onfocus).toHaveBeenCalledWith(files[0], 0);
    expect(onopen).not.toHaveBeenCalled();
  });

  it('middle click always opens in the background', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onopen = vi.fn();
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      onselect: vi.fn(),
      onopen,
      onfocus,
    });
    const cell = container.querySelector('.cell') as HTMLButtonElement;
    cell.dispatchEvent(new MouseEvent('auxclick', { bubbles: true, button: 1 }));
    expect(onopen).toHaveBeenCalledWith(files[0], 0, true);
    expect(onfocus).toHaveBeenCalledWith(files[0], 0);
  });

  it('double click always opens in the foreground', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onopen = vi.fn();
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      onselect: vi.fn(),
      onopen,
      onfocus,
    });
    const cell = container.querySelector('.cell') as HTMLButtonElement;
    cell.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    expect(onopen).toHaveBeenCalledWith(files[0], 0, false);
    expect(onfocus).toHaveBeenCalledWith(files[0], 0);
  });

  it('keyboard click opens detail', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onopen = vi.fn();
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      onselect: vi.fn(),
      onopen,
    });
    const cell = container.querySelector('.cell') as HTMLButtonElement;
    cell.dispatchEvent(new MouseEvent('click', { bubbles: true, detail: 0 }));
    expect(onopen).toHaveBeenCalledWith(files[0], 0, false);
  });

  it('plain click never opens, even without an inspector', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onopen = vi.fn();
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      onselect: vi.fn(),
      onfocus,
      onopen,
    });
    (container.querySelector('.cell') as HTMLButtonElement).dispatchEvent(
      new MouseEvent('click', { bubbles: true, detail: 1 }),
    );
    expect(onopen).not.toHaveBeenCalled();
    expect(onfocus).toHaveBeenCalledWith(files[0], 0);
  });

  it('ctrl-click selects without opening', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onopen = vi.fn();
    const onselection = vi.fn();
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      onselect: vi.fn(),
      onopen,
      onselection,
    });
    (container.querySelector('.cell') as HTMLButtonElement).dispatchEvent(
      new MouseEvent('click', { bubbles: true, detail: 1, ctrlKey: true }),
    );
    expect(onselection).toHaveBeenCalled();
    expect(onopen).not.toHaveBeenCalled();
  });

  it('prevents middle mousedown autoscroll', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      onselect: vi.fn(),
    });
    const cell = container.querySelector('.cell') as HTMLButtonElement;
    const event = new MouseEvent('mousedown', { bubbles: true, cancelable: true, button: 1 });
    cell.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
  });

  it('renders nothing but an empty spacer for an empty set', () => {
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, { files: [], columns: 3, scrollParent: parent, onselect: () => {} });
    expect(container.querySelectorAll('.cell').length).toBe(0);
  });
});

describe('Grid selection', () => {
  it('routes ctrl+click to onselection, not onselect', async () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onselect = vi.fn();
    const onselection = vi.fn();
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect,
        selected: new Set<string>(), anchor: null, onselection,
      },
    });
    const cell = container.querySelector('.cell') as HTMLButtonElement;
    cell.dispatchEvent(new MouseEvent('click', { bubbles: true, ctrlKey: true }));
    expect(onselect).not.toHaveBeenCalled();
    expect(onselection).toHaveBeenCalledTimes(1);
    const next = onselection.mock.calls[0][0];
    expect([...next.selected]).toEqual([files[0].hash]);
    expect(next.anchor).toBe(files[0].hash);
  });

  it('routes shift+click ranges through onselection', async () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onselection = vi.fn();
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(),
        selected: new Set<string>(), anchor: files[0].hash, onselection,
      },
    });
    const cells = container.querySelectorAll('.cell');
    cells[2].dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }));
    const next = onselection.mock.calls[0][0];
    expect([...next.selected].sort()).toEqual([files[0].hash, files[1].hash, files[2].hash].sort());
  });

  it('plain click selects and anchors the tile it focuses (#110)', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const onselection = vi.fn();
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(), onfocus,
        selected: new Set([files[7].hash]), anchor: files[7].hash, onselection,
      },
    });
    const cells = container.querySelectorAll('.cell');
    cells[2].dispatchEvent(new MouseEvent('click', { bubbles: true, detail: 1 }));
    const next = onselection.mock.calls[0][0];
    expect([...next.selected]).toEqual([files[2].hash]);
    expect(next.anchor).toBe(files[2].hash);
    // Still focuses the inspector as before (#63).
    expect(onfocus).toHaveBeenCalledTimes(1);
  });

  it('shift+click falls back to the focused tile when there is no anchor (#110)', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onselection = vi.fn();
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(),
        // Band select / select-all leave anchor null; arrow keys left focus here.
        selected: new Set<string>(), anchor: null, focused: files[1].hash, onselection,
      },
    });
    const cells = container.querySelectorAll('.cell');
    cells[3].dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true, detail: 1 }));
    const next = onselection.mock.calls[0][0];
    expect([...next.selected].sort()).toEqual([files[1].hash, files[2].hash, files[3].hash].sort());
    expect(next.anchor).toBe(files[1].hash);
  });

  it('marks selected cells with the selected class', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(),
        selected: new Set([files[1].hash]), anchor: null, onselection: vi.fn(),
      },
    });
    const cells = container.querySelectorAll('.cell');
    expect(cells[1].classList.contains('selected')).toBe(true);
    expect(cells[0].classList.contains('selected')).toBe(false);
  });

  function pointer(el: Element, type: string, x: number, y: number, init: MouseEventInit = {}) {
    // jsdom has no PointerEvent; MouseEvent carries everything the handlers read.
    const e = new MouseEvent(type, { bubbles: true, clientX: x, clientY: y, button: 0, ...init });
    // Flush so the band DOM reflects the event before the test asserts.
    flushSync(() => el.dispatchEvent(e));
  }

  it('drag on empty space commits a band selection over the first row', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onselection = vi.fn();
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(),
        selected: new Set<string>(), anchor: null, onselection,
      },
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    pointer(viewport, 'pointerdown', 20, 16);
    pointer(viewport, 'pointermove', 690, 220);
    expect(container.querySelector('.band')).not.toBeNull();
    pointer(viewport, 'pointerup', 690, 220);
    const next = onselection.mock.calls.at(-1)![0];
    // Row 0 = indices 0..2 (3 columns at this width).
    expect([...next.selected].sort()).toEqual([files[0].hash, files[1].hash, files[2].hash].sort());
    expect(next.anchor).toBeNull();
    expect(container.querySelector('.band')).toBeNull();
  });

  it('ctrl+drag adds to the existing selection', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onselection = vi.fn();
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(),
        selected: new Set([files[9].hash]), anchor: null, onselection,
      },
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    pointer(viewport, 'pointerdown', 20, 16, { ctrlKey: true });
    pointer(viewport, 'pointermove', 690, 220, { ctrlKey: true });
    pointer(viewport, 'pointerup', 690, 220, { ctrlKey: true });
    const next = onselection.mock.calls.at(-1)![0];
    expect(next.selected.has(files[9].hash)).toBe(true);
    expect(next.selected.has(files[0].hash)).toBe(true);
  });

  it('a plain sub-threshold click on empty space clears the selection', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onselection = vi.fn();
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(),
        selected: new Set([files[0].hash]), anchor: files[0].hash, onselection,
      },
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    pointer(viewport, 'pointerdown', 690, 350);
    pointer(viewport, 'pointerup', 690, 350);
    const next = onselection.mock.calls.at(-1)![0];
    expect(next.selected.size).toBe(0);
  });

  it('does not start a band from a pointerdown on a cell', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onselection = vi.fn();
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(),
        selected: new Set<string>(), anchor: null, onselection,
      },
    });
    const cell = container.querySelector('.cell') as HTMLElement;
    pointer(cell, 'pointerdown', 20, 20);
    pointer(container.querySelector('.grid-viewport')!, 'pointermove', 400, 300);
    expect(container.querySelector('.band')).toBeNull();
  });

  it('auto-scrolls the parent while dragging near the bottom edge, stopping at the end', () => {
    const files = makeFiles(100);
    const parent = makeScrollParent(700, 400);
    // Give the container a real box (jsdom rects are all zeros, which the
    // guard treats as "never auto-scroll") ...
    Object.defineProperty(parent, 'getBoundingClientRect', {
      configurable: true,
      value: () => ({
        top: 0, bottom: 400, height: 400, left: 0, right: 700, width: 700,
        x: 0, y: 0, toJSON: () => ({}),
      }),
    });
    // ...and a clamped scrollTop so the synchronous-rAF tick recursion
    // terminates via the tick's `scrollTop === before` end-of-scroll check.
    let st = 0;
    Object.defineProperty(parent, 'scrollTop', {
      configurable: true,
      get: () => st,
      set: (v: number) => { st = Math.max(0, Math.min(120, v)); },
    });
    const { container } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(),
        selected: new Set<string>(), anchor: null, onselection: vi.fn(),
      },
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    pointer(viewport, 'pointerdown', 20, 16);
    // Inside the 24px bottom edge strip: the synchronous rAF stub drives the
    // tick chain to the clamp during this single dispatched move.
    pointer(viewport, 'pointermove', 300, 395);
    expect(parent.scrollTop).toBeGreaterThan(0);
    expect(parent.scrollTop).toBe(120); // hit the end and stopped
  });

  it('abandons an in-flight band when the file list changes mid-drag', async () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onselection = vi.fn();
    const { container, rerender } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent, onselect: vi.fn(),
        selected: new Set<string>(), anchor: null, onselection,
      },
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    pointer(viewport, 'pointerdown', 20, 16);
    pointer(viewport, 'pointermove', 690, 220);
    expect(container.querySelector('.band')).not.toBeNull();
    // Tab switch / new search results: the files prop swaps identity while
    // pointer capture keeps the drag alive.
    await rerender({ files: makeFiles(6) });
    flushSync();
    expect(container.querySelector('.band')).toBeNull();
    pointer(viewport, 'pointerup', 690, 220);
    expect(onselection).not.toHaveBeenCalled(); // no commit against stale base
    expect(container.querySelector('.band')).toBeNull();
  });
});

describe('Grid covered (geometry freeze, #60)', () => {
  // Capturing RO stub: lets a test simulate the container resize that happens
  // when the inspector unmounts while a detail tab covers the grid.
  class CapturingRO {
    static instances: CapturingRO[] = [];
    cb: ResizeObserverCallback;
    active = false;
    constructor(cb: ResizeObserverCallback) {
      this.cb = cb;
      CapturingRO.instances.push(this);
    }
    observe() {
      this.active = true;
    }
    unobserve() {}
    disconnect() {
      this.active = false;
    }
    fire() {
      if (this.active) this.cb([], this as unknown as ResizeObserver);
    }
  }

  function setWidth(el: HTMLElement, width: number) {
    Object.defineProperty(el, 'clientWidth', { value: width, configurable: true });
  }

  it('ignores container resizes while covered and does not refetch thumbs', async () => {
    CapturingRO.instances = [];
    vi.stubGlobal('ResizeObserver', CapturingRO);
    const files = makeFiles(1000);
    const parent = makeScrollParent(700, 400);
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    const requestSpy = thumbStream.request as unknown as ReturnType<typeof vi.fn>;
    const { container, rerender } = render(Grid, {
      props: { files, columns: 3, scrollParent: parent, onselect: vi.fn(), covered: false },
    });
    const grid = () => container.querySelector('.grid') as HTMLElement;
    vi.advanceTimersByTime(125);
    const columnsBefore = grid().style.gridTemplateColumns;
    const cellsBefore = container.querySelectorAll('.cell').length;
    const callsBefore = requestSpy.mock.calls.length;
    expect(callsBefore).toBeGreaterThan(0);

    // Detail tab opens: grid gets covered, then the inspector unmounts and the
    // container widens.
    await rerender({ covered: true });
    setWidth(parent, 1200);
    CapturingRO.instances.forEach((ro) => ro.fire());
    flushSync();
    expect(grid().style.gridTemplateColumns).toBe(columnsBefore);
    expect(container.querySelectorAll('.cell').length).toBe(cellsBefore);
    expect(requestSpy.mock.calls.length).toBe(callsBefore);

    // Switch back: inspector remounts (width restored), then the grid uncovers.
    setWidth(parent, 700);
    await rerender({ covered: false });
    flushSync();
    expect(grid().style.gridTemplateColumns).toBe(columnsBefore);
    expect(container.querySelectorAll('.cell').length).toBe(cellsBefore);
    expect(requestSpy.mock.calls.length).toBe(callsBefore);
  });

  it('re-measures on uncover when the container size really did change', async () => {
    CapturingRO.instances = [];
    vi.stubGlobal('ResizeObserver', CapturingRO);
    const files = makeFiles(1000);
    const parent = makeScrollParent(700, 400);
    const { container, rerender } = render(Grid, {
      props: { files, columns: 3, scrollParent: parent, onselect: vi.fn(), covered: false },
    });
    // Columns are fixed by the zoom level (#171); a width change shows up as a
    // new derived tile size, i.e. a different row height and spacer height.
    const viewport = () => container.querySelector('.grid-viewport') as HTMLElement;
    const heightBefore = viewport().style.height;

    await rerender({ covered: true });
    setWidth(parent, 1400); // e.g. the window was resized while a detail tab was up
    await rerender({ covered: false });
    flushSync();
    expect(viewport().style.height).not.toBe(heightBefore);
  });
});

describe('Grid arrow-key navigation (F2a)', () => {
  function keydown(el: Element, key: string) {
    el.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }));
  }

  it('ArrowRight moves focus to the next file', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus, focused: files[0].hash, focusedIndex: 0,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowRight');
    expect(onfocus).toHaveBeenCalledWith(files[1], 1);
  });

  it('ArrowLeft moves focus to the previous file', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus, focused: files[3].hash, focusedIndex: 3,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowLeft');
    expect(onfocus).toHaveBeenCalledWith(files[2], 2);
  });

  it('ArrowDown moves focus by one column-count forward', () => {
    // At 700px width the grid has PAD_X=16 each side → availWidth=668.
    // computeGrid(668, 160, 10): floor((668+10)/(160+10)) = floor(678/170) = 3 columns.
    const files = makeFiles(20);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus, focused: files[0].hash, focusedIndex: 0,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowDown');
    // 3 columns → index 0 + 3 = 3
    expect(onfocus).toHaveBeenCalledWith(files[3], 3);
  });

  it('ArrowUp moves focus by one column-count backward', () => {
    // 3 columns at 700px (see ArrowDown test for derivation).
    const files = makeFiles(20);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus, focused: files[8].hash, focusedIndex: 8,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowUp');
    // 3 columns → index 8 - 3 = 5
    expect(onfocus).toHaveBeenCalledWith(files[5], 5);
  });

  it('clamps at the first item (no wrap)', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus, focused: files[0].hash, focusedIndex: 0,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowLeft');
    // Still at 0, onfocus called with files[0]
    expect(onfocus).toHaveBeenCalledWith(files[0], 0);
  });

  it('clamps at the last item (no wrap)', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus, focused: files[9].hash, focusedIndex: 9,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowRight');
    expect(onfocus).toHaveBeenCalledWith(files[9], 9);
  });

  it('starts navigation at index 0 when nothing is focused', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus, focused: null,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowRight');
    expect(onfocus).toHaveBeenCalledWith(files[0], 0);
  });

  it('does not intercept arrows when focus is in an editable element', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus, focused: files[0].hash, focusedIndex: 0,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    // Simulate arrow key from an INPUT target
    const e = new KeyboardEvent('keydown', {
      key: 'ArrowRight', bubbles: true, cancelable: true,
    });
    Object.defineProperty(e, 'target', { value: document.createElement('input') });
    viewport.dispatchEvent(e);
    expect(onfocus).not.toHaveBeenCalled();
  });

  it('does not intercept arrows when covered by a detail tab', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus,
      focused: files[0].hash, focusedIndex: 0, covered: true,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowRight');
    expect(onfocus).not.toHaveBeenCalled();
  });

  it('scrolls the target row into view when navigating off-screen', () => {
    // Grid constants: PAD_X=16, GAP=10, PAD_TOP=14.
    // availWidth=668 → 3 cols, tileWidth=216, rowHeight=226.
    // focused=index 4 (row 1). ArrowDown → index 7 (row 2).
    // Row 2: top=466, bottom=692. viewport=400 → scrollTop = 692-400 = 292.
    const PAD_X = 16, GAP = 10, PAD_TOP = 14;
    const m = computeGrid(700 - 2 * PAD_X, 3, GAP);
    const startIdx = 4;
    const targetIdx = startIdx + m.columns; // 7
    const expectedScroll = scrollTargetForIndex(targetIdx, m.columns, m.rowHeight, PAD_TOP, 0, 400);

    const files = makeFiles(1000);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus,
      focused: files[startIdx].hash, focusedIndex: startIdx,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowDown');
    expect(parent.scrollTop).toBe(expectedScroll);
  });

  it('focuses the target cell button when it is already rendered', () => {
    // 10 files all fit in one screen → every cell is in the initial slice.
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, focused: files[0].hash, focusedIndex: 0,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    const cells = container.querySelectorAll('.cell');
    keydown(viewport, 'ArrowRight');
    // After ArrowRight the cell at index 1 should hold DOM focus.
    expect(document.activeElement).toBe(cells[1]);
  });

  it('plain arrow key re-anchors so a subsequent shift-click ranges from the arrived-at item (#110)', async () => {
    // Scenario: click at index 0 (sets anchor=0), ArrowRight ×1 (focus+anchor move to 1),
    // shift-click at index 3 → range must be 1→3, not 0→3.
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onselection = vi.fn();
    const { container, rerender } = render(Grid, {
      props: {
        files, columns: 3, scrollParent: parent,
        focused: files[0].hash, focusedIndex: 0,
        selected: new Set([files[0].hash]), anchor: files[0].hash,
        onselection,
      },
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;

    // Arrow right: focus moves to index 1; onselection must update anchor too.
    keydown(viewport, 'ArrowRight');
    const afterArrow = onselection.mock.calls.at(-1)![0];
    expect(afterArrow.anchor).toBe(files[1].hash);

    // Simulate the app applying the new anchor before the shift-click.
    onselection.mockClear();
    await rerender({ anchor: files[1].hash, focusedIndex: 1, focused: files[1].hash });

    // Shift-click at index 3 must range from the arrow-arrived-at anchor (1→3).
    const cells = container.querySelectorAll('.cell');
    cells[3].dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true, detail: 1 }));
    const afterShift = onselection.mock.calls[0][0];
    expect([...afterShift.selected].sort()).toEqual(
      [files[1].hash, files[2].hash, files[3].hash].sort(),
    );
  });

  it('parks DOM focus on the viewport when the target cell is off-screen', () => {
    // Derive the initial rendered slice using the same constants as Grid.svelte.
    const PAD_X = 16, GAP = 10, PAD_TOP = 14, MIN_OVERSCAN = 2;
    const files = makeFiles(1000);
    const m = computeGrid(700 - 2 * PAD_X, 3, GAP);
    const overscan = Math.max(MIN_OVERSCAN, Math.ceil(400 / m.rowHeight));
    const initialSlice = computeWindow(files.length, m.columns, m.rowHeight, 0 - PAD_TOP, 400, overscan);
    // The last rendered cell; ArrowDown jumps +columns which lands off-screen.
    const lastRenderedIdx = initialSlice.endIndex - 1;
    const arrowTarget = lastRenderedIdx + m.columns;
    expect(arrowTarget).toBeGreaterThanOrEqual(initialSlice.endIndex); // verify assumption

    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent,
      focused: files[lastRenderedIdx].hash,
      focusedIndex: lastRenderedIdx,
    });
    const viewport = container.querySelector('.grid-viewport') as HTMLElement;
    keydown(viewport, 'ArrowDown');
    // arrowTarget is not yet in the DOM → focus falls back to the viewport.
    expect(document.activeElement).toBe(viewport);
  });
});

describe('Grid focus sync on Tab (F2b)', () => {
  it('fires onfocus when a cell receives browser focus (e.g. via Tab)', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus,
    });
    const cell = container.querySelector('.cell') as HTMLButtonElement;
    // Simulate the browser focus event (Tab-navigation).
    cell.dispatchEvent(new Event('focus', { bubbles: true }));
    expect(onfocus).toHaveBeenCalledWith(files[0], 0);
  });

  it('reports the correct global index for the second visible cell', () => {
    const files = makeFiles(10);
    const parent = makeScrollParent(700, 400);
    const onfocus = vi.fn();
    const { container } = render(Grid, {
      files, columns: 3, scrollParent: parent, onfocus,
    });
    const cells = container.querySelectorAll('.cell');
    cells[2].dispatchEvent(new Event('focus', { bubbles: true }));
    expect(onfocus).toHaveBeenCalledWith(files[2], 2);
  });
});

describe('Grid fit prop', () => {
  it('thumb img has no fill class when fit is frame', () => {
    const files = makeFiles(3);
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, { files, columns: 3, scrollParent: parent, fit: 'frame' });
    const thumb = container.querySelector('.thumb') as HTMLImageElement;
    expect(thumb).not.toBeNull();
    expect(thumb.classList.contains('fill')).toBe(false);
  });

  it('thumb img has no fill class when fit prop is omitted (default is frame)', () => {
    const files = makeFiles(3);
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, { files, columns: 3, scrollParent: parent });
    const thumb = container.querySelector('.thumb') as HTMLImageElement;
    expect(thumb).not.toBeNull();
    expect(thumb.classList.contains('fill')).toBe(false);
  });

  it('thumb img has fill class when fit is fill', () => {
    const files = makeFiles(3);
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, { files, columns: 3, scrollParent: parent, fit: 'fill' });
    const thumb = container.querySelector('.thumb') as HTMLImageElement;
    expect(thumb).not.toBeNull();
    expect(thumb.classList.contains('fill')).toBe(true);
  });

  it('toggles the fill class reactively when the fit prop changes after mount', async () => {
    const files = makeFiles(3);
    const parent = makeScrollParent(700, 400);
    const { container, rerender } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      fit: 'frame',
    });
    const thumb = () => container.querySelector('.thumb') as HTMLImageElement;
    expect(thumb().classList.contains('fill')).toBe(false);

    await rerender({ files, columns: 3, scrollParent: parent, fit: 'fill' });
    expect(thumb().classList.contains('fill')).toBe(true);

    await rerender({ files, columns: 3, scrollParent: parent, fit: 'frame' });
    expect(thumb().classList.contains('fill')).toBe(false);
  });
});

describe('Grid thumbnail loading', () => {
  it('requests a thumb through the stream for each stable visible tile', () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    const files = makeFiles(1000);
    const parent = makeScrollParent(700, 400);
    const requestSpy = thumbStream.request as unknown as ReturnType<typeof vi.fn>;
    render(Grid, { files, columns: 3, scrollParent: parent, onselect: () => {} });
    vi.advanceTimersByTime(125);
    expect(requestSpy.mock.calls.length).toBeGreaterThan(0);
    for (const [hash] of requestSpy.mock.calls) {
      expect(hash).toMatch(/^[0-9a-f]{64}$/);
    }
  });

  it('cancels a tile request when the tile scrolls out of the window', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    const files = makeFiles(1000);
    const parent = makeScrollParent(700, 400);
    const cancels: ReturnType<typeof vi.fn>[] = [];
    (thumbStream.request as unknown as ReturnType<typeof vi.fn>).mockImplementation(() => {
      const c = vi.fn();
      cancels.push(c);
      return c;
    });
    render(Grid, { files, columns: 3, scrollParent: parent, onselect: () => {} });
    vi.advanceTimersByTime(125);
    const firstBatch = cancels.length;
    expect(firstBatch).toBeGreaterThan(0);
    Object.defineProperty(parent, 'scrollTop', { value: 20000, configurable: true, writable: true });
    parent.dispatchEvent(new Event('scroll'));
    flushSync();
    expect(cancels.slice(0, firstBatch).some((c) => c.mock.calls.length > 0)).toBe(true);
  });
});

describe('scroll anchoring on reflow', () => {
  const PAD_X = 16, PAD_TOP = 14;

  afterEach(() => vi.unstubAllGlobals());

  function renderWithCapturedRO(width: number) {
    let roCb: ResizeObserverCallback | undefined;
    class CapturingRO {
      constructor(cb: ResizeObserverCallback) { roCb = cb; }
      observe() {}
      unobserve() {}
      disconnect() {}
    }
    vi.stubGlobal('ResizeObserver', CapturingRO);
    const files = makeFiles(2000);
    const parent = makeScrollParent(width, 400);
    render(Grid, { files, columns: 3, scrollParent: parent, onselect: vi.fn() });
    return { files, parent, fireResize: () => roCb!([], undefined as unknown as ResizeObserver) };
  }

  it('keeps the center tile centered when a resize changes the tile size', async () => {
    const { files, parent, fireResize } = renderWithCapturedRO(1400);
    parent.scrollTop = 5000;
    parent.dispatchEvent(new Event('scroll'));
    flushSync();

    const before = anchorForViewport(
      computeGrid(1400 - 2 * PAD_X, 3, 10), 5000, 400, PAD_TOP, files.length,
    )!;

    Object.defineProperty(parent, 'clientWidth', { value: 900, configurable: true });
    fireResize();
    flushSync();
    await tick();

    const after = anchorForViewport(
      computeGrid(900 - 2 * PAD_X, 3, 10), parent.scrollTop, 400, PAD_TOP, files.length,
    )!;
    // Both sides use newCols: the viewport center after restore (after.index) and
    // the anchor item (before.index) must land on the same row in the new layout.
    // oldCols would compare row ordinals across different layouts, which cannot match.
    const newCols = computeGrid(900 - 2 * PAD_X, 3, 10).columns;
    expect(Math.floor(after.index / newCols)).toBe(Math.floor(before.index / newCols));
    expect(after.offsetFraction).toBeCloseTo(before.offsetFraction, 3);
  });

  it('leaves scrollTop alone when geometry does not change', async () => {
    const { parent, fireResize } = renderWithCapturedRO(1400);
    parent.scrollTop = 5000;
    parent.dispatchEvent(new Event('scroll'));
    flushSync();

    fireResize(); // same clientWidth/clientHeight
    flushSync();
    await tick();
    expect(parent.scrollTop).toBe(5000);
  });

  it('first-anchor-wins on rapid resize bursts', async () => {
    const { files, parent, fireResize } = renderWithCapturedRO(1400);
    parent.scrollTop = 5000;
    parent.dispatchEvent(new Event('scroll'));
    flushSync();

    // Capture the anchor at the original position under original metrics.
    const origAnchor = anchorForViewport(
      computeGrid(1400 - 2 * PAD_X, 3, 10), 5000, 400, PAD_TOP, files.length,
    )!;

    // Fire two resizes before tick resolves — simulates a panel drag burst.
    Object.defineProperty(parent, 'clientWidth', { value: 900, configurable: true });
    fireResize(); // A1 captured as pendingAnchor
    Object.defineProperty(parent, 'clientWidth', { value: 800, configurable: true });
    fireResize(); // pendingAnchor already set; second fire keeps A1
    flushSync();
    await tick();

    // Final scrollTop must be anchored to A1 under final metrics (800px).
    const finalMetrics = computeGrid(800 - 2 * PAD_X, 3, 10);
    const expected = scrollTopForAnchor(origAnchor, finalMetrics, 400, PAD_TOP, files.length);
    expect(parent.scrollTop).toBe(expected);
  });
});
