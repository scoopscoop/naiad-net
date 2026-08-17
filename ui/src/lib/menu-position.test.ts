import { describe, expect, it } from 'vitest';
import { clampMenuPosition } from './menu-position';

const vp = { width: 1000, height: 800 };
const menu = { width: 200, height: 300 };

describe('clampMenuPosition', () => {
  it('leaves a fitting menu at the anchor', () => {
    expect(clampMenuPosition({ x: 100, y: 100 }, menu, vp)).toEqual({ x: 100, y: 100 });
  });

  it('flips left when the menu would overflow the right edge and the flip fits', () => {
    // anchor.x 950 + width 200 = 1150 > 992; flipped 950 - 200 = 750 >= 8.
    const p = clampMenuPosition({ x: 950, y: 100 }, menu, vp);
    expect(p.x).toBe(750);
    expect(p.y).toBe(100);
  });

  it('flips up when the menu would overflow the bottom edge and the flip fits', () => {
    // anchor.y 700 + height 300 = 1000 > 792; flipped 700 - 300 = 400 >= 8.
    const p = clampMenuPosition({ x: 100, y: 700 }, menu, vp);
    expect(p.y).toBe(400);
  });

  it('clamps to the far margin when neither the anchor nor the flip fits', () => {
    // Menu 200 wide in a 260 viewport (fits: 200 <= 260 - 16), anchor 100:
    // 100 + 200 = 300 > 252 overflows; flipped 100 - 200 = -100 < 8 does not
    // fit either → clamp to the far margin: 260 - 200 - 8 = 52.
    const p = clampMenuPosition({ x: 100, y: 10 }, { width: 200, height: 50 }, { width: 260, height: 400 }, 8);
    expect(p.x).toBe(52);
  });

  it('pins to the margin when the menu is larger than the viewport', () => {
    const p = clampMenuPosition({ x: 40, y: 40 }, { width: 500, height: 900 }, { width: 300, height: 300 }, 8);
    expect(p).toEqual({ x: 8, y: 8 });
  });

  it('never returns a coordinate below the margin', () => {
    const p = clampMenuPosition({ x: -50, y: -50 }, menu, vp);
    expect(p.x).toBeGreaterThanOrEqual(8);
    expect(p.y).toBeGreaterThanOrEqual(8);
  });
});
