import { describe, it, expect } from 'vitest';
import { computeGrid, computeWindow, scrollTargetForIndex, anchorForViewport, scrollTopForAnchor } from './grid-window';

const GAP = 10;

describe('computeGrid', () => {
  it('derives the tile size that makes the requested columns fill the width', () => {
    const g = computeGrid(700, 4, GAP);
    expect(g.columns).toBe(4);
    expect(g.tileWidth).toBeCloseTo(167.5); // (700 - 3*10)/4
    expect(g.rowHeight).toBeCloseTo(177.5); // tileWidth + gap
  });

  it('shrinks tiles rather than dropping columns on a narrow width', () => {
    const g = computeGrid(100, 4, GAP);
    expect(g.columns).toBe(4);
    expect(g.tileWidth).toBeCloseTo((100 - 3 * GAP) / 4);
  });

  it('degenerates fully when the width cannot fit even the gaps', () => {
    // rowHeight must be 0 (not `gap`) so downstream rowHeight <= 0 guards fire.
    const g = computeGrid(20, 16, GAP);
    expect(g.columns).toBe(16);
    expect(g.tileWidth).toBe(0);
    expect(g.rowHeight).toBe(0);
  });

  it('returns a degenerate zero-height metric for non-positive width', () => {
    const g = computeGrid(0, 4, GAP);
    expect(g).toEqual({ columns: 1, tileWidth: 0, rowHeight: 0 });
  });

  it('returns a degenerate metric for a non-positive column count', () => {
    const g = computeGrid(700, 0, GAP);
    expect(g).toEqual({ columns: 1, tileWidth: 0, rowHeight: 0 });
  });
});

describe('computeWindow', () => {
  // 100 items, 4 columns, rowHeight 100 -> 25 rows, totalHeight 2500.
  it('renders the top band plus overscan at the top', () => {
    const w = computeWindow(100, 4, 100, 0, 300, 2); // viewport 300 -> 3 rows
    expect(w.startIndex).toBe(0);
    expect(w.endIndex).toBe(20); // rows 0..5 (3 visible + 2 overscan) * 4
    expect(w.offsetY).toBe(0);
    expect(w.totalHeight).toBe(2500);
  });

  it('offsets the band and applies overscan both sides mid-scroll', () => {
    const w = computeWindow(100, 4, 100, 1000, 300, 2); // firstVisibleRow 10
    expect(w.startIndex).toBe(32); // row 8 * 4
    expect(w.endIndex).toBe(60);   // row 15 * 4
    expect(w.offsetY).toBe(800);   // row 8 * 100
  });

  it('clamps the end row near the bottom', () => {
    const w = computeWindow(100, 4, 100, 2000, 300, 2); // firstVisibleRow 20
    expect(w.startIndex).toBe(72); // row 18 * 4
    expect(w.endIndex).toBe(100);  // row 25 clamped, 25*4=100
  });

  it('clamps a partial last row to itemCount', () => {
    // 90 items, 4 cols -> 23 rows (22 full + 1 of 2). Bottom of a 2300px scroll.
    const w = computeWindow(90, 4, 100, 2000, 300, 2);
    expect(w.endIndex).toBe(90);
  });

  it('does not run past the end on scroll overshoot', () => {
    const w = computeWindow(90, 4, 100, 9999, 300, 2);
    expect(w.endIndex).toBeLessThanOrEqual(90);
    expect(w.offsetY).toBeLessThanOrEqual(w.totalHeight);
    expect(w.startIndex).toBeGreaterThanOrEqual(w.endIndex); // empty slice, no phantom rows
  });

  it('returns an empty window for an empty set', () => {
    expect(computeWindow(0, 4, 100, 0, 300, 2)).toEqual({
      startIndex: 0, endIndex: 0, offsetY: 0, totalHeight: 0,
    });
  });

  it('returns an empty window when rowHeight is degenerate', () => {
    expect(computeWindow(100, 1, 0, 0, 300, 2)).toEqual({
      startIndex: 0, endIndex: 0, offsetY: 0, totalHeight: 0,
    });
  });
});

