import type { FileDto } from './types';

export type SortKey = 'imported_at' | 'created_at' | 'modified_at' | 'name' | 'size' | 'type';
export type SortDirection = 'asc' | 'desc';

export interface GallerySort {
  key: SortKey;
  direction: SortDirection;
}

export const DEFAULT_SORT: GallerySort = { key: 'imported_at', direction: 'desc' };

export const SORT_KEY = 'naiad.gallery.sort';

export const SORT_KEYS: readonly SortKey[] = [
  'imported_at',
  'created_at',
  'modified_at',
  'name',
  'size',
  'type',
];

function isGallerySort(value: unknown): value is GallerySort {
  const sort = value as GallerySort;
  return (
    typeof value === 'object' &&
    value !== null &&
    SORT_KEYS.includes(sort.key) &&
    (sort.direction === 'asc' || sort.direction === 'desc')
  );
}

/** The persisted sort, or DEFAULT_SORT when absent or unparseable. Returns a
 *  fresh object each call so tabs never share (and mutate) one instance.
 */
export function loadSort(): GallerySort {
  try {
    const raw = localStorage.getItem(SORT_KEY);
    if (raw !== null) {
      const parsed: unknown = JSON.parse(raw);
      if (isGallerySort(parsed)) return { key: parsed.key, direction: parsed.direction };
    }
  } catch {
    // Storage unavailable (missing, or access throws) or corrupt entry —
    // fall through to the default. Tab creation must never fail on storage.
  }
  return { ...DEFAULT_SORT };
}

export function saveSort(sort: GallerySort): void {
  try {
    localStorage.setItem(SORT_KEY, JSON.stringify(sort));
  } catch {
    // Private-mode / quota failures are non-fatal — keep the in-memory value.
  }
}

export function defaultDirection(key: SortKey): SortDirection {
  return key === 'name' || key === 'type' ? 'asc' : 'desc';
}

export function nextSort(current: GallerySort, key: SortKey): GallerySort {
  if (current.key === key) {
    return { key, direction: current.direction === 'asc' ? 'desc' : 'asc' };
  }
  return { key, direction: defaultDirection(key) };
}

export function sortFiles(files: FileDto[], sort: GallerySort): FileDto[] {
  return [...files].sort((a, b) => compareFiles(a, b, sort));
}

/** Memoized sortFiles keyed on the files array's identity plus the sort
 *  fields (#55). Tabs replace `files` wholesale on every search, so identity
 *  is a safe change signal; in-place mutation of a tab's files would go
 *  unnoticed. The cache makes re-activating a gallery tab O(1): sorting reads
 *  every row through Svelte's deep-reactivity proxy, which costs ~1s at 100k
 *  files, so it must only happen when the query or sort actually changed.
 */
const sortCache = new WeakMap<FileDto[], { sort: GallerySort; result: FileDto[] }>();

export function sortFilesCached(files: FileDto[], sort: GallerySort): FileDto[] {
  const hit = sortCache.get(files);
  if (hit && hit.sort.key === sort.key && hit.sort.direction === sort.direction) {
    return hit.result;
  }
  const result = sortFiles(files, sort);
  sortCache.set(files, { sort: { ...sort }, result });
  return result;
}

function compareFiles(a: FileDto, b: FileDto, sort: GallerySort): number {
  let primary = 0;
  if (sort.key === 'name') primary = compareText(a.name, b.name, sort.direction);
  else if (sort.key === 'size') primary = compareNumber(a.size, b.size, sort.direction);
  else if (sort.key === 'type') primary = compareOptionalText(typeOf(a), typeOf(b), sort.direction);
  else primary = compareOptionalNumber(a[sort.key], b[sort.key], sort.direction);

  return (
    primary ||
    compareText(a.name, b.name, 'asc') ||
    compareText(a.path, b.path, 'asc') ||
    a.hash.localeCompare(b.hash)
  );
}

function compareNumber(a: number, b: number, direction: SortDirection): number {
  return direction === 'asc' ? a - b : b - a;
}

function compareOptionalNumber(
  a: number | null,
  b: number | null,
  direction: SortDirection,
): number {
  if (a == null && b == null) return 0;
  if (a == null) return 1;
  if (b == null) return -1;
  return compareNumber(a, b, direction);
}

function compareText(a: string, b: string, direction: SortDirection): number {
  const cmp = a.localeCompare(b, undefined, { sensitivity: 'base', numeric: true });
  return direction === 'asc' ? cmp : -cmp;
}

function compareOptionalText(
  a: string | null,
  b: string | null,
  direction: SortDirection,
): number {
  if (a == null && b == null) return 0;
  if (a == null) return 1;
  if (b == null) return -1;
  return compareText(a, b, direction);
}

function typeOf(file: FileDto): string | null {
  if (file.mime) return file.mime.toLocaleLowerCase();
  const name = file.name || file.path;
  const dot = name.lastIndexOf('.');
  if (dot < 0 || dot === name.length - 1) return null;
  return name.slice(dot + 1).toLocaleLowerCase();
}
