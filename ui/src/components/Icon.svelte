<script lang="ts">
  let { name, size = 16 }: { name: string; size?: number } = $props();

  // Every glyph is drawn in a shared 16-unit space.
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 16 16"
  fill="none"
  stroke="currentColor"
  stroke-width="1.5"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === 'minimize'}
    <line x1="3" y1="8" x2="13" y2="8" />
  {:else if name === 'maximize'}
    <rect x="3.5" y="3.5" width="9" height="9" rx="1.5" />
  {:else if name === 'close'}
    <path d="M3 3 13 13 M13 3 3 13" />
  {:else if name === 'grid'}
    <rect x="2.5" y="2.5" width="11" height="11" rx="1.5" />
    <path d="M8 2.5 V13.5 M2.5 8 H13.5" />
  {:else if name === 'search'}
    <circle cx="7" cy="7" r="4.4" />
    <line x1="10.4" y1="10.4" x2="13.5" y2="13.5" />
  {:else if name === 'settings'}
    <!-- horizontal sliders: three tracks, each with a ring knob -->
    <path d="M2.5 4 H13.5 M2.5 8 H13.5 M2.5 12 H13.5" />
    <circle cx="10.5" cy="4" r="1.9" />
    <circle cx="5.5" cy="8" r="1.9" />
    <circle cx="9.5" cy="12" r="1.9" />
  {:else if name === 'chevron-up'}
    <path d="M3 10 8 5 13 10" />
  {:else if name === 'chevron-down'}
    <path d="M3 6 8 11 13 6" />
  {:else if name === 'chevron-left'}
    <path d="M10 3 5 8 10 13" />
  {:else if name === 'chevron-right'}
    <path d="M6 3 11 8 6 13" />
  {/if}
</svg>
