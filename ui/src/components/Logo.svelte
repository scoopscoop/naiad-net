<script lang="ts">
  let { size = 18, cut = 'full' }: { size?: number; cut?: 'full' | 'small' } = $props();
</script>

{#if cut === 'small'}
  <svg width={size} height={size} viewBox="0 0 100 100" fill="none" aria-hidden="true">
    <path
      d="M11 60 C 24 38 40 38 50 51 C 60 64 76 64 89 42"
      stroke="var(--accent)"
      stroke-width="11.5"
      stroke-linecap="round"
    />
    <circle cx="33" cy="42" r="12" fill="var(--accent)" />
    <circle cx="67" cy="60" r="12" fill="var(--accent)" />
  </svg>
{:else}
  <svg width={size} height={size} viewBox="0 0 100 100" fill="none" aria-hidden="true">
    <path
      d="M7 60 C 23 34 41 34 50 51 C 59 68 77 68 93 42"
      stroke="var(--accent)"
      stroke-width="8.5"
      stroke-linecap="round"
    />
    <circle cx="33" cy="40.5" r="9" fill="var(--accent)" />
    <circle cx="67" cy="61.5" r="9" fill="var(--accent)" />
  </svg>
{/if}
