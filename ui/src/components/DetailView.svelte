<script lang="ts">
  import type { FileDto, TagDetail } from '../lib/types';
  import { tagsDetailed, addTags, removeTags, report as sendReport } from '../lib/api';
  import { createRejectFlow } from '../lib/reject-flow.svelte';
  import { createPullRemote } from '../lib/pull-remote.svelte';
  import { pullFailure } from '../lib/pull-failure.svelte';
  import { groupByCategory } from '../lib/namespace';
  import { categories } from '../lib/categories.svelte';
  import { view } from '../lib/settings.svelte';
  import ImageStage from './ImageStage.svelte';
  import TagGroupList from './TagGroupList.svelte';
  import IdentityCard from './IdentityCard.svelte';
  import TagDrawer from './TagDrawer.svelte';
  import Spinner from './Spinner.svelte';
  import RejectedSection from './RejectedSection.svelte';
  import ReportModal from './ReportModal.svelte';
  import RejectFlash from './RejectFlash.svelte';
  import { onDestroy } from 'svelte';

  interface Props {
    file: FileDto;
    onerror: (message: string) => void;
    hasPrev?: boolean;
    hasNext?: boolean;
    onprev?: () => void;
    onnext?: () => void;
    position?: { index: number; total: number } | null;
    /** Opens a new gallery tab searching for the given tag (spec §6.2). */
    onsearchtag: (tag: string) => void;
  }

  let {
    file,
    onerror,
    hasPrev = false,
    hasNext = false,
    onprev = () => {},
    onnext = () => {},
    position = null,
    onsearchtag,
  }: Props = $props();

  let tags = $state<TagDetail[]>([]);
  let groups = $derived(groupByCategory(tags, categories.list, (t) => t.tag));
  let paneEl = $state<HTMLElement>();
  let paneH = $state(0);
  let newTag = $state('');
  /** The mutation in flight for a given file hash: the tag whose row is busy, or
   *  `null` for the add-tag form. Keyed by hash because a mutation outlives the
   *  file that started it — the parent can page to the next image while the
   *  request is still running. */
  let inflight = $state<Record<string, string | null>>({});
  /** Monotonic across every `tagsDetailed` call this component makes. A load only
   *  writes `tags` if no later load has started, so an older response for the
   *  same hash — the file-change effect's, overtaken by a mutation's refresh —
   *  cannot resurrect the tag the mutation just removed. */
  let requestId = 0;

  /** Bumped after reject or restore to tell RejectedSection to re-fetch. */
  let rejectedSectionTick = $state(0);

  const flow = createRejectFlow({
    refresh: async () => { await refresh(file.hash); },
    onRejectedChanged: () => { rejectedSectionTick += 1; },
  });

  function begin(hash: string, tag: string | null) {
    inflight[hash] = tag;
  }
  function end(hash: string) {
    delete inflight[hash];
  }

  /** One mutation at a time *per file*: every mutator refreshes on completion,
   *  and a second click on a tag's `x` while the first remove is in flight would
   *  issue a duplicate remove. Controls are marked `aria-disabled` rather than
   *  `disabled` so the one the user just activated keeps focus. */
  const mutating = $derived(file.hash in inflight);
  const busyTag = $derived(inflight[file.hash] ?? null);
  const adding = $derived(mutating && busyTag === null);
  const busy = $derived(mutating);

  function report(e: unknown) {
    onerror(e instanceof Error ? e.message : String(e));
  }

  // Pull-remote acts on the single displayed file — targets is always [file.hash].
  // originHash is re-read after every await so a navigation away suppresses stale
  // result flashes (origin-hash suppression contract from pull-remote.svelte.ts).
  const pull = createPullRemote({
    targets: () => [file.hash],
    originHash: () => file.hash,
    refresh: () => refresh(file.hash),
    report,
  });

  /** A rejected action must never be silent: a number input commits on `change`,
   *  which fires on blur, so a click on Add or a remove `x` can land on a guard
   *  the user never saw take hold. */
  function rejectBusy() {
    report(new Error('Another change is still saving. Try again in a moment.'));
  }

  async function load(hash: string, seq: number) {
    try {
      const next = await tagsDetailed(hash, view.localOnly);
      if (seq === requestId && file.hash === hash) tags = next;
    } catch (e) {
      if (seq === requestId && file.hash === hash) report(e);
    }
  }

  async function refresh(hash: string) {
    requestId += 1;
    await load(hash, requestId);
  }

  // Reload tags whenever the shown file changes. Read `file.hash` first so the
  // effect tracks it, then re-fetch - guarding against a stale response winning
  // if the parent swaps `file` again before this resolves.
  $effect(() => {
    const hash = file.hash;
    void view.localOnly;
    newTag = '';
    flow.clearFlash();
    flow.dismissOffer();
    requestId += 1;
    load(hash, requestId);
  });

  // Arrow-key navigation across the gallery snapshot. Re-bound when the handlers
  // change so the listener always calls the current tab's prev/next. Ignored
  // while a text field is focused so arrows still move the caret in "add tag".
  $effect(() => {
    void onprev;
    void onnext;
    function onKey(e: KeyboardEvent) {
      const tag = (e.target as HTMLElement | null)?.tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA') return;
      // #228: the failure notice is modal — arrows must not navigate behind it.
      if (pullFailure.current !== null) return;
      if (e.key === 'ArrowRight') onnext();
      else if (e.key === 'ArrowLeft') onprev();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  $effect(() => {
    const el = paneEl;
    if (!el) return;
    paneH = el.clientHeight;
    if (typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(() => {
      paneH = el.clientHeight;
    });
    ro.observe(el);
    return () => ro.disconnect();
  });

  async function add() {
    const tag = newTag.trim();
    if (!tag) return;
    if (busy) return rejectBusy();
    const hash = file.hash;
    begin(hash, null);
    try {
      await addTags(hash, [tag]);
      if (file.hash !== hash) return;
      newTag = '';
      await refresh(hash);
    } catch (e) {
      if (file.hash === hash) report(e);
    } finally {
      end(hash);
    }
  }

  async function remove(tag: string) {
    if (busy) return rejectBusy();
    const hash = file.hash;
    begin(hash, tag);
    try {
      await removeTags(hash, [tag]);
      if (file.hash !== hash) return;
      await refresh(hash);
    } catch (e) {
      if (file.hash === hash) report(e);
    } finally {
      end(hash);
    }
  }

  /** Reject a pulled mapping — follows the remove() serialisation contract exactly.
   *  busy guard → begin/end wrap → flow.reject handles rejectTag + refresh + flash. */
  async function reject(tag: string, services: string[]) {
    if (busy) return rejectBusy();
    const hash = file.hash;
    begin(hash, tag);
    try {
      await flow.reject(tag, services, hash);
    } catch (e) {
      if (file.hash === hash) report(e);
    } finally {
      end(hash);
    }
  }

  /** Undo the most recent reject — delegates to flow which clears offer synchronously. */
  async function undoFlash() {
    await flow.undoFlash();
  }

  onDestroy(() => { flow.destroy(); pull.destroy(); });

  /** Restore a rejection from the RejectedSection — follows the remove() contract. */
  async function restore(tag: string, service: string) {
    if (busy) return rejectBusy();
    const hash = file.hash;
    begin(hash, tag);
    try {
      await flow.restore(tag, service, hash);
    } catch (e) {
      if (file.hash === hash) report(e);
    } finally {
      end(hash);
    }
  }

  // Esc dismisses the flash WITHOUT undoing. While the report modal is open,
  // Esc is handled by the modal itself; the flash handler stands down.
  $effect(() => {
    if (!flow.flash) return;
    return flow.attachEsc();
  });


</script>

<section class="pane" bind:this={paneEl}>
  <div class="stage-wrap">
    <ImageStage {file} {hasPrev} {hasNext} {onprev} {onnext} {position} />
  </div>
  <TagDrawer name={file.name} tagCount={tags.length} paneHeight={paneH}>
    <h2 title={file.name}>{file.name}</h2>

    <h3 class="section between">
      <span>TAGS · {tags.length}</span>
      <div class="section-actions">
        {#if pull.repoCount > 0}
          <button
            class="pull-remote"
            onclick={pull.run}
            disabled={pull.pulling}
            aria-label="pull remote tags">
            {#if pull.result}{pull.result}{:else if pull.pulling}pulling…{:else}pull remote{/if}
          </button>
        {/if}
      </div>
    </h3>
    <TagGroupList
      {groups}
      {busyTag}
      mutating={busy}
      fileHash={file.hash}
      onremove={remove}
      onreject={reject}
      {onsearchtag}
    />

    {#if flow.flash}
      <!-- Transient "Rejected {tag} · Undo" flash — FLASH_MS auto-dismiss, Esc dismisses
           without undoing, Undo calls undoReject per service then refreshes. -->
      <RejectFlash tag={flow.flash.tag} onundo={undoFlash} />
    {/if}

    {#if flow.reportSent}
      <p class="report-notice" role="status">Report sent</p>
    {/if}

    <RejectedSection hash={file.hash} refreshTick={rejectedSectionTick} onrestore={restore} />
    <form onsubmit={(e) => { e.preventDefault(); add(); }} aria-busy={busy}>
      <span class="plus">+</span>
      <input
        bind:value={newTag}
        placeholder="add tag..."
        aria-label="add tag"
        aria-disabled={busy}
      />
      <button type="submit" aria-disabled={busy}>
        {#if adding}<Spinner size={12} />{:else}Add{/if}
      </button>
    </form>

    <h3 class="section">IDENTITY</h3>
    <IdentityCard {file} />
    <p class="note">identity is the bytes - renaming or moving never re-imports</p>
  </TagDrawer>
</section>

{#if flow.reportOffer}
  <!-- Report modal — DESIGN §5. Appears only when exactly one service was rejected
       AND that repo reported reports=true. Cancelling leaves the rejection standing. -->
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
  .pane {
    height: 100%;
    display: flex;
    flex-direction: column;
    background: var(--ink-800);
  }
  .stage-wrap {
    flex: 1;
    min-height: 0;
    padding: 10px 16px;
  }
  h2 {
    margin: 0 0 1rem;
    font: 500 13px/1.3 var(--mono);
    color: var(--text);
    overflow-wrap: anywhere; /* break-all broke mid-word even when unnecessary (#52) */
  }
  .section {
    font: 600 10px/1 var(--mono);
    letter-spacing: 0.1em;
    color: var(--text-faint);
    margin: 18px 0 10px;
    /* reset h3 browser defaults when used as a section label */
    font-weight: 600;
  }
  .section.between {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .note {
    margin: 8px 2px 0;
    font: 500 11px/1.5 var(--mono);
    color: var(--text-faint);
  }
  form {
    margin-top: 10px;
    display: flex;
    align-items: center;
    gap: 7px;
    height: 32px;
    padding: 0 11px;
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
  .rate {
    margin-top: 8px;
    padding: 9px 11px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    display: flex;
    flex-direction: column;
    gap: 7px;
  }
  .rate-head {
    font: 600 11px/1 var(--mono);
    color: var(--text);
  }
  .section-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .report-notice {
    margin: 6px 0;
    font: 500 11.5px/1.4 var(--mono);
    color: var(--ok);
  }
  /* shared with Inspector.svelte — extract PullButton.svelte if a third consumer appears */
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
