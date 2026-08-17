/** Pure geometry for the virtualized gallery grid. No DOM — fully unit-tested. */

export interface GridMetrics {
  columns: number; // how many tiles fit per row
  tileWidth: number; // px, actual rendered tile edge (stretched to fill)
  rowHeight: number; // tileWidth + gap
}

/** Derive tile geometry from a fixed column count (the zoom level, #171).
 *  The tile size is whatever fills the row exactly, so a level means the same
 *  visual density at any window width or DPI. `availWidth` is the grid
 *  content-box width (the scroll container's clientWidth minus horizontal
 *  padding). */
export function computeGrid(availWidth: number, columns: number, gap: number): GridMetrics {
  if (availWidth <= 0 || columns <= 0) return { columns: 1, tileWidth: 0, rowHeight: 0 };
  const cols = Math.max(1, Math.floor(columns));
  const tileWidth = (availWidth - (cols - 1) * gap) / cols;
  // A width narrower than the gaps alone is fully degenerate: report rowHeight
  // 0 (not `gap`) so the rowHeight <= 0 guards downstream actually fire instead
  // of windowing thousands of zero-size rows.
  if (tileWidth <= 0) return { columns: cols, tileWidth: 0, rowHeight: 0 };
  return { columns: cols, tileWidth, rowHeight: tileWidth + gap };
}

export interface GridSlice {
  startIndex: number; // first item to render (inclusive, column-aligned)
  endIndex: number; // one past the last item to render (exclusive)
  offsetY: number; // translateY for the rendered block, px
  totalHeight: number; // full scroll height of all rows, px
}

/** Compute the scrollTop that makes the row containing `index` fully visible
 *  inside a viewport of height `viewportH`.  Returns `currentScrollTop`
 *  unchanged when the row is already in view.
 *
 *  `padTop` matches the grid's CSS top padding (PAD_TOP in Grid.svelte). */
export function scrollTargetForIndex(
  index: number,
  columns: number,
  rowHeight: number,
  padTop: number,
  currentScrollTop: number,
  viewportH: number,
): number {
  if (index < 0 || columns <= 0 || rowHeight <= 0) return currentScrollTop;
  const row = Math.floor(index / columns);
  const rowTop = row * rowHeight + padTop;
  const rowBottom = rowTop + rowHeight;
  if (rowTop < currentScrollTop) return rowTop;
  if (rowBottom > currentScrollTop + viewportH) return rowBottom - viewportH;
  return currentScrollTop;
}

/** The visible slice + spacer height + translate offset. `scrollTop` should be
 *  the container scroll offset already adjusted for the grid's top padding; it
 *  is re-clamped to >= 0 here for safety. */
export function computeWindow(
  itemCount: number,
  columns: number,
  rowHeight: number,
  scrollTop: number,
  viewportH: number,
  overscanRows: number,
): GridSlice {
  if (itemCount <= 0 || columns <= 0 || rowHeight <= 0) {
    return { startIndex: 0, endIndex: 0, offsetY: 0, totalHeight: 0 };
  }
  const totalRows = Math.ceil(itemCount / columns);
  const totalHeight = totalRows * rowHeight;
  const firstVisibleRow = Math.floor(Math.max(0, scrollTop) / rowHeight);
  const visibleRows = Math.ceil(viewportH / rowHeight);
  const startRow = Math.min(Math.max(0, firstVisibleRow - overscanRows), totalRows);
  const endRow = Math.min(totalRows, firstVisibleRow + visibleRows + overscanRows);
  const startIndex = startRow * columns;
  const endIndex = Math.min(itemCount, endRow * columns);
  const offsetY = startRow * rowHeight;
  return { startIndex, endIndex, offsetY, totalHeight };
}

/** A stable reference point for reflow: the row under the viewport's vertical
 *  center line, expressed as that row's first item plus where the line sits
 *  within the row (0..1). Survives column-count changes because the item
 *  index — not the pixel offset — is what gets restored. */
export interface ScrollAnchor {
  index: number; // first item of the anchored row
  offsetFraction: number; // center line's position within the row, [0, 1); clamped to 1 only past end of content
}

/** Capture a stable scroll reference for the row currently under the
 *  viewport's vertical center line.
 *
 *  `scrollTop` is the **raw container scroll offset** (what the DOM exposes on
 *  `.scrollTop`). This is NOT the padTop-adjusted value that `computeWindow`
 *  takes; padTop is subtracted here internally to convert to grid-content
 *  coordinates.
 *
 *  `padTop` matches the grid's CSS top padding (PAD_TOP in Grid.svelte) —
 *  same convention as `scrollTargetForIndex`.
 *
 *  The returned anchor identifies the row under the center line by its first
 *  item index (so it survives column-count changes) and records where within
 *  that row the center line fell (offsetFraction), so `scrollTopForAnchor`
 *  can restore the exact sub-row position under new metrics. */
export function anchorForViewport(
  metrics: GridMetrics,
  scrollTop: number,
  viewportH: number,
  padTop: number,
  itemCount: number,
): ScrollAnchor | null {
  if (itemCount <= 0 || metrics.columns <= 0 || metrics.rowHeight <= 0) return null;
  const totalRows = Math.ceil(itemCount / metrics.columns);
  const centerY = scrollTop + viewportH / 2 - padTop;
  const row = Math.min(totalRows - 1, Math.max(0, Math.floor(centerY / metrics.rowHeight)));
  const offsetFraction = Math.min(1, Math.max(0, centerY / metrics.rowHeight - row));
  return { index: row * metrics.columns, offsetFraction };
}

/** ScrollTop that puts the anchored row back at the same position relative to
 *  the viewport center under new metrics. Clamped to >= 0; the caller writes
 *  it to the DOM after the spacer re-renders, so the browser handles the
 *  bottom clamp against the real scrollHeight. */
export function scrollTopForAnchor(
  anchor: ScrollAnchor,
  metrics: GridMetrics,
  viewportH: number,
  padTop: number,
  itemCount: number,
): number {
  if (itemCount <= 0 || metrics.columns <= 0 || metrics.rowHeight <= 0) return 0;
  const row = Math.floor(anchor.index / metrics.columns);
  const centerY = (row + anchor.offsetFraction) * metrics.rowHeight;
  return Math.max(0, centerY + padTop - viewportH / 2);
}
