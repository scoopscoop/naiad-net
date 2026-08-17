import { describe, expect, it } from 'vitest';
import { contextMenu } from './context-menu.svelte';
import type { MenuItem, MenuList } from './menu-items';

const item = (id: string): MenuList => [{ id, label: id, onselect: () => {} }];

describe('context-menu store', () => {
  it('opens with anchor, items, and invoker', () => {
    const inv = document.createElement('button');
    contextMenu.openAt({ x: 10, y: 20 }, item('a'), inv);
    expect(contextMenu.open).toBe(true);
    expect(contextMenu.anchor).toEqual({ x: 10, y: 20 });
    expect(contextMenu.items).toHaveLength(1);
    expect(contextMenu.invoker).toBe(inv);
    contextMenu.close();
  });

  it('close clears open/items/anchor', () => {
    contextMenu.openAt({ x: 1, y: 1 }, item('a'));
    contextMenu.close();
    expect(contextMenu.open).toBe(false);
    expect(contextMenu.items).toHaveLength(0);
    expect(contextMenu.anchor).toBeNull();
  });

  it('a second openAt replaces the prior menu', () => {
    contextMenu.openAt({ x: 1, y: 1 }, item('a'));
    contextMenu.openAt({ x: 2, y: 2 }, item('b'));
    expect(contextMenu.open).toBe(true);
    expect(contextMenu.anchor).toEqual({ x: 2, y: 2 });
    expect((contextMenu.items[0] as MenuItem).id).toBe('b');
    contextMenu.close();
  });

  it('close leaves invoker intact for focus restore', () => {
    const inv = document.createElement('button');
    contextMenu.openAt({ x: 1, y: 1 }, item('a'), inv);
    contextMenu.close();
    expect(contextMenu.invoker).toBe(inv);
  });

  it('openAt without invoker clears a prior invoker', () => {
    const inv = document.createElement('button');
    contextMenu.openAt({ x: 1, y: 1 }, item('a'), inv);
    contextMenu.openAt({ x: 2, y: 2 }, item('b')); // no invoker
    expect(contextMenu.invoker).toBeNull();
    contextMenu.close();
  });
});
