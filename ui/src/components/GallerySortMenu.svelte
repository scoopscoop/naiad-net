<script lang="ts">
  import { nextSort, SORT_KEYS, type GallerySort, type SortKey } from '../lib/gallery-sort';

  interface Props {
    sort: GallerySort;
    disabled?: boolean;
    onchange: (sort: GallerySort) => void;
  }

  let { sort, disabled = false, onchange }: Props = $props();
  let open = $state(false);

  const labels: Record<SortKey, string> = {
    imported_at: 'Import date',
    created_at: 'Created date',
    modified_at: 'Modified date',
    name: 'Name',
    size: 'Size',
    type: 'Type',
  };

  const keys = SORT_KEYS;

  const label = $derived(labels[sort.key]);
  const directionLabel = $derived(sort.direction === 'asc' ? 'ascending' : 'descending');
  const glyph = $derived(sort.direction === 'asc' ? '^' : 'v');

  function toggle() {
    if (disabled) return;
    open = !open;
  }

  function pick(key: SortKey) {
    onchange(nextSort(sort, key));
    open = false;
  }
</script>

<div
  class="sort"
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
    {disabled}
    aria-haspopup="menu"
    aria-expanded={open}
    aria-label={`Sort: ${label} ${directionLabel}`}
    onclick={toggle}
  >
    <span>Sort: {label}</span>
    <span class="dir" aria-hidden="true">{glyph}</span>
  </button>

  {#if open}
    <div class="menu" role="menu">
      {#each keys as key}
        <button
          type="button"
          role="menuitem"
          class:active={key === sort.key}
          onmousedown={(e) => e.preventDefault()}
          onclick={() => pick(key)}
        >
          <span>{labels[key]}</span>
          {#if key === sort.key}<span class="dir">{glyph}</span>{/if}
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .sort {
    position: relative;
    flex: none;
  }
  .trigger {
    height: 32px;
    max-width: 168px;
    display: inline-flex;
    align-items: center;
    gap: 7px;
    padding: 0 10px;
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    background: var(--raise);
    color: var(--text);
    font: 600 12px/1 var(--mono);
    cursor: pointer;
  }
  .trigger:disabled {
    cursor: default;
  }
  .trigger:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .trigger span:first-child {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dir {
    color: var(--accent);
  }
  .menu {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    z-index: 25;
    min-width: 180px;
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
  .menu button.active {
    background: var(--ink-750);
  }
</style>
