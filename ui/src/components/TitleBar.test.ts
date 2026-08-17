import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, within } from '@testing-library/svelte';
import TitleBar from './TitleBar.svelte';
import { tabs } from '../lib/tabs.svelte';
import * as api from '../lib/api';
import { DEFAULT_SORT } from '../lib/gallery-sort';

const win = { minimize: vi.fn(), toggleMaximize: vi.fn(), close: vi.fn() };
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => win }));

const file = (hash: string, name: string) => ({
  hash,
  name,
  size: 1,
  path: `/${name}`,
  imported_at: 100,
  created_at: 80,
  modified_at: 90,
  mime: 'image/png',
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
    tabs.activate(gallery.id);
  }
}

async function makeStripOverflow(container: HTMLElement) {
  const strip = container.querySelector('.strip') as HTMLDivElement;
  const tabNav = container.querySelector('.tab-nav') as HTMLDivElement;
  Object.defineProperties(strip, {
    scrollWidth: { configurable: true, value: 600 },
    clientWidth: { configurable: true, value: 200 },
    scrollLeft: { configurable: true, writable: true, value: 0 },
  });
  Object.defineProperty(tabNav, 'clientWidth', { configurable: true, value: 200 });
  await fireEvent.scroll(strip);
  return strip;
}

describe('TitleBar', () => {
  beforeEach(() => {
    // Reset the shared singleton between tests.
    resetTabs();
    vi.spyOn(api, 'health').mockResolvedValue({
      ok: true,
      watch: null,
      scan: null,
      warmup: null,
    });
    win.minimize.mockReset();
    win.toggleMaximize.mockReset();
    win.close.mockReset();
  });

  it('always shows the gallery tab', () => {
    render(TitleBar);
    expect(screen.getByText('all media')).toBeInTheDocument();
  });

  it('renders a tab per open detail file', () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    tabs.openDetail([file('b', 'b.png')], 0);
    render(TitleBar);
    expect(screen.getByText('a.png')).toBeInTheDocument();
    expect(screen.getByText('b.png')).toBeInTheDocument();
  });

  it('clicking the gallery tab activates it', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    render(TitleBar);
    expect(tabs.activeGallery).toBeNull();
    await fireEvent.click(screen.getByText('all media'));
    expect(tabs.activeGallery).not.toBeNull();
  });

  it('clicking a detail tab close button removes it', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    render(TitleBar);
    await fireEvent.click(screen.getByLabelText('close a.png'));
    expect(tabs.list.filter((t) => t.kind === 'detail')).toHaveLength(0);
  });

  it('renders and closes extra gallery tabs', async () => {
    const g = tabs.openGallery();
    g.query = 'character:samus';
    render(TitleBar);
    expect(screen.getByText('character:samus')).toBeInTheDocument();
    await fireEvent.click(screen.getByLabelText('close character:samus'));
    expect(tabs.galleryCount).toBe(1);
  });

  it('shows overflow controls only when tabs overflow and updates their edge state', async () => {
    const { container } = render(TitleBar);
    expect(screen.queryByLabelText('scroll tabs left')).not.toBeInTheDocument();

    const strip = await makeStripOverflow(container);
    expect(screen.getByLabelText('scroll tabs left')).toBeDisabled();
    expect(screen.getByLabelText('scroll tabs right')).not.toBeDisabled();
    expect(screen.getByLabelText('show all tabs')).toBeInTheDocument();

    strip.scrollLeft = 400;
    await fireEvent.scroll(strip);
    expect(screen.getByLabelText('scroll tabs left')).not.toBeDisabled();
    expect(screen.getByLabelText('scroll tabs right')).toBeDisabled();
  });

  it('hides overflow controls when tabs fit without the controls themselves', async () => {
    const { container } = render(TitleBar);
    const strip = await makeStripOverflow(container);
    const tabNav = container.querySelector('.tab-nav') as HTMLDivElement;

    Object.defineProperty(tabNav, 'clientWidth', { configurable: true, value: 600 });
    await fireEvent.scroll(strip);

    expect(screen.queryByLabelText('scroll tabs left')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('show all tabs')).not.toBeInTheDocument();
  });

  it('maps the vertical wheel to horizontal tab scrolling', async () => {
    const { container } = render(TitleBar);
    const strip = await makeStripOverflow(container);
    const wheel = new WheelEvent('wheel', { deltaY: 40, bubbles: true, cancelable: true });

    strip.dispatchEvent(wheel);

    expect(wheel.defaultPrevented).toBe(true);
    expect(strip.scrollLeft).toBe(40);
  });

  it('lists every tab in the overflow menu and activates the selected tab', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    expect(screen.getByRole('menu', { name: 'open tabs' })).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: 'a.png' }).querySelector('img.chip')).toHaveAttribute(
      'src',
      api.thumbUrl('a'),
    );
    await fireEvent.click(screen.getByRole('menuitem', { name: 'all media' }));

    expect(tabs.activeGallery).not.toBeNull();
    expect(screen.queryByRole('menu', { name: 'open tabs' })).not.toBeInTheDocument();
  });

  it('closes the overflow menu when the tabs stop overflowing', async () => {
    const { container } = render(TitleBar);
    const strip = await makeStripOverflow(container);
    const tabNav = container.querySelector('.tab-nav') as HTMLDivElement;

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    expect(screen.getByRole('menu', { name: 'open tabs' })).toBeInTheDocument();

    Object.defineProperty(tabNav, 'clientWidth', { configurable: true, value: 600 });
    await fireEvent.scroll(strip);
    expect(screen.queryByLabelText('show all tabs')).not.toBeInTheDocument();

    Object.defineProperty(tabNav, 'clientWidth', { configurable: true, value: 200 });
    await fireEvent.scroll(strip);
    expect(screen.getByLabelText('show all tabs')).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByRole('menu', { name: 'open tabs' })).not.toBeInTheDocument();
  });

  it('gives the overflow menu a single tab stop with roving focus', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    tabs.openDetail([file('b', 'b.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    const trigger = screen.getByLabelText('show all tabs');
    await fireEvent.click(trigger);
    expect(trigger).toHaveAttribute('aria-controls', 'tab-overflow-menu');
    expect(screen.getByRole('menu', { name: 'open tabs' })).toHaveAttribute('id', 'tab-overflow-menu');

    const menu = screen.getByRole('menu', { name: 'open tabs' });
    // Every owned element of the menu carries role=menuitem, close buttons
    // included, but only the rows take part in the roving sequence.
    const items = () => Array.from(menu.querySelectorAll('[role="menuitem"]'));
    const rows = () => Array.from(menu.querySelectorAll('.menu-open'));
    expect(items()).toHaveLength(5);
    expect(rows()).toHaveLength(3);
    expect(menu.querySelectorAll('.menu-x')).toHaveLength(2);
    const stops = () => items().filter((el) => el.getAttribute('tabindex') === '0');
    expect(stops()).toHaveLength(1);
    expect(stops()[0]).toBe(rows()[0]);

    // One press per row, not one press per focusable control.
    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    expect(stops()[0]).toBe(rows()[1]);
    expect(rows()[1]).toHaveFocus();
    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    expect(rows()[2]).toHaveFocus();

    await fireEvent.keyDown(menu, { key: 'End' });
    expect(rows()[2]).toHaveFocus();
    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    expect(rows()[0]).toHaveFocus();
    await fireEvent.keyDown(menu, { key: 'ArrowUp' });
    expect(rows()[2]).toHaveFocus();
    expect(stops()).toHaveLength(1);
  });

  it('keeps exactly one tab stop while focus rests on a row close button', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    const menu = screen.getByRole('menu', { name: 'open tabs' });
    const stops = () =>
      Array.from(menu.querySelectorAll('[role="menuitem"]')).filter(
        (el) => el.getAttribute('tabindex') === '0',
      );

    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    await fireEvent.keyDown(menu, { key: 'ArrowRight' });

    const closer = screen.getByLabelText('close a.png from menu');
    await vi.waitFor(() => expect(closer).toHaveFocus());
    expect(stops()).toEqual([closer]);
  });

  it('moves focus from the expanded trigger into the menu with ArrowDown and ArrowUp', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    // ArrowDown on the collapsed trigger opens the menu on the first row.
    const trigger = screen.getByLabelText('show all tabs');
    await fireEvent.keyDown(trigger, { key: 'ArrowDown' });
    await vi.waitFor(() => expect(screen.getByRole('menuitem', { name: 'all media' })).toHaveFocus());

    // Re-focusing the trigger while expanded keeps the arrows live.
    trigger.focus();
    await fireEvent.keyDown(trigger, { key: 'ArrowUp' });
    await vi.waitFor(() => expect(screen.getByRole('menuitem', { name: 'a.png' })).toHaveFocus());

    trigger.focus();
    await fireEvent.keyDown(trigger, { key: 'ArrowDown' });
    await vi.waitFor(() => expect(screen.getByRole('menuitem', { name: 'all media' })).toHaveFocus());
    expect(screen.getByRole('menu', { name: 'open tabs' })).toBeInTheDocument();
  });

  it('reaches a row close button with ArrowRight and returns with ArrowLeft', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    const menu = screen.getByRole('menu', { name: 'open tabs' });

    // The single gallery tab is not closable, so ArrowRight is a no-op there.
    await fireEvent.keyDown(menu, { key: 'ArrowRight' });
    expect(screen.getByRole('menuitem', { name: 'all media' })).not.toHaveFocus();

    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    expect(screen.getByRole('menuitem', { name: 'a.png' })).toHaveFocus();

    await fireEvent.keyDown(menu, { key: 'ArrowRight' });
    await vi.waitFor(() => expect(screen.getByLabelText('close a.png from menu')).toHaveFocus());
    await fireEvent.keyDown(menu, { key: 'ArrowLeft' });
    await vi.waitFor(() => expect(screen.getByRole('menuitem', { name: 'a.png' })).toHaveFocus());
  });

  it('closes the focused row with Delete', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    tabs.openDetail([file('b', 'b.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    const menu = screen.getByRole('menu', { name: 'open tabs' });
    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    await fireEvent.keyDown(menu, { key: 'Delete' });

    await vi.waitFor(() => expect(screen.queryByText('a.png')).not.toBeInTheDocument());
    expect(tabs.list.filter((t) => t.kind === 'detail')).toHaveLength(1);
  });

  it('keeps the overflow menu open while a scroll arrow is used', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    const strip = await makeStripOverflow(container);
    (strip as HTMLDivElement & { scrollBy: unknown }).scrollBy = vi.fn();

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    const arrow = screen.getByLabelText('scroll tabs right');
    await fireEvent.pointerDown(arrow);
    await fireEvent.click(arrow);

    expect(screen.getByRole('menu', { name: 'open tabs' })).toBeInTheDocument();
  });

  it('dismisses the overflow menu when a strip tab is activated', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    const strip = container.querySelector('.strip') as HTMLElement;
    await fireEvent.click(within(strip).getByText('a.png'));

    expect(screen.queryByRole('menu', { name: 'open tabs' })).not.toBeInTheDocument();
    expect(document.activeElement).not.toBe(document.body);
  });

  it('dismisses the overflow menu when a strip tab is closed', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    tabs.openDetail([file('b', 'b.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    const strip = container.querySelector('.strip') as HTMLElement;
    await fireEvent.click(within(strip).getByLabelText('close a.png'));

    await vi.waitFor(() =>
      expect(screen.queryByRole('menu', { name: 'open tabs' })).not.toBeInTheDocument(),
    );
    await vi.waitFor(() => expect(document.activeElement).not.toBe(document.body));
  });

  it('keeps exactly one menu tab stop after the tab list shrinks', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    tabs.openDetail([file('b', 'b.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    const menu = screen.getByRole('menu', { name: 'open tabs' });
    await fireEvent.keyDown(menu, { key: 'End' });
    await vi.waitFor(() => expect(screen.getByRole('menuitem', { name: 'b.png' })).toHaveFocus());
    await fireEvent.keyDown(menu, { key: 'ArrowRight' });
    await vi.waitFor(() =>
      expect(screen.getByLabelText('close b.png from menu')).toHaveFocus(),
    );

    for (const tab of tabs.list.filter((t) => t.kind === 'detail')) tabs.close(tab.id);

    await vi.waitFor(() => {
      const stops = Array.from(menu.querySelectorAll('[role="menuitem"]')).filter(
        (el) => el.getAttribute('tabindex') === '0',
      );
      expect(stops).toHaveLength(1);
    });
  });

  it('dismisses the overflow menu when focus leaves the tab navigation', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    const menu = screen.getByRole('menu', { name: 'open tabs' });
    await fireEvent.focusOut(menu, { relatedTarget: screen.getByLabelText('minimize') });

    expect(screen.queryByRole('menu', { name: 'open tabs' })).not.toBeInTheDocument();
  });

  it('closes a tab straight from the overflow menu without activating it', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    tabs.openDetail([file('b', 'b.png')], 0);
    const galleryId = tabs.list[0].id;
    tabs.activate(galleryId);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    await fireEvent.click(screen.getByLabelText('close a.png from menu'));

    expect(tabs.list.filter((t) => t.kind === 'detail').map((t) => t.id)).toHaveLength(1);
    expect(screen.queryByLabelText('close a.png from menu')).not.toBeInTheDocument();
    // Activation must not have followed the close.
    expect(tabs.activeId).toBe(galleryId);
    // Menu stays open, focus moves to the row that took the closed tab's place.
    expect(screen.getByRole('menu', { name: 'open tabs' })).toBeInTheDocument();
    await vi.waitFor(() => expect(screen.getByRole('menuitem', { name: 'b.png' })).toHaveFocus());
  });

  it('dismisses the overflow menu when closing a row stops the overflow', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);
    const tabNav = container.querySelector('.tab-nav') as HTMLDivElement;

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    // The remaining tabs will fit once this one is gone.
    Object.defineProperty(tabNav, 'clientWidth', { configurable: true, value: 600 });
    await fireEvent.click(screen.getByLabelText('close a.png from menu'));

    await vi.waitFor(() => {
      expect(screen.queryByRole('menu', { name: 'open tabs' })).not.toBeInTheDocument();
      expect(screen.queryByLabelText('show all tabs')).not.toBeInTheDocument();
    });
    // The trigger unmounted with the overflow: focus falls back to the strip.
    await vi.waitFor(() => expect(document.activeElement).not.toBe(document.body));
    expect(container.querySelector('.strip')).toContainElement(
      document.activeElement as HTMLElement,
    );
  });

  it('keeps focus in the strip when a resize collapses the overflow menu', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    const strip = await makeStripOverflow(container);
    const tabNav = container.querySelector('.tab-nav') as HTMLDivElement;

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    screen.getByRole('menuitem', { name: 'a.png' }).focus();

    Object.defineProperty(tabNav, 'clientWidth', { configurable: true, value: 600 });
    await fireEvent.scroll(strip);

    await vi.waitFor(() => expect(document.activeElement).not.toBe(document.body));
    expect(strip).toContainElement(document.activeElement as HTMLElement);
  });

  it('single gallery tab offers no close row in the overflow menu', async () => {
    const { container } = render(TitleBar);
    await makeStripOverflow(container);
    await fireEvent.click(screen.getByLabelText('show all tabs'));
    expect(screen.queryByLabelText('close all media from menu')).not.toBeInTheDocument();
  });

  it('dismisses the overflow menu and refocuses the trigger on Escape', async () => {
    tabs.openDetail([file('a', 'a.png')], 0);
    const { container } = render(TitleBar);
    await makeStripOverflow(container);

    await fireEvent.click(screen.getByLabelText('show all tabs'));
    await fireEvent.keyDown(screen.getByRole('menu', { name: 'open tabs' }), { key: 'Escape' });
    expect(screen.queryByRole('menu', { name: 'open tabs' })).not.toBeInTheDocument();
    expect(screen.getByLabelText('show all tabs')).toHaveFocus();
  });

  it('scrolls a newly active tab into view', async () => {
    const scrollIntoView = vi.fn();
    Object.defineProperty(HTMLElement.prototype, 'scrollIntoView', {
      configurable: true,
      value: scrollIntoView,
    });
    try {
      render(TitleBar);
      tabs.openDetail([file('a', 'a.png')], 0);
      await vi.waitFor(() => expect(scrollIntoView).toHaveBeenCalled());
    } finally {
      delete (HTMLElement.prototype as { scrollIntoView?: unknown }).scrollIntoView;
    }
  });

  it('wires the window control buttons', async () => {
    render(TitleBar);
    await fireEvent.click(screen.getByLabelText('minimize'));
    await fireEvent.click(screen.getByLabelText('maximize'));
    await fireEvent.click(screen.getByLabelText('close window'));
    expect(win.minimize).toHaveBeenCalled();
    expect(win.toggleMaximize).toHaveBeenCalled();
    expect(win.close).toHaveBeenCalled();
  });
});
