/** Single-open context-menu store (spec §3.2). Module-level $state singleton,
 *  same convention as detail-drawer.svelte.ts. Holds only references; all
 *  geometry and DOM work lives in ContextMenu.svelte via clampMenuPosition. */

import type { MenuList } from './menu-items';

/** The positioning input: a cursor point (mouse) or an element-derived point
 *  (keyboard). */
export interface MenuAnchor {
  x: number;
  y: number;
}

let open = $state(false);
let items = $state<MenuList>([]);
let anchor = $state<MenuAnchor | null>(null);
// Not reactive: the component reads it once on close to restore focus.
let invoker: HTMLElement | null = null;

export const contextMenu = {
  get open(): boolean {
    return open;
  },
  get items(): MenuList {
    return items;
  },
  get anchor(): MenuAnchor | null {
    return anchor;
  },
  get invoker(): HTMLElement | null {
    return invoker;
  },
  /** Open (or replace) the menu at `anchor` with `list`; `inv` is the element
   *  focus returns to on close. */
  openAt(a: MenuAnchor, list: MenuList, inv?: HTMLElement | null): void {
    anchor = a;
    items = list;
    invoker = inv ?? null;
    open = true;
  },
  /** Clear the menu. `invoker` is intentionally left in place so the component
   *  can restore focus to it after `open` flips false. */
  close(): void {
    open = false;
    items = [];
    anchor = null;
  },
};
