<script lang="ts">
  import { tick } from 'svelte';
  import type { FileDto, ScanSummary } from './lib/types';
  import { getGallerySort, listRoots, search, setGallerySort } from './lib/api';
  import { quoteTagForQuery } from './lib/completion';
  import { runPullTags } from './lib/pull-runner';
  import { pullFailure } from './lib/pull-failure.svelte';
  import { DEFAULT_SORT, saveSort, sortFilesCached } from './lib/gallery-sort';
  import { matchHotkey, type HotkeyAction } from './lib/hotkeys';
  import { focusSearch } from './lib/search-focus';
  import { selectedSubset, type SelectionState } from './lib/selection';
  import { view } from './lib/settings.svelte';
  import { tabs, type GalleryTab } from './lib/tabs.svelte';
  import { thumbQueue, THUMB_LANES, THUMB_LANES_COVERED } from './lib/thumb-queue';
  import { createPending, type Pending } from './lib/pending.svelte';
  import { catchup } from './lib/catchup.svelte';
  import TitleBar from './components/TitleBar.svelte';
  import SearchBar from './components/SearchBar.svelte';
  import NavRail from './components/NavRail.svelte';
  import Inspector from './components/Inspector.svelte';
  import Grid from './components/Grid.svelte';
  import DetailView from './components/DetailView.svelte';
  import QuickLook from './components/QuickLook.svelte';
  import ContextMenu from './components/ContextMenu.svelte';
  import TagRelationsPopover from './components/TagRelationsPopover.svelte';
  import PullFailureModal from './components/PullFailureModal.svelte';

  let error = $state<string | null>(null);
  let quickLook = $state(false);
  let notice = $state<string | null>(null);
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;

  function setNotice(msg: string, ms = 4000) {
    clearTimeout(noticeTimer);
    notice = msg;
    noticeTimer = setTimeout(() => (notice = null), ms);
  }
  // Covered grid → narrow the shared thumb queue so cold-cache generations
  // can't pin the origin's connection pool under the detail image (#55 kept the
  // grid mounted; its in-flight thumb loads keep running behind the overlay).
  $effect(() => {
    thumbQueue.setMaxConcurrent(
      tabs.activeDetail !== null || quickLook ? THUMB_LANES_COVERED : THUMB_LANES,
    );
  });

  let sortChangedThisSession = false;
  // null = fetch not yet settled (prevents fresh-install panel flashing before first result) (#52)
  let roots = $state<string[] | null>(null);

  // Sorted rows of the displayed gallery — the active one, or the one kept
  // mounted behind a detail tab. Memoized on (files, sort) identity so a tab
  // flip is a cache hit: re-sorting reads every row through the deep-reactivity
  // proxy, ~1s at 100k files (#55).
  const sortedFiles = $derived.by(() => {
    const g = tabs.displayGallery;
    return g ? sortFilesCached(g.files, g.sort) : [];
  });
  // O(1) hash→file and hash→index Maps so focus lookups don't scan 95k entries
  // on every focus change (F8a). Both Maps rebuild only on query/sort change
  // because sortedFiles (the plain array from sortFilesCached) is their key.
  const sortedFilesMap = $derived(new Map(sortedFiles.map((f) => [f.hash, f])));
  const sortedFilesIndexMap = $derived(new Map(sortedFiles.map((f, i) => [f.hash, i])));
  const focusedFile = $derived.by(() => {
    const hash = tabs.displayGallery?.focused;
    return hash ? (sortedFilesMap.get(hash) ?? null) : null;
  });
  /** Numeric index of the focused file in sortedFiles, or -1 when nothing is
   *  focused. Passed to Grid so onGridKey can do an O(1) lookup instead of
   *  scanning the full files array on every arrow keypress. */
  const focusedIndex = $derived.by(() => {
    const hash = tabs.displayGallery?.focused;
    return hash ? (sortedFilesIndexMap.get(hash) ?? -1) : -1;
  });

  // Per-tab request bookkeeping. A tab may have several searches in flight —
  // a fast one can land before a slow earlier one — so each response checks it
  // is still the latest before writing. Without this the grid can end up
  // showing the results of a query the user has already replaced.
  const searchSeq = new Map<number, number>();
  const searchPending = new Map<number, Pending>();

  function pendingFor(tab: GalleryTab): Pending {
    let p = searchPending.get(tab.id);
    if (!p) {
      const id = tab.id;
      // The flag lives on the tab so SearchBar and Grid can read it without
      // reaching into this map. The lookup is guarded: reset() fires onchange
      // and the tab may already have been closed.
      p = createPending((busy) => {
        const t = tabs.list.find((x) => x.id === id);
        if (t?.kind === 'gallery') t.loading = busy;
      });
      searchPending.set(id, p);
    }
    return p;
  }

  async function runSearch(tab: GalleryTab, q: string) {
    tab.query = q;
    const seq = (searchSeq.get(tab.id) ?? 0) + 1;
    searchSeq.set(tab.id, seq);
    const pending = pendingFor(tab);
    pending.start();
    try {
      const res = await search(q, view.localOnly);
      // Stale: a newer search on this tab has already been issued.
      if (searchSeq.get(tab.id) !== seq) return;
      const target = tabs.list.find((t) => t.id === tab.id);
      if (target?.kind !== 'gallery') return;
      target.files = res;
      // New results invalidate any selection made over the old ones.
      target.selected = new Set();
      target.anchor = null;
      target.focused = null;
      if (tabs.activeId === target.id) error = null;
    } catch (e) {
      if (searchSeq.get(tab.id) !== seq) return;
      if (tabs.activeId === tab.id) error = e instanceof Error ? e.message : String(e);
    } finally {
      // Refcounted: end() for a stale request decrements, but the flag only
      // clears once the latest one has landed too.
      pending.end();
    }
  }

  function refreshGalleries() {
    for (const t of tabs.list) {
      if (t.kind === 'gallery') runSearch(t, t.query);
    }
  }

  async function copyText(text: string) {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      error = 'copy to clipboard failed';
    }
  }

  // Guard against concurrent grid context-menu pulls: a second invocation while
  // one is already in-flight would start a duplicate job and double the
  // refreshGalleries() call. Cleared in onSettled so a new pull can begin after
  // the current one completes or errors.
  let gridPulling = false;

  function pullTagsForHashes(hashes: string[]) {
    if (hashes.length === 0 || gridPulling) return;
    gridPulling = true;
    runPullTags({
      hashes,
      onResult: (line) => setNotice(line),
      onSettled: (ok) => {
        gridPulling = false;
        if (ok) refreshGalleries();
      },
    });
  }

  // Re-fetch watched roots; failed fetch → treat as none so empty state doesn't wedge (#52).
  function refreshRoots() {
    listRoots().then((r) => { roots = r; }).catch(() => { roots = []; });
  }

  function onimported(summary: ScanSummary) {
    const skipped = summary.errors.length;
    setNotice(`indexed ${summary.imported}${skipped ? ` · ${skipped} skipped` : ''}`);
    refreshGalleries();
    refreshRoots();
  }

  // A watched root was removed; re-run so its images drop out everywhere.
  function onremoved() {
    refreshGalleries();
    refreshRoots();
  }

  // The local-only scope toggled; re-run under the new scope.
  function onrescope() {
    refreshGalleries();
  }

  // A detail-pane error lives in this same shared strip, so switching or closing
  // tabs must clear it. Tracking the previous id preserves search errors raised
  // while already on the gallery, including the initial load below.
  let lastTabId = tabs.activeId;
  $effect(() => {
    if (tabs.activeId !== lastTabId) {
      lastTabId = tabs.activeId;
      error = null;
    }
  });

  // Tabs are closable; without this the maps grow for the life of the session
  // and a closed tab's in-flight search keeps a hold timer armed.
  $effect(() => {
    const live = new Set(tabs.list.map((t) => t.id));
    for (const id of [...searchPending.keys()]) {
      if (!live.has(id)) {
        searchPending.get(id)!.reset();
        searchPending.delete(id);
        searchSeq.delete(id);
      }
    }
  });

  // `tabs` outlives this component, so a delay timer still armed at teardown
  // would later flip a surviving tab's `loading` on with no end() to clear it.
  $effect(() => () => {
    for (const p of searchPending.values()) p.reset();
    searchPending.clear();
    searchSeq.clear();
  });

  // The content region scrolls the gallery; detail tabs overlay it with their
  // own scroll pane, so opening/closing a detail never moves the grid (#55).
  // Record the active gallery tab's offset as it moves; when the *displayed*
  // gallery changes (gallery↔gallery switch), restore the incoming offset.
  let contentEl = $state<HTMLElement>();
  let lastGalleryId = tabs.displayGallery?.id;

  $effect(() => {
    const el = contentEl;
    if (!el) return;
    const onScroll = () => {
      const g = tabs.activeGallery;
      if (g) g.scrollTop = el.scrollTop;
    };
    el.addEventListener('scroll', onScroll, { passive: true });
    return () => el.removeEventListener('scroll', onScroll);
  });

  $effect(() => {
    const g = tabs.displayGallery;
    if (!g || g.id === lastGalleryId) return;
    lastGalleryId = g.id;
    const el = contentEl;
    const target = g.scrollTop;
    tick().then(() => {
      if (el) el.scrollTop = target;
    });
  });

  function newGalleryTab() {
    const g = tabs.openGallery();
    runSearch(g, '');
  }

  /** Open a new gallery tab pre-searched for the given tag (spec §6.2). */
  function searchWithTag(tag: string) {
    const g = tabs.openGallery();
    runSearch(g, quoteTagForQuery(tag));
  }

  // Single open path for grid gestures, Enter, and the inspector's Open
  // button. Honors the #20 contract: if the target is part of the selection,
  // the detail tab walks the selected subset only.
  function openFromGrid(f: FileDto, i: number, background = false) {
    const g = tabs.displayGallery;
    if (!g) return;
    // Only a *multi*-file selection narrows the detail sequence. Since a plain
    // click now selects the tile it focuses (#110), a size-1 selection is just
    // "the current file" and must still open against the whole gallery.
    if (g.selected.size > 1 && g.selected.has(f.hash)) {
      const subset = selectedSubset(sortedFiles, g.selected);
      tabs.openDetail(subset, subset.findIndex((x) => x.hash === f.hash), { background });
    } else {
      // A size-1 selection of the file being opened is just "the current file"
      // (a plain click sets it) — leave it alone; anything else is stale.
      if (g.selected.size > 0 && !g.selected.has(f.hash) && !background) {
        // Foreground opens drop a stale selection; background opens are
        // deliberately non-disruptive and leave it alone.
        g.selected = new Set();
        g.anchor = null;
      }
      tabs.openDetail(sortedFiles, i, { background });
    }
  }

  function openFocused() {
    if (!focusedFile) return;
    const index = sortedFiles.findIndex((f) => f.hash === focusedFile.hash);
    // focusedFile derives from sortedFiles, so index is >= 0; guard is belt-and-braces.
    if (index >= 0) openFromGrid(focusedFile, index);
  }

  function dispatchHotkey(action: HotkeyAction) {
    if (action.kind === 'cycle') tabs.cycle(action.dir);
    else if (action.kind === 'close-tab') tabs.close(tabs.activeId);
    else if (action.kind === 'new-gallery') newGalleryTab();
    else if (action.kind === 'activate-index') tabs.activateAt(action.n);
    else if (action.kind === 'activate-last') tabs.activateLast();
    else if (action.kind === 'focus-search') {
      if (!tabs.activeGallery) {
        const g = tabs.list.find((t) => t.kind === 'gallery');
        if (g) tabs.activate(g.id);
      }
      tick().then(() => focusSearch());
    } else if (action.kind === 'escape') {
      // Escape is contextual: quick-look, else detail tab, else selection.
      if (quickLook) {
        quickLook = false;
      } else {
        const d = tabs.activeDetail;
        if (d) {
          tabs.close(d.id);
        } else {
          const g = tabs.activeGallery;
          if (g && g.selected.size > 0) {
            g.selected = new Set();
            g.anchor = null;
          }
        }
      }
    } else if (action.kind === 'open-focused') {
      if (tabs.activeDetail) return;
      // Enter promotes an open quick-look to a real detail tab.
      quickLook = false;
      openFocused();
    } else if (action.kind === 'quick-look') {
      quickLook = !quickLook;
    } else if (action.kind === 'select-all') {
      const g = tabs.activeGallery;
      if (g) g.selected = new Set(sortedFiles.map((f) => f.hash));
    }
  }

  // Global hotkeys (#27). DetailView keeps its own arrow-key handler; this
  // listener claims only browser-style tab chords and Escape.
  $effect(() => {
    function onKey(e: KeyboardEvent) {
      // #228: the failure notice is modal; app hotkeys are inert until dismissed.
      if (pullFailure.current !== null) return;
      const target = e.target as HTMLElement | null;
      const tag = target?.tagName;
      const inEditable = tag === 'INPUT' || tag === 'TEXTAREA';
      const action = matchHotkey(e, inEditable);
      if (!action) return;
      // Native activation owns Enter on buttons and Space on non-grid buttons.
      // Grid-cell Space is quick-look; letting the button synthesize a click
      // would open detail instead.
      const isGridCell = target?.closest?.('.cell') !== null;
      if (action.kind === 'open-focused' && tag === 'BUTTON') return;
      if (action.kind === 'quick-look' && tag === 'BUTTON' && !isGridCell) return;
      // Space keeps page-scrolling when there's nothing to peek at.
      if (action.kind === 'quick-look' && !quickLook && (!focusedFile || tabs.activeDetail)) return;
      e.preventDefault();
      dispatchHotkey(action);
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  // Close the quick-look overlay when focus evaporates (e.g. a new search clears `focused`).
  $effect(() => {
    if (!focusedFile) quickLook = false;
  });

  // Desktop loads the UI from the daemon's chosen port, so browser localStorage
  // can be tied to a different origin on each launch. The DB-backed preference
  // is authoritative once the daemon is reachable; localStorage remains the
  // immediate fallback while that request is in flight or unavailable.
  $effect(() => {
    getGallerySort()
      .then((sort) => {
        if (sortChangedThisSession) return;
        for (const t of tabs.list) {
          if (t.kind === 'gallery') t.sort = { ...sort };
        }
        saveSort(sort);
      })
      .catch(() => {
        // Non-fatal: the tab store already initialized from the local fallback.
      });
  });

  // Narrow-window auto-collapse (audit F9).
  // narrowWindow is true when the viewport is < 700px (distinct from the NavRail
  // hide breakpoint at 450px); it acts as a floor for the inspector's collapsed
  // state — the user's manual preference is preserved in view.inspectorCollapsed
  // and returns when the window widens again.
  let narrowWindow = $state(false);
  $effect(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return;
    const mq = window.matchMedia('(max-width: 700px)');
    narrowWindow = mq.matches;
    function onChange(e: MediaQueryListEvent) {
      narrowWindow = e.matches;
    }
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  });

  // Layer B (#119): stream freshly-indexed files into the gallery while the
  // startup catch-up scan runs. Refresh aggressively only while the active
  // gallery is (near-)empty — reshuffling a populated, sorted grid mid-scroll
  // is worse than waiting — plus one final refresh when the scan completes.
  // `lastScan*` are plain (non-reactive) locals so updating them here never
  // re-triggers this effect; the effect re-runs on store/tab/files changes.
  const CATCHUP_REFRESH_THRESHOLD = 50;
  let lastScanImported = -1;
  let lastScanComplete = false;
  $effect(() => {
    const s = catchup.status;
    if (!s) return;
    const active = tabs.list.find((t) => t.id === tabs.activeId);
    const fewFiles =
      active?.kind === 'gallery' && active.files.length < CATCHUP_REFRESH_THRESHOLD;
    const importedAdvanced = s.imported !== lastScanImported;
    const justCompleted = s.complete && !lastScanComplete;
    lastScanImported = s.imported;
    lastScanComplete = s.complete;
    if (s.imported === 0) return; // nothing indexed yet (or a warm/empty start)
    if (s.running && importedAdvanced && fewFiles) {
      refreshGalleries();
    } else if (justCompleted) {
      refreshGalleries();
    }
  });

  // Initial load: every file into the starting gallery tab.
  refreshGalleries();
  refreshRoots();
</script>

<div class="app">
  <TitleBar />

  <!-- Fixed overlay toasts: float above layout so they never shift the grid (#52).
       Error first (actionable) then notice. z-50 sits above quicklook (z-40). -->
  <div class="toasts">
    {#if error}
      <div class="toast error" role="alert">
        {error}
        <button onclick={() => (error = null)} aria-label="dismiss">×</button>
      </div>
    {/if}
    {#if notice}
      <div class="toast notice" role="status">✓ {notice}</div>
    {/if}
  </div>

  <div class="body">
    <NavRail
      activeQuery={tabs.activeGallery?.query ?? tabs.displayGallery?.query ?? ''}
      onrun={(q) => {
        const g = tabs.activeGallery ?? tabs.displayGallery;
        if (g) {
          tabs.activate(g.id);
          runSearch(g, q);
        }
      }}
      onerror={(m) => (error = m)}
    />
    <div class="center">
      <SearchBar
        count={sortedFiles.length}
        selectedCount={tabs.activeGallery?.selected.size ?? 0}
        level={view.zoomLevel}
        sort={tabs.activeGallery?.sort ?? DEFAULT_SORT}
        disabled={!tabs.activeGallery}
        loading={tabs.activeGallery?.loading ?? false}
        tabId={tabs.activeId}
        query={tabs.activeGallery?.query ?? ''}
        onsearch={(q) => {
          const g = tabs.activeGallery;
          if (g) runSearch(g, q);
        }}
        onzoom={(n) => (view.zoomLevel = n)}
        onsort={(s) => {
          const g = tabs.activeGallery;
          if (g) {
            sortChangedThisSession = true;
            g.sort = s;
            saveSort(s);
            void setGallerySort(s).catch(() => {
              // Keep the in-memory/local fallback value; the next successful
              // sort change will retry DB persistence.
            });
          }
        }}
        onsearchtag={searchWithTag}
        {onimported}
        {onremoved}
        {onrescope}
      />
      <div class="center-body">
        <!-- The grid stays mounted while a detail tab is open, hidden but with its
             layout and scroll intact, so closing the detail reveals existing DOM
             instead of remounting and re-decoding every thumbnail (#55). `covered`
             also freezes the grid's geometry: hiding the inspector widens this
             container while the detail tab is up, and reacting to that resize
             would re-window tiles and clamp the scroll offset (#60). -->
        <div
          class="content"
          class:covered={tabs.activeDetail !== null}
          class:loading={tabs.displayGallery?.loading ?? false}
          data-scroll
          bind:this={contentEl}
        >
          {#if tabs.displayGallery}
            {@const g = tabs.displayGallery}
            {#if !g.loading && sortedFiles.length === 0}
              <!-- Empty-state panel — only when gallery exists, not loading, and truly empty.
                   The !loading guard prevents "no results" from flashing mid-fetch (#52). -->
              {@const query = (g.query ?? '').trim()}
              {#if query}
                <div class="empty-state">
                  <p class="es-primary">no matches for "{query}"</p>
                </div>
              {:else if roots !== null && roots.length === 0}
                <div class="empty-state">
                  <p class="es-primary">no folders indexed yet</p>
                  <p class="es-secondary">add a folder in settings</p>
                </div>
              {:else if roots !== null}
                <div class="empty-state">
                  <p class="es-primary">no files yet</p>
                  <p class="es-secondary">scanned folders are empty or hidden</p>
                </div>
              {/if}
            {:else}
              <Grid
                files={sortedFiles}
                columns={view.zoomLevel}
                fit={view.thumbFit}
                scrollParent={contentEl}
                covered={tabs.activeDetail !== null || quickLook}
                selected={g.selected}
                anchor={g.anchor}
                focused={g.focused}
                {focusedIndex}
                onselection={(s: SelectionState) => {
                  g.selected = s.selected;
                  g.anchor = s.anchor;
                }}
                onfocus={(f) => {
                  g.focused = f.hash;
                }}
                onopen={openFromGrid}
                onquicklook={(f) => { g.focused = f.hash; quickLook = true; }}
                onpulltags={pullTagsForHashes}
                oncopyhashes={(hashes) => copyText(hashes.join('\n'))}
                oncopypaths={(paths) => copyText(paths.join('\n'))}
                onselectall={() => { g.selected = new Set(sortedFiles.map((f) => f.hash)); g.anchor = null; }}
                onrefresh={() => runSearch(g, g.query)}
              />
            {/if}
          {/if}
        </div>
        {#if tabs.activeDetail}
          {@const tab = tabs.activeDetail}
          {#key tab.id}
            <div class="detail-pane">
              <DetailView
                file={tab.file}
                hasPrev={tab.index > 0}
                hasNext={tab.index < tab.files.length - 1}
                onprev={() => tabs.prev()}
                onnext={() => tabs.next()}
                onerror={(m) => (error = m)}
                position={{ index: tab.index, total: tab.files.length }}
                onsearchtag={searchWithTag}
              />
            </div>
          {/key}
        {/if}
      </div>
    </div>
    <Inspector
      file={focusedFile}
      onopen={openFocused}
      onerror={(m) => (error = m)}
      hidden={tabs.activeDetail !== null}
      {narrowWindow}
      selectedHashes={[...(tabs.displayGallery?.selected ?? new Set())]}
      onsearchtag={searchWithTag}
    />
  </div>
  <ContextMenu />
  <TagRelationsPopover onsearchtag={searchWithTag} />
  {#if quickLook && focusedFile}
    <QuickLook file={focusedFile} onclose={() => (quickLook = false)} />
  {/if}
  <!-- #228: a failed pull against a configured repo is raised here, once for the
       whole app, so all three pull entry points get the same blocking notice. -->
  {#if pullFailure.current}
    <PullFailureModal
      {...pullFailure.current}
      ondismiss={() => pullFailure.dismiss()}
    />
  {/if}
</div>

<style>
  /* App shell: a fixed-height, non-scrolling column. The title and search bars
     are fixed chrome; only .content scrolls, so the window controls and tabs
     never scroll out of reach and each view owns its own scroll position. */
  .app {
    height: 100vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .body {
    flex: 1;
    min-height: 0;
    display: flex;
  }
  .center {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  /* Positioning context for the detail overlay; the gallery scroller and the
     detail pane are siblings so each owns its own scroll position (#55). The
     overlay covers only the center column; rail and inspector stay visible. */
  .center-body {
    flex: 1;
    min-height: 0;
    position: relative;
  }
  .content {
    height: 100%;
    overflow-y: auto;
  }
  /* visibility (not display) keeps the hidden grid's layout and scroll offset
     alive while skipping paint and hit-testing. */
  .content.covered {
    visibility: hidden;
  }
  /* Stale results stay legible but visibly inert while the next page loads.
     Opacity only — the motion rules forbid animating layout. */
  .content.loading {
    opacity: 0.55;
    transition: opacity 120ms linear;
  }
  .detail-pane {
    position: absolute;
    inset: 0;
    overflow: hidden;
    background: var(--ink-800);
  }
  /* Fixed overlay for notices and errors — detached from layout so the grid
     scroll offset is never disturbed when they appear (#52).
     z-50 sits above quicklook (z-40); pointer-events:none on the container
     keeps the gallery clickable except directly over a toast card. */
  .toasts {
    position: fixed;
    top: 57px; /* titlebar 48px + 1px border + 8px gap */
    right: 12px;
    z-index: 50;
    display: flex;
    flex-direction: column;
    gap: 6px;
    pointer-events: none;
  }
  .toast {
    pointer-events: auto;
    padding: 0.5rem 0.75rem;
    border-radius: 8px; /* popover radius — toasts float */
    font: 500 12px/1.3 var(--mono);
    box-shadow: var(--shadow-popover);
    max-width: min(360px, calc(100vw - 24px));
    overflow-wrap: anywhere;
    opacity: 1;
    transition: opacity 0.15s linear;
  }
  .toast.notice {
    background: color-mix(in srgb, var(--ok) 16%, var(--ink-800));
    border: 1px solid var(--ok-line);
    color: var(--text);
  }
  .toast.error {
    background: var(--err-bg);
    border: 1px solid var(--err-line);
    color: var(--err);
    display: flex;
    justify-content: space-between;
    gap: 1rem;
  }
  .toast.error button {
    border: 0;
    background: transparent;
    color: var(--err);
    cursor: pointer;
    flex: none;
  }
  /* Empty-state panel — centered in the gallery viewport, mono, quiet (#52). */
  .empty-state {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 4px;
  }
  .es-primary {
    margin: 0;
    font: 500 12px/1.3 var(--mono);
    color: var(--text-mute);
    overflow-wrap: anywhere;
    text-align: center;
  }
  .es-secondary {
    margin: 0;
    font: 500 11px/1.3 var(--mono);
    color: var(--text-faint);
  }
</style>