// scrollTargetForIndex: 4 cols, rowHeight=100, padTop=14, viewportH=250.
// Row 0: [14, 114), Row 1: [114, 214), Row 2: [214, 314), Row 3: [314, 414).
describe('scrollTargetForIndex', () => {
  const COLS = 4;
  const ROW_H = 100;
  const PAD = 14;
  const VP = 250;

  it('returns current scrollTop when the row is already visible', () => {
    // Index 0 → row 0: top=14, bottom=114. Viewport [0, 250] contains it.
    expect(scrollTargetForIndex(0, COLS, ROW_H, PAD, 0, VP)).toBe(0);
    // Index 4 → row 1: top=114, bottom=214. Viewport [0, 250] contains it.
    expect(scrollTargetForIndex(4, COLS, ROW_H, PAD, 0, VP)).toBe(0);
  });

  it('scrolls up when the row is above the viewport', () => {
    // Row 0 top=14. scrollTop=200 hides it (14 < 200). Return 14.
    expect(scrollTargetForIndex(0, COLS, ROW_H, PAD, 200, VP)).toBe(14);
    // Index 4 → row 1 top=114. scrollTop=200 hides it (114 < 200). Return 114.
    expect(scrollTargetForIndex(4, COLS, ROW_H, PAD, 200, VP)).toBe(114);
  });

  it('scrolls down when the row is below the viewport', () => {
    // Index 8 → row 2: top=214, bottom=314. Viewport [0, 250]: bottom 314>250.
    // New scrollTop = 314 - 250 = 64.
    expect(scrollTargetForIndex(8, COLS, ROW_H, PAD, 0, VP)).toBe(64);
    // Index 12 → row 3: top=314, bottom=414. New scrollTop = 414 - 250 = 164.
    expect(scrollTargetForIndex(12, COLS, ROW_H, PAD, 0, VP)).toBe(164);
  });

  it('returns currentScrollTop for degenerate inputs', () => {
    expect(scrollTargetForIndex(-1, COLS, ROW_H, PAD, 50, VP)).toBe(50);
    expect(scrollTargetForIndex(0, 0, ROW_H, PAD, 50, VP)).toBe(50);
    expect(scrollTargetForIndex(0, COLS, 0, PAD, 50, VP)).toBe(50);
  });

  it('uses column count to derive the row from a flat index', () => {
    // Index 7 with 4 cols → row 1 (floor(7/4)=1): top=114, bottom=214.
    // Viewport [0, 250] contains it → no change.
    expect(scrollTargetForIndex(7, COLS, ROW_H, PAD, 0, VP)).toBe(0);
    // Index 7 with 3 cols → row 2 (floor(7/3)=2): top=214, bottom=314.
    // Viewport [0, 250] does not contain it → scrollTo 314-250=64.
    expect(scrollTargetForIndex(7, 3, ROW_H, PAD, 0, VP)).toBe(64);
  });
});

describe('scroll anchoring', () => {
  // 6 columns vs 4 columns at widths chosen so both grids get 160px tiles.
  const wide = computeGrid(6 * 160 + 5 * 10, 6, 10); // 6 cols, tileWidth 160
  const narrow = computeGrid(4 * 160 + 3 * 10, 4, 10); // 4 cols, tileWidth 160
  const PAD_TOP = 14;

  it('anchors on the row under the viewport center line', () => {
    // center line = 1000 + 200 - 14 = 1186px into content space
    const a = anchorForViewport(wide, 1000, 400, PAD_TOP, 600)!;
    const row = Math.floor(1186 / wide.rowHeight);
    expect(a.index).toBe(row * wide.columns);
    expect(a.offsetFraction).toBeCloseTo(1186 / wide.rowHeight - row, 5);
  });

  it('round-trips the center item across a column-count change', () => {
    const a = anchorForViewport(wide, 1000, 400, PAD_TOP, 600)!;
    const restored = scrollTopForAnchor(a, narrow, 400, PAD_TOP, 600);
    const b = anchorForViewport(narrow, restored, 400, PAD_TOP, 600)!;
    // The real contract is that b.index is the first item of the row
    // containing a.index under the new column count, not that raw indices
    // are equal. In this specific case both happen to coincide because
    // a.index=36 is a row-start in both grids (36 = 6×6 = 9×4), but the
    // assertion below captures the general rule.
    expect(b.index).toBe(Math.floor(a.index / narrow.columns) * narrow.columns);
    expect(b.offsetFraction).toBeCloseTo(a.offsetFraction, 5);
  });

  it('round-trips across both column-count and rowHeight change', () => {
    // computeGrid(900, 5, 10): tileWidth=(900-40)/5=172, rowHeight=182.
    // wide has rowHeight=170; alt has rowHeight=182. A bug that uses the old
    // rowHeight in scrollTopForAnchor would produce wrong centerY and break
    // both assertions below.
    const alt = computeGrid(900, 5, 10); // 5 cols, rowHeight 182
    const a = anchorForViewport(wide, 1000, 400, PAD_TOP, 600)!;
    const restored = scrollTopForAnchor(a, alt, 400, PAD_TOP, 600);
    const b = anchorForViewport(alt, restored, 400, PAD_TOP, 600)!;
    // b.index must be the first item of the row containing a.index under alt.columns
    expect(b.index).toBeLessThanOrEqual(a.index);
    expect(a.index).toBeLessThan(b.index + alt.columns);
    expect(b.offsetFraction).toBeCloseTo(a.offsetFraction, 5);
  });

  it('clamps to the top of the scroll range', () => {
    const a = anchorForViewport(wide, 0, 400, PAD_TOP, 600)!;
    expect(scrollTopForAnchor(a, narrow, 4000, PAD_TOP, 600)).toBe(0);
  });

  it('returns null / 0 on degenerate inputs', () => {
    expect(anchorForViewport(wide, 100, 400, PAD_TOP, 0)).toBeNull();
    expect(anchorForViewport({ columns: 0, tileWidth: 0, rowHeight: 0 }, 100, 400, PAD_TOP, 50)).toBeNull();
    expect(scrollTopForAnchor({ index: 3, offsetFraction: 0.5 }, { columns: 0, tileWidth: 0, rowHeight: 0 }, 400, PAD_TOP, 50)).toBe(0);
  });

  it('clamps the anchor row into the grid for out-of-range scrollTops', () => {
    const a = anchorForViewport(wide, 1e9, 400, PAD_TOP, 600)!;
    expect(a.index).toBeLessThan(600);
    expect(a.index % wide.columns).toBe(0);
  });
});
