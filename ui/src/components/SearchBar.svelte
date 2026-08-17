<script lang="ts">
  import type { ScanSummary } from '../lib/types';
  import type { GallerySort } from '../lib/gallery-sort';
  import GallerySortMenu from './GallerySortMenu.svelte';
  import SettingsPanel from './SettingsPanel.svelte';
  import TagSearchInput from './TagSearchInput.svelte';
  import Spinner from './Spinner.svelte';
  import { LEVEL_MIN, LEVEL_MAX } from '../lib/settings.svelte';

  interface Props {
    count: number;
    selectedCount?: number;
    /** Zoom level: thumbs per row, LEVEL_MAX (16, min zoom) .. LEVEL_MIN (2, max zoom). */
    level: number;
    sort: GallerySort;
    disabled?: boolean;
    loading?: boolean;
    tabId: number;
    query: string;
    onsearch: (q: string) => void;
    onsearchtag: (tag: string) => void;
    onzoom: (level: number) => void;
    onsort: (sort: GallerySort) => void;
    onimported: (summary: ScanSummary) => void;
    onremoved: () => void;
    onrescope: () => void;
  }

  let {
    count,
    selectedCount = 0,
    level,
    sort,
    disabled = false,
    loading = false,
    tabId,
    query,
    onsearch,
    onsearchtag,
    onzoom,
    onsort,
    onimported,
    onremoved,
    onrescope,
  }: Props = $props();
</script>

<header class="bar" class:dim={disabled}>
  {#key tabId}
    <TagSearchInput {disabled} {onsearch} {onsearchtag} initial={query} />
  {/key}
  <GallerySortMenu {sort} disabled={disabled} onchange={onsort} />

  <span class="count">
    {#if loading}
      <Spinner size={12} />
      searching
    {:else if selectedCount > 0}
      <b>{selectedCount.toLocaleString()}</b> / {count.toLocaleString()} selected
    {:else}
      <b>{count.toLocaleString()}</b> files
    {/if}
  </span>
  <!-- A live region of its own, present from first render so the transition into
       "searching" is announced. Keeping it off .count stops every search result
       and every selection change from being read out as a status update. -->
  <span class="sr" role="status">{loading ? 'searching' : ''}</span>

  <!-- The slider runs over the level range but inverted (right = zoom in =
       fewer thumbs per row), so it feels like the old pixel slider. The same
       reflection maps both directions: slider = LEVEL_MIN + LEVEL_MAX - level. -->
  <label class="zoom">
    zoom
    <input
      type="range"
      min={LEVEL_MIN}
      max={LEVEL_MAX}
      step="1"
      value={LEVEL_MIN + LEVEL_MAX - level}
      aria-valuetext="{level} per row"
      oninput={(e) => onzoom(LEVEL_MIN + LEVEL_MAX - Number(e.currentTarget.value))}
    />
  </label>

  <SettingsPanel {onimported} {onremoved} {onrescope} />
</header>

<style>
  .bar {
    display: flex;
    align-items: center;
    gap: 0.9rem;
    height: 48px;
    padding: 0 14px;
    background: var(--ink-750);
    border-bottom: 1px solid var(--line);
  }
  .count {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font: 500 12px/1 var(--mono);
    color: var(--text-faint);
    white-space: nowrap;
  }
  .count b {
    font-weight: 500;
    color: var(--text-mute);
  }
  .sr {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  .zoom {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    font: 500 11px/1 var(--mono);
    color: var(--text-faint);
  }
  .zoom input {
    accent-color: var(--accent);
  }
  /* Dim only the secondary controls on a detail tab; the search affordance dims
     itself, and the settings modal must not be faded (see #25). */
  .bar.dim .count,
  .bar.dim .zoom {
    opacity: 0.55;
  }
</style>
