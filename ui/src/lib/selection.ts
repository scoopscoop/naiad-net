/** Pure selection logic for the gallery grid (#23). No DOM — fully
 *  unit-tested, same pattern as grid-window.ts. Selection is keyed by file
 *  hash so it survives re-sorting; every function returns fresh objects
 *  because the tab's Set is replaced, not mutated (Svelte 5 $state does not
 *  deep-proxy Sets).
 */

import type { GridMetrics } from './grid-window';
import type { FileDto } from './types';

export interface SelectionState {
  selected: ReadonlySet<string>; // file hashes
  anchor: string | null; // shift-range anchor hash
}

/** Click semantics over the presented (sorted) list, matching file-manager
 *  muscle memory (#110): a plain click collapses the selection to the clicked
 *  tile *and* sets the anchor, so the next shift-click ranges from it. */
export function applyClick(
  state: SelectionState,
  index: number,
  files: FileDto[],
  mods: { ctrl: boolean; shift: boolean },
): SelectionState {
  const file = files[index];
  if (!file) return state;

  if (mods.shift) {
    // Range from the anchor; a missing or vanished anchor degrades to the
    // clicked tile (Explorer behavior), which then becomes the anchor.
    const anchorIdx = state.anchor === null ? -1 : files.findIndex((f) => f.hash === state.anchor);
    const from = anchorIdx === -1 ? index : anchorIdx;
    const lo = Math.min(from, index);
    const hi = Math.max(from, index);
    const range = files.slice(lo, hi + 1).map((f) => f.hash);
    const selected = mods.ctrl ? new Set([...state.selected, ...range]) : new Set(range);
    return { selected, anchor: anchorIdx === -1 ? file.hash : state.anchor };
  }

  if (mods.ctrl) {
    // Toggle. The clicked tile becomes the anchor either way.
    const selected = new Set(state.selected);
    if (selected.has(file.hash)) selected.delete(file.hash);
    else selected.add(file.hash);
    return { selected, anchor: file.hash };
  }

  // Plain click: collapse to the clicked tile and re-anchor there.
  return { selected: new Set([file.hash]), anchor: file.hash };
}

/** Tile indices a rubber-band rect overlaps, in grid content-box coordinates
 *  (the caller subtracts the grid's padding). A tile at (col, row) occupies
 *  [col*(tileWidth+gap), +tileWidth] × [row*rowHeight, +tileWidth]; rects
 *  living entirely inside a gap overlap nothing. */
export function rectToIndices(
  rect: { x1: number; y1: number; x2: number; y2: number },
  metrics: GridMetrics,
  gap: number,
  itemCount: number,
): number[] {
  const { columns, tileWidth, rowHeight } = metrics;
  if (columns <= 0 || tileWidth <= 0 || rowHeight <= 0 || itemCount <= 0) return [];

  const left = Math.min(rect.x1, rect.x2);
  const right = Math.max(rect.x1, rect.x2);
  const top = Math.min(rect.y1, rect.y2);
  const bottom = Math.max(rect.y1, rect.y2);

  // Column c overlaps iff c*stride <= right and c*stride + tileWidth >= left;
  // rows likewise with rowHeight as the stride and tileWidth as tile height.
  const stride = tileWidth + gap;
  const colFrom = Math.max(0, Math.ceil((left - tileWidth) / stride));
  const colTo = Math.min(columns - 1, Math.floor(right / stride));
  const rowFrom = Math.max(0, Math.ceil((top - tileWidth) / rowHeight));
  const rowTo = Math.floor(bottom / rowHeight);
  if (colTo < colFrom || rowTo < rowFrom) return [];

  const hits: number[] = [];
  for (let r = rowFrom; r <= rowTo; r++) {
    for (let c = colFrom; c <= colTo; c++) {
      const i = r * columns + c;
      if (i < itemCount) hits.push(i);
    }
  }
  return hits;
}

/** Band commit/preview: replace the base or (additive) union it with the
 *  rect's hits. */
export function bandSelection(
  base: ReadonlySet<string>,
  hitIndices: number[],
  files: FileDto[],
  additive: boolean,
): Set<string> {
  const next = additive ? new Set(base) : new Set<string>();
  for (const i of hitIndices) {
    const f = files[i];
    if (f) next.add(f.hash);
  }
  return next;
}

/** The selected files in display order — what openDetail gets (#20 contract). */
export function selectedSubset(files: FileDto[], selected: ReadonlySet<string>): FileDto[] {
  return files.filter((f) => selected.has(f.hash));
}
