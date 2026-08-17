<script lang="ts">
  import type { TagDetail } from '../lib/types';
  import { MANUAL_ORIGIN, originKey } from '../lib/origin-visibility';
  import { view } from '../lib/settings.svelte';

  interface Props {
    /** All tags for the current file (before origin filtering). */
    tags: TagDetail[];
  }

  let { tags }: Props = $props();
  let open = $state(false);

  /** Distinct origin keys present on this file, sorted: named alphabetically
   *  first, MANUAL_ORIGIN (origin-less) last. */
  const originKeys = $derived(
    Array.from(new Set(tags.map(originKey))).sort((a, b) => {
      if (a === MANUAL_ORIGIN) return 1;
      if (b === MANUAL_ORIGIN) return -1;
      return a.localeCompare(b);
    }),
  );

  /** Number of hidden origins that are actually present on this file. */
  const hiddenOnFile = $derived(
    originKeys.filter((key) => view.isOriginHidden(key)).length,
  );

  function countOrigin(key: string): number {
    return tags.filter((t) => originKey(t) === key).length;
  }

  function displayName(key: string): string {
    return key === MANUAL_ORIGIN ? 'manual' : key;
  }

  function isVisible(key: string): boolean {
    return !view.isOriginHidden(key);
  }

  function toggle() {
    open = !open;
  }
</script>

<div
  class="origins"
  onfocusout={(e) => {
    if (
      !(e.currentTarget instanceof HTMLElement) ||
      !(e.relatedTarget instanceof Node) ||
      !e.currentTarget.contains(e.relatedTarget)
    )
      open = false;
  }}
>
  <button
    class="trigger"
    type="button"
    aria-haspopup="menu"
    aria-expanded={open}
    aria-label={hiddenOnFile > 0
      ? `Origins: ${hiddenOnFile} hidden`
      : 'Origins'}
    onclick={toggle}
  >
    <span>Origins</span>
    {#if hiddenOnFile > 0}
      <span class="hidden-badge" aria-hidden="true"
        ><span class="sep">·</span>{hiddenOnFile} hidden</span
      >
    {/if}
  </button>

  {#if open}
    <div class="menu" role="menu">
      {#each originKeys as key}
        <button
          type="button"
          role="menuitemcheckbox"
          aria-checked={isVisible(key)}
          class:checked={isVisible(key)}
          onmousedown={(e) => e.preventDefault()}
          onclick={() => view.toggleHiddenOrigin(key)}
        >
          <span class="origin-name">{displayName(key)}</span>
          <span class="count">{countOrigin(key)}</span>
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .origins {
    position: relative;
    flex: none;
  }
  .trigger {
    height: 24px;
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 0 8px;
    border: 1px solid var(--line-soft);
    border-radius: 6px;
    background: var(--raise);
    color: var(--text);
    font: 600 11px/1 var(--mono);
    cursor: pointer;
    white-space: nowrap;
  }
  .trigger:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .hidden-badge {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    font-weight: 400;
    color: var(--accent);
  }
  .sep {
    color: var(--text-faint);
  }
  .menu {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    z-index: 25;
    min-width: 160px;
    padding: 4px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    box-shadow: var(--shadow-popover);
  }
  .menu button {
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
  }
  .menu button:hover,
  .menu button.checked {
    background: var(--ink-750);
  }
  .menu button:focus-visible {
    outline: 1px solid var(--accent);
    outline-offset: -2px;
  }
  .count {
    color: var(--text-faint);
    font-size: 11px;
  }
  .origin-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
