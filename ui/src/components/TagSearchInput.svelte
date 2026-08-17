<script lang="ts">
  import { tick } from 'svelte';
  import { completeTags } from '../lib/api';
  import { lastToken, stripQuotes, applyCompletion, mergeNamespaces, type CompletionPick } from '../lib/completion';
  import { registerSearchFocus } from '../lib/search-focus';
  import { categories } from '../lib/categories.svelte';
  import { categoryOf, OTHER_COLOR } from '../lib/namespace';
  import { view } from '../lib/settings.svelte';
  import type { NamespaceSuggestion, TagSuggestion } from '../lib/types';
  import { contextMenu } from '../lib/context-menu.svelte';
  import { buildTagMenu } from '../lib/menu-items';
  import Icon from './Icon.svelte';
  import Spinner from './Spinner.svelte';

  interface Props {
    disabled?: boolean;
    initial?: string;
    onsearch: (q: string) => void;
    /** Open a new gallery tab searching for a tag (suggestion-row catalog menu). */
    onsearchtag: (tag: string) => void;
  }
  let { disabled = false, initial = '', onsearch, onsearchtag }: Props = $props();

  let query = $state('');
  let nsSuggestions = $state<NamespaceSuggestion[]>([]);
  let tagSuggestions = $state<TagSuggestion[]>([]);
  let open = $state(false);
  let highlight = $state(-1);
  let busy = $state(false);
  let inputEl: HTMLInputElement;

  type Row =
    | { kind: 'namespace'; namespace: string; count: number }
    | { kind: 'tag'; namespace: string; subtag: string; count: number; alias_source?: string | null };

  const rows = $derived<Row[]>([
    ...nsSuggestions.map(
      (n): Row => ({ kind: 'namespace', namespace: n.namespace, count: n.tag_count }),
    ),
    ...tagSuggestions.map(
      (t): Row => ({ kind: 'tag', namespace: t.namespace, subtag: t.subtag, count: t.count, alias_source: t.alias_source }),
    ),
  ]);

  let debounceId: ReturnType<typeof setTimeout> | undefined;
  let controller: AbortController | undefined;
  let graceId: ReturnType<typeof setTimeout> | undefined;

  $effect(() => {
    query = initial;
  });

  $effect(() => {
    registerSearchFocus(() => inputEl?.focus());
    return () => registerSearchFocus(null);
  });

  const categoryNamespaceList = $derived(
    categories.list.flatMap((c) => c.namespaces).filter((ns) => ns !== ''),
  );

  // Suggestion rows use onmousedown + e.preventDefault() to prevent focus steal,
  // so clicking a row never blurs the input.  onblur fires only on Tab-away or
  // click-outside — both cases should close immediately, no timeout needed.
  function closeDropdown() {
    open = false;
    highlight = -1;
    nsSuggestions = [];
    tagSuggestions = [];
    controller?.abort();
    clearTimeout(graceId);
    busy = false;
  }

  async function refresh(token: string) {
    controller?.abort();
    clearTimeout(graceId);
    // Quotes are phrase delimiters, not literal characters: `"some` queries as
    // `some` so multi-word tags are discoverable through typeahead (#49).
    const fragment = stripQuotes(token);
    if (fragment === '') {
      closeDropdown();
      return;
    }
    const ac = new AbortController();
    controller = ac;
    // Grace period: only paint the spinner if the request is still in-flight
    // after 150ms. Warm-cache responses resolve in < 150ms and never flicker.
    // The `controller === ac` guard prevents a stale grace from painting for a
    // request that has already been superseded.
    graceId = setTimeout(() => { if (controller === ac) busy = true; }, 150);
    let res;
    try {
      res = await completeTags(fragment, 20, ac.signal, view.completionMatch);
    } catch {
      // Identity guard: only clear busy/graceId for the current request.
      // A stale request rejecting must not disturb the newer in-flight spinner.
      if (controller === ac) {
        clearTimeout(graceId);
        busy = false;
      }
      if (!ac.signal.aborted) closeDropdown();
      return;
    }
    if (ac.signal.aborted) return;
    if (controller === ac) {
      clearTimeout(graceId);
      busy = false;
    }
    const wantNs = !fragment.includes(':');
    nsSuggestions = wantNs
      ? mergeNamespaces(fragment, res.namespaces, categoryNamespaceList)
      : [];
    tagSuggestions = res.tags;
    open = nsSuggestions.length + tagSuggestions.length > 0;
    highlight = -1;
  }

  function onInput() {
    clearTimeout(debounceId);
    if (query === '') {
      // Native ✕ or manual delete to empty → behave like Enter on an empty query.
      closeDropdown();
      onsearch('');
      return;
    }
    const { token } = lastToken(query);
    debounceId = setTimeout(() => refresh(token), 120);
  }

  function submit() {
    clearTimeout(debounceId);
    closeDropdown();
    onsearch(query);
  }

  async function complete(row: Row) {
    const pick: CompletionPick =
      row.kind === 'namespace'
        ? { kind: 'namespace', namespace: row.namespace }
        : { kind: 'tag', namespace: row.namespace, subtag: row.subtag };
    query = applyCompletion(query, pick);
    if (row.kind === 'namespace') {
      // Keep choosing: load subtag suggestions for the new `ns:` token.
      await tick();
      inputEl?.focus();
      refresh(lastToken(query).token);
    } else {
      closeDropdown();
      inputEl?.focus();
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (!open && rows.length > 0) {
        open = true;
        highlight = 0;
      } else if (rows.length > 0) {
        highlight = (highlight + 1) % rows.length;
      }
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (rows.length > 0) highlight = (highlight - 1 + rows.length) % rows.length;
    } else if (e.key === 'Escape') {
      if (open || busy) {
        e.preventDefault();
        closeDropdown();
      }
    } else if (e.key === 'Tab') {
      if (open && highlight >= 0) {
        e.preventDefault();
        complete(rows[highlight]);
      }
    } else if (e.key === 'Enter') {
      // With a highlight, complete and swallow the keystroke; otherwise let the
      // form's submit handler run (submit()).
      if (open && highlight >= 0) {
        e.preventDefault();
        complete(rows[highlight]);
      }
    }
  }

  function dotColor(row: { namespace: string; subtag: string }): string {
    return categoryOf(fullTag(row), categories.list)?.color ?? OTHER_COLOR;
  }

  function rowKey(row: Row): string {
    return row.kind === 'namespace'
      ? `ns:${row.namespace}`
      : `tag:${row.namespace}:${row.subtag}`;
  }

  function fullTag(row: { namespace: string; subtag: string }): string {
    return row.namespace ? `${row.namespace}:${row.subtag}` : row.subtag;
  }

  function rowContext(e: MouseEvent, row: Row) {
    if (row.kind !== 'tag') { e.preventDefault(); return; } // namespace rows get no menu
    e.preventDefault();
    const tag = fullTag(row);
    const items = buildTagMenu(tag, 'both', 'catalog', false, {
      onSearch: () => onsearchtag(tag),
      onCopy: () => {
        void navigator.clipboard?.writeText(tag).catch(() => {});
      },
    });
    contextMenu.openAt({ x: e.clientX, y: e.clientY }, items, inputEl);
  }
