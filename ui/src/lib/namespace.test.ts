import { describe, it, expect } from 'vitest';
import {
  namespaceOf,
  categoryOf,
  groupByCategory,
  defaultConfig,
  type TagCategory,
} from './namespace';

const list: TagCategory[] = [
  { id: 'artist', name: 'Artist', color: '#5a7fb5', namespaces: ['creator', 'artist'] },
  { id: 'series', name: 'Series', color: '#9a6fb0', namespaces: ['series', 'copyright'] },
  { id: 'general', name: 'General', color: '#7d7873', namespaces: [''] },
];

describe('namespaceOf', () => {
  it('returns the text before the first colon', () =>
    expect(namespaceOf('character:samus')).toBe('character'));
  it('returns empty string when there is no colon', () =>
    expect(namespaceOf('blue_sky')).toBe(''));
  it('splits on the first colon only', () =>
    expect(namespaceOf('a:b:c')).toBe('a'));
});

describe('categoryOf', () => {
  it('matches a claimed namespace', () =>
    expect(categoryOf('creator:artgerm', list)?.id).toBe('artist'));
  it('matches an aliased namespace', () =>
    expect(categoryOf('copyright:metroid', list)?.id).toBe('series'));
  it('routes unnamespaced tags to a category claiming ""', () =>
    expect(categoryOf('blue_sky', list)?.id).toBe('general'));
  it('returns null for an unclaimed namespace', () =>
    expect(categoryOf('medium:photo', list)).toBeNull());
  it('first match wins on duplicate claims', () => {
    const dup: TagCategory[] = [
      { id: 'a', name: 'A', color: '#111111', namespaces: ['x'] },
      { id: 'b', name: 'B', color: '#222222', namespaces: ['x'] },
    ];
    expect(categoryOf('x:y', dup)?.id).toBe('a');
  });
});

describe('groupByCategory', () => {
  const tags = [
    { tag: 'series:metroid' },
    { tag: 'creator:artgerm' },
    { tag: 'blue_sky' },
    { tag: 'medium:photo' },
  ];
  const groups = groupByCategory(tags, list, (t) => t.tag);

  it('orders groups by the category list, Other last', () =>
    expect(groups.map((g) => g.id)).toEqual(['artist', 'series', 'general', 'other']));
  it('drops categories with no matching tags', () => {
    const only = groupByCategory([{ tag: 'creator:x' }], list, (t) => t.tag);
    expect(only.map((g) => g.id)).toEqual(['artist']);
  });
  it('puts unclaimed tags in the Other group', () => {
    const other = groups.find((g) => g.id === 'other');
    expect(other?.tags).toEqual([{ tag: 'medium:photo' }]);
  });
});

describe('defaultConfig', () => {
  it('returns version 1 with the seeded categories', () => {
    const cfg = defaultConfig();
    expect(cfg.version).toBe(1);
    expect(cfg.list.map((c) => c.id)).toEqual([
      'artist', 'character', 'series', 'general', 'meta', 'rating', 'medium',
    ]);
  });
  it('deep-copies so callers cannot mutate shared state', () => {
    defaultConfig().list[0].namespaces.push('mutated');
    expect(defaultConfig().list[0].namespaces).not.toContain('mutated');
  });
});
