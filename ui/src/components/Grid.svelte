<script lang="ts">
  import type { FileDto } from '../lib/types';
  import type { ThumbFit } from '../lib/settings.svelte';
  import { loadThumb } from '../lib/load-thumb';
  import { computeGrid, computeWindow, scrollTargetForIndex, anchorForViewport, scrollTopForAnchor, type ScrollAnchor } from '../lib/grid-window';
  import { tilePlaceholder } from '../lib/tile-placeholder';
  import { onDestroy, tick } from 'svelte';
  import { applyClick, bandSelection, rectToIndices, type SelectionState } from '../lib/selection';
  import { contextMenu } from '../lib/context-menu.svelte';
  import { buildTileMenu, buildBackgroundMenu } from '../lib/menu-items';
  interface Props {
    files: FileDto[];
    /** Zoom level = thumbs per row (#171). Tile pixel size is derived from the
     *  measured container width so rows always fill exactly. */
    columns: number;
    /** The app's scroll container (`.content`). Undefined until App mounts. */
    scrollParent: HTMLElement | undefined;
    /** True while a detail tab overlays the grid. Freezes geometry tracking so
     *  layout shifts behind the overlay (the inspector unmounting resizes the
     *  container) can't re-window the grid, remount tiles, or shrink the spacer
     *  and clamp the scroll offset (#60). Re-measures on uncover. */
    covered?: boolean;
    onselect?: (file: FileDto, index: number) => void;
    onfocus?: (file: FileDto, index: number) => void;
    onopen?: (file: FileDto, index: number, background?: boolean) => void;
    focused?: string | null;
    /** Numeric index of the focused file in `files`, or -1 when nothing is
     *  focused. Derived O(1) by App.svelte; used by onGridKey so arrow-key
     *  navigation never scans the full array. */
    focusedIndex?: number;
    /** Committed selection (file hashes) from the gallery tab (#23). */
    selected?: ReadonlySet<string>;
    /** Shift-range anchor hash. */
    anchor?: string | null;
    /** Commit a changed selection back to the tab. */
    onselection?: (next: SelectionState) => void;
    /** Quick Look the given file (menu "Quick Look"). */
    onquicklook?: (file: FileDto, index: number) => void;
    /** Pull remote tags for a set of hashes (menu "Pull tags"). */
    onpulltags?: (hashes: string[]) => void;
    /** Copy hashes to the clipboard (menu "Copy hash(es)"). */
    oncopyhashes?: (hashes: string[]) => void;
    /** Copy filesystem paths to the clipboard (menu "Copy path(s)"). */
    oncopypaths?: (paths: string[]) => void;
    /** Select every file in the active gallery (menu "Select all"). */
    onselectall?: () => void;
    /** Re-run the current tab's search (menu "Refresh"). */
    onrefresh?: () => void;
    /** How thumbnails fill their square tile: 'frame' (contain/letterbox) or 'fill' (cover/crop). */
    fit?: ThumbFit;
  }

  let {
    files,
    columns,
    scrollParent,
    covered = false,
    fit = 'frame',
    onselect = () => {},
    onfocus = () => {},
    onopen = onselect,
    focused = null,
    focusedIndex = -1,
    selected = new Set(),
    anchor = null,
    onselection = () => {},
    onquicklook = () => {},
    onpulltags = () => {},
    oncopyhashes = () => {},
    oncopypaths = () => {},
    onselectall = () => {},
    onrefresh = () => {},
  }: Props = $props();

  // Mirror the grid's CSS so the math and layout cannot drift.
  const GAP = 10;
  const PAD_X = 16; // .grid horizontal padding (each side)
  const PAD_TOP = 14;
  const PAD_BOTTOM = 14;
  const MIN_OVERSCAN = 2; // rows rendered above and below the viewport, at minimum

  // Measured geometry of the scroll container, kept reactive.
  let clientWidth = $state(0);
  let clientHeight = $state(0);
  let scrollTop = $state(0);

  const metrics = $derived(computeGrid(Math.max(0, clientWidth - 2 * PAD_X), columns, GAP));
  // Overscan a full viewport of rows each way: mounted tiles enqueue their
  // thumb fetches, so this pre-warms about a page of scroll in either direction.
  const overscanRows = $derived(
    metrics.rowHeight > 0
      ? Math.max(MIN_OVERSCAN, Math.ceil(clientHeight / metrics.rowHeight))
      : MIN_OVERSCAN,
  );
  const slice = $derived(
    computeWindow(files.length, metrics.columns, metrics.rowHeight, scrollTop - PAD_TOP, clientHeight, overscanRows),
  );
  const visible = $derived(files.slice(slice.startIndex, slice.endIndex));
  // totalHeight counts a trailing gap after the last row; drop it so the spacer
  // is exact, then add the grid's own vertical padding.
  const spacerHeight = $derived(
    (slice.totalHeight > 0 ? slice.totalHeight - GAP : 0) + PAD_TOP + PAD_BOTTOM,
  );

  // Attach observers once the scroll container exists; re-run if its identity
  // changes. rAF-throttle scroll so a fast fling coalesces to <=1 recompute/frame.
  $effect(() => {
    const el = scrollParent;
    if (!el) return;
    // Hidden behind a detail overlay: keep the last-measured geometry and stop
    // observing. The uncover re-runs this effect and re-measures fresh (#60).
    if (covered) return;
    clientWidth = el.clientWidth;
    clientHeight = el.clientHeight;
    scrollTop = el.scrollTop;

    let raf = 0;
    // live gates any pending tick() callback: set to false in the cleanup so a
    // queued restore cannot write scrollTop after the effect is torn down
    // (unmount, scrollParent identity change, covered flipping true).
    let live = true;
    // First-anchor-wins: during a resize burst (panel drag) each RO fire before
    // the pending tick resolves would re-anchor from intermediate geometry and a
    // not-yet-restored scrollTop, drifting away from where the user was.
    // Holding the first anchor and no-oping subsequent fires keeps the restore
    // accurate; the next standalone resize re-captures fresh after the slate is clear.
    let pendingAnchor: ScrollAnchor | null = null;
    const onScroll = () => {
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        scrollTop = el.scrollTop;
      });
    };
    // Three constraints for the ResizeObserver callback:
    // (a) The anchor snapshot MUST be captured before the clientWidth write
    //     because `metrics` is $derived from clientWidth — reading prevMetrics
    //     after the write would give the new geometry, losing the old center row.
    // (b) tick() is required so Svelte re-renders the spacer (new column count →
    //     new totalHeight) before writing scrollTop; without it the browser clamps
    //     the new offset against the stale scrollHeight and the restore is wrong.
    // (c) Pure height changes skip the restore because row offsets did not move —
    //     rewriting scrollTop in that case would only add jitter.
    const ro =
      typeof ResizeObserver !== 'undefined'
        ? new ResizeObserver(() => {
            const prevMetrics = metrics;
            const prevScrollTop = el.scrollTop;
            const prevViewportH = clientHeight;
            clientWidth = el.clientWidth;
            clientHeight = el.clientHeight;
            const next = metrics; // $derived: fresh after the width write
            if (next.columns === prevMetrics.columns && next.rowHeight === prevMetrics.rowHeight) return;
            // First anchor of a resize burst wins: later fires during the same pending
            // restore would re-anchor from intermediate geometry and a not-yet-restored
            // scrollTop, drifting away from where the user actually was.
            if (pendingAnchor === null) {
              pendingAnchor = anchorForViewport(prevMetrics, prevScrollTop, prevViewportH, PAD_TOP, files.length);
              if (pendingAnchor === null) return;
            }
            tick().then(() => {
              if (!live || pendingAnchor === null) return;
              el.scrollTop = scrollTopForAnchor(pendingAnchor, metrics, el.clientHeight, PAD_TOP, files.length);
              scrollTop = el.scrollTop; // mirror the (possibly clamped) real value
              pendingAnchor = null;
            });
          })
        : undefined;
    ro?.observe(el);
    el.addEventListener('scroll', onScroll, { passive: true });
    return () => {
      live = false;
      if (raf) cancelAnimationFrame(raf);
      ro?.disconnect();
      el.removeEventListener('scroll', onScroll);
    };
  });

  // ─── Arrow-key navigation (F2a) ────────────────────────────────────────────

  /** Move app focus by one step in the given direction. Handles grid columns so
   *  up/down skip a whole row. Scrolls the target row into view even when the
   *  target cell is not yet rendered (virtual grid). */
  function onGridKey(e: KeyboardEvent) {
    // Detail tab is showing — it owns arrow keys.
    if (covered) return;
    if (!(e.key === 'ArrowLeft' || e.key === 'ArrowRight' || e.key === 'ArrowUp' || e.key === 'ArrowDown')) return;
    // Let inputs/textareas keep their native cursor movement.
    const tag = (e.target as HTMLElement | null)?.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA') return;
    if (files.length === 0) return;
    const cols = metrics.columns;
    if (cols <= 0) return;

    // O(1): index pre-derived by App.svelte via a hash→index Map.
    const currentIndex = focusedIndex;

    let next: number;
    if (currentIndex === -1) {
      // Nothing focused yet — any arrow starts at the first item.
      next = 0;
    } else {
      const delta =
        e.key === 'ArrowLeft' ? -1
        : e.key === 'ArrowRight' ? 1
        : e.key === 'ArrowUp' ? -cols
        : cols; // ArrowDown
      next = Math.max(0, Math.min(files.length - 1, currentIndex + delta));
    }

    e.preventDefault();

    const file = files[next];
    if (!file) return;

    onfocus(file, next);
    // Unmodified arrow keys re-anchor at the arrived-at item so a subsequent
    // shift-click ranges from there, not from the last mouse click (#110).
    // Selection itself is unchanged; only the anchor moves.
    if (!e.shiftKey && !e.ctrlKey) {
      onselection({ selected, anchor: file.hash });
    }

    // Bring the target row into view (it may not be in the DOM yet).
    if (scrollParent) {
      const target = scrollTargetForIndex(
        next, cols, metrics.rowHeight, PAD_TOP,
        scrollParent.scrollTop, clientHeight,
      );
      if (target !== scrollParent.scrollTop) scrollParent.scrollTop = target;
    }

    // Keep DOM focus alive so keydown events keep flowing even when the
    // previously-focused cell button scrolls out of the render window and
    // unmounts (which would send focus to <body>).
    // If the target cell is currently rendered, focus it directly;
    // otherwise park focus on the viewport (tabindex=0) — the next arrow
    // press will find the cell mounted and focus it.
    if (viewportEl) {
      if (next >= slice.startIndex && next < slice.endIndex) {
        const cells = viewportEl.querySelectorAll<HTMLButtonElement>('.cell');
        const btn = cells[next - slice.startIndex];
        btn?.focus({ preventScroll: true });
      } else {
        viewportEl.focus({ preventScroll: true });
      }
    }
  }

  // ─── Rubber-band drag ───────────────────────────────────────────────────────

  const DRAG_THRESHOLD = 4; // px before a press becomes a band
  const EDGE = 24; // px strip near the container edge that auto-scrolls
  const SCROLL_MAX_STEP = 40; // px per auto-scroll tick, cap
  const SCROLL_GAIN = 0.4; // px scrolled per px past the edge

  // In-flight rubber band, in viewport (content-space) px. `base` is the
  // committed selection the drag started from; the preview merges on top.
  type BandState = {
    x1: number; y1: number; x2: number; y2: number;
    additive: boolean;
    base: ReadonlySet<string>;
    active: boolean;
  };

  let viewportEl = $state<HTMLElement>();
  let wasCovered = false;
  let restoreFocusAfterCover = false;
  let band = $state<BandState | null>(null);
  // The files identity the band started on. Grid stays mounted across tab
  // switches (App swaps the files/selected props) and pointer capture keeps a
  // held drag alive, so a mid-drag identity change (tab switch, new results)
  // invalidates both the band's base snapshot and its hit-space: abandon the
  // band rather than preview/commit stale state. $state.raw keeps the array
  // reference un-proxied so identity comparison against the prop works.
  let bandFiles = $state.raw<FileDto[] | null>(null);

  // rAF buffer for band x2/y2 (F8b): coalesce raw pointermove events so
  // `previewSelected` re-derives at most once per animation frame.
  let pendingBandX2 = 0;
  let pendingBandY2 = 0;
  let bandMoveRaf = 0;

  function flushBandCoords() {
    bandMoveRaf = 0;
    if (!band) return;
    band.x2 = pendingBandX2;
    band.y2 = pendingBandY2;
  }

  const bandLive = $derived(band !== null && bandFiles === files);

  // O(band area) per recompute: hit-testing rescans the full spanned area, so
  // a held edge-auto-scroll drag over thousands of rows pays that each frame.
  // Bounded by drag extent (not viewport); fine for normal drags — revisit if
  // held-drags over huge libraries show measurable cost.
  const previewSelected = $derived.by(() => {
    if (!band?.active || !bandLive) return null;
    const rect = {
      x1: band.x1 - PAD_X, y1: band.y1 - PAD_TOP,
      x2: band.x2 - PAD_X, y2: band.y2 - PAD_TOP,
    };
    return bandSelection(band.base, rectToIndices(rect, metrics, GAP, files.length), files, band.additive);
  });
  const effSelected = $derived(previewSelected ?? selected);

  // Opening always focuses too, so keyboard state stays in sync (#63).
  function open(file: FileDto, index: number, background = false) {
    onfocus(file, index);
    onopen(file, index, background);
  }

  function focus(file: FileDto, index: number) {
    onfocus(file, index);
  }

  function focusGridIndex(index: number) {
    if (!viewportEl) return;
    if (index >= slice.startIndex && index < slice.endIndex) {
      const cells = viewportEl.querySelectorAll<HTMLButtonElement>('.cell');
      const btn = cells[index - slice.startIndex];
      btn?.focus({ preventScroll: true });
    } else {
      viewportEl.focus({ preventScroll: true });
    }
  }

  $effect(() => {
    if (covered && !wasCovered) {
      const active = document.activeElement as HTMLElement | null;
      restoreFocusAfterCover = !!(viewportEl && active && viewportEl.contains(active));
    }
    if (!covered && wasCovered && restoreFocusAfterCover) {
      restoreFocusAfterCover = false;
      const index = focusedIndex;
      tick().then(() => {
        const active = document.activeElement as HTMLElement | null;
        if (covered || index < 0 || (viewportEl && active && viewportEl.contains(active))) return;
        focusGridIndex(index);
      });
    }
    wasCovered = covered;
  });

  // Every real click mutates the selection, file-manager style (#110): a plain
  // click collapses to the tile and anchors there, so the next shift-click
  // ranges from what the user last clicked. Opening is reserved for
  // double-click, middle-click, and keyboard activation (Explorer parity) —
  // a plain click only focuses, regardless of inspector state.
  function cellClick(e: MouseEvent, file: FileDto, index: number) {
    // Band select and select-all leave no anchor; fall back to the focused tile
    // so a shift-click after arrow-key navigation ranges from there.
    const base = { selected, anchor: anchor ?? focused };
    onselection(applyClick(base, index, files, { ctrl: e.ctrlKey, shift: e.shiftKey }));
    if (e.ctrlKey || e.shiftKey) return;
    if (e.detail === 0) open(file, index); // Enter/Space on the cell button
    else focus(file, index);
  }

  // Middle-click opens in the background, browser-style (#63).
  function cellAux(e: MouseEvent, file: FileDto, index: number) {
    if (e.button === 1) {
      e.preventDefault();
      open(file, index, true);
    }
  }

  function cellDouble(file: FileDto, index: number) {
    open(file, index);
  }

  function cellMouseDown(e: MouseEvent) {
    if (e.button === 1) e.preventDefault();
  }

  // ─── Context menus ──────────────────────────────────────────────────────────

  /** Resolve Explorer-style target set for a right-clicked tile (spec §5.3):
   *  the full selection if the tile is in it, otherwise just that tile. */
  function resolveTargets(file: FileDto): FileDto[] {
    if (selected.has(file.hash)) return files.filter((f) => selected.has(f.hash));
    return [file];
  }

  function openTileMenu(
    pos: { x: number; y: number },
    file: FileDto,
    index: number,
    invoker: HTMLElement,
  ) {
    const targets = resolveTargets(file);
    // Explorer: right-clicking an unselected tile collapses the selection to it.
    if (!selected.has(file.hash)) {
      onselection({ selected: new Set([file.hash]), anchor: file.hash });
    }
    const hashes = targets.map((f) => f.hash);
    const paths = targets.map((f) => f.path);
    const menuItems = buildTileMenu(targets, {
      onOpen: () => open(file, index),
      onQuickLook: () => { focus(file, index); onquicklook(file, index); },
      onPullTags: () => onpulltags(hashes),
      onCopyHashes: () => oncopyhashes(hashes),
      onCopyPaths: () => oncopypaths(paths),
    });
    contextMenu.openAt(pos, menuItems, invoker);
  }

  function cellContext(e: MouseEvent, file: FileDto, index: number) {
    e.preventDefault();
    openTileMenu({ x: e.clientX, y: e.clientY }, file, index, e.currentTarget as HTMLElement);
  }

  /** Handle Shift+F10 and the ContextMenu key on a focused cell. */
  function cellMenuKey(e: KeyboardEvent, file: FileDto, index: number) {
    if ((e.key === 'F10' && e.shiftKey) || e.key === 'ContextMenu') {
      e.preventDefault();
      const btn = e.currentTarget as HTMLElement;
      const r = btn.getBoundingClientRect();
      openTileMenu({ x: r.left, y: r.bottom }, file, index, btn);
    }
  }

  /** Right-click on empty grid space — opens the background menu.
   *  Ignored when the event target is inside a tile (which already handled it). */
  function bgContext(e: MouseEvent) {
    if ((e.target as HTMLElement).closest('.cell')) return;
    e.preventDefault();
    const menuItems = buildBackgroundMenu(files.length, {
      onSelectAll: () => onselectall(),
      onRefresh: () => onrefresh(),
    });
    contextMenu.openAt({ x: e.clientX, y: e.clientY }, menuItems, viewportEl);
  }

  function toViewport(e: MouseEvent): { x: number; y: number } {
    // The viewport spans the full content height and scrolls with it, so
    // rect-relative coordinates ARE content-space coordinates.
    const r = viewportEl!.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  }

  function bandStart(e: PointerEvent) {
    if (e.button !== 0 || !viewportEl) return;
    if ((e.target as HTMLElement).closest('.cell')) return; // tiles own their clicks
    e.preventDefault();
    const p = toViewport(e);
    band = {
      x1: p.x, y1: p.y, x2: p.x, y2: p.y,
      additive: e.ctrlKey || e.shiftKey,
      base: selected,
      active: false,
    };
    bandFiles = files;
    try {
      viewportEl.setPointerCapture(e.pointerId);
    } catch {
      // jsdom: no pointer capture; tests drive events directly.
    }
  }

  function bandMove(e: PointerEvent) {
    if (!band) return;
    if (!bandLive) {
      bandCancel();
      return;
    }
    const p = toViewport(e);
    // Buffer x2/y2: commit to reactive state at most once per animation frame
    // so `previewSelected` re-derives at most once per frame (F8b).
    pendingBandX2 = p.x;
    pendingBandY2 = p.y;
    // Check activation threshold synchronously for fast UX.
    if (
      !band.active &&
      (Math.abs(p.x - band.x1) > DRAG_THRESHOLD || Math.abs(p.y - band.y1) > DRAG_THRESHOLD)
    ) {
      band.active = true;
    }
    if (!bandMoveRaf) bandMoveRaf = requestAnimationFrame(flushBandCoords);
    pointerClientY = e.clientY;
    if (band.active) armAutoScroll();
  }

  function bandEnd() {
    if (!band) return;
    if (!bandLive) {
      bandCancel();
      return;
    }
    // Flush any pending rAF so previewSelected sees the final pointer position.
    if (bandMoveRaf) {
      cancelAnimationFrame(bandMoveRaf);
      flushBandCoords();
    }
    stopAutoScroll();
    if (band.active) {
      onselection({ selected: previewSelected ?? band.base, anchor: null });
    } else if (!band.additive && selected.size > 0) {
      onselection({ selected: new Set(), anchor: null }); // plain click on empty space clears
    }
    band = null;
    bandFiles = null;
  }

  function bandCancel() {
    if (bandMoveRaf) {
      cancelAnimationFrame(bandMoveRaf);
      bandMoveRaf = 0;
    }
    stopAutoScroll();
    band = null;
    bandFiles = null;
  }

  // --- edge auto-scroll while a band is active ---
  let pointerClientY = 0;
  let scrollRaf = 0;

  // Unmount mid-drag must not leave any rAF tick armed.
  onDestroy(() => {
    stopAutoScroll();
    if (bandMoveRaf) cancelAnimationFrame(bandMoveRaf);
  });

  function armAutoScroll() {
    if (!scrollRaf) scrollRaf = requestAnimationFrame(autoScrollTick);
  }
  function stopAutoScroll() {
    if (scrollRaf) cancelAnimationFrame(scrollRaf);
    scrollRaf = 0;
  }
  function autoScrollTick() {
    scrollRaf = 0;
    const el = scrollParent;
    if (!el || !band?.active || !bandLive || !viewportEl) return;
    const r = el.getBoundingClientRect();
    if (r.height <= 0) return; // jsdom does no layout: never auto-scroll there
    let dy = 0;
    if (pointerClientY < r.top + EDGE) dy = pointerClientY - (r.top + EDGE);
    else if (pointerClientY > r.bottom - EDGE) dy = pointerClientY - (r.bottom - EDGE);
    if (dy === 0) return;
    const before = el.scrollTop;
    el.scrollTop = before + Math.max(-SCROLL_MAX_STEP, Math.min(SCROLL_MAX_STEP, dy * SCROLL_GAIN));
    if (el.scrollTop === before) return; // hit an end; re-armed by the next pointermove
    // The content scrolled under a stationary pointer: refresh the moving
    // corner so the band keeps tracking content space, then keep going.
    const vr = viewportEl.getBoundingClientRect();
    band.y2 = pointerClientY - vr.top;
    scrollRaf = requestAnimationFrame(autoScrollTick);
  }
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<!-- Rubber-band selection is pointer-only by design; the keyboard path is
     ctrl+A / Esc handled at the app level. Arrow keys are handled here.
     tabindex="0" lets the viewport hold DOM focus when the focused cell
     scrolls out of the render window and its button unmounts. -->
