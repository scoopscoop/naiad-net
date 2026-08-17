/** Persisted UI view preferences. Most prefs are purely cosmetic; some (e.g.
 *  `completionMatch`) are forwarded to the daemon as query parameters. */

export const ZOOM_LEVEL_KEY = 'naiad.view.zoom_level';
/** Pre-#171 pixel tile size; read once for migration, never written. */
export const LEGACY_TILE_KEY = 'naiad.view.tile';
export const LOCAL_ONLY_KEY = 'naiad.view.local_only';
export const COMPLETION_MATCH_KEY = 'naiad.view.completion_match';
export const THUMB_FIT_KEY = 'naiad.view.thumb_fit';
export const SHOW_ALIAS_SOURCE_KEY = 'naiad.view.show_alias_source';
export const HIDDEN_ORIGINS_KEY = 'naiad.view.hidden_origins'; // JSON string[]

export type CompletionMatch = 'prefix' | 'substring';
export type ThumbFit = 'frame' | 'fill';

/** Gallery zoom is a discrete thumbs-per-row level (#171): LEVEL_MAX = 16 per
 *  row (min zoom) down to LEVEL_MIN = 2 per row (max zoom). The pixel tile size
 *  is *derived* from the measured gallery width in Grid.svelte, so a level
 *  means the same visual density on any window size or DPI. Shared by the bar
 *  slider and the settings panel so the surfaces never disagree. At 2–3 per
 *  row on a wide window tiles upscale past the daemon's 360px served
 *  thumbnail (soft) — same trade-off as the old 1024px pixel extreme. */
export const LEVEL_MIN = 2;
export const LEVEL_MAX = 16;
const DEFAULT_LEVEL = 8;

/** Grid gap in px; mirrors GAP in Grid.svelte (only used here to estimate a
 *  level from a legacy pixel tile size during migration). */
const MIGRATION_GAP = 10;

/** Round to an integer and clamp to the level range; the default if unparseable. */
export function clampLevel(n: number): number {
  if (!Number.isFinite(n)) return DEFAULT_LEVEL;
  return Math.min(LEVEL_MAX, Math.max(LEVEL_MIN, Math.round(n)));
}

/** Map a pre-#171 pixel tile size to the level (columns) the old auto-fill
 *  formula would have produced at the current window width. Approximate on
 *  purpose — this runs once per migration and only needs to land near the
 *  density the user was looking at. */
function legacyPxToLevel(px: number): number {
  if (!Number.isFinite(px) || px <= 0) return DEFAULT_LEVEL;
  const winWidth =
    typeof window !== 'undefined' && window.innerWidth > 0 ? window.innerWidth : 1280;
  // The old auto-fill formula ran on the gallery pane, not the window: knock
  // off a nominal chrome allowance (nav rail + inspector) and floor like the
  // original `floor((avail + gap) / (px + gap))` did, so the estimate doesn't
  // land systematically more zoomed-out than what the user was looking at.
  const width = Math.max(400, winWidth - 320);
  return clampLevel(Math.floor((width + MIGRATION_GAP) / (px + MIGRATION_GAP)));
}

/** Interpret a persisted zoom number from either era: 2..16 is a level, larger
 *  values are legacy pixel tile sizes (the old range was 80..1024). */
function normalizeStoredZoom(raw: number): number {
  if (!Number.isFinite(raw)) return DEFAULT_LEVEL;
  return raw > LEVEL_MAX ? legacyPxToLevel(raw) : clampLevel(raw);
}

function load(): number {
  if (typeof localStorage === 'undefined') return DEFAULT_LEVEL;
  const raw = localStorage.getItem(ZOOM_LEVEL_KEY);
  if (raw !== null) return clampLevel(Number(raw));
  const legacy = localStorage.getItem(LEGACY_TILE_KEY);
  if (legacy !== null) {
    // One-time migration: persist the derived level immediately so a window
    // resize between sessions can't silently shift the effective zoom.
    const level = legacyPxToLevel(Number(legacy));
    try {
      localStorage.setItem(ZOOM_LEVEL_KEY, String(level));
    } catch {
      // Private-mode / quota failures are non-fatal - keep the in-memory value.
    }
    return level;
  }
  return DEFAULT_LEVEL;
}

let zoomLevel = $state(load());

/** Set once the user drags/edits the zoom level, so a late desktop load cannot
 *  undo them (mirrors `inspectorCollapsedTouched`). */
let zoomLevelTouched = false;

function storeZoomLevel(n: number): void {
  try {
    localStorage.setItem(ZOOM_LEVEL_KEY, String(n));
  } catch {
    // Private-mode / quota failures are non-fatal - keep the in-memory value.
  }
}

