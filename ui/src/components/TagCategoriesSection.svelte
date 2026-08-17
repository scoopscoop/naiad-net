<script lang="ts">
  import { categories } from '../lib/categories.svelte';

  interface Props {
    onsaved?: () => void;
  }
  let { onsaved }: Props = $props();

  // Draft text for each category's "add namespace" field, keyed by category id.
  let nsDraft = $state<Record<string, string>>({});

  function saved() {
    onsaved?.();
  }

  function rename(id: string, e: Event) {
    categories.update(id, { name: (e.currentTarget as HTMLInputElement).value });
    saved();
  }
  function recolor(id: string, e: Event) {
    categories.update(id, { color: (e.currentTarget as HTMLInputElement).value });
    saved();
  }
  function addNamespace(id: string) {
    const ns = (nsDraft[id] ?? '').trim().toLowerCase();
    const cat = categories.list.find((c) => c.id === id);
    if (!cat || cat.namespaces.includes(ns)) {
      nsDraft[id] = '';
      return;
    }
    categories.update(id, { namespaces: [...cat.namespaces, ns] });
    nsDraft[id] = '';
    saved();
  }
  function removeNamespace(id: string, ns: string) {
    const cat = categories.list.find((c) => c.id === id);
    if (!cat) return;
    categories.update(id, { namespaces: cat.namespaces.filter((n) => n !== ns) });
    saved();
  }
  function move(id: string, dir: -1 | 1) {
    categories.move(id, dir);
    saved();
  }
  function add() {
    categories.add();
    saved();
  }
  function remove(id: string) {
    categories.remove(id);
    saved();
  }
  function reset() {
    categories.reset();
    saved();
  }

  const nsLabel = (ns: string): string => (ns === '' ? '(general)' : ns);
</script>

<section>
  <h3>Tag categories</h3>
  <p class="hint">
    Group tags in the detail panel. A category claims one or more namespaces; add a blank
    namespace to capture untagged "general" tags. First match wins, top to bottom.
  </p>
  <ul class="cats">
    {#each categories.list as cat, i (cat.id)}
      <li class="cat">
        <div class="cat-head">
          <input
            type="color"
            class="swatch"
            aria-label={`color for ${cat.name}`}
            value={cat.color}
            onchange={(e) => recolor(cat.id, e)} />
          <input
            type="text"
            class="name"
            aria-label={`name for ${cat.name}`}
            value={cat.name}
            onchange={(e) => rename(cat.id, e)} />
          <div class="ord">
            <button
              aria-label={`move ${cat.name} up`}
              disabled={i === 0}
              onclick={() => move(cat.id, -1)}>↑</button>
            <button
              aria-label={`move ${cat.name} down`}
              disabled={i === categories.list.length - 1}
              onclick={() => move(cat.id, 1)}>↓</button>
            <button class="del" aria-label={`delete ${cat.name}`} onclick={() => remove(cat.id)}>✕</button>
          </div>
        </div>
        <div class="ns">
          {#each cat.namespaces as ns (ns)}
            <span class="ns-chip">
              {nsLabel(ns)}
              <button
                aria-label={`remove namespace ${nsLabel(ns)} from ${cat.name}`}
                onclick={() => removeNamespace(cat.id, ns)}>✕</button>
            </span>
          {/each}
          <form class="ns-add" onsubmit={(e) => { e.preventDefault(); addNamespace(cat.id); }}>
            <input
              type="text"
              aria-label={`add namespace to ${cat.name}`}
              placeholder="+ namespace"
              value={nsDraft[cat.id] ?? ''}
              oninput={(e) => (nsDraft[cat.id] = (e.currentTarget as HTMLInputElement).value)} />
          </form>
        </div>
      </li>
    {/each}
  </ul>
  <div class="actions">
    <button class="action-btn" onclick={add}>Add category</button>
    <button class="action-btn" onclick={reset}>Reset to defaults</button>
  </div>
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  h3 {
    margin: 0;
    font: 600 11px/1 var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  .hint {
    margin: 0;
    font: 500 11px/1.5 var(--mono);
    color: var(--text-faint);
  }
  .cats {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .cat {
    padding: 9px 10px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .cat-head {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .swatch {
    width: 26px;
    height: 26px;
    padding: 0;
    border: 1px solid var(--line-soft);
    border-radius: 6px;
    background: transparent;
    cursor: pointer;
  }
  .name {
    flex: 1;
    min-width: 0;
    height: 28px;
    padding: 0 9px;
    border: 1px solid var(--line-soft);
    border-radius: 7px;
    background: var(--ink-800);
    color: var(--text);
    font: 500 12px/1 var(--mono);
    outline: none;
  }
  .name:focus {
    border-color: var(--accent);
  }
  .ord {
    display: flex;
    gap: 4px;
  }
  .ord button {
    width: 26px;
    height: 26px;
    border: 1px solid var(--line-soft);
    border-radius: 6px;
    background: var(--ink-800);
    color: var(--text-mute);
    font: 600 12px/1 var(--mono);
    cursor: pointer;
  }
  .ord button:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }
  .ord button:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .ord .del:hover {
    border-color: var(--err-line);
    color: var(--err);
  }
  .ns {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
    align-items: center;
  }
  .ns-chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    height: 22px;
    padding: 0 8px;
    border-radius: 6px;
    background: var(--chip);
    border: 1px solid var(--chip-line);
    font: 500 11px/1 var(--mono);
    color: var(--text-mute);
  }
  .ns-chip button {
    border: 0;
    background: transparent;
    color: var(--text-faint);
    cursor: pointer;
    padding: 0;
  }
  .ns-chip button:hover {
    color: var(--err);
  }
  .ns-add input {
    height: 22px;
    width: 110px;
    padding: 0 8px;
    border: 1px dashed var(--field-dashed);
    border-radius: 6px;
    background: transparent;
    color: var(--text);
    font: 500 11px/1 var(--mono);
    outline: none;
  }
  .ns-add input:focus {
    border-color: var(--accent);
  }
  .actions {
    display: flex;
    gap: 8px;
  }
  .action-btn {
    height: 30px;
    padding: 0 12px;
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    background: var(--ink-900);
    color: var(--accent);
    font: 600 11px/1 var(--mono);
    cursor: pointer;
  }
  .action-btn:hover {
    border-color: var(--accent);
  }
</style>
