/** In-memory tab model. A uniform ordered list of gallery and detail tabs.
 *  Invariant: at least one gallery tab always exists. Closing the last one is
 *  refused, so `list` is never empty and `activeId` never dangles.
 */

import type { FileDto } from './types';
import { loadSort, type GallerySort } from './gallery-sort';

export interface GalleryTab {
  kind: 'gallery';
  id: number;
  query: string;
  files: FileDto[];
  sort: GallerySort;
  scrollTop: number;
  /** A search is in flight for this tab and has been slow enough to admit it
   *  (see `createPending`). Drives the grid dim and the SearchBar spinner. */
  loading: boolean;
  /** Multi-select (#23): selected file hashes + the shift-range anchor.
   *  Replaced wholesale on change — $state does not deep-proxy Sets, so
   *  reactivity rides on reassignment. */
  selected: ReadonlySet<string>;
  anchor: string | null;
  focused: string | null;
}

export interface DetailTab {
  kind: 'detail';
  id: number;
  files: FileDto[];
  index: number;
  /** The currently shown file, derived from files[index]. */
  readonly file: FileDto;
}

export type Tab = GalleryTab | DetailTab;

/** Create an isolated tab store. The app uses the shared `tabs` singleton; tests
 *  call this directly for a fresh instance.
 */
export function createTabs() {
  let nextId = 1;

  function makeGallery(): GalleryTab {
    return {
      kind: 'gallery',
      id: nextId++,
      query: '',
      files: [],
      // New gallery tabs start from the persisted sort preference.
      sort: loadSort(),
      scrollTop: 0,
      loading: false,
      selected: new Set<string>(),
      anchor: null,
      focused: null,
    };
  }

  const initialGallery = makeGallery();
  let list = $state<Tab[]>([initialGallery]);
  let activeId = $state<number>(initialGallery.id);
  // The most recently active gallery tab. While a detail tab is active this is
  // the gallery the grid keeps showing underneath (#55).
  let lastGalleryId = $state<number>(initialGallery.id);

  function byId(id: number): Tab | undefined {
    return list.find((t) => t.id === id);
  }

  function setActive(id: number): void {
    activeId = id;
    if (byId(id)?.kind === 'gallery') lastGalleryId = id;
  }

  return {
    get list(): Tab[] {
      return list;
    },
    get activeId(): number {
      return activeId;
    },
    get galleryCount(): number {
      return list.filter((t) => t.kind === 'gallery').length;
    },
    get activeGallery(): GalleryTab | null {
      const t = byId(activeId);
      return t?.kind === 'gallery' ? t : null;
    },
    get activeDetail(): DetailTab | null {
      const t = byId(activeId);
      return t?.kind === 'detail' ? t : null;
    },
    /** The gallery tab the grid should render: the active one, else the most
     *  recently active gallery (kept mounted behind a detail tab), else any
     *  remaining gallery. Non-null by the store invariant.
     */
    get displayGallery(): GalleryTab | null {
      const active = byId(activeId);
      if (active?.kind === 'gallery') return active;
      const last = byId(lastGalleryId);
      if (last?.kind === 'gallery') return last;
      const first = list.find((t) => t.kind === 'gallery');
      return first?.kind === 'gallery' ? first : null;
    },
    findByHash(hash: string): DetailTab | undefined {
      return list.find(
        (t): t is DetailTab => t.kind === 'detail' && t.file.hash === hash,
      );
    },
    /** Append a fresh gallery tab and return the instance from `list`, so caller
     *  mutations update the reactive state object.
     */
    openGallery(): GalleryTab {
      list = [...list, makeGallery()];
      const tab = list[list.length - 1] as GalleryTab;
      setActive(tab.id);
      return tab;
    },
    /** Open a detail tab. `background: true` appends it without switching to
     *  it (browser-style middle-click). (#63)
     */
    openDetail(files: FileDto[], index: number, opts?: { background?: boolean }): void {
      const start = Math.min(Math.max(index, 0), files.length - 1);
      const tab: DetailTab = {
        kind: 'detail',
        id: nextId++,
        files,
        index: start,
        get file() {
          return this.files[this.index];
        },
      };
      list = [...list, tab];
      if (!opts?.background) setActive(tab.id);
    },
    next(): void {
      const t = byId(activeId);
      if (t?.kind === 'detail' && t.index < t.files.length - 1) t.index += 1;
    },
    prev(): void {
      const t = byId(activeId);
      if (t?.kind === 'detail' && t.index > 0) t.index -= 1;
    },
    activate(id: number): void {
      if (byId(id)) setActive(id);
    },
    /** Activate the tab at 0-based position n, clamped to the list bounds. */
    activateAt(n: number): void {
      const i = Math.min(Math.max(n, 0), list.length - 1);
      setActive(list[i].id);
    },
    activateLast(): void {
      setActive(list[list.length - 1].id);
    },
    /** Next/previous tab with wraparound. */
    cycle(dir: 1 | -1): void {
      const idx = list.findIndex((t) => t.id === activeId);
      const n = list.length;
      setActive(list[(idx + dir + n) % n].id);
    },
    close(id: number): void {
      const idx = list.findIndex((t) => t.id === id);
      if (idx === -1) return;
      const tab = list[idx];
      if (
        tab.kind === 'gallery' &&
        list.filter((t) => t.kind === 'gallery').length === 1
      ) {
        return;
      }
      const wasActive = activeId === id;
      list = list.filter((t) => t.id !== id);
      if (wasActive) {
        const neighbour = list[idx] ?? list[idx - 1];
        setActive(neighbour.id);
      }
    },
  };
}

/** App-wide singleton. */
export const tabs = createTabs();
