import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, within } from '@testing-library/svelte';
import SettingsPanel from './SettingsPanel.svelte';
import { getFocusableElements } from '../lib/focus-trap';
import * as api from '../lib/api';
import { view } from '../lib/settings.svelte';
import { activity } from '../lib/activity.svelte';

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return {
    ...actual,
    scanStream: vi.fn(),
    listRoots: vi.fn(),
    removeRoot: vi.fn(),
    hydrusConfigure: vi.fn(),
    sourceImport: vi.fn(),
    sourceImportStream: vi.fn(),
    hydrusRelationsStream: vi.fn(),
    listRepos: vi.fn(),
    addRepo: vi.fn(),
    removeRepo: vi.fn(),
  };
});

vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn() }));

function open() {
  return fireEvent.click(screen.getByLabelText('settings'));
}

describe('SettingsPanel', () => {
  beforeEach(() => {
    vi.mocked(api.listRoots).mockReset();
    vi.mocked(api.listRoots).mockResolvedValue([]);
    vi.mocked(api.hydrusConfigure).mockReset();
    vi.mocked(api.hydrusConfigure).mockResolvedValue();
    vi.mocked(api.sourceImport).mockReset();
    vi.mocked(api.sourceImport).mockResolvedValue({
      mappings_staged: 0,
      mappings_resolved: 0,
      siblings: 0,
      parents: 0,
      sha256_backfilled: 0,
    });
    vi.mocked(api.sourceImportStream).mockReset();
    vi.mocked(api.sourceImportStream).mockReturnValue(() => {});
    vi.mocked(api.hydrusRelationsStream).mockReset();
    vi.mocked(api.hydrusRelationsStream).mockReturnValue(() => {});
    vi.mocked(api.listRepos).mockReset();
    vi.mocked(api.listRepos).mockResolvedValue([]);
    vi.mocked(api.addRepo).mockReset();
    vi.mocked(api.addRepo).mockResolvedValue();
    vi.mocked(api.removeRepo).mockReset();
    vi.mocked(api.removeRepo).mockResolvedValue();
    localStorage.clear();
    view.zoomLevel = 8;
    view.localOnly = false;
    view.thumbFit = 'frame';
    view.showAliasSource = false;
  });

  it('opens the modal from the gear button', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    await open();
    expect(screen.getByRole('dialog', { name: 'settings' })).toBeInTheDocument();
  });

  it('closes on the × button', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.click(screen.getByLabelText('close settings'));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('closes on Escape', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('stays open on a backdrop click — closing is the × button or Escape only', async () => {
    const { container } = render(SettingsPanel, {
      onimported: () => {},
      onremoved: () => {},
      onrescope: () => {},
    });
    await open();
    const backdrop = container.querySelector('.backdrop');
    expect(backdrop).not.toBeNull();
    await fireEvent.click(backdrop!);
    expect(screen.getByRole('dialog')).toBeInTheDocument();
  });

  it('shows the Display tab by default with the thumbnail control', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    expect(screen.getByLabelText('thumbs per row')).toBeInTheDocument();
    // Folders live under Library, not Display.
    expect(screen.queryByLabelText('folder path')).not.toBeInTheDocument();
  });

  it('edits thumbs per row, persists it, and shows "Saved."', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    const input = screen.getByLabelText('thumbs per row');
    await fireEvent.change(input, { target: { value: '4' } });
    expect(view.zoomLevel).toBe(4);
    expect(localStorage.getItem('naiad.view.zoom_level')).toBe('4');
    expect(screen.getByText('Saved.')).toBeInTheDocument();
  });

  it('clamps an out-of-range thumbs per row', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    const input = screen.getByLabelText('thumbs per row') as HTMLInputElement;
    await fireEvent.change(input, { target: { value: '9999' } });
    expect(view.zoomLevel).toBe(16);
    expect(input.value).toBe('16');
  });

  it('shows the thumbnail fit select in the Display tab', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    expect(screen.getByLabelText('thumbnail fit')).toBeInTheDocument();
  });

  it('thumbnail fit select reflects view.thumbFit and updates the store on change', async () => {
    view.thumbFit = 'frame';
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    const sel = screen.getByLabelText('thumbnail fit') as HTMLSelectElement;
    expect(sel.value).toBe('frame');
    await fireEvent.change(sel, { target: { value: 'fill' } });
    expect(view.thumbFit).toBe('fill');
    expect(localStorage.getItem('naiad.view.thumb_fit')).toBe('fill');
    expect(screen.getByText('Saved.')).toBeInTheDocument();
  });

  it('hosts the Folders controls under the Library tab', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.click(screen.getByRole('tab', { name: 'Library' }));
    expect(screen.getByLabelText('folder path')).toBeInTheDocument();
    expect(screen.getByText('Scan')).toBeInTheDocument();
  });

  it('toggles local-only, persists it, and fires onrescope', async () => {
    const onrescope = vi.fn();
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope });
    await open();
    const box = screen.getByLabelText('local tags only') as HTMLInputElement;
    expect(box.checked).toBe(false);
    await fireEvent.click(box);
    expect(view.localOnly).toBe(true);
    expect(localStorage.getItem('naiad.view.local_only')).toBe('true');
    expect(onrescope).toHaveBeenCalled();
    expect(screen.getByText('Saved.')).toBeInTheDocument();
  });

  it('alias-source checkbox flips view.showAliasSource', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    const box = screen.getByLabelText('show alias source in completions') as HTMLInputElement;
    expect(box.checked).toBe(false);
    await fireEvent.click(box);
    expect(view.showAliasSource).toBe(true);
    expect(screen.getByText('Saved.')).toBeInTheDocument();
  });

  it('Sync tab is reachable and shows the placeholder and repos sections', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.click(screen.getByRole('tab', { name: 'Sync' }));
    expect(screen.getByText('Blocked tags')).toBeInTheDocument();
    expect(screen.getByText('Repos')).toBeInTheDocument();
  });

  it('shows an inert placeholder under the Blocked tags section', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.click(screen.getByRole('tab', { name: 'Sync' }));
    const blockedSection = screen.getByText('Blocked tags').closest('section')!;
    // Blocked tags placeholder section carries no interactive controls.
    expect(within(blockedSection).queryByRole('textbox')).not.toBeInTheDocument();
    expect(within(blockedSection).queryByRole('button')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /scan/i })).not.toBeInTheDocument();
  });

  async function gotoPlugins() {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.click(screen.getByRole('tab', { name: 'Plugins' }));
  }

  it('saves the Hydrus config with parsed service ids', async () => {
    await gotoPlugins();
    await fireEvent.input(screen.getByLabelText('Hydrus DB directory'), {
      target: { value: '/my/hydrus' },
    });
    await fireEvent.input(screen.getByLabelText('tag service IDs'), { target: { value: '9, 14' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect(api.hydrusConfigure).toHaveBeenCalledWith('/my/hydrus', [9, 14]);
  });

  it('runs a full import and renders the summary', async () => {
    vi.mocked(api.sourceImport).mockResolvedValue({
      mappings_staged: 5,
      mappings_resolved: 3,
      siblings: 2,
      parents: 1,
      sha256_backfilled: 4,
    });
    await gotoPlugins();
    await fireEvent.click(screen.getByRole('button', { name: 'Import all files' }));
    expect(api.sourceImport).toHaveBeenCalledWith('hydrus', false);
    expect(await screen.findByText(/2 siblings/)).toBeInTheDocument();
    expect(screen.getByText(/3 resolved/)).toBeInTheDocument();
  });

  it('streams a library import and renders the summary', async () => {
    vi.mocked(api.sourceImportStream).mockImplementation((_id, h) => {
      h.onSummary({
        mappings_staged: 12,
        mappings_resolved: 12,
        siblings: 0,
        parents: 0,
        sha256_backfilled: 0,
      });
      return () => {};
    });
    await gotoPlugins();
    await fireEvent.click(screen.getByRole('button', { name: 'Import for my library' }));
    expect(api.sourceImportStream).toHaveBeenCalledWith('hydrus', expect.anything());
    expect(await screen.findByText(/12 resolved/)).toBeInTheDocument();
  });

  it('shows live progress while a library import streams', async () => {
    vi.mocked(api.sourceImportStream).mockImplementation((_id, h) => {
      h.onProgress?.({ files: 2, total: 4, mappings: 10 });
      return () => {};
    });
    await gotoPlugins();
    await fireEvent.click(screen.getByRole('button', { name: 'Import for my library' }));
    expect(await screen.findByText(/2\/4 files/)).toBeInTheDocument();
  });

  it('reports the library import into the activity store', async () => {
    vi.mocked(api.sourceImportStream).mockImplementation((_id, h) => {
      h.onProgress?.({ files: 1, total: 4, mappings: 3 });
      return () => {};
    });
    await gotoPlugins();
    await fireEvent.click(screen.getByRole('button', { name: 'Import for my library' }));
    // The most-recent import activity is the one this test just started (begin
    // never replaces a still-running entry, so .at(-1) is robust to leftovers).
    const latest = activity.activities.filter((a) => a.kind === 'import').at(-1);
    expect(latest?.status).toBe('running');
    expect(latest?.total).toBe(4);
    expect(latest?.done).toBe(1);
    if (latest) activity.dismiss(latest.id);
  });

  it('pulls tag relations and shows the summary', async () => {
    vi.mocked(api.hydrusRelationsStream).mockImplementation((h) => {
      h.onSummary({ siblings: 5, parents: 3 });
      return () => {};
    });
    await gotoPlugins();
    await fireEvent.click(screen.getByRole('button', { name: 'Pull tag relations' }));
    expect(api.hydrusRelationsStream).toHaveBeenCalled();
    expect(await screen.findByText(/5 siblings, 3 parents/)).toBeInTheDocument();
  });

  it('shows determinate progress while relations stream', async () => {
    vi.mocked(api.hydrusRelationsStream).mockImplementation((h) => {
      h.onProgress?.({ edges_done: 1000, edges_total: 4000, siblings: 900, parents: 100 });
      return () => {};
    });
    await gotoPlugins();
    await fireEvent.click(screen.getByRole('button', { name: 'Pull tag relations' }));
    expect(await screen.findByText(/1000\/4000 edges/)).toBeInTheDocument();
  });

  it('shares the busy state with the other import buttons', async () => {
    vi.mocked(api.hydrusRelationsStream).mockImplementation(() => () => {});
    await gotoPlugins();
    await fireEvent.click(screen.getByRole('button', { name: 'Pull tag relations' }));
    // All three import buttons flip to the busy label and disable.
    const busy = screen.getAllByText('Importing…');
    expect(busy).toHaveLength(3);
    for (const b of busy) expect(b.closest('button')).toBeDisabled();
  });

  it('surfaces a full-import error', async () => {
    vi.mocked(api.sourceImport).mockRejectedValue(new Error('import boom'));
    await gotoPlugins();
    await fireEvent.click(screen.getByRole('button', { name: 'Import all files' }));
    expect(await screen.findByText('import boom')).toBeInTheDocument();
  });

  it('surfaces a config-save error separately from import errors', async () => {
    vi.mocked(api.hydrusConfigure).mockRejectedValue(new Error('bad dir'));
    await gotoPlugins();
    await fireEvent.input(screen.getByLabelText('Hydrus DB directory'), { target: { value: '/x' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByText('bad dir')).toBeInTheDocument();
  });

  it('shows the Tag categories section under the Tags tab', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await fireEvent.click(screen.getByLabelText('settings'));
    await fireEvent.click(screen.getByRole('tab', { name: 'Tags' }));
    expect(screen.getByText('Tag categories')).toBeInTheDocument();
  });

  // --- a11y: focus management ---

  it('moves focus to the modal when it opens', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    const modal = screen.getByRole('dialog', { name: 'settings' });
    await vi.waitFor(() => {
      expect(document.activeElement).toBe(modal);
    });
  });

  it('Tab on the last focusable in the modal wraps to the first', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    const modal = screen.getByRole('dialog', { name: 'settings' });
    await vi.waitFor(() => expect(document.activeElement).toBe(modal));

    // Collect focusables the same way the trap does.
    const focusables = getFocusableElements(modal);
    const last = focusables[focusables.length - 1];
    const first = focusables[0];
    last.focus();
    last.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true }));
    expect(document.activeElement).toBe(first);
  });

  it('Shift+Tab on the first focusable in the modal wraps to the last', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    const modal = screen.getByRole('dialog', { name: 'settings' });
    await vi.waitFor(() => expect(document.activeElement).toBe(modal));

    const focusables = getFocusableElements(modal);
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    first.focus();
    first.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true, cancelable: true }),
    );
    expect(document.activeElement).toBe(last);
  });

  it('restores focus to the settings trigger button when the modal closes via Escape', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    const trigger = screen.getByLabelText('settings');
    trigger.focus();
    await open();
    await vi.waitFor(() => expect(document.activeElement).not.toBe(trigger));

    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(document.activeElement).toBe(trigger);
  });

  it('restores focus to the settings trigger button when the modal closes via × button', async () => {
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    const trigger = screen.getByLabelText('settings');
    trigger.focus();
    await open();
    await vi.waitFor(() => expect(document.activeElement).not.toBe(trigger));

    await fireEvent.click(screen.getByLabelText('close settings'));
    expect(document.activeElement).toBe(trigger);
  });

  it('sync tab lists repos and the empty state', async () => {
    // Empty state
    vi.mocked(api.listRepos).mockResolvedValueOnce([]);
    const { unmount } = render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.click(screen.getByRole('tab', { name: 'Sync' }));
    await vi.waitFor(() => expect(screen.getByText('no repos subscribed')).toBeInTheDocument());
    unmount();

    // With a repo
    vi.mocked(api.listRepos).mockResolvedValueOnce([{ name: 'r', url: 'http://x' }]);
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.click(screen.getByRole('tab', { name: 'Sync' }));
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'remove repo r' })).toBeInTheDocument(),
    );
    expect(screen.getByText('r')).toBeInTheDocument();
  });

  it('add-repo flow: empty url shows inline error; filled url calls addRepo with url-only body and refreshes list', async () => {
    vi.mocked(api.listRepos).mockResolvedValue([]);
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.click(screen.getByRole('tab', { name: 'Sync' }));
    await vi.waitFor(() => expect(screen.getByText('no repos subscribed')).toBeInTheDocument());

    // The name input must not be present (repo name is resolved from caps).
    expect(screen.queryByLabelText('repo name')).toBeNull();

    // Empty url → inline error
    await fireEvent.click(screen.getByRole('button', { name: 'add' }));
    expect(screen.getByText('url is required')).toBeInTheDocument();

    // Filled url → addRepo called with url only, list refreshes to show new repo
    vi.mocked(api.listRepos).mockResolvedValueOnce([{ name: 'myrepo', url: 'http://x:9000' }]);
    await fireEvent.input(screen.getByLabelText('repo url'), { target: { value: 'http://x:9000' } });
    await fireEvent.click(screen.getByRole('button', { name: 'add' }));
    await vi.waitFor(() => expect(api.addRepo).toHaveBeenCalledWith('http://x:9000'));
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'remove repo myrepo' })).toBeInTheDocument(),
    );
  });

  it('remove asks for confirmation with purge unchecked by default', async () => {
    vi.mocked(api.listRepos).mockResolvedValue([{ name: 'r', url: 'http://x' }]);
    vi.mocked(api.removeRepo).mockResolvedValue();
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await open();
    await fireEvent.click(screen.getByRole('tab', { name: 'Sync' }));
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'remove repo r' })).toBeInTheDocument(),
    );

    await fireEvent.click(screen.getByRole('button', { name: 'remove repo r' }));
    // Confirmation row appears with purge checkbox unchecked by default
    const checkbox = screen.getByRole('checkbox') as HTMLInputElement;
    expect(checkbox.checked).toBe(false);

    // Click confirm — removeRepo called with purge=false
    await fireEvent.click(screen.getByRole('button', { name: 'confirm remove r' }));
    await vi.waitFor(() =>
      expect(api.removeRepo).toHaveBeenCalledWith('r', false),
    );
  });
});

