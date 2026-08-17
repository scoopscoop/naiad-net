<script lang="ts">
  import { listRejections } from '../lib/api';
  import type { Rejection } from '../lib/types';

  interface Props {
    hash: string;
    /** Increment to trigger a re-fetch without changing the hash. */
    refreshTick?: number;
    onrestore: (tag: string, service: string) => void;
  }
  let { hash, refreshTick = 0, onrestore }: Props = $props();

  let open = $state(false);
  let rejections = $state<Rejection[]>([]);
  let loadVersion = 0;

  $effect(() => {
    // Track both hash and refreshTick to re-fetch on either change.
    const h = hash;
    void refreshTick;
    const v = ++loadVersion;
    void listRejections(h).then((rej) => {
      if (v === loadVersion) {
        rejections = rej;
      }
    });
  });
</script>

{#if rejections.length > 0}
  <div class="rejected-section">
    <button
      class="head"
      onclick={() => (open = !open)}
      aria-expanded={open}
      aria-label="Rejected tags"
    >
      <span class="chev" class:open>›</span>
      <span class="head-label">REJECTED · {rejections.length}</span>
    </button>

    {#if open}
      <ul class="rows">
        {#each rejections as r (`${r.service}:${r.tag}`)}
          <li class="row">
            <span class="rtag">{r.tag}</span>
            <span class="rsvc">{r.service}</span>
            <button
              class="restore"
              aria-label={`Restore ${r.tag} from ${r.service}`}
              onclick={() => onrestore(r.tag, r.service)}
            >Restore</button>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
{/if}

<style>
  .rejected-section {
    margin-top: 10px;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 5px;
    background: transparent;
    border: 0;
    padding: 0;
    cursor: pointer;
    width: 100%;
    text-align: left;
  }
  .chev {
    display: inline-block;
    color: var(--text-mute);
    font: 500 14px/1 var(--mono);
    transition: transform 0.15s;
  }
  .chev.open {
    transform: rotate(90deg);
  }
  .head-label {
    font: 600 10px/1 var(--mono);
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  .head:hover .chev {
    color: var(--accent);
  }
  .head:hover .head-label {
    color: var(--text-dim);
  }
  .rows {
    list-style: none;
    margin: 6px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 6px;
    height: 24px;
    padding: 0 8px;
    background: var(--ink-900);
    border: 1px solid var(--line);
    border-radius: 6px;
  }
  .rtag {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font: 500 11.5px/1 var(--mono);
    color: var(--text);
  }
  .rsvc {
    font: 500 10px/1 var(--mono);
    color: var(--text-faint);
    flex: none;
  }
  /* Restore: ghost / micro button per DESIGN §5 */
  .restore {
    flex: none;
    background: transparent;
    border: 0;
    padding: 0;
    color: var(--text-faint);
    font: 500 10px/1 var(--mono);
    cursor: pointer;
    transition: color 0.15s;
  }
  .restore:hover {
    color: var(--accent);
  }

  @media (prefers-reduced-motion: reduce) {
    .chev {
      transition: none;
    }
    .restore {
      transition: none;
    }
  }
</style>
