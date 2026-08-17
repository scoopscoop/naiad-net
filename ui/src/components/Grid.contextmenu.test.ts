import { describe, expect, it, vi, afterEach, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import Grid from './Grid.svelte';
import { contextMenu } from '../lib/context-menu.svelte';
import { thumbQueue } from '../lib/thumb-queue';
import type { FileDto } from '../lib/types';
import type { MenuItem } from '../lib/menu-items';

// ─── jsdom stubs (mirrors Grid.test.ts) ─────────────────────────────────────

class RO {
  constructor(_cb: ResizeObserverCallback) {}
  observe() {}
  unobserve() {}
  disconnect() {}
}

beforeEach(() => {
  vi.stubGlobal('ResizeObserver', RO);
  vi.stubGlobal('requestAnimationFrame', (fn: FrameRequestCallback) => {
    fn(0);
    return 0;
  });
  vi.stubGlobal('cancelAnimationFrame', () => {});
  vi.spyOn(thumbQueue, 'request').mockImplementation(() => () => {});
  vi.stubGlobal('URL', {
    ...URL,
    createObjectURL: vi.fn(() => 'blob:stub'),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => contextMenu.close());

// ─── Helpers ─────────────────────────────────────────────────────────────────

const file = (h: string): FileDto => ({
  hash: h,
  name: `${h}.png`,
  size: 1,
  path: `C:/img/${h}.png`,
  imported_at: 0,
  created_at: null,
  modified_at: null,
  mime: null,
});

const files = [file('a'), file('b'), file('c')];

function makeScrollParent(width: number, height: number): HTMLElement {
  const el = document.createElement('div');
  Object.defineProperty(el, 'clientWidth', { value: width, configurable: true });
  Object.defineProperty(el, 'clientHeight', { value: height, configurable: true });
  Object.defineProperty(el, 'scrollTop', { value: 0, writable: true, configurable: true });
  document.body.appendChild(el);
  return el;
}

/** Find a non-separator item by id in the open menu. */
function label(id: string): string | undefined {
  return (contextMenu.items.find((e): e is MenuItem => e !== 'separator' && e.id === id))?.label;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe('Grid context menus', () => {
  it('right-click on an unselected tile collapses selection and targets one', async () => {
    const onselection = vi.fn();
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      selected: new Set(['b']),
      onselection,
    });
    const cells = container.querySelectorAll('.cell');
    await fireEvent.contextMenu(cells[0]); // tile 'a', not in selection
    expect(onselection).toHaveBeenCalledWith({ selected: new Set(['a']), anchor: 'a' });
    expect(contextMenu.open).toBe(true);
    expect(label('copy-hash')).toBe('Copy hash');
  });

  it('right-click on a selected tile targets the whole selection', async () => {
    const onselection = vi.fn();
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      selected: new Set(['a', 'b']),
      onselection,
    });
    const cells = container.querySelectorAll('.cell');
    await fireEvent.contextMenu(cells[0]); // tile 'a', in selection
    expect(onselection).not.toHaveBeenCalled(); // selection must not collapse
    expect(label('copy-hash')).toBe('Copy 2 hashes');
    expect(label('pull-tags')).toBe('Pull tags — 2 files');
  });

  it('right-click on empty grid space opens the background menu', async () => {
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, { files, columns: 3, scrollParent: parent });
    const viewport = container.querySelector('.grid-viewport')!;
    await fireEvent.contextMenu(viewport); // target is the viewport, not a cell
    expect(contextMenu.open).toBe(true);
    expect(contextMenu.items.map((e) => (e === 'separator' ? 'separator' : e.id))).toEqual([
      'select-all',
      'refresh',
    ]);
  });

  it('ContextMenu key on a focused cell opens the tile menu', async () => {
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      selected: new Set(['a']),
    });
    const cells = container.querySelectorAll('.cell');
    await fireEvent.keyDown(cells[0], { key: 'ContextMenu' });
    expect(contextMenu.open).toBe(true);
    expect(label('copy-hash')).toBeDefined();
    expect(label('open')).toBeDefined();
  });

  it('Shift+F10 on a focused cell opens the tile menu', async () => {
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      selected: new Set(['a']),
    });
    const cells = container.querySelectorAll('.cell');
    await fireEvent.keyDown(cells[0], { key: 'F10', shiftKey: true });
    expect(contextMenu.open).toBe(true);
    expect(label('copy-hash')).toBeDefined();
    expect(label('pull-tags')).toBeDefined();
  });

  it('copy-path writes newline-joined paths to the clipboard', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    const parent = makeScrollParent(700, 400);
    const { container } = render(Grid, {
      files,
      columns: 3,
      scrollParent: parent,
      selected: new Set(['a', 'b']),
      oncopypaths: (paths: string[]) => navigator.clipboard.writeText(paths.join('\n')),
    });
    await fireEvent.contextMenu(container.querySelector('.cell')!);
    const item = contextMenu.items.find(
      (e): e is MenuItem => e !== 'separator' && e.id === 'copy-path',
    )!;
    item.onselect();
    expect(writeText).toHaveBeenCalledWith('C:/img/a.png\nC:/img/b.png');
  });
});
