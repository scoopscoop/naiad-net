<script lang="ts">
  import { FLASH_MS } from '../lib/reject-flow.svelte';

  interface Props {
    tag: string;
    onundo: () => void;
  }
  let { tag, onundo }: Props = $props();
</script>

<div class="reject-flash" role="status" style="--flash-ms: {FLASH_MS}ms">
  <span class="flash-msg">Rejected {tag}</span>
  <span class="sep" aria-hidden="true"> · </span>
  <button class="flash-undo" onclick={onundo}>Undo</button>
</div>

<style>
  .reject-flash {
    display: flex;
    align-items: center;
    gap: 4px;
    margin-top: 6px;
    padding: 5px 10px;
    background: var(--ink-900);
    border: 1px solid var(--line);
    border-radius: 6px;
    animation: reject-flash var(--flash-ms) forwards;
  }
  .flash-msg {
    font: 500 11.5px/1 var(--mono);
    color: var(--text-mute);
  }
  .sep {
    font: 500 11.5px/1 var(--mono);
    color: var(--text-faint);
    flex: none;
  }
  .flash-undo {
    flex: none;
    background: transparent;
    border: 0;
    padding: 0;
    color: var(--accent);
    font: 600 11px/1 var(--mono);
    cursor: pointer;
  }
  .flash-undo:hover {
    color: var(--text);
  }
  @keyframes reject-flash {
    0% { opacity: 1; }
    85% { opacity: 1; }
    100% { opacity: 0; }
  }
  @media (prefers-reduced-motion: reduce) {
    .reject-flash { animation: none; }
  }
</style>
