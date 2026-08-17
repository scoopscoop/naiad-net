<script lang="ts">
  import { tick } from 'svelte';
  import { tagRelationsPopover } from '../lib/tag-relations.svelte';
  import { fetchTagRelations } from '../lib/api';
  import { clampMenuPosition } from '../lib/menu-position';
  import type { TagRelations, RelationSection } from '../lib/types';

  interface Props {
    /** Opens a gallery search for a tag (same action as the chip menu Search). */
    onsearchtag: (tag: string) => void;
  }
  let { onsearchtag }: Props = $props();

  let el = $state<HTMLElement | undefined>(undefined);
  let data = $state<TagRelations | null>(null);
  let pos = $state({ x: 0, y: 0 });
  let controller: AbortController | null = null;

  // Lazy fetch on open; abort on close/re-open.
  $effect(() => {
    if (!tagRelationsPopover.open || !tagRelationsPopover.tag) {
      data = null;
      controller?.abort();
      controller = null;
      return;
    }
    const tag = tagRelationsPopover.tag;
    const hash = tagRelationsPopover.fileHash ?? undefined;
    controller?.abort();
    controller = new AbortController();
    const signal = controller.signal;
    data = null;
    fetchTagRelations(tag, hash, 10, signal)
      .then((r) => { if (!signal.aborted) data = r; })
      .catch(() => { /* offline/error: render nothing, never block */ });
  });

  // Position after render: measure the mounted element, then clamp to viewport.
  $effect(() => {
    if (!tagRelationsPopover.open || !el || !tagRelationsPopover.anchor) return;
    const rect = el.getBoundingClientRect();
    pos = clampMenuPosition(
      tagRelationsPopover.anchor,
      { width: rect.width, height: rect.height },
      { width: window.innerWidth, height: window.innerHeight },
    );
  });

  // Move focus into the popover once its rows are present.
  $effect(() => {
    if (tagRelationsPopover.open && data && el) {
      tick().then(() => el?.querySelector<HTMLElement>('[role="menuitem"]')?.focus());
    }
  });

  // Restore focus to the invoker when the popover closes.
  let wasOpen = false;
  $effect(() => {
    const isOpen = tagRelationsPopover.open;
    if (!isOpen && wasOpen) tagRelationsPopover.invoker?.focus();
    wasOpen = isOpen;
  });

  // All dismissal listeners live only while open — mirrors ContextMenu exactly.
  $effect(() => {
    if (!tagRelationsPopover.open) return;
    const onPointerDown = (e: Event) => {
      if (!el || !el.contains(e.target as Node)) tagRelationsPopover.close();
    };
    const onScroll = (e: Event) => {
      if (el && el.contains(e.target as Node)) return;
      tagRelationsPopover.close();
    };
    const onResize = () => tagRelationsPopover.close();
    const onBlur = () => tagRelationsPopover.close();
    // Escape is handled here (gated on open only) so it fires even when the
    // dialog is not rendered (e.g. fetch failed — data is null, {#if open && data} = false).
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        tagRelationsPopover.close();
      }
    };
    window.addEventListener('pointerdown', onPointerDown, true);
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', onResize);
    window.addEventListener('blur', onBlur);
    window.addEventListener('keydown', onKeyDown, true);
    return () => {
      window.removeEventListener('pointerdown', onPointerDown, true);
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', onResize);
      window.removeEventListener('blur', onBlur);
      window.removeEventListener('keydown', onKeyDown, true);
    };
  });

  function pick(tag: string) {
    onsearchtag(tag);
    tagRelationsPopover.close();
  }

  const nonEmpty = (s: RelationSection) => s.items.length > 0;
</script>

{#if tagRelationsPopover.open && data}
  <div
    class="rpop"
    role="dialog"
    aria-label={`relations for ${data.canonical}`}
    tabindex="-1"
    bind:this={el}
    style="left: {pos.x}px; top: {pos.y}px"
  >
    <section class="sec">
      <h5 class="head">Shown as</h5>
      <div class="canon-row">
        <span class="canon">{data.canonical}</span>
        <span class="cnt">{data.count}</span>
      </div>
      {#if data.via_alias}<div class="note">via an alias on this file</div>{/if}
    </section>

    {#each [['Aliases', data.aliases], ['Implies', data.parents], ['Implied by', data.children]] as [title, sec] (title)}
      {#if nonEmpty(sec as RelationSection)}
        <section class="sec">
          <h5 class="head">
            <span>{title}</span>
            <span class="hcnt">{(sec as RelationSection).total}</span>
          </h5>
          <!-- svelte-ignore a11y_interactive_supports_focus -->
          <div role="menu">
            {#each (sec as RelationSection).items as it (it.tag)}
              <button type="button" class="row" role="menuitem" onclick={() => pick(it.tag)}>
                <span class="rtag">{it.tag}</span>
                <!-- Aliases usually have their own raw count of 0 (files are
                     stored canonical); hide a 0 so the row reads as a spelling,
                     not a tag with zero files. -->
                {#if it.count > 0}<span class="cnt">{it.count}</span>{/if}
              </button>
            {/each}
          </div>
          {#if (sec as RelationSection).total > (sec as RelationSection).items.length}
            <div class="more" aria-hidden="true">… {(sec as RelationSection).total - (sec as RelationSection).items.length} more</div>
          {/if}
        </section>
      {/if}
    {/each}
  </div>
{/if}

<style>
  /* Shared popover recipe (DESIGN.md); mutually exclusive with the context
     menu, so it reuses z-28. */
  .rpop {
    position: fixed;
    z-index: 28;
    min-width: 200px;
    max-width: 320px;
    padding: 4px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    box-shadow: var(--shadow-popover);
  }
  .sec { padding: 4px 4px 6px; }
  .head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    font: 600 10px/1 var(--mono);
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--text-faint);
    margin: 2px 4px 4px;
  }
  /* Section total (e.g. how many aliases exist) — same faint mono as the label. */
  .hcnt { letter-spacing: 0; }
  .canon-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 0 4px;
  }
  .canon { font: 500 12px/1 var(--mono); color: var(--text); }
  .note { font: 500 10px/1 var(--mono); color: var(--text-faint); padding: 3px 4px 0; }
  .row {
    width: 100%;
    height: 26px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    border: 0;
    border-radius: 6px;
    padding: 0 6px;
    background: transparent;
    color: var(--text);
    font: 12px/1 var(--mono);
    cursor: pointer;
    text-align: left;
  }
  /* Focus indicator: background swap matching ContextMenu convention (outline: none). */
  .row:hover,
  .row:focus-visible {
    background: var(--ink-750);
    color: var(--accent);
    outline: none;
  }
  .cnt { color: var(--text-faint); }
  .more { font: 500 11px/1 var(--mono); color: var(--text-faint); padding: 4px 6px 2px; }
</style>
