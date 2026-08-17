<script lang="ts">
  import type { FileDto, TagDetail } from '../lib/types';
  import { addTags, removeTags, tagsDetailed, report as sendReport } from '../lib/api';
  import { loadThumbHttp } from '../lib/load-thumb';
  import { categories } from '../lib/categories.svelte';
  import { groupByCategory } from '../lib/namespace';
  import { view } from '../lib/settings.svelte';
  import { originKey } from '../lib/origin-visibility';
  import TagGroupList from './TagGroupList.svelte';
  import IdentityCard from './IdentityCard.svelte';
  import Icon from './Icon.svelte';
  import { createPending } from '../lib/pending.svelte';
  import Spinner from './Spinner.svelte';
  import RejectedSection from './RejectedSection.svelte';
  import ReportModal from './ReportModal.svelte';
  import RejectFlash from './RejectFlash.svelte';
  import { createRejectFlow } from '../lib/reject-flow.svelte';
  import { createPullRemote } from '../lib/pull-remote.svelte';
  import OriginVisibilityMenu from './OriginVisibilityMenu.svelte';
  import { onDestroy } from 'svelte';

  interface Props {
    file: FileDto | null;
    onopen: () => void;
    onerror: (message: string) => void;
    hidden?: boolean;
    /** True when the viewport is narrower than the 700px breakpoint; ANDed into
     *  the collapsed computation as a floor so the user's preference returns
     *  automatically when the window widens. */
    narrowWindow?: boolean;
    /** Hashes currently selected in the gallery; the pull button acts on them
     *  when the focused file is part of the selection, else on the focused file. */
    selectedHashes?: string[];
    /** Opens a new gallery tab searching for the given tag (spec §6.2). */
    onsearchtag: (tag: string) => void;
  }
  let { file, onopen, onerror, hidden = false, narrowWindow = false, selectedHashes = [], onsearchtag }: Props = $props();

  // effectiveCollapsed: narrow viewport forces the 28px strip regardless of the
  // user's stored preference; the preference is not mutated so it survives a
  // resize back above the threshold.
  // Intentionally duplicates App.svelte's effectiveInspectorCollapsed — Inspector also
  // needs raw narrowWindow for the disabled/aria attributes. Keep both in sync.
  const effectiveCollapsed = $derived(narrowWindow || view.inspectorCollapsed);

  let tags = $state<TagDetail[]>([]);
  let newTag = $state('');
  let requestId = 0;
  const loadPending = createPending();
  /** The mutation in flight for a given file hash: the tag whose row is busy, or
   *  `null` for the add-tag form. Keyed by hash because a mutation outlives the
   *  selection that started it — the request keeps running when the user
   *  switches away, and is still running when they switch back.
   *
   *  Single source of truth for both the guard and the spinners. Deriving the
   *  display from it, instead of tracking it in flags the file-change effect
   *  resets, is what closes the gap: flags cleared on switch used to reopen the
   *  guard on a call that had not finished, so a round trip A -> B -> A let a
   *  second mutation start against the same hash. */
  let inflight = $state<Record<string, string | null>>({});

  let rejectedSectionTick = $state(0);

  const flow = createRejectFlow({
    refresh: async () => { await refresh(); },
    onRejectedChanged: () => { rejectedSectionTick += 1; },
  });

  function begin(hash: string, tag: string | null) {
    inflight[hash] = tag;
  }
  function end(hash: string) {
    delete inflight[hash];
  }

  /** One mutation at a time *per file*: every mutator refreshes on completion,
   *  and two overlapping refreshes on the same file would strand one another's
   *  sequence number. Mutations on different files stay independent. Every
   *  mutator checks this, and every control that triggers one is marked
   *  `aria-disabled` while it holds — `disabled` would drop focus from the very
   *  control the user just activated, and would not stop an Enter press in the
   *  tag field from submitting the form anyway. */
  const mutating = $derived(file != null && file.hash in inflight);
  /** The single tag whose mutation is in flight — its row shows a spinner. */
  const busyTag = $derived(file != null ? (inflight[file.hash] ?? null) : null);
  const adding = $derived(mutating && busyTag === null);
  /** Tags whose origin is not in the hidden set. Filtered before grouping so
   *  hidden-origin tags simply drop out of the existing category groups. */
  const visibleTags = $derived(
    tags.filter((t) => !view.isOriginHidden(originKey(t))),
  );
  const groups = $derived(groupByCategory(visibleTags, categories.list, (t) => t.tag));

  function report(e: unknown) {
    onerror(e instanceof Error ? e.message : String(e));
  }

  /** A rejected action must never be silent. Mutations are serialised per file;
   *  if one is already in flight, every other mutator is blocked here. */
  function rejectBusy() {
    report(new Error('Another change is still saving. Try again in a moment.'));
  }

  async function load(hash: string, seq: number) {
    loadPending.start();
    try {
      const next = await tagsDetailed(hash, view.localOnly);
      if (seq === requestId && file?.hash === hash) tags = next;
    } catch (e) {
      if (seq === requestId && file?.hash === hash) report(e);
    } finally {
      loadPending.end();
    }
  }

  $effect(() => {
    const hash = file?.hash ?? null;
    const localOnly = view.localOnly;
    requestId += 1;
    const seq = requestId;
    tags = [];
    newTag = '';
    flow.clearFlash();
    flow.dismissOffer();
    if (!hash) return;
    const timer = setTimeout(() => {
      void localOnly;
      load(hash, seq);
    }, 100);
    return () => clearTimeout(timer);
  });

  // A request in flight when the Inspector unmounts leaves createPending's delay
  // and hold timers armed against a destroyed component's $state.
  $effect(() => () => loadPending.reset());

  async function refresh() {
    if (!file) return;
    requestId += 1;
    await load(file.hash, requestId);
  }

  async function add() {
    if (!file) return;
    const tag = newTag.trim();
    if (!tag) return;
    if (mutating) return rejectBusy();
    const hash = file.hash;
    begin(hash, null);
    try {
      await addTags(hash, [tag]);
      if (file?.hash !== hash) return;
      newTag = '';
      await refresh();
    } catch (e) {
      if (file?.hash === hash) report(e);
    } finally {
      end(hash);
    }
  }

  async function remove(tag: string) {
    if (!file) return;
    // A second click while the first is in flight would double-remove.
    if (mutating) return rejectBusy();
    const hash = file.hash;
    begin(hash, tag);
    try {
      await removeTags(hash, [tag]);
      // The user may have switched files while this was in flight. Refreshing
      // now would fetch the new file's tags under this call's sequence number
      // and strand the load the file-change effect already started.
      if (file?.hash !== hash) return;
      await refresh();
    } catch (e) {
      if (file?.hash === hash) report(e);
    } finally {
      // Keyed by hash, so a stale mutation clears only its own entry and never
      // the spinner of a mutation running on the file the user moved to.
      end(hash);
    }
  }

  /** Reject a pulled mapping — mirrors the remove() serialisation contract. */
  async function reject(tag: string, services: string[]) {
    if (!file) return;
    if (mutating) return rejectBusy();
    const hash = file.hash;
    begin(hash, tag);
    try {
      await flow.reject(tag, services, hash);
    } catch (e) {
      if (file?.hash === hash) report(e);
    } finally {
      end(hash);
    }
  }

  async function undoFlash() {
    await flow.undoFlash();
  }

  onDestroy(() => { flow.destroy(); pull.destroy(); });

  const pullTargets = $derived(
    file
      ? selectedHashes.length > 1 && selectedHashes.includes(file.hash)
        ? selectedHashes
        : [file.hash]
      : [],
  );

  const pull = createPullRemote({
    targets: () => pullTargets,
    originHash: () => file?.hash ?? null,
    refresh,
    report,
  });

  async function restore(tag: string, service: string) {
    if (!file) return;
    if (mutating) return rejectBusy();
    const hash = file.hash;
    begin(hash, tag);
    try {
      await flow.restore(tag, service, hash);
    } catch (e) {
      if (file?.hash === hash) report(e);
    } finally {
      end(hash);
    }
  }

  // Esc dismisses the flash without undoing. Stands down when report modal is open.
  $effect(() => {
    if (!flow.flash) return;
    return flow.attachEsc();
  });

