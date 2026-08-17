<script lang="ts">
  import type { FileDto } from '../lib/types';
  import { createPending } from '../lib/pending.svelte';
  import { loadStageImage } from '../lib/stage-loader';
  import Spinner from './Spinner.svelte';
  import { FIT, zoomAbout, wheelFactor, clampPan, toggleZoom, type View } from '../lib/zoompan';

  interface Props {
    file: FileDto;
    hasPrev: boolean;
    hasNext: boolean;
    onprev: () => void;
    onnext: () => void;
    position?: { index: number; total: number } | null;
  }
  let { file, hasPrev, hasNext, onprev, onnext, position = null }: Props = $props();

  let stage = $state<HTMLDivElement>();
  let view = $state<View>({ ...FIT });
  let dragging = $state(false);
  // Cached rect for wheel/zoom. ResizeObserver refreshes it on element resize
  // (rare). A scroll listener on window marks it dirty instead of re-reading
  // immediately — TagDrawer/Inspector can scroll while ImageStage is mounted,
  // so a direct getBoundingClientRect per scroll tick would move the hot-path
  // cost rather than remove it. The lazy re-read happens at most once per wheel
  // burst that follows a scroll.
  // Plain `let` — only imperative handlers read/write these; making them
  // $state would fire a signal write on every scroll tick for no reactive gain.
  let stageRect: DOMRect | null = null;
  let rectDirty = false;
  // Drag-start snapshot: rect captured once at pointerdown so pointermove
  // never touches the layout engine at up to 120 Hz.
  let start = { x: 0, y: 0, panX: 0, panY: 0, stageW: 0, stageH: 0 };

  // Eagerly refresh the cached rect (used by ResizeObserver and on-demand by
  // onWheel when the dirty flag is set).
  function refreshRect() {
    if (!stage) return;
    stageRect = stage.getBoundingClientRect();
    rectDirty = false;
  }

  /** Load feedback driven by the stage-loader action.
   *  The action calls onLoadStart/onLoadEnd in a balanced 1:1 fashion,
   *  including for superseded cycles, so createPending's refcount stays at 0/1. */
  const loadPending = createPending();

  const onLoadStart = () => loadPending.start();
  const onLoadEnd = () => loadPending.end();

  // Reset zoom/pan whenever the shown file changes (in-place navigation).
  $effect(() => {
    file.hash; // track
    view = { ...FIT };
  });

  // Drop pending timers on unmount so they never fire into a dead component.
  $effect(() => () => loadPending.reset());

  // Prime the rect cache and keep it current.
  // ResizeObserver calls refreshRect directly (resize is rare; immediate read
  // is fine). The scroll listener only marks dirty — onWheel does the lazy read.
  $effect(() => {
    const el = stage;
    if (!el) return;

    refreshRect(); // initial measurement

    // jsdom (unit tests) has no ResizeObserver; guard matches Grid.svelte pattern.
    const ro =
      typeof ResizeObserver !== 'undefined'
        ? new ResizeObserver(refreshRect)
        : undefined;
    ro?.observe(el);

    const markDirty = () => { rectDirty = true; };
    window.addEventListener('scroll', markDirty, { capture: true, passive: true });

    return () => {
      ro?.disconnect();
      window.removeEventListener('scroll', markDirty, { capture: true });
    };
  });

  function onWheel(e: WheelEvent) {
    // Lazily recompute after a scroll (rect.left/top may have shifted).
    if (rectDirty || !stageRect) refreshRect();
    if (!stageRect) return;
    e.preventDefault();
    const cx = e.clientX - stageRect.left - stageRect.width / 2;
    const cy = e.clientY - stageRect.top - stageRect.height / 2;
    view = zoomAbout(view, wheelFactor(e.deltaY), cx, cy);
  }

  function onPointerDown(e: PointerEvent) {
    if (e.button !== 0 || !stage) return;
    dragging = true;
    // Fresh BCR here (not refreshRect/stageRect) so the drag starts from
    // guaranteed-current dimensions without overwriting the shared rect cache
    // with drag-only values that would corrupt wheel zoom until the next resize.
    const rect = stage.getBoundingClientRect();
    start = {
      x: e.clientX,
      y: e.clientY,
      panX: view.panX,
      panY: view.panY,
      stageW: rect.width,
      stageH: rect.height,
    };
    stage.setPointerCapture(e.pointerId);
  }

  function onPointerMove(e: PointerEvent) {
    if (!dragging) return;
    // Use the dimensions captured at drag-start — no layout read at move time.
    view = {
      ...view,
      panX: clampPan(start.panX + (e.clientX - start.x), start.stageW, view.scale),
      panY: clampPan(start.panY + (e.clientY - start.y), start.stageH, view.scale),
    };
  }

  function endDrag(e: PointerEvent) {
    dragging = false;
    try {
      stage?.releasePointerCapture(e.pointerId);
    } catch {
      /* pointer already released */
    }
  }

  function stopDrag(e: PointerEvent) {
    // Keep arrow clicks from starting a pan.
    e.stopPropagation();
  }
