import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import {
  clampLevel,
  ZOOM_LEVEL_KEY,
  LEGACY_TILE_KEY,
  COMPLETION_MATCH_KEY,
  THUMB_FIT_KEY,
} from './settings.svelte';

describe('clampLevel', () => {
  it('clamps below the minimum to 2', () => expect(clampLevel(1)).toBe(2));
  it('clamps above the maximum to 16', () => expect(clampLevel(40)).toBe(16));
  it('rounds non-integers', () => expect(clampLevel(7.6)).toBe(8));
  it('falls back to the default for non-finite input', () => expect(clampLevel(NaN)).toBe(8));
});

describe('view.zoomLevel (localStorage-backed)', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
  });

  it('defaults to 8 when nothing is stored', async () => {
    const { view } = await import('./settings.svelte');
    expect(view.zoomLevel).toBe(8);
  });

  it('loads a stored level on init', async () => {
    localStorage.setItem(ZOOM_LEVEL_KEY, '4');
    const { view } = await import('./settings.svelte');
    expect(view.zoomLevel).toBe(4);
  });

  it('writes through to localStorage on set', async () => {
    const { view } = await import('./settings.svelte');
    view.zoomLevel = 12;
    expect(localStorage.getItem(ZOOM_LEVEL_KEY)).toBe('12');
  });

  it('clamps on set', async () => {
    const { view } = await import('./settings.svelte');
    view.zoomLevel = 9999;
    expect(view.zoomLevel).toBe(16);
    expect(localStorage.getItem(ZOOM_LEVEL_KEY)).toBe('16');
  });

  it('migrates a legacy pixel tile size to the nearest level and persists it', async () => {
    // Gallery-width proxy = 1280 - 320 chrome = 960; floor((960+10)/(160+10)) = 5,
    // matching the old auto-fill column count for 160px tiles in that pane.
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 1280 });
    localStorage.setItem(LEGACY_TILE_KEY, '160');
    vi.resetModules();
    const { view } = await import('./settings.svelte');
    expect(view.zoomLevel).toBe(5);
    // Sticky: a between-session window resize must not shift the zoom again.
    expect(localStorage.getItem(ZOOM_LEVEL_KEY)).toBe('5');
  });

  it('migrates a huge legacy pixel size to max zoom (2 per row)', async () => {
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 1280 });
    localStorage.setItem(LEGACY_TILE_KEY, '1024');
    vi.resetModules();
    const { view } = await import('./settings.svelte');
    expect(view.zoomLevel).toBe(2);
  });

  it('prefers the new key over a lingering legacy key', async () => {
    localStorage.setItem(ZOOM_LEVEL_KEY, '5');
    localStorage.setItem(LEGACY_TILE_KEY, '1024');
    vi.resetModules();
    const { view } = await import('./settings.svelte');
    expect(view.zoomLevel).toBe(5);
  });
});

describe('view.localOnly (localStorage-backed)', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
  });

  it('defaults to false when nothing is stored', async () => {
    const { view } = await import('./settings.svelte');
    expect(view.localOnly).toBe(false);
  });

  it('loads a stored true value on init', async () => {
    localStorage.setItem('naiad.view.local_only', 'true');
    const { view } = await import('./settings.svelte');
    expect(view.localOnly).toBe(true);
  });

  it('writes through to localStorage on set', async () => {
    const { view } = await import('./settings.svelte');
    view.localOnly = true;
    expect(localStorage.getItem('naiad.view.local_only')).toBe('true');
    view.localOnly = false;
    expect(localStorage.getItem('naiad.view.local_only')).toBe('false');
  });
});

describe('view.completionMatch (localStorage-backed)', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
  });

  it('defaults to prefix when nothing is stored', async () => {
    const { view } = await import('./settings.svelte');
    expect(view.completionMatch).toBe('prefix');
  });

  it('loads a stored substring value on init', async () => {
    localStorage.setItem(COMPLETION_MATCH_KEY, 'substring');
    const { view } = await import('./settings.svelte');
    expect(view.completionMatch).toBe('substring');
  });

  it('writes through to localStorage on set', async () => {
    const { view } = await import('./settings.svelte');
    view.completionMatch = 'substring';
    expect(localStorage.getItem(COMPLETION_MATCH_KEY)).toBe('substring');
    view.completionMatch = 'prefix';
    expect(localStorage.getItem(COMPLETION_MATCH_KEY)).toBe('prefix');
  });

  it('falls back to prefix for an invalid stored value', async () => {
    localStorage.setItem(COMPLETION_MATCH_KEY, 'bogus');
    const { view } = await import('./settings.svelte');
    expect(view.completionMatch).toBe('prefix');
  });
});