function loadLocalOnly(): boolean {
  return typeof localStorage !== 'undefined' && localStorage.getItem(LOCAL_ONLY_KEY) === 'true';
}

let localOnly = $state(loadLocalOnly());

function loadCompletionMatch(): CompletionMatch {
  const raw =
    typeof localStorage !== 'undefined' ? localStorage.getItem(COMPLETION_MATCH_KEY) : null;
  return raw === 'substring' ? 'substring' : 'prefix';
}

let completionMatch = $state<CompletionMatch>(loadCompletionMatch());

function loadThumbFit(): ThumbFit {
  const raw = typeof localStorage !== 'undefined' ? localStorage.getItem(THUMB_FIT_KEY) : null;
  return raw === 'fill' ? 'fill' : 'frame';
}

let thumbFit = $state<ThumbFit>(loadThumbFit());

function loadShowAliasSource(): boolean {
  return (
    typeof localStorage !== 'undefined' &&
    localStorage.getItem(SHOW_ALIAS_SOURCE_KEY) === 'true'
  );
}

let showAliasSource = $state(loadShowAliasSource());

function loadHiddenOrigins(): Set<string> {
  if (typeof localStorage === 'undefined') return new Set();
  const raw = localStorage.getItem(HIDDEN_ORIGINS_KEY);
  if (raw === null) return new Set();
  try {
    const parsed: unknown = JSON.parse(raw);
    if (Array.isArray(parsed) && parsed.every((v) => typeof v === 'string')) {
      return new Set(parsed as string[]);
    }
  } catch {
    // Corrupt entry — fall back to empty.
  }
  return new Set();
}

let hiddenOriginsSet = $state<Set<string>>(loadHiddenOrigins());

export const INSPECTOR_COLLAPSED_KEY = 'naiad.view.inspector_collapsed';

type DesktopViewState = {
  inspector_collapsed?: boolean | null;
  /** Zoom level (thumbs per row) since #171; a pre-#171 store holds the old
   *  pixel tile size, which `normalizeStoredZoom` converts on load. */
  tile?: number | null;
};

function isTauriRuntime(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

function loadInspectorCollapsed(): boolean {
  const raw =
    typeof localStorage !== 'undefined' ? localStorage.getItem(INSPECTOR_COLLAPSED_KEY) : null;
  if (raw === 'true') return true;
  if (raw === 'false') return false;
  // First run: collapse on narrow windows (the old component-local heuristic).
  return typeof window !== 'undefined' && window.innerWidth < 1100;
}

let inspectorCollapsed = $state(loadInspectorCollapsed());

/** Set once the user toggles the inspector, so a late load cannot undo them. */
let inspectorCollapsedTouched = false;

function storeInspectorCollapsed(collapsed: boolean): void {
  try {
    localStorage.setItem(INSPECTOR_COLLAPSED_KEY, String(collapsed));
  } catch {
    // Private-mode / quota failures are non-fatal - keep the in-memory value.
  }
}

async function loadDesktopViewState(): Promise<void> {
  if (!isTauriRuntime()) return;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const state = await invoke<DesktopViewState>('load_view_state');
    // A toggle/drag that landed while the invoke was in flight is the user's
    // intent; each field guards independently so one touch can't block another.
    if (!inspectorCollapsedTouched && typeof state.inspector_collapsed === 'boolean') {
      inspectorCollapsed = state.inspector_collapsed;
      // Mirror into localStorage so the browser and dev-server agree, but do not
      // write back to the Tauri store we just read from.
      storeInspectorCollapsed(state.inspector_collapsed);
    }
    if (!zoomLevelTouched && typeof state.tile === 'number') {
      zoomLevel = normalizeStoredZoom(state.tile);
      storeZoomLevel(zoomLevel);
      // A legacy pixel value was converted: write the level back so the next
      // launch reads a level directly (one-time migration, not an echo).
      if (state.tile > LEVEL_MAX) saveDesktopZoomLevel(zoomLevel);
    }
  } catch (err) {
    // Inside Tauri this means the store is unreachable (e.g. the command lost
    // its capability grant), not merely that we are in a browser.
    console.warn('load_view_state failed; falling back to localStorage', err);
  }
}

function saveDesktopInspectorCollapsed(collapsed: boolean): void {
  if (!isTauriRuntime()) return;
  void import('@tauri-apps/api/core')
    .then(({ invoke }) => invoke('save_inspector_collapsed', { collapsed }))
    .catch((err: unknown) => {
      // The in-memory and localStorage values still update, but a silent no-op
      // would hide a capability regression behind session-local persistence.
      console.warn('save_inspector_collapsed failed; preference is session-local', err);
    });
}

