import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { FileDto } from './types';
import {
  DEFAULT_SORT,
  SORT_KEY,
  defaultDirection,
  loadSort,
  nextSort,
  saveSort,
  sortFiles,
  sortFilesCached,
} from './gallery-sort';

function file(name: string, overrides: Partial<FileDto> = {}): FileDto {
  return {
    hash: name.padEnd(64, '0').slice(0, 64),
    name,
    path: `/lib/${name}`,
    size: 1,
    imported_at: 100,
    created_at: 80,
    modified_at: 90,
    mime: 'image/png',
    ...overrides,
  };
}

function names(files: FileDto[]): string[] {
  return files.map((f) => f.name);
}

describe('gallery sort', () => {
  it('uses sensible default directions', () => {
    expect(defaultDirection('imported_at')).toBe('desc');
    expect(defaultDirection('created_at')).toBe('desc');
    expect(defaultDirection('modified_at')).toBe('desc');
    expect(defaultDirection('size')).toBe('desc');
    expect(defaultDirection('name')).toBe('asc');
    expect(defaultDirection('type')).toBe('asc');
  });

  it('toggles direction when selecting the current key', () => {
    expect(nextSort({ key: 'name', direction: 'asc' }, 'name')).toEqual({
      key: 'name',
      direction: 'desc',
    });
  });

  it('applies default direction when selecting a different key', () => {
    expect(nextSort({ key: 'name', direction: 'asc' }, 'size')).toEqual({
      key: 'size',
      direction: 'desc',
    });
  });

  it('sorts import date ascending and descending', () => {
    const input = [
      file('old.png', { imported_at: 1 }),
      file('new.png', { imported_at: 3 }),
      file('mid.png', { imported_at: 2 }),
    ];
    expect(names(sortFiles(input, { key: 'imported_at', direction: 'desc' }))).toEqual([
      'new.png',
      'mid.png',
      'old.png',
    ]);
    expect(names(sortFiles(input, { key: 'imported_at', direction: 'asc' }))).toEqual([
      'old.png',
      'mid.png',
      'new.png',
    ]);
  });

  it('sorts null created dates last in both directions', () => {
    const input = [
      file('unknown.png', { created_at: null }),
      file('old.png', { created_at: 1 }),
      file('new.png', { created_at: 3 }),
    ];
    expect(names(sortFiles(input, { key: 'created_at', direction: 'asc' }))).toEqual([
      'old.png',
      'new.png',
      'unknown.png',
    ]);
    expect(names(sortFiles(input, { key: 'created_at', direction: 'desc' }))).toEqual([
      'new.png',
      'old.png',
      'unknown.png',
    ]);
  });

  it('uses name as a stable tie-breaker for modified dates', () => {
    const input = [
      file('b.png', { modified_at: 2 }),
      file('a.png', { modified_at: 2 }),
      file('c.png', { modified_at: 1 }),
    ];
    expect(names(sortFiles(input, { key: 'modified_at', direction: 'desc' }))).toEqual([
      'a.png',
      'b.png',
      'c.png',
    ]);
  });

  it('sorts names case-insensitively', () => {
    const input = [file('beta.png'), file('Alpha.png'), file('alpha-2.png')];
    expect(names(sortFiles(input, { key: 'name', direction: 'asc' }))).toEqual([
      'alpha-2.png',
      'Alpha.png',
      'beta.png',
    ]);
  });

  it('sorts size numerically', () => {
    const input = [file('small.png', { size: 1 }), file('big.png', { size: 10 })];
    expect(names(sortFiles(input, { key: 'size', direction: 'desc' }))).toEqual([
      'big.png',
      'small.png',
    ]);
  });

  it('sorts type by mime then extension fallback with unknown last', () => {
    const input = [
      file('unknown', { mime: null }),
      file('b.jpg', { mime: 'image/jpeg' }),
      file('a.gif', { mime: null }),
      file('c.png', { mime: 'image/png' }),
    ];
    expect(names(sortFiles(input, { key: 'type', direction: 'asc' }))).toEqual([
      'a.gif',
      'b.jpg',
      'c.png',
      'unknown',
    ]);
    expect(names(sortFiles(input, { key: 'type', direction: 'desc' }))).toEqual([
      'c.png',
      'b.jpg',
      'a.gif',
      'unknown',
    ]);
  });

  it('does not mutate the input array', () => {
    const input = [file('b.png'), file('a.png')];
    const sorted = sortFiles(input, { key: 'name', direction: 'asc' });
    expect(names(sorted)).toEqual(['a.png', 'b.png']);
    expect(names(input)).toEqual(['b.png', 'a.png']);
  });
});