describe('view.thumbFit (localStorage-backed)', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
  });

  it('defaults to frame when nothing is stored', async () => {
    const { view } = await import('./settings.svelte');
    expect(view.thumbFit).toBe('frame');
  });

  it('loads a stored fill value on init', async () => {
    localStorage.setItem(THUMB_FIT_KEY, 'fill');
    const { view } = await import('./settings.svelte');
    expect(view.thumbFit).toBe('fill');
  });

  it('writes through to localStorage on set', async () => {
    const { view } = await import('./settings.svelte');
    view.thumbFit = 'fill';
    expect(localStorage.getItem(THUMB_FIT_KEY)).toBe('fill');
    view.thumbFit = 'frame';
    expect(localStorage.getItem(THUMB_FIT_KEY)).toBe('frame');
  });

  it('falls back to frame for an invalid stored value', async () => {
    localStorage.setItem(THUMB_FIT_KEY, 'bogus');
    const { view } = await import('./settings.svelte');
    expect(view.thumbFit).toBe('frame');
  });
});

describe('view.showAliasSource (localStorage-backed)', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
  });

  it('defaults to false when nothing is stored', async () => {
    const { view } = await import('./settings.svelte');
    expect(view.showAliasSource).toBe(false);
  });

  it('writes through to localStorage on set', async () => {
    const { view, SHOW_ALIAS_SOURCE_KEY } = await import('./settings.svelte');
    view.showAliasSource = true;
    expect(localStorage.getItem(SHOW_ALIAS_SOURCE_KEY)).toBe('true');
    view.showAliasSource = false;
    expect(localStorage.getItem(SHOW_ALIAS_SOURCE_KEY)).toBe('false');
  });
});

describe('view.inspectorCollapsed (localStorage-backed)', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
  });

  afterEach(() => {
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 0 });
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    vi.doUnmock('@tauri-apps/api/core');
  });

  it('persists writes', async () => {
    const { view, INSPECTOR_COLLAPSED_KEY } = await import('./settings.svelte');
    view.inspectorCollapsed = true;
    expect(localStorage.getItem(INSPECTOR_COLLAPSED_KEY)).toBe('true');
    view.inspectorCollapsed = false;
    expect(localStorage.getItem(INSPECTOR_COLLAPSED_KEY)).toBe('false');
  });

  it('loads a persisted value over the width heuristic', async () => {
    const { INSPECTOR_COLLAPSED_KEY } = await import('./settings.svelte');
    localStorage.setItem(INSPECTOR_COLLAPSED_KEY, 'false');
    vi.resetModules();
    const { view } = await import('./settings.svelte');
    expect(view.inspectorCollapsed).toBe(false);
  });

  it('defaults to the width heuristic when no key is stored', async () => {
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 900 });
    vi.resetModules();
    let mod = await import('./settings.svelte');
    expect(mod.view.inspectorCollapsed).toBe(true);

    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 1400 });
    vi.resetModules();
    mod = await import('./settings.svelte');
    expect(mod.view.inspectorCollapsed).toBe(false);
  });

  it('loads a desktop persisted value when running under Tauri', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    // Wide window, so the heuristic default is false and only the load can flip it.
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 1400 });
    vi.doMock('@tauri-apps/api/core', () => ({
      invoke: vi.fn(async (command: string) =>
        command === 'load_view_state' ? { inspector_collapsed: true } : undefined,
      ),
    }));
    vi.resetModules();

    const { view } = await import('./settings.svelte');
    expect(view.inspectorCollapsed).toBe(false);

    await vi.waitFor(() => expect(view.inspectorCollapsed).toBe(true));
  });

  it('mirrors the desktop value into localStorage so the browser agrees', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    // Wide window, so the heuristic default is false and only the load can flip it.
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 1400 });
    const invoke = vi.fn(async (command: string) =>
      command === 'load_view_state' ? { inspector_collapsed: true } : undefined,
    );
    vi.doMock('@tauri-apps/api/core', () => ({ invoke }));
    vi.resetModules();

    const { view, INSPECTOR_COLLAPSED_KEY } = await import('./settings.svelte');
    expect(view.inspectorCollapsed).toBe(false);

    await vi.waitFor(() => expect(view.inspectorCollapsed).toBe(true));
    expect(localStorage.getItem(INSPECTOR_COLLAPSED_KEY)).toBe('true');
    // Applying a value we just read must not write it straight back.
    expect(invoke).not.toHaveBeenCalledWith('save_inspector_collapsed', expect.anything());
  });

  it('lets a toggle that lands mid-load win over the persisted value', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    let resolveLoad!: (state: { inspector_collapsed: boolean }) => void;
    const loaded = new Promise<{ inspector_collapsed: boolean }>((r) => {
      resolveLoad = r;
    });
    vi.doMock('@tauri-apps/api/core', () => ({
      invoke: vi.fn(async (command: string) =>
        command === 'load_view_state' ? await loaded : undefined,
      ),
    }));

    const { view, INSPECTOR_COLLAPSED_KEY } = await import('./settings.svelte');
    view.inspectorCollapsed = false;
    resolveLoad({ inspector_collapsed: true });

    // Let the in-flight load resolve and apply before checking it stayed away.
    await loaded;
    await new Promise((r) => setTimeout(r, 0));

    expect(view.inspectorCollapsed).toBe(false);
    expect(localStorage.getItem(INSPECTOR_COLLAPSED_KEY)).toBe('false');
  });

  it('saves inspector writes to the desktop store when running under Tauri', async () => {
    const invoke = vi.fn(async () => ({}));
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    vi.doMock('@tauri-apps/api/core', () => ({ invoke }));

    const { view } = await import('./settings.svelte');
    view.inspectorCollapsed = true;

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('save_inspector_collapsed', { collapsed: true }),
    );
  });
});

