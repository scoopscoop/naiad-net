<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { trapFocus } from '../lib/focus-trap';

  interface Props {
    kind: 'repo' | 'fatal';
    repos?: string[];
    message: string;
    ondismiss: () => void;
  }
  let { kind, repos = [], message, ondismiss }: Props = $props();

  let dismissEl = $state<HTMLButtonElement | undefined>();
  // The element that had focus when this overlay opened; restored on destroy.
  let previousFocus: HTMLElement | null = null;

  const names = $derived(repos.join(', '));
  const plural = $derived(repos.length === 1 ? ' is' : 's are');

  onMount(() => {
    // Capture focus before we move it. Skip if already on body (e.g. a pull
    // button that disabled itself); restoring to body is a no-op at best.
    const active = document.activeElement as HTMLElement | null;
    previousFocus = active !== document.body ? active : null;
    dismissEl?.focus();
  });

  onDestroy(() => {
    // Guard: only restore if the element is still attached to the document.
    // A detached target (removed from DOM before dismiss) must not be focused.
    if (previousFocus && document.contains(previousFocus)) {
      previousFocus.focus();
    }
  });
</script>

<!-- Backdrop dismisses on click. tabindex="-1" keeps it out of the focus ring
     while still being a pointer target (mirrors ReportModal pattern). -->
<button
  class="backdrop"
  onclick={ondismiss}
  aria-label="dismiss pull failure notice"
  tabindex="-1"
></button>

<!-- alertdialog (not dialog): ARIA spec — unsolicited error interruption with no
     decision to make; screen readers announce the body without waiting for focus. -->
<!-- svelte-ignore a11y_interactive_supports_focus -->
<div
  class="modal"
  role="alertdialog"
  aria-modal="true"
  aria-labelledby="pull-failure-title"
  aria-describedby={message ? 'pull-failure-body pull-failure-error' : 'pull-failure-body'}
  tabindex="-1"
  use:trapFocus
  onkeydown={(e) => {
    // Both Escape and Enter dismiss: with one action the mapping is unambiguous
    // and satisfies keyboard-first without modifier gymnastics (#228 decision 4).
    if (e.key === 'Escape' || e.key === 'Enter') {
      e.preventDefault();
      e.stopPropagation();
      ondismiss();
    }
  }}
  onclick={(e) => e.stopPropagation()}
>
  <h2 class="title" id="pull-failure-title">Pull failed</h2>

  <p class="body-copy" id="pull-failure-body">
    {#if kind === 'repo'}
      Could not pull from <span class="em">{names}</span>. The repo{plural} still configured, but this pull returned nothing from it.
    {:else}
      The pull did not complete. No tags were applied.
    {/if}
  </p>

  <!-- Show the daemon's error string verbatim and selectable — the only text
       that distinguishes "auth expired" from "connection refused" (#228 decision 4). -->
  {#if message}
    <p class="error-text" id="pull-failure-error">{message}</p>
  {/if}

  <div class="actions">
    <button class="btn-cta" bind:this={dismissEl} onclick={ondismiss}>Dismiss</button>
  </div>
</div>

<style>
  /* #228: an unsolicited error interrupt must outrank QuickLook (z-40), popovers,
     and the z-50 toast strip so it cannot mount invisible while stealing focus. */
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 59;
    border: 0;
    padding: 0;
    background: var(--overlay-backdrop);
    cursor: default;
  }
  .modal {
    position: fixed;
    z-index: 60;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(520px, 94vw);
    padding: 20px;
    background: var(--ink-800);
    border: 1px solid var(--line);
    border-radius: 11px;
    box-shadow: var(--shadow-modal);
    display: flex;
    flex-direction: column;
    gap: 14px;
    outline: none;
  }
  .title {
    margin: 0;
    font: 600 14px/1 var(--mono);
    color: var(--text);
  }
  .body-copy {
    margin: 0;
    font: 500 11.5px/1.5 var(--mono);
    color: var(--text-mute);
  }
  .em {
    color: var(--accent);
    font-style: normal;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 2px;
  }
  /* Single CTA — Dusk Periwinkle gradient fill */
  .btn-cta {
    height: 32px;
    padding: 0 14px;
    background: var(--accent-grad);
    border: 0;
    border-radius: 8px;
    color: var(--on-accent);
    font: 600 12px/1 var(--mono);
    cursor: pointer;
  }
  .btn-cta:hover {
    filter: brightness(1.06);
  }
  .btn-cta:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .error-text {
    margin: 0;
    padding: 8px 10px;
    background: var(--err-bg);
    border: 1px solid var(--err-line);
    border-radius: 8px;
    color: var(--err);
    font: 500 11.5px/1.4 var(--mono);
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 160px;
    overflow-y: auto;
  }
</style>
