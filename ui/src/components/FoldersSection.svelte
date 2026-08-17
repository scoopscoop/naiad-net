<script lang="ts">
  import { scanStream, listRoots, removeRoot } from '../lib/api';
  import type { ScanSummary, ScanError } from '../lib/types';
  import { activity, type JobHandle } from '../lib/activity.svelte';
  import Spinner from './Spinner.svelte';

  interface Props {
    onimported: (summary: ScanSummary) => void;
    onsaved?: () => void;
    onremoved?: () => void;
  }
  let { onimported, onsaved, onremoved }: Props = $props();

  // Only the Tauri webview exposes the IPC bridge; in a plain browser there is
  // no native dialog, so the picker button stays hidden and the path field is
  // the only way in.
  const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

  let path = $state('');
  let errorMsg = $state<string | null>(null);
  let skipped = $state<ScanError[]>([]);
  let roots = $state<string[]>([]);

  // The scan's progress lives in the shared activity store, not in this
  // component, so closing and reopening the settings panel (which remounts us)
  // re-attaches to a still-running scan instead of losing the bar. `job` — the
  // handle that drives the store — is plain local state held only by the mount
  // that started the scan; its closures keep updating the store even after that
  // mount unmounts. `current`/`busy` are read back from the store so any mount
  // reflects the live scan.
  let job: JobHandle | null = null;
  let current = $derived(
    activity.activities.findLast((a) => a.kind === 'scan' && a.status === 'running') ?? null
  );
  let busy = $derived(current !== null);

  async function refreshRoots() {
    try {
      roots = await listRoots();
    } catch (e) {
      errorMsg = e instanceof Error ? e.message : String(e);
    }
  }

  async function choose() {
    try {
      const { open: openDialog } = await import('@tauri-apps/plugin-dialog');
      const dir = await openDialog({ directory: true });
      if (typeof dir === 'string') path = dir;
    } catch (e) {
      // A denied capability or missing IPC bridge rejects here; surface it
      // instead of failing silently (the path field still works as a fallback).
      errorMsg = e instanceof Error ? e.message : String(e);
    }
  }

  function run() {
    const folder = path.trim();
    if (!folder || busy) return;
    errorMsg = null;
    skipped = [];
    // Put the scanned folder in the label so it is visible everywhere the
    // activity surfaces — this inline bar and the future global indicator (#34) —
    // and so a reopened panel (whose path field has been cleared) still shows
    // what is being scanned. Seed a "preparing" detail so the bar shows a hint
    // during the daemon's pre-count walk, before the first tick supplies a total.
    job = activity.begin({ label: `Scanning ${folder}`, kind: 'scan', detail: 'Preparing…' });
    scanStream(folder, {
      onProgress: (p) => {
        // total counts only walk-OK images; walk-level errors land in `skipped`
        // and can nudge done past total, so clamp. ScanSummary is authoritative.
        // Show the running tally against the pre-counted total. Before the
        // total is known (total 0) drop the denominator rather than print "/0".
        const tally = p.total > 0 ? `indexed ${p.imported}/${p.total}` : `indexed ${p.imported}`;
        job?.progress({
          detail: `${tally} · ${p.skipped} skipped`,
          done: Math.min(p.imported + p.skipped, p.total),
          total: p.total,
        });
      },
      onSummary: (summary) => {
        if (summary.errors.length > 0) {
          job?.warn(`${summary.errors.length} skipped`, {
            detail: `indexed ${summary.imported} · ${summary.errors.length} skipped`,
          });
        } else {
          job?.succeed({ detail: `indexed ${summary.imported}` });
        }
        onimported(summary);
        skipped = summary.errors; // [] on a clean run → no list rendered
        refreshRoots(); // surface the newly-watched root
        path = '';
        onsaved?.();
      },
      onError: (message) => {
        job?.fail(message);
        errorMsg = message;
      },
    });
  }

  // The root awaiting a keep/hide decision; non-null shows the confirm modal.
  let pendingRoot = $state<string | null>(null);

  /** The root whose removal is in flight. Distinct from `pendingRoot`, which is
   *  the one awaiting the user's keep/hide decision. */
  let removingRoot = $state<string | null>(null);

  function askRemove(root: string) {
    // The buttons are only `aria-disabled` — so the row the user activated keeps
    // focus while its own removal runs — which means the click still lands. A
    // rejected action must never be silent, so say why no dialog opened.
    if (removingRoot !== null) {
      errorMsg = 'Another folder is still being removed. Try again in a moment.';
      return;
    }
    pendingRoot = root;
  }

  async function confirmRemove(hide: boolean) {
    const target = pendingRoot;
    pendingRoot = null;
    if (!target) return;
    errorMsg = null;
    removingRoot = target;
    try {
      await removeRoot(target, hide);
      await refreshRoots();
      onsaved?.();
      onremoved?.(); // re-fetch the grid so hidden files drop out
    } catch (e) {
      errorMsg = e instanceof Error ? e.message : String(e);
    } finally {
      removingRoot = null;
    }
  }

  // The modal mounts this section fresh each time it opens, so loading the
  // watched roots on init is equivalent to the old open-popover refresh.
  refreshRoots();
