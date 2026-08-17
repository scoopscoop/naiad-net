<script lang="ts">
  import type { TagDetail } from '../lib/types';
  import type { TagGroup } from '../lib/namespace';
  import { contextMenu } from '../lib/context-menu.svelte';
  import { tagRelationsPopover } from '../lib/tag-relations.svelte';
  import { buildTagMenu } from '../lib/menu-items';

  interface Props {
    groups: TagGroup<TagDetail>[];
    /** The one tag currently mutating, if any. Retained for parent parity;
     *  no longer drives inline UI since chip buttons were removed in the
     *  context-menu migration (spec §8.1). */
    busyTag?: string | null;
    /** True while any tag mutation is in flight. Gates context-menu hide/remove. */
    mutating?: boolean;
    /** Content hash of the file whose tags these are; scopes the relations
     *  popover's `via_alias`. Omitted on catalog surfaces. */
    fileHash?: string;
    onremove: (tag: string) => void;
    /** Called when the user rejects a pulled mapping. Receives the tag string
     *  and the distinct set of services that supply it. */
    onreject: (tag: string, services: string[]) => void;
    /** Opens a new gallery tab searching for the given tag (spec §6.2). */
    onsearchtag: (tag: string) => void;
  }

  let {
    groups,
    busyTag: _busyTag = null, // retained for API parity; no longer drives inline UI
    mutating = false,
    fileHash,
    onremove,
    onreject,
    onsearchtag,
  }: Props = $props();

  async function copyTag(tag: string) {
    try {
      await navigator.clipboard.writeText(tag);
    } catch {
      // Non-fatal: clipboard permission denial is not actionable from here.
    }
  }

  function openRelations(anchor: { x: number; y: number }, t: TagDetail, inv: HTMLElement) {
    contextMenu.close(); // mutually exclusive transient surfaces
    tagRelationsPopover.openAt(anchor, t.tag, fileHash, inv);
  }

  function glyphClick(e: MouseEvent | KeyboardEvent, t: TagDetail) {
    e.stopPropagation();
    const el = e.currentTarget as HTMLElement;
    const r = el.getBoundingClientRect();
    openRelations({ x: r.left, y: r.bottom }, t, el);
  }

  function openTagMenu(anchor: { x: number; y: number }, t: TagDetail, invoker: HTMLElement) {
    tagRelationsPopover.close(); // mutually exclusive transient surfaces
    const items = buildTagMenu(t.tag, t.presence, 'file', mutating, {
      onSearch: () => onsearchtag(t.tag),
      onCopy: () => copyTag(t.tag),
      onHide: () => onreject(t.tag, t.services),
      onRemove: () => onremove(t.tag),
      onRelations: t.relations
        ? () => {
            const r = invoker.getBoundingClientRect();
            openRelations({ x: r.left, y: r.bottom }, t, invoker);
          }
        : undefined,
    });
    contextMenu.openAt(anchor, items, invoker);
  }

  function chipContext(e: MouseEvent, t: TagDetail) {
    e.preventDefault();
    openTagMenu({ x: e.clientX, y: e.clientY }, t, e.currentTarget as HTMLElement);
  }

  function chipKey(e: KeyboardEvent, t: TagDetail) {
    if (e.key === 'r' && !e.ctrlKey && !e.metaKey && !e.altKey && t.presence === 'pulled' && !mutating) {
      onreject(t.tag, t.services);
      return;
    }
    if ((e.key === 'F10' && e.shiftKey) || e.key === 'ContextMenu') {
      e.preventDefault();
      const el = e.currentTarget as HTMLElement;
      const r = el.getBoundingClientRect();
      openTagMenu({ x: r.left, y: r.bottom }, t, el);
    }
  }


</script>

<div class="tag-groups">
  {#each groups as g (g.id)}
    <div class="group">
      <h4 class="group-head" data-testid="group-head">{g.name}</h4>
      <div class="tags">
        {#each g.tags as t (t.tag)}
          <button
            type="button"
            class="chip"
            aria-haspopup="menu"
            aria-keyshortcuts={t.presence === 'pulled' ? 'r' : undefined}
            oncontextmenu={(e) => chipContext(e, t)}
            onkeydown={(e) => chipKey(e, t)}
          >
            <span class="dot" style="background: {g.color}"></span>
            <span class="label">{t.tag}</span>
            {#if t.origin}
              <span class="origin-chip" data-testid="origin-chip">{t.origin}</span>
            {/if}
            {#if t.relations}
              <span
                role="button"
                tabindex="0"
                class="glyph"
                aria-label={`relations for ${t.tag}`}
                onclick={(e) => glyphClick(e, t)}
                onkeydown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    e.stopPropagation();
                    glyphClick(e, t);
                  }
                }}
              >⇆</span>
            {/if}
          </button>
        {/each}
      </div>
    </div>
  {/each}
</div>

<style>
  .tag-groups {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .group-head {
    font: 600 10px/1 var(--mono);
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--text-faint);
    margin: 0 0 6px;
  }
  .tags {
    margin: 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 25px;
    padding: 0 9px;
    border-radius: 6px;
    background: var(--chip);
    border: 1px solid var(--chip-line);
    cursor: default;
    font: inherit;
    color: inherit;
  }
  .chip:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex: none;
  }
  .label {
    font: 500 12px/1 var(--mono);
    color: var(--text);
  }
  .origin-chip {
    flex: none;
    font: 500 10px/1 var(--mono);
    color: var(--text-faint);
    background: var(--raise);
    padding: 1px 4px;
    border-radius: 4px;
  }
  .glyph {
    border: 0;
    background: transparent;
    padding: 0 0 0 2px;
    font: 500 12px/1 var(--mono);
    color: var(--text-faint);
    cursor: pointer;
  }
  .glyph:hover {
    color: var(--accent);
  }
  .glyph:focus-visible {
    color: var(--accent);
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