describe('view.zoomLevel (desktop-backed)', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
  });

  afterEach(() => {
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    vi.doUnmock('@tauri-apps/api/core');
  });

  it('loads a desktop persisted level when running under Tauri', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    vi.doMock('@tauri-apps/api/core', () => ({
      invoke: vi.fn(async (command: string) =>
        command === 'load_view_state' ? { tile: 4 } : undefined,
      ),
    }));
    vi.resetModules();

    const { view } = await import('./settings.svelte');
    // localStorage is empty (ephemeral port), so it starts at the default.
    expect(view.zoomLevel).toBe(8);

    await vi.waitFor(() => expect(view.zoomLevel).toBe(4));
    expect(localStorage.getItem(ZOOM_LEVEL_KEY)).toBe('4');
  });

  it('converts a legacy pixel value from the desktop store and writes the level back', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 1280 });
    const invoke = vi.fn(async (command: string) =>
      command === 'load_view_state' ? { tile: 1024 } : undefined,
    );
    vi.doMock('@tauri-apps/api/core', () => ({ invoke }));
    vi.resetModules();

    const { view } = await import('./settings.svelte');
    await vi.waitFor(() => expect(view.zoomLevel).toBe(2));
    expect(localStorage.getItem(ZOOM_LEVEL_KEY)).toBe('2');
    // One-time migration: the converted level replaces the px value on disk.
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('save_tile', { tile: 2 }));
  });

  it('saves level writes to the desktop store when running under Tauri', async () => {
    const invoke = vi.fn(async () => ({}));
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    vi.doMock('@tauri-apps/api/core', () => ({ invoke }));

    const { view } = await import('./settings.svelte');
    view.zoomLevel = 6;

    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('save_tile', { tile: 6 }));
  });

  it('lets a drag that lands mid-load win over the persisted level', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    let resolveLoad!: (state: { tile: number }) => void;
    const loaded = new Promise<{ tile: number }>((r) => {
      resolveLoad = r;
    });
    vi.doMock('@tauri-apps/api/core', () => ({
      invoke: vi.fn(async (command: string) =>
        command === 'load_view_state' ? await loaded : undefined,
      ),
    }));

    const { view } = await import('./settings.svelte');
    view.zoomLevel = 12;
    resolveLoad({ tile: 4 });

    await loaded;
    await new Promise((r) => setTimeout(r, 0));

    expect(view.zoomLevel).toBe(12);
  });
});

