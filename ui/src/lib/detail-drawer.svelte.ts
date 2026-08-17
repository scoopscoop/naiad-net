/** Shared, persisted state for the detail tab's tag drawer. Deliberately global:
 *  every detail tab and every prev/next step reads the same {open, height}. */

export const DRAWER_KEY = 'naiad.detail.drawer';

const MIN_HEIGHT = 120;
const MAX_FRAC = 0.7;
const DEFAULT_HEIGHT = 280;

/** Clamp a drawer height against the current pane height; default on garbage. */
export function clampHeight(px: number, paneHeight: number): number {
  if (!Number.isFinite(px)) return DEFAULT_HEIGHT;
  const max = Math.max(MIN_HEIGHT, Math.floor(paneHeight * MAX_FRAC));
  return Math.min(max, Math.max(MIN_HEIGHT, Math.round(px)));
}

function load(): { open: boolean; height: number } {
  try {
    const raw = typeof localStorage !== 'undefined' ? localStorage.getItem(DRAWER_KEY) : null;
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (
        typeof (parsed as { open?: unknown }).open === 'boolean' &&
        typeof (parsed as { height?: unknown }).height === 'number'
      ) {
        return parsed as { open: boolean; height: number };
      }
    }
  } catch {
    // Fall through to defaults.
  }
  return { open: true, height: DEFAULT_HEIGHT };
}

const initial = load();
let open = $state(initial.open);
let height = $state(initial.height);

function persist() {
  try {
    localStorage.setItem(DRAWER_KEY, JSON.stringify({ open, height }));
  } catch {
    // Private-mode / quota failures are non-fatal - keep the in-memory value.
  }
}

export const drawer = {
  get open(): boolean {
    return open;
  },
  set open(v: boolean) {
    open = v;
    persist();
  },
  get height(): number {
    return height;
  },
  set height(px: number) {
    height = px;
    persist();
  },
};
