<script lang="ts">
  /** Decorative busy ring. The accessible name belongs to the caller's
   *  `role="status"` container — a spinner announces nothing on its own. */
  interface Props {
    /** 12 or 14. Chrome type is capped at 14px (the Contact-Sheet Rule). */
    size?: number;
  }
  let { size = 12 }: Props = $props();
</script>

<span class="spinner" aria-hidden="true" style="--sz: {size}px"></span>

<style>
  .spinner {
    display: inline-block;
    flex: none;
    width: var(--sz);
    height: var(--sz);
    border: 1.5px solid var(--line-soft);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: spin 0.7s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  /* app.css already clamps animation-duration globally; this also stops the
     frozen ring from reading as a solid arc. */
  @media (prefers-reduced-motion: reduce) {
    .spinner {
      animation: none;
      opacity: 0.55;
    }
  }
</style>
