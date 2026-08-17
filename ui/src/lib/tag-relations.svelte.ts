/** Single-open relations-popover store (spec §6). Module-level $state singleton,
 *  same convention as context-menu.svelte.ts. Holds only references; the
 *  fetch + DOM work live in TagRelationsPopover.svelte. */

/** Positioning input: an element-derived point (the glyph or invoking chip). */
export interface PopoverAnchor {
  x: number;
  y: number;
}

let open = $state(false);
let anchor = $state<PopoverAnchor | null>(null);
let tag = $state<string | null>(null);
let fileHash = $state<string | null>(null);
// Not reactive: read once on close to restore focus.
let invoker: HTMLElement | null = null;

export const tagRelationsPopover = {
  get open(): boolean {
    return open;
  },
  get anchor(): PopoverAnchor | null {
    return anchor;
  },
  get tag(): string | null {
    return tag;
  },
  get fileHash(): string | null {
    return fileHash;
  },
  get invoker(): HTMLElement | null {
    return invoker;
  },
  /** Open (or replace) the popover for `t`, anchored at `a`; `hash` scopes
   *  `via_alias` (undefined -> no file); `inv` gets focus back on close. */
  openAt(a: PopoverAnchor, t: string, hash: string | undefined, inv?: HTMLElement | null): void {
    anchor = a;
    tag = t;
    fileHash = hash ?? null;
    invoker = inv ?? null;
    open = true;
  },
  /** Clear the popover. `invoker` is intentionally left in place so the
   *  component can restore focus to it after `open` flips false. */
  close(): void {
    open = false;
    anchor = null;
    tag = null;
    fileHash = null;
  },
};
