import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';

import { tick } from 'svelte';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import App from './App.svelte';
import * as api from './lib/api';
import { DEFAULT_SORT } from './lib/gallery-sort';
import { tabs } from './lib/tabs.svelte';
import { thumbQueue, THUMB_LANES, THUMB_LANES_COVERED } from './lib/thumb-queue';
import { catchup } from './lib/catchup.svelte';
import { view } from './lib/settings.svelte';
import { pullFailure } from './lib/pull-failure.svelte';
import type { FileDto } from './lib/types';

// Reset the pull-failure singleton so notices cannot bleed between tests (#228).
afterEach(() => {
  pullFailure.dismiss();
});

const file = (hash: string, name: string, imported_at = 100): FileDto => ({
  hash,
  name,
  size: 1,
  path: `/${name}`,
  imported_at,
  created_at: imported_at,
  modified_at: imported_at,
  mime: 'image/png',
});

const press = (key: string, mods: Partial<KeyboardEventInit> = {}) => {
  const event = new KeyboardEvent('keydown', {
    key,
    bubbles: true,
    cancelable: true,
    ...mods,
  });
  window.dispatchEvent(event);
  return event;
};

const pressTarget = (target: Element, key: string, mods: Partial<KeyboardEventInit> = {}) => {
  const event = new KeyboardEvent('keydown', {
    key,
    bubbles: true,
    cancelable: true,
    ...mods,
  });
  target.dispatchEvent(event);
  return event;
};

const openTile = (cell: Element) =>
  fireEvent(cell, new MouseEvent('dblclick', { bubbles: true }));

vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({ minimize: vi.fn(), toggleMaximize: vi.fn(), close: vi.fn() }),
}));

vi.mock('./lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return {
    ...actual,
    search: vi.fn(),
    scanStream: vi.fn(),
    listRoots: vi.fn(),
    getGallerySort: vi.fn(),
    setGallerySort: vi.fn(),
    listRejections: vi.fn(),
    // Mocked so NavRail's mount call doesn't produce a spurious error toast in tests.
    listNamespaces: vi.fn(),
  };
});

function resetTabs() {
  for (const tab of [...tabs.list]) {
    if (tab.kind === 'detail') tabs.close(tab.id);
  }
  while (tabs.galleryCount > 1) {
    const gallery = [...tabs.list].reverse().find((t) => t.kind === 'gallery');
    if (gallery) tabs.close(gallery.id);
  }
  const gallery = tabs.list.find((t) => t.kind === 'gallery');
  if (gallery?.kind === 'gallery') {
    gallery.query = '';
    gallery.files = [];
    gallery.sort = { ...DEFAULT_SORT };
    gallery.scrollTop = 0;
    gallery.loading = false;
    gallery.selected = new Set();
    gallery.anchor = null;
    gallery.focused = null;
    tabs.activate(gallery.id);
  }
}

// jsdom doesn't implement matchMedia. This factory returns a minimal stub
// whose 'change' listeners can be fired manually via the returned `fire`
// helper, letting us simulate viewport crossing events.
type MQChangeHandler = (e: { matches: boolean; media: string }) => void;
function makeMatchMediaStub(initiallyNarrow = false) {
  const listeners = new Set<MQChangeHandler>();
  const stub = vi.fn((query: string) => ({
    matches: initiallyNarrow,
    media: query,
    onchange: null,
    addEventListener: (_type: string, fn: MQChangeHandler) => listeners.add(fn),
    removeEventListener: (_type: string, fn: MQChangeHandler) => listeners.delete(fn),
    dispatchEvent: vi.fn(),
  }));
  function fire(matches: boolean) {
    for (const fn of listeners) fn({ matches, media: '(max-width: 700px)' });
  }
  return { stub, fire };
}

