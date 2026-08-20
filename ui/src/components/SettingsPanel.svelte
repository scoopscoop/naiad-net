<script lang="ts">
  import { onMount, tick } from 'svelte';
  import type { BackupSummary, ImportProgress, RelationsImportSummary, RelationsProgress, RepoDto, ScanSummary, SourceImportSummary } from '../lib/types';
  import { view, LEVEL_MIN, LEVEL_MAX } from '../lib/settings.svelte';
  import { addRepo, backup, hydrusConfig, hydrusConfigure, hydrusRelationsStream, listRepos, removeRepo, setRepoQueryBits, sourceImport, sourceImportStream } from '../lib/api';
  import { crowdForBits, effectiveBits, softCapBits, bytesPerLookup, formatBytes,
           CROWD_FLOOR, SERVER_FLOOR_BITS, MAX_BITS } from '../lib/crowd';
  import { activity } from '../lib/activity.svelte';
  import FoldersSection from './FoldersSection.svelte';
  import TagCategoriesSection from './TagCategoriesSection.svelte';
  import Icon from './Icon.svelte';
  import { trapFocus } from '../lib/focus-trap';

  interface Props {
    onimported: (summary: ScanSummary) => void;
    onremoved: () => void;
    onrescope: () => void;
  }
  let { onimported, onremoved, onrescope }: Props = $props();

  // Only the Tauri webview exposes the IPC bridge; in a plain browser the
  // native dialog is unavailable, so the picker button stays hidden.
  const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

  type Tab = 'display' | 'tags' | 'library' | 'sync' | 'plugins';

  let open = $state(false);
  let tab = $state<Tab>('display');
  // Bumped on every autosave; the {#key} below remounts the indicator so its
  // flash animation restarts.
  let savedTick = $state(0);

  // DOM refs for focus management.
  let triggerEl = $state<HTMLElement | null>(null);
  let modalEl = $state<HTMLElement | null>(null);

  // Plugins tab state
  let hydrusDir = $state('');
  let hydrusServices = $state('');
  let importBusy = $state(false);
  let importResult = $state<SourceImportSummary | null>(null);
  let importProgress = $state<ImportProgress | null>(null);
  let importError = $state<string | null>(null);
  let configError = $state<string | null>(null);
  let relationsResult = $state<RelationsImportSummary | null>(null);
  let relationsProgress = $state<RelationsProgress | null>(null);

  // Library tab — backup state
  let backupBusy = $state(false);
  let backupResult = $state<BackupSummary | null>(null);
  let backupError = $state<string | null>(null);

  onMount(async () => {
    try {
      const cfg = await hydrusConfig();
      hydrusDir = cfg.dir ?? '';
      hydrusServices = cfg.tag_services.join(', ');
    } catch {
      // Non-fatal: leave the fields empty if the read fails (e.g. no daemon).
    }
  });

  function markSaved() {
    savedTick += 1;
  }

  async function openPanel() {
    tab = 'display'; // always land on Display
    open = true;
    await tick();
    modalEl?.focus();
  }

  function close() {
    // Restore focus to the trigger before collapsing the modal so screen
    // readers announce the return to the triggering context.
    triggerEl?.focus();
    open = false;
  }

  function onThumb(e: Event) {
    view.zoomLevel = Number((e.currentTarget as HTMLInputElement).value); // store clamps + persists
    markSaved();
  }

  function onLocalOnly(e: Event) {
    view.localOnly = (e.currentTarget as HTMLInputElement).checked;
    markSaved();
    onrescope(); // re-run the current search so the grid reflects the new scope
  }

  function onCompletionMatch(e: Event) {
    const val = (e.currentTarget as HTMLSelectElement).value;
    view.completionMatch = val === 'substring' ? 'substring' : 'prefix';
    markSaved();
  }

  function onAliasSource(e: Event) {
    view.showAliasSource = (e.currentTarget as HTMLInputElement).checked;
    markSaved();
  }

  function onThumbFit(e: Event) {
    const val = (e.currentTarget as HTMLSelectElement).value;
    view.thumbFit = val === 'fill' ? 'fill' : 'frame';
    markSaved();
  }

  function parsedServices(): number[] {
    return hydrusServices
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean)
      .map(Number)
      .filter((n) => Number.isFinite(n));
  }

  async function saveHydrusConfig() {
    configError = null;
    try {
      await hydrusConfigure(hydrusDir, parsedServices());
      markSaved();
    } catch (e) {
      configError = e instanceof Error ? e.message : String(e);
    }
  }

  async function chooseHydrusDir() {
    configError = null;
    try {
      const { open: openDialog } = await import('@tauri-apps/plugin-dialog');
      const dir = await openDialog({ directory: true });
      if (typeof dir === 'string') hydrusDir = dir;
    } catch (e) {
      // A denied capability or missing bridge rejects here; surface it (the text
      // field still works as a fallback).
      configError = e instanceof Error ? e.message : String(e);
    }
  }

  // The full import is one blocking request; the library import streams so tags
  // land — and report — file-by-file. Both also report into the activity store
  // (the seam for the future central indicator, #34).
  async function runImport(libraryOnly: boolean) {
    importBusy = true;
    importResult = null;
    importProgress = null;
    importError = null;
    const job = activity.begin({ label: 'Hydrus import', kind: 'import' });
    if (!libraryOnly) {
      try {
        importResult = await sourceImport('hydrus', false);
        job.succeed({ detail: `${importResult.mappings_resolved} resolved` });
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        importError = msg;
        job.fail(msg);
      } finally {
        importBusy = false;
      }
      return;
    }
    sourceImportStream('hydrus', {
      onProgress: (p) => {
        importProgress = p;
        job.progress({
          detail: `${p.files}/${p.total} files · ${p.mappings} tags`,
          done: p.files,
          total: p.total,
        });
      },
      onSummary: (s) => {
        importResult = s;
        importProgress = null;
        importBusy = false;
        job.succeed({ detail: `${s.mappings_resolved} resolved` });
        onrescope(); // re-run the current search so newly-tagged files reflect it
      },
      onError: (m) => {
        importError = m;
        importBusy = false;
        job.fail(m);
      },
    });
  }

  async function runBackup() {
    backupBusy = true;
    backupResult = null;
    backupError = null;
    const job = activity.begin({ label: 'DB backup', kind: 'backup' });
    try {
      const result = await backup();
      backupResult = result;
      job.succeed({ detail: `${(result.bytes / 1_048_576).toFixed(1)} MB` });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      backupError = msg;
      job.fail(msg);
    } finally {
      backupBusy = false;
    }
  }

  // Sync tab — repos state
  let repos = $state<RepoDto[]>([]);
  let newRepoUrl = $state('');
  let repoError = $state<string | null>(null);
  let repoBusy = $state(false);
  let confirmRemove = $state<string | null>(null);
  let purgeOnRemove = $state(false);

  async function refreshRepos() {
    try {
      repos = await listRepos();
    } catch (e) {
      repoError = e instanceof Error ? e.message : String(e);
    }
  }

  async function doAddRepo() {
    const url = newRepoUrl.trim();
    if (!url) {
      repoError = 'url is required';
      return;
    }
    repoBusy = true;
    repoError = null;
    try {
      await addRepo(url);
      newRepoUrl = '';
      markSaved();
      await refreshRepos();
    } catch (e) {
      repoError = e instanceof Error ? e.message : String(e);
    } finally {
      repoBusy = false;
    }
  }

  async function doRemoveRepo(name: string) {
    repoError = null;
    try {
      await removeRepo(name, purgeOnRemove);
      confirmRemove = null;
      markSaved();
      await refreshRepos();
    } catch (e) {
      repoError = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    if (tab === 'sync') void refreshRepos();
  });

  let nakedByRepo = $state<Record<string, boolean>>({});

  // The app's default per-repo privacy ceiling (mirrors the daemon's global
  // default). When a repo does not report its size (count == null) we cannot
  // compute a crowd, so this same value is the safe cap for the bits-only
  // fallback control — anything above it counts as a "naked" width.
  const DEFAULT_CEILING = 24;
  const FALLBACK_CAP = DEFAULT_CEILING;
  function currentBits(repo: RepoDto): number { return repo.max_query_bits ?? DEFAULT_CEILING; }
  function nakedFor(repo: RepoDto): boolean {
    if (repo.name in nakedByRepo) return nakedByRepo[repo.name];
    if (repo.count != null) {
      return crowdForBits(repo.count, effectiveBits(repo.advertised_bits, currentBits(repo), repo.min_query_bits)) < CROWD_FLOOR;
    }
    return currentBits(repo) > FALLBACK_CAP;
  }
  async function saveBits(repo: RepoDto, bits: number) {
    try {
      await setRepoQueryBits(repo.name, bits);
      await refreshRepos();
    } catch (e) {
      repoError = `failed to set query width for ${repo.name}: ${e}`;
    }
  }

  /** Svelte action: focuses the node immediately after it mounts.
   *  Used to move keyboard focus to the confirm button when the inline
   *  confirm row appears (replacing the remove button). */
  function focusOnMount(node: HTMLElement) {
    node.focus();
  }

  // Relations pull: same SSE + activity treatment as the library import, but
  // determinate — the Hydrus relation tables give the total up front.
  function runRelationsPull() {
    importBusy = true;
    relationsResult = null;
    relationsProgress = null;
    importError = null;
    const job = activity.begin({ label: 'Pull tag relations', kind: 'import' });
    hydrusRelationsStream({
      onProgress: (p) => {
        relationsProgress = p;
        job.progress({
          detail: `${p.edges_done}/${p.edges_total} edges · ${p.siblings} siblings, ${p.parents} parents`,
          done: p.edges_done,
          total: p.edges_total,
        });
      },
      onSummary: (s) => {
        relationsResult = s;
        relationsProgress = null;
        importBusy = false;
        job.succeed({ detail: `${s.siblings} siblings, ${s.parents} parents` });
        onrescope(); // aliases change how existing tags display/search
      },
      onError: (m) => {
        importError = m;
        importBusy = false;
        job.fail(m);
      },
    });
  }