</script>

<div class="folders">
  <div class="pick">
    {#if isTauri}
      <button type="button" onclick={choose}>Choose folder</button>
    {/if}
    <input bind:value={path} placeholder="/path/to/folder" aria-label="folder path" />
  </div>
  <button class="scan" onclick={run} disabled={busy || !path.trim()}>
    {busy ? 'Scanning…' : 'Scan'}
  </button>

  {#if current}
    <div class="job" role="status">
      <span class="scanning">{current.label}</span>
      <div class="progress">
        {#if current.total && current.total > 0}
          <progress max={current.total} value={current.done ?? 0}></progress>
        {:else}
          <progress></progress>
        {/if}
        <span class="count">{current.detail}</span>
      </div>
    </div>
  {/if}

  {#if errorMsg}
    <p class="err" role="alert">{errorMsg}</p>
  {/if}

  {#if skipped.length > 0}
    <ul class="skipped">
      {#each skipped as e (e.path)}
        <li><span class="p">{e.path}</span> — {e.message}</li>
      {/each}
    </ul>
  {/if}

  <div class="divider"></div>
  <div class="watching">
    {#if roots.length === 0}
      <p class="empty">No folders watched yet.</p>
    {:else}
      <ul class="roots">
        {#each roots as root (root)}
          <li aria-busy={root === removingRoot}>
            <span class="p">{root}</span>
            <button
              type="button"
              class="rm"
              class:busy={root === removingRoot}
              aria-disabled={removingRoot !== null}
              aria-busy={root === removingRoot}
              aria-label={root === removingRoot ? `removing ${root}` : `stop watching ${root}`}
              onclick={() => askRemove(root)}
            >
              {#if root === removingRoot}<Spinner size={12} />{:else}×{/if}
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  </div>

  {#if pendingRoot}
    <div class="confirm-backdrop"></div>
    <div class="confirm" role="dialog" aria-modal="true" aria-label="stop watching folder">
      <p class="title">Stop watching this folder?</p>
      <p class="path">{pendingRoot}</p>
      <p class="body">This stops live-watching. Your files are <b>NOT deleted</b>.</p>
      <ul class="choices">
        <li><b>Keep files</b> — they stay in your library and gallery.</li>
        <li>
          <b>Hide files</b> — they disappear from the gallery (marked missing). Nothing is deleted;
          a re-scan brings them back.
        </li>
      </ul>
      <div class="actions">
        <button type="button" onclick={() => (pendingRoot = null)}>Cancel</button>
        <button type="button" onclick={() => confirmRemove(false)}>Keep files</button>
        <button type="button" class="primary" onclick={() => confirmRemove(true)}>Hide files</button>
      </div>
    </div>
  {/if}
</div>

<style>
  .folders {
    display: flex;
    flex-direction: column;
    gap: 9px;
  }
  .pick {
    display: flex;
    gap: 7px;
  }
  .pick :global(button) {
    flex: none;
    height: 32px;
    padding: 0 12px;
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    background: var(--ink-900);
    color: var(--text);
    font: 500 12px/1 var(--mono);
    cursor: pointer;
  }
  .pick input {
    flex: 1;
    min-width: 0;
    height: 32px;
    padding: 0 11px;
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    background: var(--ink-900);
    color: var(--text);
    font: 12px/1 var(--mono);
    outline: none;
  }
  .pick input:focus {
    border-color: var(--accent);
  }
  .scan {
    height: 32px;
    border: 0;
    border-radius: 8px;
    background: var(--accent-grad);
    color: var(--on-accent);
    font-weight: 600;
    cursor: pointer;
  }
  .scan:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .err {
    margin: 0;
    font: 500 11.5px/1.4 var(--mono);
    color: var(--err);
  }
  .job {
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  /* Full-width line above the bar: a long path wraps here (matching the
     component's other path rows) instead of crushing the flexing bar. */
  .job .scanning {
    font: 500 11.5px/1.4 var(--mono);
    color: var(--text-mute);
    word-break: break-all;
  }
  .progress {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .progress progress {
    flex: 1;
    min-width: 0;
    height: 6px;
    accent-color: var(--accent);
  }
  .progress .count {
    flex: none;
    font: 500 11.5px/1.4 var(--mono);
    color: var(--text-mute);
  }
  .skipped {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 30vh;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .skipped li {
    font: 500 11px/1.4 var(--mono);
    color: var(--text-faint);
    word-break: break-all;
  }
  .skipped .p {
    color: var(--text-mute);
  }
  .divider {
    height: 1px;
    background: var(--line-soft);
    margin: 2px 0;
  }
  .watching .empty {
    margin: 0;
    font: 500 11.5px/1.4 var(--mono);
    color: var(--text-faint);
  }
  .roots {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 30vh;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .roots li {
    display: flex;
    align-items: center;
    gap: 7px;
    font: 500 11.5px/1.4 var(--mono);
    color: var(--text-mute);
  }
  .roots .p {
    flex: 1;
    min-width: 0;
    word-break: break-all;
  }
  .roots .rm {
    flex: none;
    width: 22px;
    height: 22px;
    border: 1px solid var(--line-soft);
    border-radius: 6px;
    background: var(--ink-900);
    color: var(--text);
    font: 600 13px/1 var(--mono);
    cursor: pointer;
  }
  .roots .rm:not([aria-disabled='true']):hover {
    border-color: var(--accent);
  }
  .roots .rm[aria-disabled='true'] {
    opacity: 0.5;
    cursor: default;
  }
  .rm.busy {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    cursor: default;
  }
  .confirm-backdrop {
    position: fixed;
    inset: 0;
    z-index: 11;
    background: var(--overlay-backdrop);
  }
  .confirm {
    position: fixed;
    z-index: 12;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(380px, 90vw);
    padding: 16px;
    background: var(--ink-800);
    border: 1px solid var(--line);
    border-radius: 11px;
    box-shadow: var(--shadow-modal);
    display: flex;
    flex-direction: column;
    gap: 9px;
  }
  .confirm .title {
    margin: 0;
    font: 600 13px/1.3 var(--mono);
    color: var(--text);
  }
  .confirm .path {
    margin: 0;
    font: 500 11.5px/1.4 var(--mono);
    color: var(--text-mute);
    word-break: break-all;
  }
  .confirm .body {
    margin: 0;
    font: 500 11.5px/1.4 var(--mono);
    color: var(--text-mute);
  }
  .confirm .choices {
    margin: 0;
    padding-left: 16px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    font: 500 11px/1.4 var(--mono);
    color: var(--text-faint);
  }
  .confirm .actions {
    display: flex;
    justify-content: flex-end;
    gap: 7px;
    margin-top: 4px;
  }
  .confirm .actions button {
    height: 30px;
    padding: 0 12px;
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    background: var(--ink-900);
    color: var(--text);
    font: 500 12px/1 var(--mono);
    cursor: pointer;
  }
  .confirm .actions .primary {
    border: 0;
    background: var(--accent-grad);
    color: var(--on-accent);
    font-weight: 600;
  }
</style>
