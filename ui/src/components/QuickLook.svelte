<script lang="ts">
  import { tick, onMount, onDestroy } from 'svelte';
  import type { FileDto } from '../lib/types';
  import ImageStage from './ImageStage.svelte';
  import { trapFocus } from '../lib/focus-trap';

  interface Props {
    file: FileDto;
    onclose: () => void;
  }
  let { file, onclose }: Props = $props();

  let frameEl = $state<HTMLElement | null>(null);
  // The element that had focus when this overlay opened; restored on destroy.
  let previousFocus: HTMLElement | null = null;

  onMount(async () => {
    previousFocus = document.activeElement as HTMLElement | null;
    await tick();
    frameEl?.focus();
  });

  onDestroy(() => {
    if (previousFocus && document.contains(previousFocus)) {
      previousFocus.focus();
    }
  });
</script>

<!-- Quick-look (#63): a transient full-window peek at the focused image.
     No tags, no tab - Space/Escape dismiss, Enter promotes to a detail tab.
     Key handling lives in App's global hotkey listener; this component only
     renders and handles scrim clicks. -->
<div class="scrim" role="presentation" onclick={onclose}>
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- Key events are handled by App's global window listener; this onclick is stopPropagation only. -->
  <div
    class="frame"
    role="dialog"
    aria-label={`quick look: ${file.name}`}
    aria-modal="true"
    tabindex="-1"
    bind:this={frameEl}
    use:trapFocus
    onclick={(e) => e.stopPropagation()}
  >
    <ImageStage {file} hasPrev={false} hasNext={false} onprev={() => {}} onnext={() => {}} />
  </div>
</div>

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 40;
    background: rgba(10, 9, 7, 0.82);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .frame {
    width: min(92vw, 1600px);
    height: 90vh;
  }
</style>