describe('App import wiring', () => {
  beforeEach(() => {
    // jsdom does no layout, so every element reports 0x0. Grid virtualizes off
    // the scroll container's real client box, so give it plausible numbers
    // (matches the gallery test viewport used elsewhere) or nothing renders.
    Object.defineProperty(HTMLElement.prototype, 'clientWidth', {
      configurable: true,
      value: 700,
    });
    Object.defineProperty(HTMLElement.prototype, 'clientHeight', {
      configurable: true,
      value: 400,
    });
    vi.mocked(api.search).mockReset();
    vi.mocked(api.scanStream).mockReset();
    vi.mocked(api.listRoots).mockReset();
    vi.mocked(api.getGallerySort).mockReset();
    vi.mocked(api.setGallerySort).mockReset();
    vi.mocked(api.listRoots).mockResolvedValue([]);
    vi.mocked(api.getGallerySort).mockResolvedValue({ ...DEFAULT_SORT });
    vi.mocked(api.setGallerySort).mockResolvedValue(undefined);
    vi.mocked(api.listRejections).mockResolvedValue([]);
    vi.mocked(api.listNamespaces).mockResolvedValue([]);
    // The tabs store is an app-wide singleton; reset to one empty gallery tab.
    resetTabs();
    catchup.set(null);
    // Keep the inspector visually open so tests that exercise its UI start from
    // a known expanded state (jsdom's innerWidth can trigger the narrow-window heuristic).
    view.inspectorCollapsed = false;
    // jsdom has no matchMedia; install a wide-window stub so App.svelte's
    // $effect doesn't skip setup and existing tests stay unaffected.
    const { stub } = makeMatchMediaStub(false);
    vi.stubGlobal('matchMedia', stub);
  });

  it('renders the settings gear in the bar', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    expect(await screen.findByLabelText('settings')).toBeInTheDocument();
  });

  it('re-runs the search and shows a toast after a successful import', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onSummary({ imported: 5, marked_missing: 0, errors: [] });
      return () => {};
    });
    render(App);
    await screen.findByLabelText('settings');
    const initialCalls = vi.mocked(api.search).mock.calls.length;

    await fireEvent.click(screen.getByLabelText('settings'));
    await fireEvent.click(screen.getByRole('tab', { name: 'Library' }));
    await fireEvent.input(screen.getByLabelText('folder path'), {
      target: { value: '/photos' },
    });
    await fireEvent.click(screen.getByText('Scan'));

    expect(await screen.findByText(/indexed 5/)).toBeInTheDocument();
    expect(vi.mocked(api.search).mock.calls.length).toBeGreaterThan(initialCalls);
  });

  it('persists zoom changes through the view store', async () => {
    localStorage.clear();
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    const slider = await screen.findByLabelText('zoom');
    // Slider is inverted (right = zoom in): position 12 → level 2+16-12 = 6.
    await fireEvent.input(slider, { target: { value: '12' } });
    expect(localStorage.getItem('naiad.view.zoom_level')).toBe('6');
  });

  it('loads the persisted gallery sort from the daemon on startup', async () => {
    localStorage.clear();
    vi.mocked(api.getGallerySort).mockResolvedValue({ key: 'name', direction: 'asc' });
    vi.mocked(api.search).mockResolvedValue([]);

    render(App);

    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: /sort: name ascending/i })).toBeInTheDocument(),
    );
    expect(tabs.activeGallery?.sort).toEqual({ key: 'name', direction: 'asc' });
  });

  it('saves gallery sort changes through the daemon', async () => {
    localStorage.clear();
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await screen.findByLabelText('settings');

    await fireEvent.click(screen.getByRole('button', { name: /sort:/i }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /name/i }));

    expect(vi.mocked(api.setGallerySort)).toHaveBeenCalledWith({
      key: 'name',
      direction: 'asc',
    });
  });

  it('re-runs the search when local-only is toggled', async () => {
    localStorage.clear();
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await screen.findByLabelText('settings');
    const initialCalls = vi.mocked(api.search).mock.calls.length;

    await fireEvent.click(screen.getByLabelText('settings'));
    await fireEvent.click(screen.getByLabelText('local tags only'));

    expect(vi.mocked(api.search).mock.calls.length).toBeGreaterThan(initialCalls);
    expect(vi.mocked(api.search).mock.calls.at(-1)).toEqual(['', true]);
  });

  it('disables the search input when a detail tab is active', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    // Open the file's detail tab from the gallery.
    const cell = await screen.findByTitle('a.png');
    await openTile(cell);
    expect((screen.getByLabelText('search') as HTMLInputElement).disabled).toBe(true);
  });

  // jsdom does no layout, so scrollTop is always 0; stub it as a stored value so
  // the content region's save/restore logic is observable.
  function stubScroll(el: HTMLElement, initial = 0): { get value(): number } {
    let pos = initial;
    Object.defineProperty(el, 'scrollTop', {
      configurable: true,
      get: () => pos,
      set: (v: number) => (pos = v),
    });
    return {
      get value() {
        return pos;
      },
    };
  }

  it('leaves the gallery scroll untouched across a detail round-trip (#55)', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    const { container } = render(App);
    const cell = await screen.findByTitle('a.png');
    const el = container.querySelector('[data-scroll]') as HTMLElement;
    const content = stubScroll(el, 640);

    // The detail view lives in its own overlay pane, so opening and closing it
    // must not move the gallery's scroll container at all.
    el.dispatchEvent(new Event('scroll'));
    await openTile(cell);
    await vi.waitFor(() => expect(tabs.activeDetail).not.toBeNull());
    expect(content.value).toBe(640);

    press('Escape');
    await vi.waitFor(() => expect(tabs.activeDetail).toBeNull());
    expect(content.value).toBe(640);
  });

  it('restores the incoming gallery scroll when switching between gallery tabs', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    const { container } = render(App);
    await screen.findByLabelText('settings');
    const el = container.querySelector('[data-scroll]') as HTMLElement;
    const content = stubScroll(el, 640);

    // Record an offset on the first gallery, open a second (starts at 0),
    // then switch back: the first tab's offset must come back.
    el.dispatchEvent(new Event('scroll'));
    const first = tabs.activeId;
    press('n', { ctrlKey: true });
    await vi.waitFor(() => expect(content.value).toBe(0));
    tabs.activate(first);
    await vi.waitFor(() => expect(content.value).toBe(640));
  });

  it('keeps the gallery grid mounted while a detail tab is open (#55)', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    const { container } = render(App);
    const cell = await screen.findByTitle('a.png');

    await openTile(cell);
    await vi.waitFor(() => expect(tabs.activeDetail).not.toBeNull());
    // The grid cell is still in the DOM (hidden behind the detail pane), so
    // closing the detail reveals existing DOM instead of remounting. The
    // inspector heading also carries title="a.png", hence getAllByTitle.
    expect(screen.getAllByTitle('a.png')).toContain(cell);

    press('Escape');
    await vi.waitFor(() => expect(tabs.activeDetail).toBeNull());
    // Prove the detail pane DOM tore down (inspector heading remains, so we
    // scope to .detail-pane rather than querying by heading role globally).
    expect(container.querySelector('.detail-pane')).toBeNull();
    expect(screen.getAllByTitle('a.png')).toContain(cell);
  });

  it('ctrl+a selects every presented file and esc clears the selection', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png'), file('b', 'b.png')]);
    render(App);
    await screen.findByTitle('a.png');
    const g = tabs.activeGallery;
    if (!g) throw new Error('no active gallery');

    const event = press('a', { ctrlKey: true });
    expect(event.defaultPrevented).toBe(true);
    expect([...g.selected].sort()).toEqual(['a', 'b']);

    press('Escape');
    expect(g.selected.size).toBe(0);
    expect(g.anchor).toBeNull();
    expect(tabs.activeGallery).not.toBeNull(); // esc cleared the selection, not the tab
  });

  it('navigates to the next file with ArrowRight from a detail tab', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png'), file('b', 'b.png')]);
    render(App);
    await openTile(await screen.findByTitle('a.png'));
    // Wait for the detail view heading to confirm DetailView is mounted and the
    // keydown listener is wired up before dispatching ArrowRight.
    await screen.findByRole('heading', { name: 'a.png' });
    await fireEvent.keyDown(document.body, { key: 'ArrowRight' });
    expect(await screen.findByRole('heading', { name: 'b.png' })).toBeInTheDocument();
  });

  it('opens detail navigation in the selected sorted order', async () => {
    vi.mocked(api.search).mockResolvedValue([
      file('old', 'old.png', 1),
      file('new', 'new.png', 3),
      file('mid', 'mid.png', 2),
    ]);
    render(App);

    await openTile(await screen.findByTitle('new.png'));
    await screen.findByRole('heading', { name: 'new.png' });
    await fireEvent.keyDown(document.body, { key: 'ArrowRight' });
    expect(await screen.findByRole('heading', { name: 'mid.png' })).toBeInTheDocument();
  });

  it('ctrl+n opens and activates a new gallery tab with an empty search', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    const before = tabs.list.length;
    const event = press('n', { ctrlKey: true });
    expect(event.defaultPrevented).toBe(true);
    expect(tabs.list.length).toBe(before + 1);
    expect(tabs.activeGallery).not.toBeNull();
    expect(vi.mocked(api.search).mock.calls.at(-1)?.[0]).toBe('');
  });

  it('ctrl+w is a no-op on the last gallery tab, closes otherwise', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    press('w', { ctrlKey: true });
    expect(tabs.galleryCount).toBe(1);
    press('n', { ctrlKey: true });
    expect(tabs.galleryCount).toBe(2);
    press('w', { ctrlKey: true });
    expect(tabs.galleryCount).toBe(1);
  });

  it('ctrl+tab cycles and ctrl+1/ctrl+9 jump between tabs', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    const first = tabs.activeId;
    press('n', { ctrlKey: true });
    expect(tabs.activeId).not.toBe(first);
    press('Tab', { ctrlKey: true });
    expect(tabs.activeId).toBe(first);
    press('9', { ctrlKey: true });
    expect(tabs.activeId).not.toBe(first);
    press('1', { ctrlKey: true });
    expect(tabs.activeId).toBe(first);
  });

  it('esc closes a detail tab and returns to a gallery', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    tabs.openDetail([file('a', 'a.png')], 0);
    expect(tabs.activeDetail).not.toBeNull();
    const event = press('Escape');
    expect(event.defaultPrevented).toBe(true);
    expect(tabs.activeDetail).toBeNull();
    expect(tabs.activeGallery).not.toBeNull();
  });

  it('unmatched keys do not call preventDefault', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    const event = press('x');
    expect(event.defaultPrevented).toBe(false);
  });

  it('ctrl+f from a detail tab activates a gallery and focuses search', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    await openTile(await screen.findByTitle('a.png'));
    expect(tabs.activeDetail).not.toBeNull();
    press('f', { ctrlKey: true });
    expect(tabs.activeGallery).not.toBeNull();
    await vi.waitFor(() => expect(screen.getByLabelText('search')).toHaveFocus());
  });

  it('opens detail over the selected subset when clicking a selected tile', async () => {
    vi.mocked(api.search).mockResolvedValue([
      file('a', 'a.png', 3),
      file('b', 'b.png', 2),
      file('c', 'c.png', 1),
    ]);
    render(App);
    const cellA = await screen.findByTitle('a.png');
    const g = tabs.activeGallery;
    if (!g) throw new Error('no active gallery');
    g.selected = new Set(['a', 'c']); // skip b

    await openTile(cellA);
    await screen.findByRole('heading', { name: 'a.png' });
    await fireEvent.keyDown(document.body, { key: 'ArrowRight' });
    // Next in the SELECTED subset is c.png, not b.png.
    expect(await screen.findByRole('heading', { name: 'c.png' })).toBeInTheDocument();
  });

  it('clears the selection and opens the full list when clicking an unselected tile', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png', 3), file('b', 'b.png', 2)]);
    render(App);
    const cellA = await screen.findByTitle('a.png');
    const g = tabs.activeGallery;
    if (!g) throw new Error('no active gallery');
    g.selected = new Set(['b']);

    await openTile(cellA);
    await screen.findByRole('heading', { name: 'a.png' });
    expect(g.selected.size).toBe(0);
    await fireEvent.keyDown(document.body, { key: 'ArrowRight' });
    expect(await screen.findByRole('heading', { name: 'b.png' })).toBeInTheDocument();
  });

  it('enter opens the focused file as a detail tab', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    const cell = await screen.findByTitle('a.png');
    // Plain click with detail=1 focuses (not opens) when inspector is expanded.
    fireEvent(cell, new MouseEvent('click', { bubbles: true, detail: 1 }));
    // Blur the button so the global handler (not native activation) is exercised.
    (document.activeElement as HTMLElement | null)?.blur();
    press('Enter');
    await vi.waitFor(() => expect(tabs.activeDetail).not.toBeNull());
  });

  it('enter does nothing when a detail tab is already open', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    const cell = await screen.findByTitle('a.png');
    // Open a detail tab via double-click.
    await openTile(cell);
    await vi.waitFor(() => expect(tabs.activeDetail).not.toBeNull());
    const len = tabs.list.length;
    press('Enter');
    await tick();
    expect(tabs.list.length).toBe(len);
  });

  it('grid arrow keys do not advance focus while quick-look is open', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png'), file('b', 'b.png')]);
    const { container } = render(App);
    const cell = await screen.findByTitle('a.png');

    // Focus file a via single click.
    fireEvent(cell, new MouseEvent('click', { bubbles: true, detail: 1 }));
    // Blur so Space is handled globally (not as native button activation).
    (document.activeElement as HTMLElement | null)?.blur();
    press(' ');
    await vi.waitFor(() => expect(document.querySelector('.scrim')).not.toBeNull());

    const g = tabs.activeGallery;
    if (!g) throw new Error('no active gallery');
    const focusBefore = g.focused;

    // Arrow right on the grid viewport — covered=true (quickLook open) so focus must not move.
    const viewport = container.querySelector('.grid-viewport');
    if (viewport) fireEvent.keyDown(viewport, { key: 'ArrowRight', bubbles: true });
    await tick();

    expect(g.focused).toBe(focusBefore);
  });

  it('space on a focused grid cell opens quick-look instead of activating the tile', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    const cell = await screen.findByTitle('a.png');

    fireEvent(cell, new MouseEvent('click', { bubbles: true, detail: 1 }));
    (cell as HTMLElement).focus();
    const event = pressTarget(cell, ' ');

    expect(event.defaultPrevented).toBe(true);
    await vi.waitFor(() => expect(document.querySelector('.scrim')).not.toBeNull());
    expect(tabs.activeDetail).toBeNull();
  });

  it('escape closes quick-look before touching selection or tabs', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    const cell = await screen.findByTitle('a.png');
    // Plain click with detail=1 focuses (not opens) when inspector is expanded.
    fireEvent(cell, new MouseEvent('click', { bubbles: true, detail: 1 }));
    // Blur the button so Space is handled globally (not as native button activation).
    (document.activeElement as HTMLElement | null)?.blur();
    press(' ');
    await vi.waitFor(() => expect(document.querySelector('.scrim')).not.toBeNull());
    press('Escape');
    await vi.waitFor(() => expect(document.querySelector('.scrim')).toBeNull());
    // Escape hit the overlay — not a detail tab or selection.
    expect(tabs.activeDetail).toBeNull();
  });

  it('narrows thumbnail lanes while a detail tab is open and restores them on close', async () => {
    const spy = vi.spyOn(thumbQueue, 'setMaxConcurrent');
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await screen.findByLabelText('settings');
    spy.mockClear();

    tabs.openDetail([file('a', 'a.png')], 0);
    await tick();
    expect(spy).toHaveBeenLastCalledWith(THUMB_LANES_COVERED);

    press('Escape');
    await tick();
    expect(tabs.activeDetail).toBeNull();
    expect(spy).toHaveBeenLastCalledWith(THUMB_LANES);
  });

  it('narrows thumbnail lanes while quick-look is open and restores them on close', async () => {
    const spy = vi.spyOn(thumbQueue, 'setMaxConcurrent');
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    const cell = await screen.findByTitle('a.png');
    fireEvent(cell, new MouseEvent('click', { bubbles: true, detail: 1 }));
    (document.activeElement as HTMLElement | null)?.blur();
    spy.mockClear();

    press(' ');
    await vi.waitFor(() => expect(document.querySelector('.scrim')).not.toBeNull());
    expect(spy).toHaveBeenLastCalledWith(THUMB_LANES_COVERED);

    press('Escape');
    await vi.waitFor(() => expect(document.querySelector('.scrim')).toBeNull());
    expect(spy).toHaveBeenLastCalledWith(THUMB_LANES);
  });

  it('restores grid focus after closing a detail tab opened from the focused grid', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    const cell = await screen.findByTitle('a.png') as HTMLElement;

    fireEvent(cell, new MouseEvent('click', { bubbles: true, detail: 1 }));
    cell.focus();
    await openTile(cell);
    await vi.waitFor(() => expect(tabs.activeDetail).not.toBeNull());

    document.body.tabIndex = -1;
    document.body.focus();
    expect(document.body).toHaveFocus();
    press('Escape');

    await vi.waitFor(() => expect(tabs.activeDetail).toBeNull());
    await vi.waitFor(() => expect(cell).toHaveFocus());
  });

  it('re-running a search clears the selection', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    await screen.findByTitle('a.png');
    const g = tabs.activeGallery;
    if (!g) throw new Error('no active gallery');
    g.selected = new Set(['a']);

    // The local-only toggle re-runs every gallery's search (onrescope).
    await fireEvent.click(screen.getByLabelText('settings'));
    await fireEvent.click(screen.getByLabelText('local tags only'));
    await vi.waitFor(() => expect(g.selected.size).toBe(0));
  });

  it('shows the selection count in the bar', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png'), file('b', 'b.png')]);
    render(App);
    await screen.findByTitle('a.png');
    const g = tabs.activeGallery;
    if (!g) throw new Error('no active gallery');
    expect(screen.getByText(/files/)).toBeInTheDocument();

    g.selected = new Set(['a']);
    expect(await screen.findByText(/selected/)).toBeInTheDocument();
  });

  // Narrow-window auto-collapse (audit F9): below 700px the inspector must show
  // as the 28px strip even when the user's preference is expanded; the preference
  // returns when the viewport widens again.
  it('auto-collapses inspector below 700px and restores preference when wide', async () => {
    const { stub, fire } = makeMatchMediaStub(true); // start narrow
    vi.stubGlobal('matchMedia', stub);
    view.inspectorCollapsed = false;

    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await screen.findByLabelText('settings');

    // Narrow window: strip is visible even though user preference is expanded
    await vi.waitFor(() =>
      expect(screen.getByLabelText('window too narrow to expand inspector')).toBeInTheDocument(),
    );
    expect(screen.queryByLabelText('collapse inspector')).toBeNull();
    // Preference not mutated
    expect(view.inspectorCollapsed).toBe(false);

    // Window widens: preference (expanded) returns
    fire(false);
    await vi.waitFor(() =>
      expect(screen.getByLabelText('collapse inspector')).toBeInTheDocument(),
    );
    expect(screen.queryByLabelText('expand inspector')).toBeNull();
  });

  // Explorer parity (#110): a plain single-click only focuses, never opens a tab —
  // regardless of window width or inspector state.
  it('single-click never opens a detail tab, even at narrow width', async () => {
    const { stub } = makeMatchMediaStub(true); // narrow window
    vi.stubGlobal('matchMedia', stub);
    view.inspectorCollapsed = false; // user preference is expanded, but window is narrow

    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    render(App);
    const cell = await screen.findByTitle('a.png');

    // Single click (detail=1) — should focus only, never open a tab
    fireEvent(cell, new MouseEvent('click', { bubbles: true, detail: 1 }));
    // Negative assertion: there is nothing to poll for, so we wait a beat to let
    // any accidental async side-effects settle before asserting no tab was opened.
    await new Promise((r) => setTimeout(r, 50));
    expect(tabs.activeDetail).toBeNull();
  });

  // §1 — toasts are rendered inside .toasts overlay, not between titlebar and .body (#52)
  it('notice renders inside the .toasts overlay container (#52)', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onSummary({ imported: 3, marked_missing: 0, errors: [] });
      return () => {};
    });
    const { container } = render(App);
    await screen.findByLabelText('settings');

    await fireEvent.click(screen.getByLabelText('settings'));
    await fireEvent.click(screen.getByRole('tab', { name: 'Library' }));
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));

    // findByText finds the notice; then confirm it is inside .toasts (not in .body flow)
    const noticeEl = await screen.findByText(/indexed 3/);
    const toasts = container.querySelector('.toasts');
    expect(toasts).not.toBeNull();
    expect(toasts!.contains(noticeEl)).toBe(true);
  });

  it('error renders inside the .toasts overlay container (#52)', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    const { container } = render(App);
    await screen.findByLabelText('settings');

    // Trigger an error via a failed search on the active gallery tab
    const g = tabs.activeGallery;
    expect(g).not.toBeNull();
    vi.mocked(api.search).mockRejectedValueOnce(new Error('network error'));
    const input = screen.getByLabelText('search');
    await fireEvent.input(input, { target: { value: 'boom' } });
    await fireEvent.submit(input.closest('form')!);

    const alertEl = await screen.findByRole('alert');
    const toasts = container.querySelector('.toasts');
    expect(toasts).not.toBeNull();
    expect(toasts!.contains(alertEl)).toBe(true);
  });

  it('error dismiss button clears the error (#52)', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await screen.findByLabelText('settings');

    vi.mocked(api.search).mockRejectedValueOnce(new Error('boom'));
    const input = screen.getByLabelText('search');
    await fireEvent.input(input, { target: { value: 'x' } });
    await fireEvent.submit(input.closest('form')!);

    await screen.findByRole('alert');
    await fireEvent.click(screen.getByLabelText('dismiss'));
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('gallery scroll is unchanged when a notice appears (#52)', async () => {
    vi.mocked(api.search).mockResolvedValue([file('a', 'a.png')]);
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onSummary({ imported: 5, marked_missing: 0, errors: [] });
      return () => {};
    });
    const { container } = render(App);
    await screen.findByTitle('a.png');
    const el = container.querySelector('[data-scroll]') as HTMLElement;
    const content = stubScroll(el, 640);
    el.dispatchEvent(new Event('scroll'));

    await fireEvent.click(screen.getByLabelText('settings'));
    await fireEvent.click(screen.getByRole('tab', { name: 'Library' }));
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));

    await screen.findByText(/indexed 5/);
    // Toast is a fixed overlay — the grid scroll container must not have moved (#52)
    expect(content.value).toBe(640);
  });

  // §2 — empty-state panels (#52)
  it('shows fresh-install panel when search is empty and no roots (#52)', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    vi.mocked(api.listRoots).mockResolvedValue([]);
    render(App);
    expect(await screen.findByText('no folders indexed yet')).toBeInTheDocument();
    expect(await screen.findByText('add a folder in settings')).toBeInTheDocument();
  });

  it('shows empty-library panel when search is empty and roots are present (#52)', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    vi.mocked(api.listRoots).mockResolvedValue(['/photos']);
    render(App);
    expect(await screen.findByText('no files yet')).toBeInTheDocument();
  });

  it('shows no-results panel when query is non-empty (#52)', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    vi.mocked(api.listRoots).mockResolvedValue(['/photos']);
    render(App);
    await screen.findByLabelText('settings'); // wait for initial render

    const input = screen.getByLabelText('search');
    await fireEvent.input(input, { target: { value: 'sunset beach' } });
    await fireEvent.submit(input.closest('form')!);

    expect(await screen.findByText(/no matches for/)).toBeInTheDocument();
    expect(await screen.findByText(/"sunset beach"/)).toBeInTheDocument();
  });

  it('empty-state not shown while gallery is loading (#52)', async () => {
    // Search never resolves → loading becomes true after the 150ms delay
    vi.mocked(api.search).mockImplementation(() => new Promise<FileDto[]>(() => {}));
    vi.mocked(api.listRoots).mockResolvedValue([]);
    render(App);
    // Wait past createPending's DELAY_MS=150 so loading is true
    await new Promise((r) => setTimeout(r, 220));
    expect(screen.queryByText('no folders indexed yet')).toBeNull();
    expect(screen.queryByText('no files yet')).toBeNull();
    expect(screen.queryByText(/no matches for/)).toBeNull();
  });

  it('streams catch-up scan results into an empty gallery while scanning', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await waitFor(() => expect(vi.mocked(api.search).mock.calls.length).toBeGreaterThanOrEqual(1));
    const before = vi.mocked(api.search).mock.calls.length;

    catchup.set({
      running: true,
      imported: 1000,
      errors: 0,
      roots_total: 1,
      roots_done: 0,
      current: 'D:/img/newstuff',
      complete: false,
    });
    await tick();
    expect(vi.mocked(api.search).mock.calls.length).toBeGreaterThan(before);
  });

  it('does a final refresh when the scan completes', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await waitFor(() => expect(vi.mocked(api.search).mock.calls.length).toBeGreaterThanOrEqual(1));

    catchup.set({
      running: true,
      imported: 100,
      errors: 0,
      roots_total: 1,
      roots_done: 0,
      current: 'D:/a',
      complete: false,
    });
    await tick();
    const mid = vi.mocked(api.search).mock.calls.length;

    catchup.set({
      running: false,
      imported: 200,
      errors: 0,
      roots_total: 1,
      roots_done: 1,
      current: null,
      complete: true,
    });
    await tick();
    expect(vi.mocked(api.search).mock.calls.length).toBeGreaterThan(mid);
  });

  // #228: pull failure modal — App hosts the singleton PullFailureModal.
  it('raises alertdialog when pullFailure.raise() is called (#228)', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    const { container } = render(App);
    await screen.findByLabelText('settings');

    pullFailure.raise({ kind: 'repo', repos: ['ptr'], message: 'ptr: connection refused' });
    await tick();

    const dialog = screen.getByRole('alertdialog');
    expect(dialog).toHaveTextContent('ptr');
    expect(dialog).toHaveTextContent('ptr: connection refused');
    // No App error toast — the failure is not double-reported (#228 decision 3).
    expect(container.querySelector('.toasts .toast.error')).toBeNull();
  });

  it('Dismiss from the modal clears pullFailure.current (#228)', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await screen.findByLabelText('settings');

    pullFailure.raise({ kind: 'repo', repos: ['ptr'], message: 'ptr: connection refused' });
    await tick();
    screen.getByRole('alertdialog');

    await fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    await tick();

    expect(screen.queryByRole('alertdialog')).toBeNull();
    expect(pullFailure.current).toBeNull();
  });

  // F4: global hotkeys are inert while the pull-failure modal is open (#228).
  it('global hotkey (Ctrl+F) does nothing while pull-failure modal is open', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await screen.findByLabelText('settings');

    pullFailure.raise({ kind: 'repo', repos: ['ptr'], message: 'ptr: timeout' });
    await tick();
    screen.getByRole('alertdialog');

    // Ctrl+F would normally focus the search input; with the modal open it must not.
    await fireEvent.keyDown(window, { key: 'f', ctrlKey: true });
    await tick();
    // The search input must not have focus (modal still owns it).
    const searchInput = document.querySelector('input[type="search"], input[placeholder]') as HTMLElement | null;
    if (searchInput) {
      expect(document.activeElement).not.toBe(searchInput);
    }
    // Modal remains open.
    screen.getByRole('alertdialog');
  });

  it('does not refresh a populated gallery mid-scan (no reshuffle)', async () => {
    const many = Array.from({ length: 60 }, (_, i) => file(`h${i}`, `f${i}.png`));
    vi.mocked(api.search).mockResolvedValue(many);
    render(App);
    await waitFor(() => {
      const g = tabs.list.find((t) => t.kind === 'gallery');
      expect(g?.kind === 'gallery' && g.files.length).toBe(60);
    });
    const before = vi.mocked(api.search).mock.calls.length;

    catchup.set({
      running: true,
      imported: 1000,
      errors: 0,
      roots_total: 1,
      roots_done: 0,
      current: 'D:/a',
      complete: false,
    });
    await tick();
    expect(vi.mocked(api.search).mock.calls.length).toBe(before);
  });

  it('ignores a null or empty scan status', async () => {
    vi.mocked(api.search).mockResolvedValue([]);
    render(App);
    await waitFor(() => expect(vi.mocked(api.search).mock.calls.length).toBeGreaterThanOrEqual(1));
    const before = vi.mocked(api.search).mock.calls.length;

    catchup.set(null);
    await tick();
    catchup.set({
      running: true,
      imported: 0,
      errors: 0,
      roots_total: 1,
      roots_done: 0,
      current: 'D:/a',
      complete: false,
    });
    await tick();
    expect(vi.mocked(api.search).mock.calls.length).toBe(before);
  });
});

