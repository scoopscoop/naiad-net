<script lang="ts">
  import type { NamespaceSuggestion } from '../lib/types';
  import { listNamespaces, getAppVersion } from '../lib/api';
  import { loadSaved, saveSaved, type SavedSearch } from '../lib/rail-saved';

  interface Props {
    activeQuery: string;
    onrun: (query: string) => void;
    onerror: (message: string) => void;
  }
  let { activeQuery, onrun, onerror }: Props = $props();

  let saved = $state<SavedSearch[]>(loadSaved());
  let namespaces = $state<NamespaceSuggestion[]>([]);
  let appVersion = $state<string | null>(null);

  const CAT_VARS = ['--cat-rose', '--cat-sage', '--cat-peri', '--cat-butter', '--cat-lilac', '--cat-sky'];
  function dot(i: number): string {
    return `var(${CAT_VARS[i % CAT_VARS.length]})`;
  }

  $effect(() => {
    listNamespaces()
      .then((ns) => (namespaces = ns))
      .catch((e) => onerror(e instanceof Error ? e.message : String(e)));
  });

  $effect(() => {
    getAppVersion().then((v) => { appVersion = v; }).catch(() => {});
  });

  const canPin = $derived(
    activeQuery.trim() !== '' && !saved.some((s) => s.query === activeQuery.trim()),
  );

  function pinCurrent() {
    const q = activeQuery.trim();
    if (!q || saved.some((s) => s.query === q)) return;
    saved = [...saved, { name: q, query: q }];
    saveSaved(saved);
  }

  function unpin(query: string) {
    saved = saved.filter((s) => s.query !== query);
    saveSaved(saved);
  }
</script>

<nav class="rail" aria-label="navigation rail">
  <div class="section between">
    <h3 class="section-label">SAVED</h3>
    {#if canPin}
      <button class="pin" onclick={pinCurrent} aria-label="pin current search">+ pin</button>
    {/if}
  </div>
  <button class="row" class:active={activeQuery === ''} onclick={() => onrun('')}>
    <span class="dot" style="background: var(--text-faint)"></span>
    <span class="label">all media</span>
  </button>
  {#each saved as s, i (s.query)}
    <div class="row-wrap">
      <button class="row" class:active={activeQuery === s.query} onclick={() => onrun(s.query)}>
        <span class="dot" style="background: {dot(i)}"></span>
        <span class="label" title={s.query}>{s.name}</span>
      </button>
      <button class="rm" onclick={() => unpin(s.query)} aria-label={`unpin ${s.name}`}>x</button>
    </div>
  {/each}

  <h3 class="section">NAMESPACES</h3>
  {#each namespaces as ns, i (ns.namespace)}
    <button
      class="row"
      class:active={activeQuery === `${ns.namespace}:*`}
      onclick={() => onrun(`${ns.namespace}:*`)}
    >
      <span class="dot" style="background: {dot(i)}"></span>
      <span class="label">{ns.namespace}</span>
      <span class="count">{ns.tag_count.toLocaleString('en-US')}</span>
    </button>
  {/each}

  {#if appVersion}
    <div class="version-stamp" aria-label="app version">{appVersion}</div>
  {/if}
</nav>

<style>
  .rail {
    width: 180px;
    flex: none;
    overflow-y: auto;
    background: var(--ink-800);
    border-right: 1px solid var(--line);
    padding: 10px 8px;
  }
  /* Below 450px the rail hides entirely; the gallery fills the full width.
     Auto-restores when the window widens — no toggle button needed.
     (Distinct from App.svelte's 700px breakpoint, which collapses the inspector.) */
  @media (max-width: 450px) {
    .rail {
      display: none;
    }
  }
  .section {
    font: 600 10px/1 var(--mono);
    letter-spacing: 0.1em;
    color: var(--text-faint);
    margin: 14px 6px 8px;
  }
  /* h3 used as section label inside .section.between — reset browser heading defaults */
  .section-label {
    margin: 0;
    font: inherit;
    letter-spacing: inherit;
    color: inherit;
  }
  /* h3.section used standalone — the font: shorthand resets ALL UA h3 defaults
     (size, family, line-height, weight); margin is already set by .section above */
  h3.section {
    font: 600 10px/1 var(--mono);
  }
  .section.between {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .pin {
    border: 0;
    background: transparent;
    color: var(--text-faint);
    font: 500 10px/1 var(--mono);
    cursor: pointer;
    letter-spacing: 0;
  }
  .pin:hover {
    color: var(--accent);
  }
  .pin:focus-visible,
  .rm:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .row-wrap {
    display: flex;
    align-items: center;
  }
  .row-wrap .row {
    flex: 1;
    min-width: 0;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 7px;
    width: 100%;
    height: 26px;
    padding: 0 6px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-dim);
    font: 500 12px/1 var(--mono);
    cursor: pointer;
    text-align: left;
  }
  .row:hover {
    background: var(--raise);
    color: var(--text);
  }
  .row.active {
    background: var(--raise);
    color: var(--accent);
  }
  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex: none;
  }
  .label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .count {
    flex: none;
    color: var(--text-faint);
    font-size: 10.5px;
  }
  .rm {
    border: 0;
    background: transparent;
    color: var(--text-faint);
    cursor: pointer;
    padding: 0 4px;
    visibility: hidden;
    font-size: 10px;
  }
  .row-wrap:hover .rm {
    visibility: visible;
  }
  .rm:focus-visible {
    visibility: visible;
  }
  .rm:hover {
    color: var(--accent);
  }
  .version-stamp {
    margin-top: 12px;
    padding: 0 6px 4px;
    font: 500 10px/1 var(--mono);
    color: var(--text-faint);
    user-select: none;
  }
</style>