</script>

<div
  class="stage"
  class:dragging
  bind:this={stage}
  role="img"
  aria-label={file.name}
  onwheel={onWheel}
  onpointerdown={onPointerDown}
  onpointermove={onPointerMove}
  onpointerup={endDrag}
  onpointercancel={endDrag}
  ondblclick={() => (view = toggleZoom(view))}
>
  <img
    class="media"
    use:loadStageImage={{ hash: file.hash, onLoadStart, onLoadEnd }}
    alt={file.name}
    draggable="false"
    decoding="async"
    style="transform: translate({view.panX}px, {view.panY}px) scale({view.scale})"
  />

  {#if loadPending.busy}
    <div class="loading" role="status" aria-label="loading image">
      <Spinner size={14} />
    </div>
  {/if}

  {#if hasPrev}
    <button class="nav prev" aria-label="previous image" onpointerdown={stopDrag} onclick={onprev}>‹</button>
  {/if}
  {#if hasNext}
    <button class="nav next" aria-label="next image" onpointerdown={stopDrag} onclick={onnext}>›</button>
  {/if}
  {#if position}
    <div class="position">{position.index + 1} / {position.total}</div>
  {/if}
</div>

<style>
  .stage {
    position: relative;
    height: 100%;
    border-radius: 8px;
    background: var(--ink-900);
    overflow: hidden;
    display: flex;
    align-items: center;
    justify-content: center;
    touch-action: none;
    cursor: grab;
  }
  .stage.dragging {
    cursor: grabbing;
  }
  .media {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    transform-origin: center;
    will-change: transform;
    user-select: none;
    -webkit-user-drag: none;
    transition: opacity 0.15s;
    /* Hidden until the stage-loader paints a real src. On a fresh mount (every
       tab switch — App wraps DetailView in {#key tab.id}) the srcless <img>
       would otherwise flash the browser's broken-image icon over the alt text.
       In-place cycling keeps the previous frame because .ready is never removed. */
    opacity: 0;
  }
  /* .ready and .preview are toggled imperatively by the stage-loader action, so
     :global is needed to avoid the unused-selector lint warning. */
  :global(.media.ready) {
    opacity: 1;
  }
  /* Preview = upscaled thumbnail while the full /file fetch is in flight. */
  :global(.media.preview) {
    filter: blur(1px);
  }
  .loading {
    position: absolute;
    right: 10px;
    bottom: 10px;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 6px;
    border-radius: 999px;
    background: rgba(20, 18, 14, 0.68);
  }
  .nav {
    position: absolute;
    top: 50%;
    transform: translateY(-50%);
    width: 36px;
    height: 56px;
    display: flex;
    align-items: center;
    justify-content: center;
    border: 0;
    border-radius: 8px;
    background: rgba(20, 18, 14, 0.55);
    color: var(--text);
    font-size: 26px;
    line-height: 1;
    cursor: pointer;
  }
  .nav:hover {
    background: rgba(20, 18, 14, 0.8);
    color: var(--accent);
  }
  .nav:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .nav.prev {
    left: 8px;
  }
  .nav.next {
    right: 8px;
  }
  .position {
    position: absolute;
    left: 10px;
    bottom: 10px;
    padding: 4px 8px;
    border-radius: 999px;
    background: rgba(20, 18, 14, 0.68);
    color: var(--text-dim);
    font: 600 11px/1 var(--mono);
  }
</style>