describe('search loading state', () => {
  beforeEach(() => {
    Object.defineProperty(HTMLElement.prototype, 'clientWidth', { configurable: true, value: 700 });
    Object.defineProperty(HTMLElement.prototype, 'clientHeight', { configurable: true, value: 400 });
    vi.mocked(api.search).mockReset();
    vi.mocked(api.scanStream).mockReset();
    vi.mocked(api.listRoots).mockReset();
    vi.mocked(api.getGallerySort).mockReset();
    vi.mocked(api.setGallerySort).mockReset();
    vi.mocked(api.listRoots).mockResolvedValue([]);
    vi.mocked(api.getGallerySort).mockResolvedValue({ ...DEFAULT_SORT });
    vi.mocked(api.setGallerySort).mockResolvedValue(undefined);
    vi.mocked(api.listNamespaces).mockResolvedValue([]);
    // Pre-consume the initial refreshGalleries() call so the test's
    // mockImplementationOnce slots apply to the form submits only.
    vi.mocked(api.search).mockImplementationOnce(() => Promise.resolve([]));
    resetTabs();
    view.inspectorCollapsed = false;
    const { stub } = makeMatchMediaStub(false);
    vi.stubGlobal('matchMedia', stub);
  });

  it('a stale response never overwrites a fresher one', async () => {
    // Two searches on the same tab. The first resolves last.
    let resolveSlow!: (v: FileDto[]) => void;
    let resolveFast!: (v: FileDto[]) => void;
    vi.mocked(api.search)
      .mockImplementationOnce(() => new Promise((r) => (resolveSlow = r)))
      .mockImplementationOnce(() => new Promise((r) => (resolveFast = r)));

    render(App);
    await tick();

    const input = screen.getByLabelText('search');
    await fireEvent.input(input, { target: { value: 'slow' } });
    await fireEvent.submit(input.closest('form')!);
    await fireEvent.input(input, { target: { value: 'fast' } });
    await fireEvent.submit(input.closest('form')!);

    resolveFast([file('bbb', 'fast.png')]);
    await tick();
    resolveSlow([file('aaa', 'slow.png')]);
    await tick();

    const gallery = tabs.list.find((t) => t.kind === 'gallery')!;
    expect(gallery.kind === 'gallery' && gallery.files.map((f) => f.hash)).toEqual(['bbb']);
  });

  it('unmounting with a search in flight leaves the tab unloaded', async () => {
    vi.mocked(api.search).mockReset();
    vi.mocked(api.search).mockImplementation(() => new Promise<FileDto[]>(() => {}));

    const { unmount } = render(App);
    await tick();
    unmount();

    // Longer than createPending's 150ms delay: a surviving timer would flip the
    // module-level tab's `loading` on with no end() left to clear it.
    await new Promise((r) => setTimeout(r, 220));

    const gallery = tabs.list.find((t) => t.kind === 'gallery')!;
    expect(gallery.kind === 'gallery' && gallery.loading).toBe(false);
  });

  it('a stale rejection does not surface an error', async () => {
    let rejectSlow!: (e: Error) => void;
    vi.mocked(api.search)
      .mockImplementationOnce(() => new Promise((_, rej) => (rejectSlow = rej)))
      .mockImplementationOnce(() => Promise.resolve([file('bbb', 'fast.png')]));

    render(App);
    await tick();

    const input = screen.getByLabelText('search');
    await fireEvent.input(input, { target: { value: 'slow' } });
    await fireEvent.submit(input.closest('form')!);
    await fireEvent.input(input, { target: { value: 'fast' } });
    await fireEvent.submit(input.closest('form')!);
    await tick();

    rejectSlow(new Error('boom'));
    await tick();

    expect(screen.queryByText(/boom/)).toBeNull();
  });
});
