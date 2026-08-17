import { beforeEach, describe, expect, it, vi } from 'vitest';

describe('detail drawer store', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
  });

  it('persists open and height to localStorage', async () => {
    const { DRAWER_KEY, drawer } = await import('./detail-drawer.svelte');
    drawer.open = false;
    drawer.height = 300;
    expect(JSON.parse(localStorage.getItem(DRAWER_KEY)!)).toEqual({ open: false, height: 300 });
  });

  it('clamps height to min and pane fraction', async () => {
    const { clampHeight } = await import('./detail-drawer.svelte');
    expect(clampHeight(10, 800)).toBe(120);
    expect(clampHeight(10_000, 800)).toBe(560);
    expect(clampHeight(Number.NaN, 800)).toBe(280);
  });

  it('loads persisted state on module init', async () => {
    localStorage.setItem('naiad.detail.drawer', JSON.stringify({ open: false, height: 340 }));
    const { drawer } = await import('./detail-drawer.svelte');
    expect(drawer.open).toBe(false);
    expect(drawer.height).toBe(340);
  });
});
