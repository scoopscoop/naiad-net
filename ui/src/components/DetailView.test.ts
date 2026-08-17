import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import { tick } from 'svelte';
import DetailView from './DetailView.svelte';
import * as api from '../lib/api';
import { categories } from '../lib/categories.svelte';
import { view } from '../lib/settings.svelte';
import { contextMenu } from '../lib/context-menu.svelte';
import type { MenuItem } from '../lib/menu-items';
import type { TagDetail } from '../lib/types';

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return {
    ...actual,
    tagsDetailed: vi.fn(),
    addTags: vi.fn(),
    removeTags: vi.fn(),
    rejectTag: vi.fn(),
    undoReject: vi.fn(),
    listRejections: vi.fn(),
    report: vi.fn(),
    listRepos: vi.fn(),
    pullFileTagsStream: vi.fn(),
  };
});

const file = {
  hash: 'abc',
  name: 'a.png',
  size: 1,
  path: '/a.png',
  imported_at: 100,
  created_at: 80,
  modified_at: 90,
  mime: 'image/png',
};

const localTag = { tag: 'character:samus', presence: 'local' as const, services: [], relations: false };
const pulledTag = { tag: 'series:metroid', presence: 'pulled' as const, services: ['repo'], relations: false };
const otherTag = { tag: 'photoset:beach', presence: 'local' as const, services: [], relations: false };

/** Find the chip <button> for a tag via the label span inside the chip. */
function chipLi(tagText: string): HTMLElement {
  return screen.getByText(tagText, { selector: 'span' }).closest('button')!;
}

/** Trigger remove for a tag via the chip context menu. */
async function triggerRemoveViaMenu(tagText: string) {
  await fireEvent.contextMenu(chipLi(tagText));
  const removeItem = contextMenu.items.find(
    (e): e is MenuItem => e !== 'separator' && e.id === 'tag-remove',
  )!;
  removeItem.onselect();
  contextMenu.close();
}

/** Trigger reject/hide for a pulled tag via the chip context menu. */
async function triggerRejectViaMenu(tagText: string) {
  await fireEvent.contextMenu(chipLi(tagText));
  const hideItem = contextMenu.items.find(
    (e): e is MenuItem => e !== 'separator' && e.id === 'tag-hide',
  )!;
  hideItem.onselect();
  contextMenu.close();
}

