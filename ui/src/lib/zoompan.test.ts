import { describe, it, expect } from 'vitest';
import {
  clampScale, zoomAbout, wheelFactor, clampPan, toggleZoom, FIT, MIN_SCALE, MAX_SCALE,
} from './zoompan';

describe('zoompan', () => {
  it('clampScale floors at 0.2 and ceils at 8', () => {
    expect(clampScale(0.05)).toBe(MIN_SCALE);
    expect(clampScale(99)).toBe(MAX_SCALE);
    expect(clampScale(1)).toBe(1);
  });

  it('wheelFactor zooms in on wheel-up, out on wheel-down', () => {
    expect(wheelFactor(-100)).toBeGreaterThan(1);
    expect(wheelFactor(100)).toBeLessThan(1);
  });

  it('zoomAbout keeps the cursor point fixed', () => {
    const v = zoomAbout(FIT, 2, 10, 0); // cursor 10px right of centre
    expect(v.scale).toBe(2);
    expect(v.panX + v.scale * 10).toBeCloseTo(10);
  });

  it('zoomAbout respects the scale ceiling', () => {
    const v = zoomAbout({ scale: 6, panX: 0, panY: 0 }, 2, 0, 0);
    expect(v.scale).toBe(MAX_SCALE); // 12 clamped to 8
  });

  it('clampPan limits to half the scaled stage extent', () => {
    expect(clampPan(1000, 800, 1)).toBe(400); // limit = 800*1/2
    expect(clampPan(-1000, 800, 1)).toBe(-400);
    expect(clampPan(100, 800, 1)).toBe(100);
  });

  it('toggleZoom flips fit and 2x', () => {
    expect(toggleZoom(FIT).scale).toBe(2);
    expect(toggleZoom({ scale: 4, panX: 5, panY: 5 })).toEqual(FIT);
  });
});
