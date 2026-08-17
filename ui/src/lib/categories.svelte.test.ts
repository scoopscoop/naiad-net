import { describe, it, expect, beforeEach, vi } from 'vitest';
import { loadCategories, CATEGORIES_KEY } from './categories.svelte';

describe('loadCategories', () => {
  beforeEach(() => localStorage.clear());

  it('returns defaults when nothing is stored', () => {
    expect(loadCategories().map((c) => c.id)[0]).toBe('artist');
    expect(loadCategories().length).toBe(7);
  });

  it('returns defaults on corrupt JSON', () => {
    localStorage.setItem(CATEGORIES_KEY, '{not json');
    expect(loadCategories().length).toBe(7);
  });

  it('returns defaults when the version is wrong', () => {
    localStorage.setItem(CATEGORIES_KEY, JSON.stringify({ version: 99, list: [] }));
    expect(loadCategories().length).toBe(7);
  });

  it('loads a stored valid list', () => {
    const list = [{ id: 'x', name: 'X', color: '#111111', namespaces: ['a'] }];
    localStorage.setItem(CATEGORIES_KEY, JSON.stringify({ version: 1, list }));
    expect(loadCategories()).toEqual(list);
  });
});

describe('categories store (localStorage-backed)', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
  });

  it('add appends a category and persists', async () => {
    const { categories } = await import('./categories.svelte');
    const before = categories.list.length;
    categories.add();
    expect(categories.list.length).toBe(before + 1);
    const stored = JSON.parse(localStorage.getItem(CATEGORIES_KEY)!);
    expect(stored.list.length).toBe(before + 1);
  });

  it('update patches fields by id', async () => {
    const { categories } = await import('./categories.svelte');
    categories.update('artist', { name: 'Creators', namespaces: ['creator'] });
    const cat = categories.list.find((c) => c.id === 'artist');
    expect(cat?.name).toBe('Creators');
    expect(cat?.namespaces).toEqual(['creator']);
  });

  it('remove deletes by id', async () => {
    const { categories } = await import('./categories.svelte');
    categories.remove('meta');
    expect(categories.list.find((c) => c.id === 'meta')).toBeUndefined();
  });

  it('move swaps adjacent entries and is a no-op at the ends', async () => {
    const { categories } = await import('./categories.svelte');
    const ids = () => categories.list.map((c) => c.id);
    const first = ids()[0];
    categories.move(first, -1);
    expect(ids()[0]).toBe(first); // already at top
    const last = ids()[ids().length - 1];
    categories.move(last, 1);
    expect(ids()[ids().length - 1]).toBe(last); // already at bottom
    categories.move(first, 1);
    expect(ids()[1]).toBe(first); // moved down one
  });

  it('reset restores the defaults', async () => {
    const { categories } = await import('./categories.svelte');
    categories.remove('meta');
    categories.reset();
    expect(categories.list.length).toBe(7);
    expect(categories.list.find((c) => c.id === 'meta')).toBeDefined();
  });
});
