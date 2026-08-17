import { describe, expect, it, vi, afterEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import ContextMenu from './ContextMenu.svelte';
import { contextMenu } from '../lib/context-menu.svelte';
import type { MenuList } from '../lib/menu-items';

afterEach(() => contextMenu.close());

function openMenu(list: MenuList, invoker?: HTMLElement) {
  contextMenu.openAt({ x: 20, y: 20 }, list, invoker);
}

describe('ContextMenu.svelte', () => {
  it('renders items as menuitems and separators as separators', async () => {
    render(ContextMenu);
    openMenu([
      { id: 'a', label: 'Alpha', onselect: () => {} },
      'separator',
      { id: 'b', label: 'Bravo', onselect: () => {} },
    ]);
    expect(await screen.findByRole('menu')).toBeInTheDocument();
    expect(screen.getAllByRole('menuitem')).toHaveLength(2);
    expect(screen.getByRole('separator')).toBeInTheDocument();
  });

  it('Enter activates the focused item and closes', async () => {
    const onselect = vi.fn();
    render(ContextMenu);
    openMenu([{ id: 'a', label: 'Alpha', onselect }]);
    const menu = await screen.findByRole('menu');
    await fireEvent.keyDown(menu, { key: 'Enter' });
    expect(onselect).toHaveBeenCalledOnce();
    expect(contextMenu.open).toBe(false);
  });

  it('ArrowDown skips separators and disabled rows and wraps', async () => {
    render(ContextMenu);
    const hitA = vi.fn();
    const hit = vi.fn();
    openMenu([
      { id: 'a', label: 'A', onselect: hitA },
      'separator',
      { id: 'b', label: 'B', disabled: true, onselect: () => {} },
      { id: 'c', label: 'C', onselect: hit },
    ]);
    const menu = await screen.findByRole('menu');
    // start on A (first enabled); down → C (skips sep + disabled B)
    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    await fireEvent.keyDown(menu, { key: 'Enter' });
    expect(hit).toHaveBeenCalledOnce();
  });

  it('ArrowDown wraps from last enabled back to first', async () => {
    render(ContextMenu);
    const hitA = vi.fn();
    const hitC = vi.fn();
    openMenu([
      { id: 'a', label: 'A', onselect: hitA },
      'separator',
      { id: 'b', label: 'B', disabled: true, onselect: () => {} },
      { id: 'c', label: 'C', onselect: hitC },
    ]);
    const menu = await screen.findByRole('menu');
    // A → C (skip sep + disabled B), then C → A (wrap)
    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    await fireEvent.keyDown(menu, { key: 'Enter' });
    expect(hitA).toHaveBeenCalledOnce();
    expect(hitC).not.toHaveBeenCalled();
  });

  it('ArrowUp wraps from first enabled to last', async () => {
    render(ContextMenu);
    const hitA = vi.fn();
    const hitB = vi.fn();
    openMenu([
      { id: 'a', label: 'A', onselect: hitA },
      { id: 'b', label: 'B', onselect: hitB },
    ]);
    const menu = await screen.findByRole('menu');
    // focus starts on A (first enabled); ArrowUp wraps to B (last)
    await fireEvent.keyDown(menu, { key: 'ArrowUp' });
    await fireEvent.keyDown(menu, { key: 'Enter' });
    expect(hitB).toHaveBeenCalledOnce();
    expect(hitA).not.toHaveBeenCalled();
  });

  it('Space activates the focused item and closes', async () => {
    const onselect = vi.fn();
    render(ContextMenu);
    openMenu([{ id: 'a', label: 'A', onselect }]);
    const menu = await screen.findByRole('menu');
    await fireEvent.keyDown(menu, { key: ' ' });
    expect(onselect).toHaveBeenCalledOnce();
    expect(contextMenu.open).toBe(false);
  });

  it('Escape closes and restores focus to the invoker', async () => {
    const invoker = document.createElement('button');
    document.body.appendChild(invoker);
    render(ContextMenu);
    openMenu([{ id: 'a', label: 'A', onselect: () => {} }], invoker);
    const menu = await screen.findByRole('menu');
    await fireEvent.keyDown(menu, { key: 'Escape' });
    expect(contextMenu.open).toBe(false);
    expect(document.activeElement).toBe(invoker);
    invoker.remove();
  });

  it('outside pointerdown closes', async () => {
    render(ContextMenu);
    openMenu([{ id: 'a', label: 'A', onselect: () => {} }]);
    await screen.findByRole('menu');
    await fireEvent.pointerDown(document.body);
    expect(contextMenu.open).toBe(false);
  });

  it('a disabled row is not activatable on click', async () => {
    const onselect = vi.fn();
    render(ContextMenu);
    openMenu([{ id: 'a', label: 'A', disabled: true, onselect }]);
    const row = await screen.findByRole('menuitem');
    await fireEvent.click(row);
    expect(onselect).not.toHaveBeenCalled();
  });
});
