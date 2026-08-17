/**
 * Tag categorization. Tags arrive as full `namespace:subtag` strings. A category
 * is a user-defined, colored bucket that claims one or more namespaces; the empty
 * string `""` claims unnamespaced ("general") tags. Categorization is pure
 * presentation — never read by the daemon.
 */

export interface TagCategory {
  /** Stable key; survives renames. */
  id: string;
  /** Display label, e.g. "Artist". */
  name: string;
  /** Dot + header accent color as `#rrggbb` (settings uses `<input type=color>`). */
  color: string;
  /** Claimed namespaces; "" means unnamespaced/general. */
  namespaces: string[];
}

export interface TagCategoryConfig {
  version: 1;
  list: TagCategory[];
}

/** Neutral color for the implicit "Other" catch-all group. */
export const OTHER_COLOR = '#7d7873';

/** Default categories, hex approximations of the prior `--ns-*` palette. */
export const DEFAULT_CATEGORIES: TagCategory[] = [
  { id: 'artist', name: 'Artist', color: '#5a7fb5', namespaces: ['creator', 'artist'] },
  { id: 'character', name: 'Character', color: '#5f9e6b', namespaces: ['character'] },
  { id: 'series', name: 'Series', color: '#9a6fb0', namespaces: ['series', 'copyright'] },
  { id: 'general', name: 'General', color: '#7d7873', namespaces: [''] },
  { id: 'meta', name: 'Meta', color: '#9a9080', namespaces: ['meta'] },
  { id: 'rating', name: 'Rating', color: '#c06a5f', namespaces: ['rating'] },
  { id: 'medium', name: 'Medium', color: '#5a9aa8', namespaces: ['medium'] },
];

/** A fresh default config; deep-copied so callers cannot mutate the shared seed. */
export function defaultConfig(): TagCategoryConfig {
  return {
    version: 1,
    list: DEFAULT_CATEGORIES.map((c) => ({ ...c, namespaces: [...c.namespaces] })),
  };
}

/** The raw namespace of a tag: text before the first `:`, or "" if none. */
export function namespaceOf(tag: string): string {
  const i = tag.indexOf(':');
  return i < 0 ? '' : tag.slice(0, i);
}

/**
 * The category claiming this tag's namespace, or null for the "Other" bucket.
 * First match wins: an earlier category claiming the same namespace takes it.
 */
export function categoryOf(tag: string, list: TagCategory[]): TagCategory | null {
  const ns = namespaceOf(tag);
  return list.find((c) => c.namespaces.includes(ns)) ?? null;
}

export interface TagGroup<T> {
  /** Category id, or "other" for the catch-all. */
  id: string;
  name: string;
  color: string;
  tags: T[];
}

/**
 * Partition items into ordered, non-empty groups: one per category in config
 * order (empty groups dropped), then a trailing "Other" group for anything
 * unclaimed. `key` extracts the tag string from each item.
 */
export function groupByCategory<T>(
  items: T[],
  list: TagCategory[],
  key: (item: T) => string,
): TagGroup<T>[] {
  const buckets = new Map<string, T[]>();
  const other: T[] = [];
  for (const item of items) {
    const cat = categoryOf(key(item), list);
    if (cat === null) {
      other.push(item);
    } else {
      const arr = buckets.get(cat.id);
      if (arr) arr.push(item);
      else buckets.set(cat.id, [item]);
    }
  }
  const groups: TagGroup<T>[] = [];
  for (const c of list) {
    const tags = buckets.get(c.id);
    if (tags && tags.length > 0) {
      groups.push({ id: c.id, name: c.name, color: c.color, tags });
    }
  }
  if (other.length > 0) {
    groups.push({ id: 'other', name: 'Other', color: OTHER_COLOR, tags: other });
  }
  return groups;
}