describe('SettingsPanel repo width line (#179 §8.4)', () => {
  /** Navigate to Sync tab with a mocked single-repo response, return when rendered. */
  async function renderWithRepo(repoObj: { name: string; url: string; max_query_bits?: number; min_query_bits?: number }) {
    vi.mocked(api.listRoots).mockResolvedValue([]);
    vi.mocked(api.listRepos).mockResolvedValue([repoObj]);
    vi.mocked(api.hydrusConfigure).mockResolvedValue();
    vi.mocked(api.sourceImport).mockResolvedValue({ mappings_staged: 0, mappings_resolved: 0, siblings: 0, parents: 0, sha256_backfilled: 0 });
    vi.mocked(api.sourceImportStream).mockReturnValue(() => {});
    vi.mocked(api.addRepo).mockResolvedValue();
    vi.mocked(api.removeRepo).mockResolvedValue();
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await fireEvent.click(screen.getByLabelText('settings'));
    await fireEvent.click(screen.getByRole('tab', { name: 'Sync' }));
    // Wait until the repo name is visible (listRepos resolved).
    await vi.waitFor(() => expect(screen.getByText(repoObj.name)).toBeInTheDocument());
  }

  beforeEach(() => {
    vi.mocked(api.listRoots).mockReset();
    vi.mocked(api.listRepos).mockReset();
    vi.mocked(api.hydrusConfigure).mockReset();
    vi.mocked(api.sourceImport).mockReset();
    vi.mocked(api.sourceImportStream).mockReset();
    vi.mocked(api.hydrusRelationsStream).mockReset();
    vi.mocked(api.hydrusRelationsStream).mockReturnValue(() => {});
    vi.mocked(api.addRepo).mockReset();
    vi.mocked(api.removeRepo).mockReset();
    localStorage.clear();
    view.zoomLevel = 8;
  });

  it('ceiling-only: renders "query width ≤ N bits" when min_query_bits is absent', async () => {
    await renderWithRepo({ name: 'ptr', url: 'http://x', max_query_bits: 24 });
    expect(screen.getByText(/query width ≤ 24 bits/)).toBeInTheDocument();
  });

  it('both-bounds-with-headroom: renders "query width N bits (repo min M)" when min ≤ max', async () => {
    await renderWithRepo({ name: 'ptr', url: 'http://x', max_query_bits: 24, min_query_bits: 16 });
    expect(screen.getByText(/query width 24 bits \(repo min 16\)/)).toBeInTheDocument();
    // Must NOT carry the warning class in this (healthy) case.
    const span = screen.getByText(/query width 24 bits \(repo min 16\)/).closest('span');
    expect(span?.classList.contains('repo-width-clamped')).toBe(false);
  });

  it('clamp-up: renders warning form and .repo-width-clamped class when min > max', async () => {
    await renderWithRepo({ name: 'ptr', url: 'http://x', max_query_bits: 12, min_query_bits: 16 });
    expect(screen.getByText(/query width 12 → 16 bits \(raised to repo minimum\)/)).toBeInTheDocument();
    const span = screen.getByText(/query width 12 → 16 bits \(raised to repo minimum\)/).closest('span');
    // The --warn token branch must be active via the clamped class.
    expect(span?.classList.contains('repo-width-clamped')).toBe(true);
  });
});

