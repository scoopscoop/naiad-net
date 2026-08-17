import { describe, it, expect } from 'vitest';
import { applyClick, bandSelection, rectToIndices, selectedSubset } from './selection';
import type { FileDto } from './types';

const files: FileDto[] = Array.from({ length: 10 }, (_, i) => ({
  hash: `h${i}`,
  name: `f${i}.png`,
  size: 1,
  path: `/f${i}.png`,
  imported_at: 100 + i,
  created_at: null,
  modified_at: null,
  mime: 'image/png',
}));
const set = (...hashes: string[]) => new Set(hashes);

describe('applyClick', () => {
  it('ctrl+click adds an unselected tile and anchors it', () => {
    const next = applyClick({ selected: set('h0'), anchor: 'h0' }, 2, files, { ctrl: true, shift: false });
    expect([...next.selected].sort()).toEqual(['h0', 'h2']);
    expect(next.anchor).toBe('h2');
  });

  it('ctrl+click removes a selected tile but still anchors it', () => {
    const next = applyClick({ selected: set('h0', 'h2'), anchor: 'h0' }, 2, files, { ctrl: true, shift: false });
    expect([...next.selected]).toEqual(['h0']);
    expect(next.anchor).toBe('h2');
  });

  it('shift+click replaces the selection with the anchor→index range', () => {
    const next = applyClick({ selected: set('h9'), anchor: 'h1' }, 4, files, { ctrl: false, shift: true });
    expect([...next.selected].sort()).toEqual(['h1', 'h2', 'h3', 'h4']);
    expect(next.anchor).toBe('h1');
  });

  it('shift+click works with an inverted range (click above the anchor)', () => {
    const next = applyClick({ selected: set(), anchor: 'h4' }, 1, files, { ctrl: false, shift: true });
    expect([...next.selected].sort()).toEqual(['h1', 'h2', 'h3', 'h4']);
  });

  it('ctrl+shift+click adds the range to the existing selection', () => {
    const next = applyClick({ selected: set('h9'), anchor: 'h1' }, 2, files, { ctrl: true, shift: true });
    expect([...next.selected].sort()).toEqual(['h1', 'h2', 'h9']);
  });

  it('shift+click with no anchor selects just the clicked tile and anchors it', () => {
    const next = applyClick({ selected: set(), anchor: null }, 3, files, { ctrl: false, shift: true });
    expect([...next.selected]).toEqual(['h3']);
    expect(next.anchor).toBe('h3');
  });

  it('shift+click with a vanished anchor falls back to the clicked tile', () => {
    const next = applyClick({ selected: set(), anchor: 'gone' }, 3, files, { ctrl: false, shift: true });
    expect([...next.selected]).toEqual(['h3']);
    expect(next.anchor).toBe('h3');
  });

  it('plain click collapses the selection to the clicked tile and anchors it', () => {
    const next = applyClick({ selected: set('h0', 'h5'), anchor: 'h5' }, 2, files, { ctrl: false, shift: false });
    expect([...next.selected]).toEqual(['h2']);
    expect(next.anchor).toBe('h2');
  });

  it('shift+click after a plain click ranges from the plain-clicked tile (#110)', () => {
    const clicked = applyClick({ selected: set(), anchor: null }, 2, files, { ctrl: false, shift: false });
    const ranged = applyClick(clicked, 5, files, { ctrl: false, shift: true });
    expect([...ranged.selected].sort()).toEqual(['h2', 'h3', 'h4', 'h5']);
    // A second shift-click re-ranges from the same anchor, not from the last one.
    const again = applyClick(ranged, 4, files, { ctrl: false, shift: true });
    expect([...again.selected].sort()).toEqual(['h2', 'h3', 'h4']);
    expect(again.anchor).toBe('h2');
  });

  it('is a no-op for an out-of-range index', () => {
    const state = { selected: set('h0'), anchor: 'h0' };
    expect(applyClick(state, 99, files, { ctrl: true, shift: false })).toBe(state);
  });

  it('never mutates the input set', () => {
    const input = set('h0');
    applyClick({ selected: input, anchor: null }, 1, files, { ctrl: true, shift: false });
    expect([...input]).toEqual(['h0']);
  });
});

describe('rectToIndices', () => {
  // 3 columns of 100px tiles, 10px gap → stride 110, rowHeight 110.
  const m = { columns: 3, tileWidth: 100, rowHeight: 110 };

  it('returns the tiles the rect overlaps', () => {
    // x 50..160 covers cols 0,1; y 50..170 covers rows 0,1.
    expect(rectToIndices({ x1: 50, y1: 50, x2: 160, y2: 170 }, m, 10, 10)).toEqual([0, 1, 3, 4]);
  });

  it('normalizes inverted rects (drag up-left)', () => {
    expect(rectToIndices({ x1: 160, y1: 170, x2: 50, y2: 50 }, m, 10, 10)).toEqual([0, 1, 3, 4]);
  });

  it('selects nothing when the rect lives wholly in a gap', () => {
    // x 102..108 is inside the gap between col 0 (…100) and col 1 (110…).
    expect(rectToIndices({ x1: 102, y1: 0, x2: 108, y2: 500 }, m, 10, 10)).toEqual([]);
  });

  it('clamps to itemCount on the partial last row', () => {
    // 10 items in 3 columns → row 3 holds only index 9.
    expect(rectToIndices({ x1: 0, y1: 330, x2: 320, y2: 340 }, m, 10, 10)).toEqual([9]);
  });

  it('returns [] for rects entirely outside the grid', () => {
    expect(rectToIndices({ x1: -50, y1: -50, x2: -5, y2: -5 }, m, 10, 10)).toEqual([]);
  });

  it('returns [] on degenerate metrics', () => {
    expect(
      rectToIndices({ x1: 0, y1: 0, x2: 10, y2: 10 }, { columns: 0, tileWidth: 0, rowHeight: 0 }, 10, 10),
    ).toEqual([]);
  });
});

describe('bandSelection', () => {
  it('replaces the base when not additive', () => {
    expect([...bandSelection(set('h9'), [0, 1], files, false)].sort()).toEqual(['h0', 'h1']);
  });

  it('unions with the base when additive', () => {
    expect([...bandSelection(set('h9'), [0], files, true)].sort()).toEqual(['h0', 'h9']);
  });

  it('ignores out-of-range indices', () => {
    expect([...bandSelection(set(), [0, 99], files, false)]).toEqual(['h0']);
  });
});

describe('selectedSubset', () => {
  it('returns selected files in display order, dropping unknown hashes', () => {
    const subset = selectedSubset(files, set('h7', 'h2', 'gone'));
    expect(subset.map((f) => f.hash)).toEqual(['h2', 'h7']);
  });
});