let zoomSaveTimer: ReturnType<typeof setTimeout> | undefined;

function saveDesktopZoomLevel(n: number): void {
  if (!isTauriRuntime()) return;
  // The bar's zoom slider fires `input` continuously through a drag, so
  // writing view-state.json on every value would read-modify-write the file
  // dozens of times per gesture. Debounce so one gesture yields one disk
  // write; localStorage still updates immediately. The desktop store's `tile`
  // field carries the level since #171 (same u32 slot, new meaning).
  clearTimeout(zoomSaveTimer);
  zoomSaveTimer = setTimeout(() => {
    void import('@tauri-apps/api/core')
      .then(({ invoke }) => invoke('save_tile', { tile: n }))
      .catch((err: unknown) => {
        // The in-memory and localStorage values still update, but a silent no-op
        // would hide a capability regression behind session-local persistence.
        console.warn('save_tile failed; preference is session-local', err);
      });
  }, 200);
}

void loadDesktopViewState();

/**
 * Reactive, self-persisting view preferences. Both the bar slider and the
 * settings panel write through this single source so they stay in sync.
 *
 * All prefs persist to `localStorage`; `inspectorCollapsed` and `zoomLevel`
 * additionally write through to the desktop shell's `view-state.json` when
 * running under Tauri, so they survive a restart even though `localStorage` is
 * keyed by the daemon's ephemeral port.
 */
export const view = {
  /** Gallery zoom as thumbs per row (#171): LEVEL_MAX=16 (min zoom) .. LEVEL_MIN=2 (max zoom). */
  get zoomLevel(): number {
    return zoomLevel;
  },
  set zoomLevel(n: number) {
    zoomLevelTouched = true;
    zoomLevel = clampLevel(n);
    storeZoomLevel(zoomLevel);
    // Also write through to the desktop shell's view-state.json so the level
    // survives a restart, since localStorage is keyed by the ephemeral port.
    saveDesktopZoomLevel(zoomLevel);
  },
  get localOnly(): boolean {
    return localOnly;
  },
  set localOnly(v: boolean) {
    localOnly = v;
    try {
      localStorage.setItem(LOCAL_ONLY_KEY, String(v));
    } catch {
      // Private-mode / quota failures are non-fatal - keep the in-memory value.
    }
  },
  get completionMatch(): CompletionMatch {
    return completionMatch;
  },
  set completionMatch(v: CompletionMatch) {
    completionMatch = v;
    try {
      localStorage.setItem(COMPLETION_MATCH_KEY, v);
    } catch {
      // Private-mode / quota failures are non-fatal - keep the in-memory value.
    }
  },
  get thumbFit(): ThumbFit {
    return thumbFit;
  },
  set thumbFit(v: ThumbFit) {
    thumbFit = v;
    try {
      localStorage.setItem(THUMB_FIT_KEY, v);
    } catch {
      // Private-mode / quota failures are non-fatal - keep the in-memory value.
    }
  },
  get showAliasSource(): boolean {
    return showAliasSource;
  },
  set showAliasSource(v: boolean) {
    showAliasSource = v;
    try {
      localStorage.setItem(SHOW_ALIAS_SOURCE_KEY, String(v));
    } catch {
      // Private-mode / quota failures are non-fatal - keep the in-memory value.
    }
  },
  get inspectorCollapsed(): boolean {
    return inspectorCollapsed;
  },
  set inspectorCollapsed(v: boolean) {
    inspectorCollapsedTouched = true;
    inspectorCollapsed = v;
    saveDesktopInspectorCollapsed(v);
    storeInspectorCollapsed(v);
  },
  /** Origin keys whose tags are hidden in the inspector. Set-backed for O(1)
   *  membership; prefer `isOriginHidden` for lookups. Persisted to
   *  `localStorage` only — no Tauri write-through (low-stakes preference). */
  get hiddenOrigins(): string[] {
    return Array.from(hiddenOriginsSet);
  },
  /** O(1) membership test against the hidden-origin set. */
  isOriginHidden(key: string): boolean {
    return hiddenOriginsSet.has(key);
  },
  /** Toggle the visibility of one origin key. Adds it to the hidden set if
   *  absent, removes it if present; persists the updated array. */
  toggleHiddenOrigin(key: string): void {
    const next = new Set(hiddenOriginsSet);
    if (next.has(key)) {
      next.delete(key);
    } else {
      next.add(key);
    }
    hiddenOriginsSet = next;
    try {
      localStorage.setItem(HIDDEN_ORIGINS_KEY, JSON.stringify(Array.from(next)));
    } catch {
      // Private-mode / quota failures are non-fatal — keep the in-memory value.
    }
  },
};
