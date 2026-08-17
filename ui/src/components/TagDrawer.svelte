<script lang="ts">
  import type { Snippet } from 'svelte';
  import { clampHeight, drawer } from '../lib/detail-drawer.svelte';
  import Icon from './Icon.svelte';

  interface Props {
    name: string;
    tagCount: number;
    paneHeight: number;
    children: Snippet;
  }
  let { name, tagCount, paneHeight, children }: Props = $props();

  let resizing = $state(false);
  let startY = 0;
  let startH = 0;

  function down(e: PointerEvent) {
    resizing = true;
    startY = e.clientY;
    startH = drawer.height;
    try {
      (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    } catch {
      // jsdom has no pointer capture.
    }
  }

  function move(e: PointerEvent) {
    if (!resizing) return;
    drawer.height = clampHeight(startH + (startY - e.clientY), paneHeight);
  }

  function up() {
    resizing = false;
  }
</script>

{#if drawer.open}
  <section class="drawer" style="height: {clampHeight(drawer.height, paneHeight)}px">
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
      class="handle"
      role="separator"
      aria-orientation="horizontal"
      aria-label="resize tag drawer"
      onpointerdown={down}
      onpointermove={move}
      onpointerup={up}
      onpointercancel={up}
    >
      <span class="grip"></span>
      <button
        class="chev"
        onclick={() => (drawer.open = false)}
        onpointerdown={(e) => e.stopPropagation()}
        aria-label="minimize tag drawer"
        aria-expanded="true"
        title="minimize tag drawer"
      >
        <Icon name="chevron-down" size={14} />
      </button>
    </div>
    <div class="body" data-scroll>
      {@render children()}
    </div>
  </section>
{:else}
  <button
    class="bar"
    onclick={() => (drawer.open = true)}
    aria-label="expand tag drawer"
    aria-expanded="false"
    title="expand tag drawer"
  >
    <span class="name" title={name}>{name}</span>
    <span class="count">TAGS - {tagCount}</span>
    <span class="bar-icon"><Icon name="chevron-up" size={16} /></span>
  </button>
{/if}

<style>
  .drawer {
    flex: none;
    display: flex;
    flex-direction: column;
    min-height: 0;
    background: var(--ink-800);
    border-top: 1px solid var(--line);
  }
  .handle {
    flex: none;
    height: 14px;
    display: flex;
    align-items: center;
    justify-content: center;
    position: relative;
    cursor: ns-resize;
    touch-action: none;
  }
  .handle:hover .grip,
  .handle:active .grip {
    background: var(--accent);
  }
  .grip {
    width: 44px;
    height: 3px;
    border-radius: 2px;
    background: var(--line-soft);
  }
  .handle .chev {
    position: absolute;
    right: 10px;
    top: 0;
    width: 22px;
    height: 14px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border: 0;
    background: transparent;
    color: var(--text-mute);
    cursor: pointer;
  }
  .handle .chev:hover {
    color: var(--accent);
  }
  .body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 0 16px 16px;
  }
  .bar {
    flex: none;
    display: flex;
    align-items: center;
    gap: 12px;
    height: 30px;
    padding: 0 14px;
    border: 0;
    border-top: 1px solid var(--line);
    background: var(--ink-800);
    color: var(--text-dim);
    font: 500 11.5px/1 var(--mono);
    cursor: pointer;
    text-align: left;
  }
  .bar:hover {
    color: var(--text);
  }
  .bar .name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .bar .count {
    flex: none;
    color: var(--text-faint);
    letter-spacing: 0.08em;
    font-size: 10px;
  }
  .bar .bar-icon {
    flex: none;
    display: inline-flex;
    color: var(--text-mute);
  }
  .bar:hover .bar-icon {
    color: var(--accent);
  }
</style>
