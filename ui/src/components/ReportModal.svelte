<script lang="ts">
  import { onMount } from 'svelte';
  import { trapFocus } from '../lib/focus-trap';

  interface Props {
    repo: string;
    tag: string;
    onsend: (note: string | null) => void;
    oncancel: () => void;
  }
  let { repo, tag, onsend, oncancel }: Props = $props();

  let note = $state('');
  let noteEl = $state<HTMLInputElement | undefined>();

  // Focus the note input when the modal mounts (no inner tick so it fires
  // synchronously within the first onMount microtask, visible to tests).
  onMount(() => {
    noteEl?.focus();
  });

  function handleSend() {
    onsend(note || null);
  }
</script>

<!-- Backdrop dismisses on click (same as SettingsPanel pattern). -->
<button
  class="backdrop"
  onclick={oncancel}
  aria-label="dismiss report dialog"
  tabindex="-1"
></button>

<!-- DESIGN §5 modal: ink-800, 11px radius, --line border, min(520px,94vw), modal shadow. -->
<!-- svelte-ignore a11y_interactive_supports_focus -->
<div
  class="modal"
  role="dialog"
  aria-label={`Report ${tag} to ${repo}?`}
  aria-modal="true"
  tabindex="-1"
  use:trapFocus
  onkeydown={(e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      oncancel();
    }
  }}
  onclick={(e) => e.stopPropagation()}
>
  <h2 class="title">Report {tag} to {repo}?</h2>

  <p class="body-copy">
    Ask <span class="em">{repo}</span>'s moderators to remove <strong class="tag-em">{tag}</strong>
    from this file for everyone. This reveals the file's hash to <span class="em">{repo}</span>.
  </p>

  <div class="field-group">
    <span class="field-label">note (optional)</span>
    <input
      class="note-input"
      aria-label="note (optional)"
      bind:value={note}
      bind:this={noteEl}
      placeholder="optional note"
      onkeydown={(e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          handleSend();
        }
      }}
    />
  </div>

  <div class="actions">
    <button class="btn-cancel" onclick={oncancel}>Cancel</button>
    <button class="btn-cta" onclick={handleSend}>Send report</button>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 9;
    border: 0;
    padding: 0;
    background: var(--overlay-backdrop);
    cursor: default;
  }
  .modal {
    position: fixed;
    z-index: 10;
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
  .tag-em {
    font-weight: 600;
    color: var(--text);
  }
  .field-group {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .field-label {
    font: 600 10px/1 var(--mono);
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  .note-input {
    height: 32px;
    padding: 0 12px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    color: var(--text);
    font: 12px/1 var(--mono);
    outline: none;
  }
  .note-input:focus {
    border-color: var(--accent);
  }
  .note-input::placeholder {
    color: var(--text-faint);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 2px;
  }
  /* Outline accent — the Cancel workhorse */
  .btn-cancel {
    height: 30px;
    padding: 0 12px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    color: var(--accent);
    font: 600 11px/1 var(--mono);
    cursor: pointer;
    transition: border-color 0.15s;
  }
  .btn-cancel:hover {
    border-color: var(--accent);
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

  @media (prefers-reduced-motion: reduce) {
    .modal {
      transition: none;
    }
  }
</style>
