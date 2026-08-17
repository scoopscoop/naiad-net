<script lang="ts">
  import { tick } from 'svelte';
  import { contextMenu } from '../lib/context-menu.svelte';
  import { clampMenuPosition } from '../lib/menu-position';
  import type { MenuItem } from '../lib/menu-items';

  let menuEl = $state<HTMLElement>();
  let pos = $state({ x: 0, y: 0 });
  let activeIndex = $state(-1);
  // Per-render button refs, indexed to match `entries`.
  let buttons = $state<HTMLButtonElement[]>([]);

  const entries = $derived(contextMenu.items);
  /** Indices of enabled (activatable) rows — the roving-focus order. */
  const enabledIndexes = $derived(
    entries
      .map((e, i) => (e !== 'separator' && !(e as MenuItem).disabled ? i : -1))
      .filter((i) => i >= 0),
  );

  function focusActive() {
    buttons[activeIndex]?.focus();
  }

  // Position after render: measure the mounted menu, then clamp to the viewport.
  $effect(() => {
    if (!contextMenu.open || !menuEl || !contextMenu.anchor) return;
    const rect = menuEl.getBoundingClientRect();
    pos = clampMenuPosition(
      contextMenu.anchor,
      { width: rect.width, height: rect.height },
      { width: window.innerWidth, height: window.innerHeight },
    );
  });

  // On open, move focus into the menu (first enabled item).
  $effect(() => {
    if (!contextMenu.open) return;
    activeIndex = enabledIndexes[0] ?? -1;
    tick().then(focusActive);
  });

  // Restore focus to the invoker when the menu closes (any cause).
  let wasOpen = false;
  $effect(() => {
    const isOpen = contextMenu.open;
    if (!isOpen && wasOpen) contextMenu.invoker?.focus();
    wasOpen = isOpen;
  });

  // Dismissal listeners live only while the menu is open.
  $effect(() => {
    if (!contextMenu.open) return;
    const onPointerDown = (e: Event) => {
      if (menuEl && !menuEl.contains(e.target as Node)) contextMenu.close();
    };
    const onScroll = (e: Event) => {
      if (menuEl && menuEl.contains(e.target as Node)) return;
      contextMenu.close();
    };
    const onResize = () => contextMenu.close();
    const onBlur = () => contextMenu.close();
    window.addEventListener('pointerdown', onPointerDown, true);
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', onResize);
    window.addEventListener('blur', onBlur);
    return () => {
      window.removeEventListener('pointerdown', onPointerDown, true);
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', onResize);
      window.removeEventListener('blur', onBlur);
    };
  });

  function move(dir: 1 | -1) {
    const list = enabledIndexes;
    if (list.length === 0) return;
    const cur = list.indexOf(activeIndex);
    const next = cur === -1 ? (dir > 0 ? 0 : list.length - 1) : (cur + dir + list.length) % list.length;
    activeIndex = list[next];
    focusActive();
  }

  function activate(i: number) {
    const e = entries[i];
    if (e && e !== 'separator' && !e.disabled) {
      e.onselect();
      contextMenu.close();
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      move(1);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      move(-1);
    } else if (e.key === 'Home') {
      e.preventDefault();
      activeIndex = enabledIndexes[0] ?? -1;
      focusActive();
    } else if (e.key === 'End') {
      e.preventDefault();
      activeIndex = enabledIndexes[enabledIndexes.length - 1] ?? -1;
      focusActive();
    } else if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      activate(activeIndex);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      contextMenu.close();
    }
  }
</script>

{#if contextMenu.open}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div
    class="cmenu"
    role="menu"
    tabindex="-1"
    bind:this={menuEl}
    style="left: {pos.x}px; top: {pos.y}px"
    onkeydown={onKey}
  >
    {#each entries as entry, i (entry === 'separator' ? `sep-${i}` : (entry as MenuItem).id)}
      {#if entry === 'separator'}
        <div class="sep" role="separator"></div>
      {:else}
        <button
          type="button"
          role="menuitem"
          class="row"
          class:danger={entry.danger}
          aria-disabled={entry.disabled ? 'true' : undefined}
          tabindex={i === activeIndex ? 0 : -1}
          bind:this={buttons[i]}
          onmousedown={(e) => e.preventDefault()}
          onclick={() => activate(i)}
        >
          <span class="lbl">{entry.label}</span>
          {#if entry.hint}<span class="hint">{entry.hint}</span>{/if}
        </button>
      {/if}
    {/each}
  </div>
{/if}

<style>
  /* Mirrors GallerySortMenu.svelte .menu; fixed to the clamped point, z-28. */
  .cmenu {
    position: fixed;
    z-index: 28;
    min-width: 180px;
    padding: 4px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    box-shadow: var(--shadow-popover);
  }
  .row {
    width: 100%;
    height: 30px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    border: 0;
    border-radius: 6px;
    padding: 0 8px;
    background: transparent;
    color: var(--text);
    font: 12px/1 var(--mono);
    cursor: pointer;
    text-align: left;
  }
  .row:hover,
  .row:focus-visible {
    background: var(--ink-750);
    outline: none;
  }
  .row.danger:hover,
  .row.danger:focus-visible {
    color: var(--err);
  }
  .row[aria-disabled='true'] {
    opacity: 0.5;
    cursor: default;
  }
  .row[aria-disabled='true']:hover {
    background: transparent;
  }
  .hint {
    color: var(--text-faint);
  }
  .sep {
    height: 1px;
    margin: 4px 6px;
    background: var(--line-soft);
  }
</style>