describe('DetailView', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    categories.reset();
    view.localOnly = false;
    vi.mocked(api.tagsDetailed).mockResolvedValue([localTag, pulledTag, otherTag]);
    vi.mocked(api.addTags).mockResolvedValue();
    vi.mocked(api.removeTags).mockResolvedValue();
    vi.mocked(api.rejectTag).mockResolvedValue({ reports: false });
    vi.mocked(api.undoReject).mockResolvedValue();
    vi.mocked(api.listRejections).mockResolvedValue([]);
    vi.mocked(api.report).mockResolvedValue();
    vi.mocked(api.listRepos).mockResolvedValue([]);
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      handlers.onSummary({ results: [], matched_files: 0, mappings: 0 });
      return () => {};
    });
  });

  afterEach(() => {
    contextMenu.close();
  });

  it('lists tags with a local and a pulled tag on mount', async () => {
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    expect(await screen.findByText('character:samus')).toBeInTheDocument();
    expect(await screen.findByText('series:metroid')).toBeInTheDocument();
    expect(screen.queryByLabelText('trust for series:metroid')).toBeNull();
    expect(screen.queryByLabelText('trust for character:samus')).toBeNull();
  });

  it('adds a tag then refreshes the list', async () => {
    const extraTag = { tag: 'series:metroid2', presence: 'local' as const, services: [], relations: false };
    vi.mocked(api.tagsDetailed)
      .mockResolvedValueOnce([localTag, pulledTag])
      .mockResolvedValueOnce([localTag, pulledTag, extraTag]);
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('character:samus');

    await fireEvent.input(screen.getByLabelText('add tag'), {
      target: { value: 'series:metroid2' },
    });
    await fireEvent.click(screen.getByText('Add'));

    expect(api.addTags).toHaveBeenCalledWith('abc', ['series:metroid2']);
    expect(await screen.findByText('series:metroid2')).toBeInTheDocument();
  });

  it('removes a tag', async () => {
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('character:samus');

    await triggerRemoveViaMenu('character:samus');

    expect(api.removeTags).toHaveBeenCalledWith('abc', ['character:samus']);
  });

  it('rejects a second remove while the first is in flight', async () => {
    let release: () => void = () => {};
    vi.mocked(api.removeTags).mockReturnValueOnce(
      new Promise<void>((resolve) => {
        release = resolve;
      }),
    );
    const onerror = vi.fn();
    render(DetailView, { file, onerror, onsearchtag: () => {} });
    await screen.findByText('character:samus');

    // Trigger first remove — stays in flight
    await triggerRemoveViaMenu('character:samus');
    await tick();

    // Trigger second remove while first is in flight — guard fires
    await triggerRemoveViaMenu('character:samus');
    await tick();

    expect(api.removeTags).toHaveBeenCalledTimes(1);
    expect(onerror).toHaveBeenCalledWith('Another change is still saving. Try again in a moment.');
    release();
  });

  it('a stale tag load cannot resurrect a tag a later refresh removed', async () => {
    let releaseStale: (tags: TagDetail[]) => void = () => {};
    const stale = new Promise<TagDetail[]>((resolve) => {
      releaseStale = resolve;
    });
    vi.mocked(api.tagsDetailed)
      .mockResolvedValueOnce([localTag]) // initial load
      .mockReturnValueOnce(stale) // localOnly toggle: overtaken below
      .mockResolvedValueOnce([]); // refresh after the remove
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('character:samus');

    view.localOnly = true;
    await vi.waitFor(() => expect(api.tagsDetailed).toHaveBeenCalledTimes(2));

    await triggerRemoveViaMenu('character:samus');
    await vi.waitFor(() => expect(screen.queryByText('character:samus')).toBeNull());

    releaseStale([localTag]);
    await stale;
    await tick();
    expect(screen.queryByText('character:samus')).toBeNull();
  });

  it('reports a load failure through onerror', async () => {
    vi.mocked(api.tagsDetailed).mockRejectedValue(new Error('boom'));
    const onerror = vi.fn();
    render(DetailView, { file, onerror, onsearchtag: () => {} });
    await vi.waitFor(() => expect(onerror).toHaveBeenCalledWith('boom'));
  });

  it('groups tags under category headers with Other last', async () => {
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('character:samus');
    expect(screen.getByText('Character')).toBeInTheDocument();
    expect(screen.getByText('Series')).toBeInTheDocument();
    expect(screen.getByText('Other')).toBeInTheDocument();
    const heads = screen.getAllByTestId('group-head').map((el) => el.textContent);
    expect(heads).toEqual(['Character', 'Series', 'Other']);
  });

  it('ArrowRight navigates next and ArrowLeft navigates prev', async () => {
    const onnext = vi.fn();
    const onprev = vi.fn();
    render(DetailView, { file, onerror: () => {}, hasPrev: true, hasNext: true, onprev, onnext, onsearchtag: () => {} });
    await screen.findByText('character:samus');
    await fireEvent.keyDown(document.body, { key: 'ArrowRight' });
    await fireEvent.keyDown(document.body, { key: 'ArrowLeft' });
    expect(onnext).toHaveBeenCalledTimes(1);
    expect(onprev).toHaveBeenCalledTimes(1);
  });

  it('ignores arrow keys while the add-tag input is focused', async () => {
    const onnext = vi.fn();
    render(DetailView, { file, onerror: () => {}, hasNext: true, onnext, onsearchtag: () => {} });
    await screen.findByText('character:samus');
    await fireEvent.keyDown(screen.getByLabelText('add tag'), { key: 'ArrowRight' });
    expect(onnext).not.toHaveBeenCalled();
  });

  it('reject follows the remove serialization contract and shows the undo flash', async () => {
    let resolveReject!: (v: { reports: boolean }) => void;
    const inflightReject = new Promise<{ reports: boolean }>((res) => {
      resolveReject = res;
    });
    vi.mocked(api.rejectTag).mockReturnValueOnce(inflightReject);
    const onerror = vi.fn();
    render(DetailView, { file, onerror, onsearchtag: () => {} });
    await screen.findByText('series:metroid');

    // Fire reject for the pulled tag (via context menu)
    await triggerRejectViaMenu('series:metroid');
    await tick();

    // While in flight, try to remove character:samus — busy guard fires
    await triggerRemoveViaMenu('character:samus');
    expect(api.removeTags).not.toHaveBeenCalled();
    expect(onerror).toHaveBeenCalledWith('Another change is still saving. Try again in a moment.');

    // Resolve the in-flight reject
    resolveReject({ reports: false });

    // Flash appears with the Undo button
    expect(await screen.findByText('Rejected series:metroid')).toBeInTheDocument();
    const undoBtn = screen.getByRole('button', { name: 'Undo' });
    expect(undoBtn).toBeInTheDocument();

    // tagsDetailed was refreshed after the reject
    expect(api.tagsDetailed).toHaveBeenCalledTimes(2);

    // Esc dismisses the flash WITHOUT calling undoReject
    await fireEvent.keyDown(document.body, { key: 'Escape' });
    expect(screen.queryByText('Rejected series:metroid')).not.toBeInTheDocument();
    expect(api.undoReject).not.toHaveBeenCalled();
  });

  it('Undo calls undoReject per service then refreshes', async () => {
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('series:metroid');

    await triggerRejectViaMenu('series:metroid');
    expect(await screen.findByText('Rejected series:metroid')).toBeInTheDocument();

    // Click Undo
    await fireEvent.click(screen.getByRole('button', { name: 'Undo' }));

    expect(api.undoReject).toHaveBeenCalledWith('abc', 'series:metroid', 'repo');
    // refresh is called again after undo
    await vi.waitFor(() => expect(api.tagsDetailed).toHaveBeenCalledTimes(3));
    // Flash is gone
    expect(screen.queryByText('Rejected series:metroid')).not.toBeInTheDocument();
  });

  it('opens the report modal when reports=true and single service', async () => {
    vi.mocked(api.rejectTag).mockResolvedValueOnce({ reports: true });
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('series:metroid');

    await triggerRejectViaMenu('series:metroid');

    // Modal appears
    expect(await screen.findByRole('dialog', { name: 'Report series:metroid to repo?' })).toBeInTheDocument();
    // Flash also appears
    expect(screen.getByText('Rejected series:metroid')).toBeInTheDocument();
  });

  it('no report modal when reports=false', async () => {
    vi.mocked(api.rejectTag).mockResolvedValueOnce({ reports: false });
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('series:metroid');

    await triggerRejectViaMenu('series:metroid');

    await screen.findByText('Rejected series:metroid');
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('no report modal when two services rejected', async () => {
    const multiTag = { tag: 'series:zelda', presence: 'pulled' as const, services: ['repo1', 'repo2'], relations: false };
    vi.mocked(api.tagsDetailed).mockResolvedValue([localTag, multiTag]);
    vi.mocked(api.rejectTag).mockResolvedValue({ reports: true });
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('series:zelda');

    await triggerRejectViaMenu('series:zelda');

    await screen.findByText('Rejected series:zelda');
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('clicking Undo while report modal is up clears both flash and modal', async () => {
    vi.mocked(api.rejectTag).mockResolvedValueOnce({ reports: true });
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('series:metroid');

    await triggerRejectViaMenu('series:metroid');
    // Both flash and report modal should appear
    await screen.findByRole('dialog', { name: 'Report series:metroid to repo?' });
    expect(screen.getByText('Rejected series:metroid')).toBeInTheDocument();

    // Click Undo
    await fireEvent.click(screen.getByRole('button', { name: 'Undo' }));

    // Wait for the full undo flow — refresh = 3rd tagsDetailed call
    await vi.waitFor(() => expect(api.tagsDetailed).toHaveBeenCalledTimes(3));
    // Both flash and modal must be gone
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(screen.queryByText('Rejected series:metroid')).not.toBeInTheDocument();
    // Rejection was undone
    expect(api.undoReject).toHaveBeenCalledWith('abc', 'series:metroid', 'repo');
    // Report was NEVER sent
    expect(api.report).not.toHaveBeenCalled();
  });

  it('cancel report leaves rejection standing (undoReject not called)', async () => {
    vi.mocked(api.rejectTag).mockResolvedValueOnce({ reports: true });
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await screen.findByText('series:metroid');

    await triggerRejectViaMenu('series:metroid');
    await screen.findByRole('dialog', { name: 'Report series:metroid to repo?' });

    // Cancel the report
    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));

    // Modal is gone
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    // Rejection is NOT undone — undoReject never called
    expect(api.undoReject).not.toHaveBeenCalled();
    // report not called either
    expect(api.report).not.toHaveBeenCalled();
    // tags were refreshed exactly once after the reject (no second refresh from cancel)
    expect(api.tagsDetailed).toHaveBeenCalledTimes(2);
  });

  // pull-remote button — wired to the single displayed file (#106)
  it('shows pull remote when a repo is subscribed', async () => {
    vi.mocked(api.listRepos).mockResolvedValue([{ name: 'local', url: 'http://x' }]);
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    expect(await screen.findByRole('button', { name: 'pull remote tags' })).toBeInTheDocument();
  });

  it('hides pull remote when no repos are subscribed', async () => {
    vi.mocked(api.listRepos).mockResolvedValue([]);
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    await vi.waitFor(() => expect(api.listRepos).toHaveBeenCalled());
    await tick();
    expect(screen.queryByRole('button', { name: 'pull remote tags' })).toBeNull();
  });

  it('pulls for the displayed file only and reports the count', async () => {
    vi.mocked(api.listRepos).mockResolvedValue([{ name: 'local', url: 'http://x' }]);
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      handlers.onSummary({
        results: [{ repo: 'local', matched_files: 1, mappings: 2, missing_sha256: 0 }],
        matched_files: 1,
        mappings: 2,
      });
      return () => {};
    });
    render(DetailView, { file, onerror: () => {}, onsearchtag: () => {} });
    const btn = await screen.findByRole('button', { name: 'pull remote tags' });
    await fireEvent.click(btn);
    expect(api.pullFileTagsStream).toHaveBeenCalledWith([file.hash], expect.anything());
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'pull remote tags' })).toHaveTextContent(
        '2 mappings',
      ),
    );
  });

  it('stays hidden when listRepos rejects', async () => {
    vi.mocked(api.listRepos).mockRejectedValue(new Error('network'));
    const onerror = vi.fn();
    render(DetailView, { file, onerror, onsearchtag: () => {} });
    await vi.waitFor(() => expect(api.listRepos).toHaveBeenCalled());
    await tick();
    expect(screen.queryByRole('button', { name: 'pull remote tags' })).toBeNull();
    // The factory silently swallows the listRepos failure (repoCount stays 0);
    // it must never route the error through report() and surface it as a toast.
    expect(onerror).not.toHaveBeenCalled();
  });
});