</script>

<svelte:window
  onkeydown={(e) => {
    if (open && e.key === 'Escape') close();
  }}
/>

<button class="settings-trigger" aria-label="settings" bind:this={triggerEl} onclick={openPanel}>
  <Icon name="settings" size={16} />
</button>

{#if open}
  <!-- Inert dim layer: clicking out must not discard mid-edit settings state;
       closing is the × button or Escape only. -->
  <div class="backdrop"></div>
  <div class="modal" role="dialog" aria-modal="true" aria-label="settings" tabindex="-1" bind:this={modalEl} use:trapFocus>
    <header>
      <h2>Settings</h2>
      <button class="close" aria-label="close settings" onclick={close}><Icon name="close" size={16} /></button>
    </header>

    <div class="body">
      <div class="tabs" role="tablist" aria-label="settings sections">
        <button
          role="tab"
          class:active={tab === 'display'}
          aria-selected={tab === 'display'}
          onclick={() => (tab = 'display')}>Display</button>
        <button
          role="tab"
          class:active={tab === 'tags'}
          aria-selected={tab === 'tags'}
          onclick={() => (tab = 'tags')}>Tags</button>
        <button
          role="tab"
          class:active={tab === 'library'}
          aria-selected={tab === 'library'}
          onclick={() => (tab = 'library')}>Library</button>
        <button
          role="tab"
          class:active={tab === 'sync'}
          aria-selected={tab === 'sync'}
          onclick={() => (tab = 'sync')}>Sync</button>
        <button
          role="tab"
          class:active={tab === 'plugins'}
          aria-selected={tab === 'plugins'}
          onclick={() => (tab = 'plugins')}>Plugins</button>
      </div>

      <div class="content" role="tabpanel">
        {#if tab === 'display'}
          <section>
            <h3>Thumbnails</h3>
            <label
              class="row"
              title="Fewer per row means larger tiles; at 2–3 per row thumbnails upscale past the daemon's 360px source and go soft.">
              <span>thumbs per row</span>
              <input
                type="number"
                min={LEVEL_MIN}
                max={LEVEL_MAX}
                step="1"
                aria-label="thumbs per row"
                value={view.zoomLevel}
                onchange={onThumb} />
            </label>
            <div class="row">
              <span>thumbnail fit</span>
              <select aria-label="thumbnail fit" value={view.thumbFit} onchange={onThumbFit}>
                <option value="frame">frame</option>
                <option value="fill">fill</option>
              </select>
            </div>
          </section>
          <section>
            <h3>Search</h3>
            <label class="row">
              <span>local tags only</span>
              <input
                type="checkbox"
                class="toggle"
                aria-label="local tags only"
                checked={view.localOnly}
                onchange={onLocalOnly} />
            </label>
            <div class="row">
              <span>tag completion match</span>
              <select
                aria-label="tag completion match"
                value={view.completionMatch}
                onchange={onCompletionMatch}>
                <option value="prefix">prefix</option>
                <option value="substring">substring</option>
              </select>
            </div>
            <label class="row">
              <span>show alias source in completions</span>
              <input
                type="checkbox"
                class="toggle"
                aria-label="show alias source in completions"
                checked={view.showAliasSource}
                onchange={onAliasSource} />
            </label>
          </section>
        {:else if tab === 'tags'}
          <TagCategoriesSection onsaved={markSaved} />
        {:else if tab === 'library'}
          <section>
            <h3>Folders</h3>
            <FoldersSection {onimported} {onremoved} onsaved={markSaved} />
          </section>
          <section>
            <h3>Backup</h3>
            <div class="row">
              <span>writes a consistent snapshot next to the database while the app keeps running</span>
              <button
                class="action-btn"
                onclick={runBackup}
                disabled={backupBusy}>
                {backupBusy ? 'Backing up…' : 'Back up database'}
              </button>
            </div>
            {#if backupResult}
              <p class="import-result">
                backed up to {backupResult.dest} ({(backupResult.bytes / 1_048_576).toFixed(1)} MB)
              </p>
            {/if}
            {#if backupError}
              <p class="import-error">{backupError}</p>
            {/if}
          </section>
        {:else if tab === 'sync'}
          <section class="placeholder">
            <h3>Blocked tags</h3>
            <p class="soon">Coming soon — block individual tags, patterns, or authors.</p>
          </section>
          <section>
            <h3>Repos</h3>
            {#if repos.length === 0}
              <p class="soon">no repos subscribed</p>
            {/if}
            {#each repos as repo (repo.name)}
              <div class="row repo-row">
                <span class="repo-id"><span class="repo-name">{repo.name}</span> <span class="repo-url">{repo.url}</span></span>
                {#if repo.advertised_bits == null}
                  <span class="repo-width">no bucketing yet — pulled whole, or awaiting first handshake</span>
                {:else if repo.count == null}
                  <div class="repo-crowd">
                    <label class="repo-width">query width ≤
                      <input type="range" min={SERVER_FLOOR_BITS} max={nakedFor(repo) ? MAX_BITS : FALLBACK_CAP}
                        value={currentBits(repo)} onchange={(e) => saveBits(repo, +e.currentTarget.value)} />
                      {currentBits(repo)} bits
                    </label>
                    <label class="naked-opt">
                      <input type="checkbox" checked={nakedFor(repo)}
                        onchange={(e) => {
                          const on = e.currentTarget.checked;
                          nakedByRepo[repo.name] = on;
                          if (!on && currentBits(repo) > FALLBACK_CAP) { saveBits(repo, FALLBACK_CAP); }
                        }} />
                      allow naked pulls (near-exact hashes; VPN/trusted only)
                    </label>
                  </div>
                {:else}
                  {@const N = repo.count}
                  {@const eff = effectiveBits(repo.advertised_bits, currentBits(repo), repo.min_query_bits)}
                  {@const naked = nakedFor(repo)}
                  <!-- The slider tracks the EFFECTIVE width (what actually sets the
                       crowd), not the raw ceiling — so the handle and the readout
                       always agree. floorBits: can't pull coarser than the server
                       floor. hiNonNaked: finest width still ≥ CROWD_FLOOR that the
                       repo serves. Inverted axis: dragging right = coarser = more
                       cover crowd. Saving stores the chosen width as the ceiling. -->
                  {@const floorBits = Math.max(SERVER_FLOOR_BITS, repo.min_query_bits ?? SERVER_FLOOR_BITS)}
                  {@const hiNonNaked = Math.max(floorBits, Math.min(repo.advertised_bits ?? MAX_BITS, softCapBits(N)))}
                  {@const hi = naked ? MAX_BITS : hiNonNaked}
                  {@const displayBits = Math.min(Math.max(eff, floorBits), hi)}
                  <div class="repo-crowd">
                    <label>Cover crowd
                      <input type="range" min={floorBits} max={hi}
                        value={floorBits + hi - displayBits}
                        onchange={(e) => saveBits(repo, floorBits + hi - +e.currentTarget.value)} />
                    </label>
                    <span class="crowd-readout">
                      ≈ {crowdForBits(N, eff).toLocaleString()} cover files · ~{formatBytes(bytesPerLookup(N, eff))} per file looked up
                      {#if currentBits(repo) > (repo.advertised_bits ?? currentBits(repo))}
                        <span class="soon">(repo serves {repo.advertised_bits}-bit buckets — larger ceiling has no effect here)</span>
                      {/if}
                    </span>
                    <label class="naked-opt">
                      <input type="checkbox" checked={naked}
                        onchange={(e) => {
                          const on = e.currentTarget.checked;
                          nakedByRepo[repo.name] = on;
                          if (!on && currentBits(repo) > hiNonNaked) { saveBits(repo, hiNonNaked); }
                        }} />
                      allow naked pulls (crowd below {CROWD_FLOOR.toLocaleString()} — reveals near-exact hashes; VPN/trusted only)
                    </label>
                  </div>
                {/if}
                {#if confirmRemove === repo.name}
                  <span class="repo-confirm">
                    <label class="purge-opt">
                      <input type="checkbox" bind:checked={purgeOnRemove} />
                      also delete its pulled tags
                    </label>
                    <button
                      class="action-btn"
                      aria-label={`confirm remove ${repo.name}`}
                      use:focusOnMount
                      onclick={() => doRemoveRepo(repo.name)}>confirm</button>
                    <button
                      class="ghost-btn"
                      aria-label={`cancel remove ${repo.name}`}
                      onclick={() => (confirmRemove = null)}>cancel</button>
                  </span>
                {:else}
                  <button
                    class="ghost-btn"
                    aria-label={`remove repo ${repo.name}`}
                    onclick={() => { confirmRemove = repo.name; purgeOnRemove = false; }}>remove</button>
                {/if}
              </div>
            {/each}
            <div class="row repo-add">
              <input type="text" class="text-input" aria-label="repo url" placeholder="http://host:port" bind:value={newRepoUrl} />
              <button class="action-btn" onclick={doAddRepo} disabled={repoBusy}>add</button>
            </div>
            {#if repoError}
              <p class="import-error">{repoError}</p>
            {/if}
          </section>
        {:else if tab === 'plugins'}
          <section>
            <h3>Hydrus import</h3>
            <div class="row">
              <span>Hydrus DB directory</span>
              <span class="dir-pick">
                {#if isTauri}
                  <button
                    type="button"
                    class="action-btn"
                    aria-label="choose Hydrus DB folder"
                    onclick={chooseHydrusDir}>Choose folder</button>
                {/if}
                <input
                  type="text"
                  class="text-input"
                  aria-label="Hydrus DB directory"
                  bind:value={hydrusDir}
                  placeholder="/path/to/hydrus/db" />
              </span>
            </div>
            <label class="row">
              <span>Tag service IDs (comma-separated, empty = all)</span>
              <input
                type="text"
                class="text-input"
                aria-label="tag service IDs"
                bind:value={hydrusServices}
                placeholder="1, 2, 3" />
            </label>
            <div class="row">
              <span></span>
              <button class="action-btn" onclick={saveHydrusConfig}>Save</button>
            </div>
            {#if configError}
              <p class="import-error">{configError}</p>
            {/if}
          </section>
          <section>
            <h3>Import tags</h3>
            <div class="row">
              <span>Pull Hydrus tags for files already in my library</span>
              <button
                class="action-btn"
                onclick={() => runImport(true)}
                disabled={importBusy}>
                {importBusy ? 'Importing…' : 'Import for my library'}
              </button>
            </div>
            <div class="row">
              <span>Import every Hydrus-owned file plus the tag-relation graph</span>
              <button
                class="action-btn"
                onclick={() => runImport(false)}
                disabled={importBusy}>
                {importBusy ? 'Importing…' : 'Import all files'}
              </button>
            </div>
            <div class="row">
              <span>Pull the tag alias/hierarchy graph (typo→canonical merges for all your tags)</span>
              <button
                class="action-btn"
                onclick={runRelationsPull}
                disabled={importBusy}>
                {importBusy ? 'Importing…' : 'Pull tag relations'}
              </button>
            </div>
            <p class="import-note">
              The library import is bounded by your collection and applies tags file-by-file; the
              full import can be very large on big sources like the PTR. The UI stays responsive
              either way. Relations are independent of the tag imports and canonicalize tags
              pulled before or after; safe to re-run.
            </p>
            {#if importProgress}
              <p class="import-result" role="status">
                Importing… {importProgress.files}/{importProgress.total} files,
                {importProgress.mappings} tags applied.
              </p>
            {/if}
            {#if importResult}
              <p class="import-result">
                Imported: {importResult.siblings} siblings, {importResult.parents} parents,
                {importResult.mappings_staged} mappings ({importResult.mappings_resolved} resolved),
                {importResult.sha256_backfilled} files hashed.
              </p>
            {/if}
            {#if relationsProgress}
              <p class="import-result" role="status">
                Pulling relations… {relationsProgress.edges_done}/{relationsProgress.edges_total} edges,
                {relationsProgress.siblings} siblings, {relationsProgress.parents} parents.
              </p>
            {/if}
            {#if relationsResult}
              <p class="import-result">
                Imported {relationsResult.siblings} siblings, {relationsResult.parents} parents.
              </p>
            {/if}
            {#if importError}
              <p class="import-error">{importError}</p>
            {/if}
          </section>
        {/if}
      </div>
    </div>

    {#key savedTick}
      {#if savedTick > 0}
        <p class="saved" role="status">Saved.</p>
      {/if}
    {/key}
  </div>
{/if}

<style>
  .settings-trigger {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    background: var(--ink-900);
    color: var(--text);
    cursor: pointer;
  }
  .settings-trigger:hover {
    border-color: var(--accent);
    color: var(--accent);
  }
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 9;
    background: var(--overlay-backdrop);
  }
  .modal {
    position: fixed;
    z-index: 10;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(520px, 94vw);
    max-height: 86vh;
    overflow-y: auto;
    padding: 16px;
    background: var(--ink-800);
    border: 1px solid var(--line);
    border-radius: 11px;
    box-shadow: var(--shadow-modal);
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .modal header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .modal h2 {
    margin: 0;
    font: 600 14px/1 var(--mono);
    color: var(--text);
  }
  .modal h3 {
    margin: 0 0 8px;
    font: 600 11px/1 var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  .close {
    width: 26px;
    height: 26px;
    border: 1px solid var(--line-soft);
    border-radius: 6px;
    background: var(--ink-900);
    color: var(--text);
    font: 600 15px/1 var(--mono);
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .close:hover {
    border-color: var(--accent);
  }
  .body {
    display: flex;
    gap: 14px;
    align-items: flex-start;
  }
  .tabs {
    flex: none;
    width: 104px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .tabs button {
    text-align: left;
    height: 30px;
    padding: 0 10px;
    border: 1px solid transparent;
    border-radius: 7px;
    background: transparent;
    color: var(--text-mute);
    font: 500 12px/1 var(--mono);
    cursor: pointer;
  }
  .tabs button:hover {
    color: var(--text);
  }
  .tabs button.active {
    background: var(--ink-900);
    border-color: var(--line-soft);
    color: var(--accent);
  }
  .content {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    font: 500 12px/1.3 var(--mono);
    color: var(--text-mute);
  }
  .row input[type='number'] {
    width: 88px;
    height: 30px;
    padding: 0 10px;
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    background: var(--ink-900);
    color: var(--text);
    font: 12px/1 var(--mono);
    outline: none;
  }
  .row input[type='number']:focus {
    border-color: var(--accent);
  }
  .row select {
    height: 30px;
    padding: 0 10px;
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    background: var(--ink-900);
    color: var(--text);
    font: 12px/1 var(--mono);
    outline: none;
    cursor: pointer;
  }
  .row select:focus {
    border-color: var(--accent);
  }
  .row input.toggle {
    width: 16px;
    height: 16px;
    accent-color: var(--accent);
    cursor: pointer;
  }
  .placeholder {
    opacity: 0.5;
  }
  .placeholder .soon {
    margin: 0;
    font: 500 11.5px/1.5 var(--mono);
    color: var(--text-faint);
  }
  .saved {
    margin: 0;
    text-align: right;
    font: 500 11.5px/1 var(--mono);
    /* Starts saturated, eases to muted over 300ms, then fades out by ~2.5s. */
    animation: saved-flash 2.5s forwards;
  }
  @keyframes saved-flash {
    0% {
      color: var(--accent);
      opacity: 1;
    }
    12% {
      color: var(--text-mute);
      opacity: 1;
    }
    85% {
      color: var(--text-mute);
      opacity: 1;
    }
    100% {
      color: var(--text-mute);
      opacity: 0;
    }
  }
  .text-input {
    flex: 1;
    min-width: 0;
    max-width: 200px;
    height: 30px;
    padding: 0 10px;
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    background: var(--ink-900);
    color: var(--text);
    font: 12px/1 var(--mono);
    outline: none;
  }
  .text-input:focus {
    border-color: var(--accent);
  }
  .text-input::placeholder {
    color: var(--text-mute);
  }
  .dir-pick {
    display: flex;
    gap: 7px;
    align-items: center;
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
  .action-btn:hover:not(:disabled) {
    border-color: var(--accent);
  }
  .action-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .import-note {
    margin: 8px 0 0;
    font: 500 11px/1.5 var(--mono);
    color: var(--text-faint);
  }
  .import-result {
    margin: 6px 0 0;
    font: 500 11.5px/1.5 var(--mono);
    color: var(--ok);
  }
  .import-error {
    margin: 6px 0 0;
    font: 500 11.5px/1.5 var(--mono);
    color: var(--err);
  }
  .soon {
    margin: 0;
    font: 500 11.5px/1.5 var(--mono);
    color: var(--text-faint);
  }
  .repo-row {
    flex-wrap: wrap;
    gap: 6px 10px;
    margin-bottom: 4px;
  }
  .repo-id {
    display: inline-flex;
    font: 12px/1 var(--mono);
    gap: 6px;
    align-items: baseline;
    flex: 1;
    min-width: 0;
  }
  .repo-url {
    color: var(--text-mute);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .repo-width {
    font: 11px/1 var(--mono);
    color: var(--text-faint);
    flex-basis: 100%;
    margin-top: 2px;
  }
  .repo-confirm {
    display: inline-flex;
    gap: 8px;
    align-items: center;
  }
  .repo-add {
    margin-top: 6px;
  }
  .repo-add .text-input {
    max-width: none;
  }
  .ghost-btn {
    background: none;
    border: 0;
    color: var(--text-mute);
    font: 12px/1 var(--mono);
    cursor: pointer;
    padding: 0;
  }
  .ghost-btn:hover {
    color: var(--accent);
  }
  .ghost-btn:focus-visible {
    outline: 1px solid var(--accent);
    outline-offset: 2px;
  }
  .purge-opt {
    display: inline-flex;
    gap: 6px;
    font: 12px/1 var(--mono);
    color: var(--text-mute);
    align-items: center;
    cursor: pointer;
  }
  .repo-crowd {
    flex-basis: 100%;
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin-top: 2px;
  }
  .repo-crowd label {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font: 11px/1 var(--mono);
    color: var(--text-faint);
    cursor: pointer;
  }
  .repo-crowd input[type='range'] {
    accent-color: var(--accent);
    cursor: pointer;
  }
  .crowd-readout {
    font: 11px/1.4 var(--mono);
    color: var(--text-faint);
  }
  .naked-opt {
    display: inline-flex;
    gap: 6px;
    font: 11px/1.4 var(--mono);
    color: var(--text-mute);
    align-items: flex-start;
    cursor: pointer;
  }
  .naked-opt input[type='checkbox'] {
    accent-color: var(--accent);
    cursor: pointer;
    flex-shrink: 0;
    margin-top: 1px;
  }
</style>