</script>

{#if !hidden}
  {#if effectiveCollapsed}
    <button
      class="strip"
      onclick={() => (view.inspectorCollapsed = false)}
      disabled={narrowWindow}
      aria-label={narrowWindow ? 'window too narrow to expand inspector' : 'expand inspector'}
      aria-expanded="false"
      title={narrowWindow ? 'window too narrow to expand inspector' : undefined}
    >
      <Icon name="chevron-left" size={16} />
    </button>
  {:else}
    <aside class="insp">
      <div class="head">
        <h3>INSPECTOR</h3>
        <button
          class="chev"
          onclick={() => (view.inspectorCollapsed = true)}
          aria-label="collapse inspector"
          aria-expanded="true"
          title="collapse inspector"
        >
          <Icon name="chevron-right" size={16} />
        </button>
      </div>

      {#if file}
        <div class="peek">
          <!-- impeccable-disable-next-line broken-image: src is assigned by the loadThumbHttp action after queue fetch -->
          <img class="thumb" decoding="async" alt={file.name} use:loadThumbHttp={file.hash} />
          <div class="title-row">
            <h2 title={file.name}>{file.name}</h2>
            <button class="open" onclick={onopen} aria-label={`open ${file.name}`}>Open</button>
          </div>
        </div>

        <h3 class="section">
          TAGS - {visibleTags.length}
          <span class="loading" role="status">
            {#if loadPending.busy}
              <Spinner size={12} />
              <span class="sr">loading tags</span>
            {/if}
          </span>
          <span class="section-right">
            <OriginVisibilityMenu {tags} />
            {#if pull.repoCount > 0}
              <button
                class="pull-remote"
                onclick={pull.run}
                disabled={pull.pulling}
                aria-label="pull remote tags">
                {#if pull.result}
                  {pull.result}
                {:else if pull.pulling}
                  pulling…
                {:else}
                  pull remote{pullTargets.length > 1 ? ` (${pullTargets.length})` : ''}
                {/if}
              </button>
            {/if}
          </span>
        </h3>
        <TagGroupList
          {groups}
          {busyTag}
          {mutating}
          fileHash={file?.hash}
          onremove={remove}
          onreject={reject}
          {onsearchtag}
        />

        {#if flow.flash}
          <RejectFlash tag={flow.flash.tag} onundo={undoFlash} />
        {/if}

        {#if flow.reportSent}
          <p class="report-notice" role="status">Report sent</p>
        {/if}

        <RejectedSection hash={file.hash} refreshTick={rejectedSectionTick} onrestore={restore} />

        <form onsubmit={(e) => { e.preventDefault(); add(); }} aria-busy={mutating}>
          <span class="plus">+</span>
          <input
            bind:value={newTag}
            placeholder="add tag..."
            aria-label="add inspector tag"
            aria-disabled={mutating}
          />
          <button type="submit" aria-disabled={mutating}>
            {#if adding}<Spinner size={12} />{:else}Add{/if}
          </button>
        </form>

        <h3 class="section">IDENTITY</h3>
        <IdentityCard {file} />
      {:else}
        <p class="empty">select a file</p>
      {/if}
    </aside>
  {/if}
{/if}

{#if flow.reportOffer}
  <ReportModal
    repo={flow.reportOffer.repo}
    tag={flow.reportOffer.tag}
    onsend={async (note) => {
      if (!flow.reportOffer) return;
      const offer = flow.reportOffer;
      flow.dismissOffer();
      try {
        await sendReport(offer.hash, offer.tag, offer.repo, note);
        flow.notifyReportSent();
      } catch (e) {
        report(e);
      }
    }}
    oncancel={() => { flow.dismissOffer(); }}
  />
{/if}

<style>
  .insp {
    width: 236px;
    flex: none;
    overflow-y: auto;
    background: var(--ink-800);
    border-left: 1px solid var(--line);
    padding: 10px 12px 14px;
  }
  .strip {
    width: 28px;
    flex: none;
    border: 0;
    border-left: 1px solid var(--line);
    background: var(--ink-800);
    color: var(--text-mute);
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  .strip:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .strip:not(:disabled):hover {
    color: var(--accent);
  }
  .head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font: 600 10px/1 var(--mono);
    letter-spacing: 0.1em;
    color: var(--text-faint);
    margin: 4px 0 10px;
  }
  .head h3 {
    margin: 0;
    font: inherit;
    letter-spacing: inherit;
    color: inherit;
  }
  .head .chev {
    width: 22px;
    height: 22px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border: 0;
    background: transparent;
    color: var(--text-mute);
    cursor: pointer;
  }
  .head .chev:hover {
    color: var(--accent);
  }
  .empty {
    font: 500 11.5px/1.4 var(--mono);
    color: var(--text-faint);
  }
  .peek {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .thumb {
    width: 100%;
    aspect-ratio: 1;
    object-fit: cover;
    border-radius: 7px;
    background: var(--ink-800);
    border: 1px solid var(--line);
    display: block;
    opacity: 0;
    transition: opacity 0.4s ease;
  }
  .thumb:global(.loaded) {
    opacity: 1;
  }
  .title-row {
    display: flex;
    gap: 8px;
    align-items: flex-start;
  }
  h2 {
    flex: 1;
    min-width: 0;
    margin: 0;
    font: 500 12px/1.35 var(--mono);
    color: var(--text);
    overflow-wrap: anywhere; /* break-all broke mid-word even when unnecessary (#52) */
  }
  .open {
    flex: none;
    height: 24px;
    padding: 0 8px;
    border: 1px solid var(--line-soft);
    border-radius: 6px;
    background: transparent;
    color: var(--accent);
    font: 600 11px/1 var(--mono);
    cursor: pointer;
  }
  .open:hover {
    border-color: var(--accent);
  }
  .section {
    font: 600 10px/1 var(--mono);
    letter-spacing: 0.1em;
    color: var(--text-faint);
    margin: 16px 0 10px;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .section .loading {
    display: inline-flex;
    align-items: center;
  }
  .sr {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  form {
    margin-top: 10px;
    display: flex;
    align-items: center;
    gap: 7px;
    min-height: 32px;
    padding: 0 9px;
    background: var(--ink-900);
    border: 1px dashed var(--field-dashed);
    border-radius: 7px;
  }
  form:focus-within {
    border-color: var(--accent);
    border-style: solid;
  }
  .plus {
    color: var(--accent);
    font-size: 14px;
  }
  form input {
    flex: 1;
    min-width: 0;
    border: 0;
    background: transparent;
    color: var(--text);
    font: 12px/1 var(--mono);
    outline: none;
  }
  form input::placeholder {
    color: var(--text-mute);
  }
  form input[aria-disabled='true'] {
    opacity: 0.5;
  }
  form button {
    border: 0;
    background: transparent;
    color: var(--accent);
    font-weight: 600;
    cursor: pointer;
  }
  form button[aria-disabled='true'] {
    opacity: 0.5;
    cursor: default;
  }
  .report-notice {
    margin: 4px 0;
    font: 500 11.5px/1.4 var(--mono);
    color: var(--ok);
  }
  /* Groups the OriginVisibilityMenu and pull-remote on the right of the section header. */
  .section-right {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 4px;
  }
  /* shared with DetailView.svelte — extract PullButton.svelte if a third consumer appears */
  .pull-remote {
    background: none;
    border: 0;
    color: var(--text-mute);
    font: 11px/1 var(--mono);
    cursor: pointer;
    padding: 0;
  }
  .pull-remote:hover:not(:disabled) {
    color: var(--accent);
  }
  .pull-remote:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .pull-remote:focus-visible {
    outline: 1px solid var(--accent);
    outline-offset: 2px;
  }
</style>