</script>

<div
  class="search"
  class:dim={disabled}
>
  <form onsubmit={(e) => { e.preventDefault(); submit(); }}>
    <span class="field">
      <span class="glass"><Icon name="search" size={14} /></span>
      <input
        bind:this={inputEl}
        bind:value={query}
        type="search"
        placeholder="search tags…"
        aria-label="search"
        role="combobox"
        aria-expanded={open}
        aria-haspopup="listbox"
        aria-autocomplete="list"
        aria-controls="tag-suggest-list"
        aria-activedescendant={highlight >= 0 ? `tag-suggest-${highlight}` : undefined}
        {disabled}
        oninput={onInput}
        onkeydown={onKeydown}
        onblur={closeDropdown}
      />
      {#if busy}<Spinner size={12} />{/if}
    </span>
    <button type="submit" {disabled}>Search</button>
  </form>

  {#if open && rows.length > 0}
    <ul class="suggest" class:stale={busy} id="tag-suggest-list" role="listbox">
      {#each rows as row, i (rowKey(row))}
        <li
          id={`tag-suggest-${i}`}
          role="option"
          aria-selected={i === highlight}
          class:active={i === highlight}
          onmousedown={(e) => { if (e.button !== 0) return; e.preventDefault(); complete(row); }}
          oncontextmenu={(e) => rowContext(e, row)}
        >
          {#if row.kind === 'namespace'}
            <span class="ns">{row.namespace}:</span>
            <span class="count">{row.count.toLocaleString()} tags</span>
          {:else}
            <span class="dot" style:background={dotColor(row)}></span>
            {#if view.showAliasSource && row.alias_source}
              <span class="sr">alias:</span>
              <span class="alias-src">{row.alias_source}</span>
              <span class="alias-arrow" aria-hidden="true">→</span>
            {/if}
            <span class="tag">{row.namespace ? `${row.namespace}:${row.subtag}` : row.subtag}</span>
            <span class="count">{row.count.toLocaleString()}</span>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}

  <!-- Live region: announced by screen readers when completion is in progress.
       Placed outside the listbox so result-set changes are not also read as
       status updates. The combobox ARIA contract on the input is unchanged. -->
  <span class="sr" role="status">{busy ? 'loading suggestions' : ''}</span>
</div>

<style>
  .search {
    position: relative;
    flex: 1;
    max-width: 640px;
  }
  .search.dim {
    opacity: 0.55;
  }
  form {
    display: flex;
    gap: 0.5rem;
  }
  .field {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 9px;
    height: 32px;
    padding: 0 12px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
  }
  .field:focus-within {
    border-color: var(--accent);
  }
  .glass {
    flex: none;
    display: flex;
    align-items: center;
    color: var(--accent);
  }
  .field input[type='search'] {
    flex: 1;
    border: 0;
    background: transparent;
    color: var(--text);
    font: 13px/1 var(--mono);
    outline: none;
  }
  .field input::placeholder {
    color: var(--text-mute);
  }
  button {
    height: 32px;
    padding: 0 14px;
    border: 0;
    border-radius: 8px;
    background: var(--accent-grad);
    color: var(--on-accent);
    font-weight: 600;
    cursor: pointer;
  }
  .suggest {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    right: 0;
    z-index: 20;
    margin: 0;
    padding: 4px;
    list-style: none;
    max-height: 320px;
    overflow-y: auto;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    box-shadow: var(--shadow-popover);
    transition: opacity 0.15s;
  }
  .suggest.stale {
    opacity: 0.7;
  }
  .suggest li {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px;
    border-radius: 6px;
    cursor: pointer;
    font: 13px/1 var(--mono);
    color: var(--text);
  }
  .suggest li.active {
    background: var(--ink-750);
  }
  .dot {
    flex: none;
    width: 8px;
    height: 8px;
    border-radius: 50%;
  }
  .ns {
    color: var(--accent);
  }
  .tag {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .count {
    margin-left: auto;
    color: var(--text-faint);
    font-size: 11px;
  }
  .alias-src {
    color: var(--text-faint);
    font-size: 11px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 12em;
  }
  .alias-arrow {
    color: var(--text-faint);
    white-space: nowrap;
    flex: none;
  }
  .sr {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
</style>