<main
  class="grid-viewport"
  tabindex="0"
  style="height: {spacerHeight}px"
  bind:this={viewportEl}
  onkeydown={onGridKey}
  oncontextmenu={bgContext}
  onpointerdown={bandStart}
  onpointermove={bandMove}
  onpointerup={bandEnd}
  onpointercancel={bandCancel}
>
  <div
    class="grid"
    style="grid-template-columns: repeat({metrics.columns}, minmax(0, 1fr)); transform: translateY({slice.offsetY + PAD_TOP}px)"
  >
    {#each visible as file, i (file.hash)}
      <button
        class="cell"
        class:selected={effSelected.has(file.hash)}
        class:focused={focused === file.hash && !effSelected.has(file.hash)}
        style="background: {tilePlaceholder(file.hash)}"
        onclick={(e) => cellClick(e, file, slice.startIndex + i)}
        onauxclick={(e) => cellAux(e, file, slice.startIndex + i)}
        ondblclick={() => cellDouble(file, slice.startIndex + i)}
        onmousedown={cellMouseDown}
        oncontextmenu={(e) => cellContext(e, file, slice.startIndex + i)}
        onkeydown={(e) => cellMenuKey(e, file, slice.startIndex + i)}
        onfocus={() => focus(file, slice.startIndex + i)}
        title={file.name}
      >
        <img
          class="thumb"
          class:fill={fit === 'fill'}
          decoding="async"
          alt={file.name}
          use:loadThumb={file.hash}
        />
      </button>
    {/each}
  </div>
  {#if band?.active && bandLive}
    <div
      class="band"
      style="left: {Math.min(band.x1, band.x2)}px; top: {Math.min(band.y1, band.y2)}px; width: {Math.abs(band.x2 - band.x1)}px; height: {Math.abs(band.y2 - band.y1)}px"
    ></div>
  {/if}
</main>

<style>
  /* The viewport is a full-height spacer sized to every row so the scrollbar
     reflects the whole result set; only the visible block is rendered inside it,
     absolutely positioned and translated into place. */
  .grid-viewport {
    position: relative;
    min-height: 100%;
    background: var(--ink-850);
    /* Suppress the default browser focus ring on the viewport itself. The
       visual keyboard-navigation indicator is the .focused cell outline; the
       viewport only holds focus as a fallback when the focused cell is
       off-screen and not yet rendered. */
    outline: none;
  }
  .grid {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    display: grid;
    gap: 10px;
    padding: 0 16px;
    align-content: start;
  }
  .cell {
    position: relative;
    border: 0;
    padding: 0;
    border-radius: 7px;
    overflow: hidden;
    cursor: pointer;
    aspect-ratio: 1;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.4);
    transition:
      outline-color 0.1s,
      transform 0.1s;
    outline: 2.5px solid transparent;
    outline-offset: -2.5px;
  }
  .cell:hover {
    transform: translateY(-1px);
  }
  .cell:focus-visible {
    outline-color: var(--accent);
  }
  .cell.selected {
    outline-color: var(--accent);
  }
  .cell.focused {
    outline-color: color-mix(in srgb, var(--accent) 62%, transparent);
  }
  /* Tint overlay so selection reads even where the focus ring sits. */
  .cell.selected::after {
    content: '';
    position: absolute;
    inset: 0;
    background: color-mix(in srgb, var(--accent) 18%, transparent);
    pointer-events: none;
  }
  .thumb {
    width: 100%;
    height: 100%;
    object-fit: contain;
    display: block;
    opacity: 0;
    transition: opacity 0.4s ease;
    background: var(--ink-900);
  }
  .thumb.fill {
    object-fit: cover;
  }
  .thumb:global(.loaded) {
    opacity: 1;
  }
  .band {
    position: absolute;
    border: 1px solid var(--accent);
    background: color-mix(in srgb, var(--accent) 12%, transparent);
    pointer-events: none;
  }
</style>
