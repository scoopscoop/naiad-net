/** Pure zoom/pan math for the detail-view image stage. Kept DOM-free so it can
 *  be unit-tested without layout (jsdom has none). */

export const MIN_SCALE = 0.2;
export const MAX_SCALE = 8;
export const FIT_SCALE = 1;

export interface View {
  scale: number;
  panX: number;
  panY: number;
}

/** The reset/fit view: image contained, centred, no pan. */
export const FIT: View = { scale: FIT_SCALE, panX: 0, panY: 0 };

export function clampScale(scale: number): number {
  return Math.min(Math.max(scale, MIN_SCALE), MAX_SCALE);
}

/** A wheel deltaY → multiplicative zoom factor (wheel-up/negative = zoom in). */
export function wheelFactor(deltaY: number): number {
  return Math.exp(-deltaY * 0.0015);
}

/** Zoom `view` by `factor` about cursor offset (cx,cy) measured from the stage
 *  centre, keeping the image point under the cursor stationary. */
export function zoomAbout(view: View, factor: number, cx: number, cy: number): View {
  const scale = clampScale(view.scale * factor);
  const ratio = scale / view.scale;
  return {
    scale,
    panX: cx - ratio * (cx - view.panX),
    panY: cy - ratio * (cy - view.panY),
  };
}

/** Clamp a pan offset so the image can't be dragged fully off-stage: the limit
 *  is half the scaled stage extent on that axis. */
export function clampPan(pan: number, stageSize: number, scale: number): number {
  const limit = (stageSize * scale) / 2;
  return Math.min(Math.max(pan, -limit), limit);
}

/** Double-click toggle: fit → 2× inspect, anything zoomed → fit. */
export function toggleZoom(view: View): View {
  return view.scale > FIT_SCALE ? { ...FIT } : { scale: 2, panX: 0, panY: 0 };
}
