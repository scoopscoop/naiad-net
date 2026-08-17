/**
 * Reactive, self-persisting tag-category config. Mirrors the `view` store in
 * settings.svelte.ts: a single source the detail panel and settings UI share.
 * Pure presentation — never read by the daemon.
 */
import { defaultConfig, type TagCategory, type TagCategoryConfig } from './namespace';

export const CATEGORIES_KEY = 'naiad.view.categories';

/** Parse stored config into a list, falling back to defaults on any problem. */
export function loadCategories(): TagCategory[] {
  if (typeof localStorage === 'undefined') return defaultConfig().list;
  const raw = localStorage.getItem(CATEGORIES_KEY);
  if (raw === null) return defaultConfig().list;
  try {
    const parsed = JSON.parse(raw) as TagCategoryConfig;
    if (parsed && parsed.version === 1 && Array.isArray(parsed.list)) {
      return parsed.list;
    }
  } catch {
    // Corrupt JSON — fall through to defaults.
  }
  return defaultConfig().list;
}

let list = $state<TagCategory[]>(loadCategories());

function persist() {
  try {
    localStorage.setItem(CATEGORIES_KEY, JSON.stringify({ version: 1, list }));
  } catch {
    // Private-mode / quota failures are non-fatal — keep the in-memory value.
  }
}

let seq = 0;
function newId(): string {
  return `cat-${Date.now().toString(36)}-${(seq++).toString(36)}`;
}

export const categories = {
  get list(): TagCategory[] {
    return list;
  },
  /** Replace a category by id with patched fields. */
  update(id: string, patch: Partial<Omit<TagCategory, 'id'>>): void {
    list = list.map((c) => (c.id === id ? { ...c, ...patch } : c));
    persist();
  },
  add(): void {
    list = [...list, { id: newId(), name: 'New category', color: '#888888', namespaces: [] }];
    persist();
  },
  remove(id: string): void {
    list = list.filter((c) => c.id !== id);
    persist();
  },
  /** Swap a category with its neighbour; no-op past the ends. */
  move(id: string, dir: -1 | 1): void {
    const i = list.findIndex((c) => c.id === id);
    if (i < 0) return;
    const j = i + dir;
    if (j < 0 || j >= list.length) return;
    const next = [...list];
    [next[i], next[j]] = [next[j], next[i]];
    list = next;
    persist();
  },
  reset(): void {
    list = defaultConfig().list;
    persist();
  },
};