describe('sort persistence (localStorage-backed)', () => {
  beforeEach(() => localStorage.clear());

  it('round-trips save → load', () => {
    saveSort({ key: 'name', direction: 'asc' });
    expect(loadSort()).toEqual({ key: 'name', direction: 'asc' });
  });

  it('returns a fresh default when nothing is saved', () => {
    const loaded = loadSort();
    expect(loaded).toEqual(DEFAULT_SORT);
    expect(loaded).not.toBe(DEFAULT_SORT);
  });

  it('returns a fresh object on every load so tabs never share one', () => {
    saveSort({ key: 'size', direction: 'desc' });
    const first = loadSort();
    const second = loadSort();
    expect(second).toEqual(first);
    expect(second).not.toBe(first);
  });

  it('falls back to the default on corrupt JSON', () => {
    localStorage.setItem(SORT_KEY, '{not json');
    expect(loadSort()).toEqual(DEFAULT_SORT);
  });

  it('falls back to the default on an unknown key or direction', () => {
    localStorage.setItem(SORT_KEY, JSON.stringify({ key: 'rating', direction: 'asc' }));
    expect(loadSort()).toEqual(DEFAULT_SORT);
    localStorage.setItem(SORT_KEY, JSON.stringify({ key: 'name', direction: 'sideways' }));
    expect(loadSort()).toEqual(DEFAULT_SORT);
    localStorage.setItem(SORT_KEY, JSON.stringify(null));
    expect(loadSort()).toEqual(DEFAULT_SORT);
  });

  it('falls back to the default when storage access throws', () => {
    const spy = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new DOMException('storage disabled', 'SecurityError');
    });
    try {
      expect(loadSort()).toEqual(DEFAULT_SORT);
    } finally {
      spy.mockRestore();
    }
  });
});

describe('sortFilesCached', () => {
  it('returns the identical array for repeated (files, sort) inputs', () => {
    const input = [file('b.png'), file('a.png')];
    const sort = { key: 'name', direction: 'asc' } as const;
    const first = sortFilesCached(input, sort);
    expect(names(first)).toEqual(['a.png', 'b.png']);
    // Same files identity + equal sort → cache hit, no re-sort. A fresh sort
    // object with equal fields must still hit (the app replaces the sort object
    // on every change).
    expect(sortFilesCached(input, { ...sort })).toBe(first);
  });

  it('re-sorts when the sort changes and when the files identity changes', () => {
    const input = [file('b.png'), file('a.png')];
    const asc = sortFilesCached(input, { key: 'name', direction: 'asc' });
    const desc = sortFilesCached(input, { key: 'name', direction: 'desc' });
    expect(names(desc)).toEqual(['b.png', 'a.png']);
    expect(desc).not.toBe(asc);

    const replaced = [...input, file('c.png')];
    const again = sortFilesCached(replaced, { key: 'name', direction: 'desc' });
    expect(names(again)).toEqual(['c.png', 'b.png', 'a.png']);
  });

  it('caches per files array, not globally', () => {
    const a = [file('a.png')];
    const b = [file('b.png')];
    const sort = { key: 'name', direction: 'asc' } as const;
    const sortedA = sortFilesCached(a, sort);
    sortFilesCached(b, sort);
    // Coming back to `a` with the same sort is still a hit.
    expect(sortFilesCached(a, sort)).toBe(sortedA);
  });
});
