/** Saved searches for the nav rail. Pure UI preference - never read by daemon. */

export const RAIL_SAVED_KEY = 'naiad.rail.saved';

export interface SavedSearch {
  name: string;
  query: string;
}

export function loadSaved(): SavedSearch[] {
  try {
    const raw = typeof localStorage !== 'undefined' ? localStorage.getItem(RAIL_SAVED_KEY) : null;
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    if (!parsed.every((s) => typeof s?.name === 'string' && typeof s?.query === 'string')) {
      return [];
    }
    return parsed as SavedSearch[];
  } catch {
    return [];
  }
}

export function saveSaved(list: SavedSearch[]): void {
  try {
    localStorage.setItem(RAIL_SAVED_KEY, JSON.stringify(list));
  } catch {
    // Private-mode / quota failures are non-fatal - keep the in-memory value.
  }
}