describe('SettingsPanel Hydrus DB picker under Tauri', () => {
  beforeEach(() => {
    vi.mocked(api.listRoots).mockReset();
    vi.mocked(api.listRoots).mockResolvedValue([]);
    vi.mocked(api.hydrusConfigure).mockReset();
    vi.mocked(api.hydrusConfigure).mockResolvedValue();
    vi.mocked(api.sourceImport).mockReset();
    vi.mocked(api.sourceImport).mockResolvedValue({ mappings_staged: 0, mappings_resolved: 0, siblings: 0, parents: 0, sha256_backfilled: 0 });
    vi.mocked(api.sourceImportStream).mockReset();
    vi.mocked(api.sourceImportStream).mockReturnValue(() => {});
    vi.mocked(api.listRepos).mockReset();
    vi.mocked(api.listRepos).mockResolvedValue([]);
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  });

  afterEach(() => {
    delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  });

  it('fills the Hydrus DB directory from the native dialog', async () => {
    const { open: openDialog } = await import('@tauri-apps/plugin-dialog');
    vi.mocked(openDialog).mockResolvedValue('/picked/hydrus');
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await fireEvent.click(screen.getByLabelText('settings'));
    await fireEvent.click(screen.getByRole('tab', { name: 'Plugins' }));
    await fireEvent.click(screen.getByLabelText('choose Hydrus DB folder'));
    expect(await screen.findByDisplayValue('/picked/hydrus')).toBeInTheDocument();
  });

  it('leaves the Hydrus DB directory unchanged when the dialog is cancelled', async () => {
    const { open: openDialog } = await import('@tauri-apps/plugin-dialog');
    vi.mocked(openDialog).mockResolvedValue(null);
    render(SettingsPanel, { onimported: () => {}, onremoved: () => {}, onrescope: () => {} });
    await fireEvent.click(screen.getByLabelText('settings'));
    await fireEvent.click(screen.getByRole('tab', { name: 'Plugins' }));
    await fireEvent.click(screen.getByLabelText('choose Hydrus DB folder'));
    expect((screen.getByLabelText('Hydrus DB directory') as HTMLInputElement).value).toBe('');
  });
});
