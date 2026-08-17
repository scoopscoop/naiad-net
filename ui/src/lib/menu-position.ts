/** Pure viewport clamp + flip math for the context menu. DOM-free so it is
 *  unit-tested in isolation, same pattern as selection.ts / grid-window.ts. */

export interface Point {
  x: number;
  y: number;
}

export interface Size {
  width: number;
  height: number;
}

/** Clamp one axis: keep [pos, pos+size] inside [margin, extent-margin].
 *  Prefer the anchor; if it overflows the far edge, flip to open before the
 *  anchor when that fits, else clamp to the far margin; a size larger than the
 *  usable span pins to the near margin. */
function clampAxis(anchor: number, size: number, extent: number, margin: number): number {
  if (size > extent - 2 * margin) return margin;
  let pos = anchor;
  if (pos + size > extent - margin) {
    const flipped = anchor - size;
    pos = flipped >= margin ? flipped : extent - size - margin;
  }
  if (pos < margin) pos = margin;
  return pos;
}

/** Top-left corner at which to render a menu of `menuSize`, anchored at
 *  `anchor` (cursor point or element-derived point), kept inside `viewport`
 *  with at least `margin` px clearance from every edge. */
export function clampMenuPosition(
  anchor: Point,
  menuSize: Size,
  viewport: Size,
  margin = 8,
): Point {
  return {
    x: clampAxis(anchor.x, menuSize.width, viewport.width, margin),
    y: clampAxis(anchor.y, menuSize.height, viewport.height, margin),
  };
}
