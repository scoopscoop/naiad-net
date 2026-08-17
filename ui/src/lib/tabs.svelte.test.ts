import { describe, it, expect, beforeEach } from 'vitest';
import { createTabs } from './tabs.svelte';
import { DEFAULT_SORT, saveSort } from './gallery-sort';
import type { FileDto } from './types';

const file = (hash: string, name: string): FileDto => ({
  hash,
  name,
  size: 1,
  path: `/${name}`,
  imported_at: 100,
  created_at: 80,
  modified_at: 90,
  mime: 'image/png',
});

describe('tabs store - uniform model', () => {
  beforeEach(() => localStorage.clear());

  it('starts with a single active empty gallery tab', () => {
    const t = createTabs();
    expect(t.list).toHaveLength(1);
    const g = t.activeGallery;
    expect(g).not.toBeNull();
    expect(g!.query).toBe('');
    expect(g!.files).toEqual([]);
    expect(g!.sort).toEqual(DEFAULT_SORT);
    expect(g!.scrollTop).toBe(0);
    expect(g!.focused).toBeNull();
    expect(t.activeDetail).toBeNull();
    expect(t.galleryCount).toBe(1);
  });

  it('restores the persisted sort as the initial sort of every gallery tab', () => {
    saveSort({ key: 'name', direction: 'asc' });
    // A fresh store (as on app reload) starts from the saved sort, not DEFAULT_SORT.
    const t = createTabs();
    expect(t.activeGallery!.sort).toEqual({ key: 'name', direction: 'asc' });
    expect(t.openGallery().sort).toEqual({ key: 'name', direction: 'asc' });
  });

  it('gallery tabs never share one sort object', () => {
    saveSort({ key: 'name', direction: 'asc' });
    const t = createTabs();
    const first = t.activeGallery!;
    const second = t.openGallery();
    second.sort = { key: 'size', direction: 'desc' };
    expect(first.sort).toEqual({ key: 'name', direction: 'asc' });
  });

  it('openGallery appends, activates, and returns the reactive tab', () => {
    const t = createTabs();
    const g = t.openGallery();
    expect(t.list).toHaveLength(2);
    expect(t.activeId).toBe(g.id);
    expect(t.galleryCount).toBe(2);
    g.query = 'character:samus';
    expect((t.list[1] as { query: string }).query).toBe('character:samus');
  });

  it('gallery tabs hold independent state', () => {
    const t = createTabs();
    const first = t.activeGallery!;
    first.query = 'a';
    first.scrollTop = 500;
    const second = t.openGallery();
    expect(second.query).toBe('');
    expect(second.scrollTop).toBe(0);
    expect(second.focused).toBeNull();
    expect(first.query).toBe('a');
  });

  it('displayGallery tracks the last-active gallery behind a detail tab (#55)', () => {
    const t = createTabs();
    const first = t.activeGallery!;
    const second = t.openGallery();

    // A detail opened from `second` keeps `second` as the displayed gallery.
    t.openDetail([file('a', 'a.png')], 0);
    expect(t.activeGallery).toBeNull();
    expect(t.displayGallery?.id).toBe(second.id);

    // Cycling back to a gallery makes it both active and displayed.
    t.activate(first.id);
    expect(t.displayGallery?.id).toBe(first.id);
  });

  it('displayGallery falls back to a remaining gallery when the displayed one closes', () => {
    const t = createTabs();
    const first = t.activeGallery!;
    const second = t.openGallery();
    t.openDetail([file('a', 'a.png')], 0);
    expect(t.displayGallery?.id).toBe(second.id);

    // Closing the displayed gallery while a detail tab is active must not
    // leave the grid without a gallery to show.
    t.close(second.id);
    expect(t.displayGallery?.id).toBe(first.id);
  });

  it('refuses to close the last remaining gallery tab', () => {
    const t = createTabs();
    const only = t.activeGallery!;
    t.close(only.id);
    expect(t.list).toHaveLength(1);
    expect(t.activeGallery).not.toBeNull();
  });

  it('closes a gallery tab when another gallery remains', () => {
    const t = createTabs();
    const first = t.activeGallery!;
    t.openGallery();
    t.close(first.id);
    expect(t.galleryCount).toBe(1);
    expect(t.list.find((x) => x.id === first.id)).toBeUndefined();
  });

  it('still refuses when only detail tabs would remain', () => {
    const t = createTabs();
    const g = t.activeGallery!;
    t.openDetail([file('a', 'a.png')], 0);
    t.close(g.id);
    expect(t.galleryCount).toBe(1);
  });

  it('openDetail appends a tab and activates it', () => {
    const t = createTabs();
    t.openDetail([file('a', 'a.png')], 0);
    expect(t.activeGallery).toBeNull();
    expect(t.activeDetail?.file.hash).toBe('a');
  });

  it('closing the active detail tab falls back to the right neighbour, else left', () => {
    const t = createTabs();
    t.openDetail([file('a', 'a.png')], 0);
    t.openDetail([file('b', 'b.png')], 0);
    t.openDetail([file('c', 'c.png')], 0);
    const [, b] = t.list.filter((x) => x.kind === 'detail');
    t.activate(b.id);
    t.close(b.id);
    expect(t.activeDetail?.file.hash).toBe('c');
    t.close(t.activeId);
    expect(t.activeDetail?.file.hash).toBe('a');
  });

  it('closing the only detail tab returns to a gallery tab', () => {
    const t = createTabs();
    t.openDetail([file('a', 'a.png')], 0);
    t.close(t.activeId);
    expect(t.activeGallery).not.toBeNull();
  });

  it('closing an inactive tab leaves the active tab unchanged', () => {
    const t = createTabs();
    t.openDetail([file('a', 'a.png')], 0);
    const a = t.activeId;
    t.openDetail([file('b', 'b.png')], 0);
    t.close(a);
    expect(t.activeDetail?.file.hash).toBe('b');
  });

  it('cycle wraps in both directions', () => {
    const t = createTabs();
    const g = t.activeGallery!;
    t.openDetail([file('a', 'a.png')], 0);
    const d = t.activeId;
    t.cycle(1);
    expect(t.activeId).toBe(g.id);
    t.cycle(-1);
    expect(t.activeId).toBe(d);
  });

  it('activateAt clamps out-of-range indices', () => {
    const t = createTabs();
    const g = t.activeGallery!;
    t.openDetail([file('a', 'a.png')], 0);
    const d = t.activeId;
    t.activateAt(0);
    expect(t.activeId).toBe(g.id);
    t.activateAt(99);
    expect(t.activeId).toBe(d);
    t.activateAt(-5);
    expect(t.activeId).toBe(g.id);
  });

  it('activateLast activates the rightmost tab', () => {
    const t = createTabs();
    t.openDetail([file('a', 'a.png')], 0);
    const d = t.activeId;
    t.activateAt(0);
    t.activateLast();
    expect(t.activeId).toBe(d);
  });

  it('activate ignores unknown ids', () => {
    const t = createTabs();
    const before = t.activeId;
    t.activate(9999);
    expect(t.activeId).toBe(before);
  });

  it('findByHash locates an open detail tab', () => {
    const t = createTabs();
    t.openDetail([file('a', 'a.png')], 0);
    expect(t.findByHash('a')?.file.name).toBe('a.png');
    expect(t.findByHash('zzz')).toBeUndefined();
  });

  it('openDetail snapshots the list, clamps the index, next/prev clamp at ends', () => {
    const t = createTabs();
    const files = [file('a', 'a.png'), file('b', 'b.png'), file('c', 'c.png')];
    t.openDetail(files, 99);
    expect(t.activeDetail?.file.hash).toBe('c');
    t.next();
    expect(t.activeDetail?.file.hash).toBe('c');
    t.prev();
    t.prev();
    expect(t.activeDetail?.file.hash).toBe('a');
    t.prev();
    expect(t.activeDetail?.file.hash).toBe('a');
  });

  it('openDetail with background appends without activating', () => {
    const t = createTabs();
    const g = t.activeGallery!;
    t.openDetail([file('a', 'a.png')], 0, { background: true });
    expect(t.list).toHaveLength(2);
    expect(t.list[1].kind).toBe('detail');
    expect(t.activeId).toBe(g.id);
    expect(t.activeDetail).toBeNull();
  });

  it('openDetail without background still activates', () => {
    const t = createTabs();
    t.openDetail([file('a', 'a.png')], 0);
    expect(t.activeDetail).not.toBeNull();
  });

  it('a fresh gallery tab is not loading', () => {
    const t = createTabs();
    const g = t.list[0];
    expect(g.kind === 'gallery' && g.loading).toBe(false);
  });
});
