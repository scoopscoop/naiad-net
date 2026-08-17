import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { tick } from 'svelte';
import { render, screen, fireEvent } from '@testing-library/svelte';
import Inspector from './Inspector.svelte';
import * as api from '../lib/api';
import { view } from '../lib/settings.svelte';
import { contextMenu } from '../lib/context-menu.svelte';
import type { MenuItem } from '../lib/menu-items';
import type { FileDto, TagDetail } from '../lib/types';
import { thumbQueue } from '../lib/thumb-queue';
import { thumbStream } from '../lib/thumb-stream';

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

const file = (hash: string, name = `${hash}.png`): FileDto => ({
  hash,
  name,
  size: 1536,
  path: `/${name}`,
  imported_at: 100,
  created_at: 80,
  modified_at: 90,
  mime: 'image/png',
});

const tag: TagDetail = { tag: 'character:samus', presence: 'local', services: [], relations: false };
const aFile = file('a', 'a.png');

/** Find the chip <button> for a tag. Uses selector:'span' to target the label
 *  span directly and avoid ambiguity with any other elements. */
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

describe('Inspector', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 1280 });
    localStorage.clear();
    view.inspectorCollapsed = false;
    vi.clearAllMocks();
    vi.spyOn(thumbQueue, 'request').mockImplementation(() => () => {});
    vi.spyOn(thumbStream, 'request').mockImplementation(() => () => {});
    vi.mocked(api.tagsDetailed).mockResolvedValue([tag]);
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
    vi.useRealTimers();
    vi.restoreAllMocks();
    contextMenu.close();
  });

  it('renders an empty state without a focused file', () => {
    render(Inspector, { file: null, onopen: () => {}, onerror: () => {}, onsearchtag: () => {} });
    expect(screen.getByText('select a file')).toBeInTheDocument();
  });

  it('requests the peek thumbnail immediately over HTTP without using the stream', () => {
    const cancel = vi.fn();
    const httpRequest = vi.mocked(thumbQueue.request).mockReturnValue(cancel);
    const streamRequest = vi.mocked(thumbStream.request);

    const { unmount } = render(Inspector, {
      file: aFile,
      onopen: () => {},
      onerror: () => {},
      onsearchtag: () => {},
    });

    expect(httpRequest).toHaveBeenCalledOnce();
    expect(httpRequest).toHaveBeenCalledWith('/thumb/a', expect.anything());
    expect(streamRequest).not.toHaveBeenCalled();
    unmount();
    expect(cancel).toHaveBeenCalledOnce();
  });

  it('debounces tag loading and renders the focused file peek', async () => {
    render(Inspector, { file: file('a', 'a.png'), onopen: () => {}, onerror: () => {}, onsearchtag: () => {} });

    expect(screen.getByText('a.png')).toBeInTheDocument();
    expect(api.tagsDetailed).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(100);

    expect(api.tagsDetailed).toHaveBeenCalledWith('a', false);
    expect(await screen.findByText('character:samus')).toBeInTheDocument();
    expect(screen.getByText('1.5 KB')).toBeInTheDocument();
  });

  it('drops stale tag responses when focus changes quickly', async () => {
    let resolveA!: (tags: TagDetail[]) => void;
    vi.mocked(api.tagsDetailed)
      .mockReturnValueOnce(new Promise((resolve) => { resolveA = resolve; }))
      .mockResolvedValueOnce([{ tag: 'series:metroid', presence: 'local', services: [], relations: false }]);
    const { rerender } = render(Inspector, { file: file('a'), onopen: () => {}, onerror: () => {}, onsearchtag: () => {} });

    await vi.advanceTimersByTimeAsync(100);
    await rerender({ file: file('b'), onopen: () => {}, onerror: () => {}, onsearchtag: () => {} });
    await vi.advanceTimersByTimeAsync(100);
    resolveA([tag]);

    expect(await screen.findByText('series:metroid')).toBeInTheDocument();
    expect(screen.queryByText('character:samus')).toBeNull();
  });

  it('opens the focused file from the Open button', async () => {
    const onopen = vi.fn();
    render(Inspector, { file: file('a', 'a.png'), onopen, onerror: () => {}, onsearchtag: () => {} });
    await fireEvent.click(screen.getByLabelText('open a.png'));
    expect(onopen).toHaveBeenCalledTimes(1);
  });

  it('renders nothing when hidden, with a file focused', () => {
    render(Inspector, { file: file('a', 'a.png'), onopen: () => {}, onerror: () => {}, hidden: true, onsearchtag: () => {} });
    expect(screen.queryByText('INSPECTOR')).toBeNull();
    expect(screen.queryByLabelText('expand inspector')).toBeNull();
    expect(screen.queryByLabelText('collapse inspector')).toBeNull();
    expect(screen.queryByText('a.png')).toBeNull();
  });

  it('renders nothing when hidden, with no file focused', () => {
    render(Inspector, { file: null, onopen: () => {}, onerror: () => {}, hidden: true, onsearchtag: () => {} });
    expect(screen.queryByText('INSPECTOR')).toBeNull();
    expect(screen.queryByText('select a file')).toBeNull();
    expect(screen.queryByLabelText('expand inspector')).toBeNull();
  });

  it('still renders normally when hidden is explicitly false', () => {
    render(Inspector, { file: null, onopen: () => {}, onerror: () => {}, hidden: false, onsearchtag: () => {} });
    expect(screen.getByText('select a file')).toBeInTheDocument();
  });

  it('collapse/expand round-trip drives view.inspectorCollapsed and re-renders', async () => {
    render(Inspector, { file: file('a', 'a.png'), onopen: () => {}, onerror: () => {}, onsearchtag: () => {} });

    // Start expanded (beforeEach sets view.inspectorCollapsed = false)
    const collapse = screen.getByRole('button', { name: 'collapse inspector' });
    expect(collapse).toHaveAttribute('aria-expanded', 'true');
    expect(screen.queryByText('>')).toBeNull();

    // Click the collapse chevron — inspector should collapse
    await fireEvent.click(collapse);
    expect(view.inspectorCollapsed).toBe(true);
    const expand = screen.getByRole('button', { name: 'expand inspector' });
    expect(expand).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByText('<')).toBeNull();

    // Click the expand strip — inspector should expand again
    await fireEvent.click(expand);
    expect(view.inspectorCollapsed).toBe(false);
    expect(screen.getByLabelText('collapse inspector')).toBeInTheDocument();
  });

  // narrowWindow acts as a floor: inspector shows the 28px strip even when the
  // user's stored preference is expanded.
  it('narrowWindow=true forces the collapsed strip even when view.inspectorCollapsed is false', () => {
    view.inspectorCollapsed = false;
    render(Inspector, { file: file('a', 'a.png'), onopen: () => {}, onerror: () => {}, narrowWindow: true, onsearchtag: () => {} });
    expect(screen.getByLabelText('window too narrow to expand inspector')).toBeInTheDocument();
    expect(screen.queryByLabelText('collapse inspector')).toBeNull();
  });

  it('narrowWindow=false respects the user preference (expanded)', () => {
    view.inspectorCollapsed = false;
    render(Inspector, { file: file('a', 'a.png'), onopen: () => {}, onerror: () => {}, narrowWindow: false, onsearchtag: () => {} });
    expect(screen.getByLabelText('collapse inspector')).toBeInTheDocument();
  });

  // The strip is disabled when narrowWindow=true — clicking it must not expand
  // the inspector, and the aria-label communicates the constraint.
  it('strip is disabled and labelled appropriately when narrowWindow=true', () => {
    view.inspectorCollapsed = false;
    render(Inspector, { file: file('a', 'a.png'), onopen: () => {}, onerror: () => {}, narrowWindow: true, onsearchtag: () => {} });
    const strip = screen.getByLabelText('window too narrow to expand inspector') as HTMLButtonElement;
    expect(strip).toBeDisabled();
  });

  it('clicking disabled strip is a no-op: view.inspectorCollapsed unchanged', async () => {
    view.inspectorCollapsed = false;
    render(Inspector, { file: file('a', 'a.png'), onopen: () => {}, onerror: () => {}, narrowWindow: true, onsearchtag: () => {} });
    const strip = screen.getByLabelText('window too narrow to expand inspector');
    await fireEvent.click(strip);
    // Strip was disabled — preference must not have been set to false (already false) or true
    expect(view.inspectorCollapsed).toBe(false);
    // Still showing the narrow strip
    expect(screen.getByLabelText('window too narrow to expand inspector')).toBeInTheDocument();
  });

  it('clicking expand strip sets view.inspectorCollapsed=false without mutating narrowWindow', async () => {
    view.inspectorCollapsed = true; // user previously collapsed
    const { rerender } = render(Inspector, {
      file: file('a', 'a.png'),
      onopen: () => {},
      onerror: () => {},
      narrowWindow: true,
      onsearchtag: () => {},
    });
    // Strip is visible and disabled because narrowWindow is true
    expect(screen.getByLabelText('window too narrow to expand inspector')).toBeDisabled();
    // Re-render with narrowWindow cleared — now the normal expand strip appears
    await rerender({ file: file('a', 'a.png'), onopen: () => {}, onerror: () => {}, narrowWindow: false, onsearchtag: () => {} });
    // view.inspectorCollapsed is still true, so the non-disabled expand strip shows
    expect(screen.getByLabelText('expand inspector')).toBeInTheDocument();
    expect(screen.getByLabelText('expand inspector')).not.toBeDisabled();
    await fireEvent.click(screen.getByLabelText('expand inspector'));
    expect(view.inspectorCollapsed).toBe(false);
    expect(screen.getByLabelText('collapse inspector')).toBeInTheDocument();
  });

  it('mutating gates the add form while a removal is in flight', async () => {
    let resolveRemove!: () => void;
    vi.mocked(api.removeTags).mockImplementationOnce(
      () => new Promise<void>((r) => (resolveRemove = r)),
    );

    render(Inspector, { file: aFile, onopen: vi.fn(), onerror: vi.fn(), onsearchtag: () => {} });
    await vi.waitFor(() => screen.getByText('character:samus'));

    await triggerRemoveViaMenu('character:samus');
    await tick();
    expect(screen.getByRole('button', { name: 'Add' })).toHaveAttribute('aria-disabled', 'true');

    resolveRemove();
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'Add' })).not.toHaveAttribute('aria-disabled', 'true'),
    );
  });

  it('makes the add form inert, and an Enter press a no-op, while a removal is in flight', async () => {
    let resolveRemove!: () => void;
    vi.mocked(api.removeTags).mockImplementationOnce(
      () => new Promise<void>((r) => (resolveRemove = r)),
    );

    render(Inspector, { file: aFile, onopen: vi.fn(), onerror: vi.fn(), onsearchtag: () => {} });
    await vi.waitFor(() => screen.getByText('character:samus'));
    const addBtn = screen.getByRole('button', { name: 'Add' });
    const input = screen.getByLabelText('add inspector tag');
    expect(addBtn).not.toHaveAttribute('aria-disabled', 'true');

    await triggerRemoveViaMenu('character:samus');
    await tick();
    expect(addBtn).toHaveAttribute('aria-disabled', 'true');
    expect(input).toHaveAttribute('aria-disabled', 'true');

    // Enter in the tag field submits the form whatever the button's state, so
    // the guard in add() — not the attribute — is what has to stop the add.
    await fireEvent.input(input, { target: { value: 'series:metroid' } });
    await fireEvent.submit(input.closest('form')!);
    await tick();
    expect(api.addTags).not.toHaveBeenCalled();

    resolveRemove();
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'Add' })).not.toHaveAttribute(
        'aria-disabled',
        'true',
      ),
    );
  });

  it('reports a rejected action rather than dropping it silently', async () => {
    let resolveRemove!: () => void;
    vi.mocked(api.removeTags).mockImplementationOnce(
      () => new Promise<void>((r) => (resolveRemove = r)),
    );
    const onerror = vi.fn();

    render(Inspector, { file: aFile, onopen: vi.fn(), onerror, onsearchtag: () => {} });
    await vi.waitFor(() => screen.getByText('character:samus'));

    await triggerRemoveViaMenu('character:samus');
    await tick();

    const input = screen.getByLabelText('add inspector tag') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'series:metroid' } });
    await fireEvent.submit(input.closest('form')!);
    await tick();

    expect(api.addTags).not.toHaveBeenCalled();
    expect(onerror).toHaveBeenCalledWith(expect.stringContaining('still saving'));
    // The typed tag survives, so the retry the message asks for costs nothing.
    expect(input.value).toBe('series:metroid');

    resolveRemove();
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'Add' })).not.toHaveAttribute(
        'aria-disabled',
        'true',
      ),
    );
  });

  it('an empty add is not reported as a busy rejection', async () => {
    let resolveRemove!: () => void;
    vi.mocked(api.removeTags).mockImplementationOnce(
      () => new Promise<void>((r) => (resolveRemove = r)),
    );
    const onerror = vi.fn();

    render(Inspector, { file: aFile, onopen: vi.fn(), onerror, onsearchtag: () => {} });
    await vi.waitFor(() => screen.getByText('character:samus'));

    await triggerRemoveViaMenu('character:samus');
    await tick();
    await fireEvent.submit(screen.getByLabelText('add inspector tag').closest('form')!);
    await tick();

    expect(onerror).not.toHaveBeenCalled();

    resolveRemove();
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'Add' })).not.toHaveAttribute('aria-disabled', 'true'),
    );
  });

  it('context-menu remove is disabled while an add is in flight', async () => {
    let resolveAdd!: () => void;
    vi.mocked(api.addTags).mockImplementationOnce(
      () => new Promise<void>((r) => (resolveAdd = r)),
    );

    render(Inspector, { file: aFile, onopen: vi.fn(), onerror: vi.fn(), onsearchtag: () => {} });
    await vi.waitFor(() => screen.getByText('character:samus'));

    await fireEvent.input(screen.getByLabelText('add inspector tag'), {
      target: { value: 'series:metroid' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await tick();

    // While add is in flight, open context menu on the tag chip
    const tagEl = screen.getByText('character:samus').closest('button')!;
    await fireEvent.contextMenu(tagEl);
    const rmItem = contextMenu.items.find(
      (e): e is MenuItem => e !== 'separator' && e.id === 'tag-remove',
    )!;
    expect(rmItem.disabled).toBe(true);

    // Calling onselect() on the disabled item is blocked by the mutating guard in remove()
    rmItem.onselect();
    contextMenu.close();
    await tick();
    expect(api.removeTags).not.toHaveBeenCalled();

    resolveAdd();
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'Add' })).not.toHaveAttribute(
        'aria-disabled',
        'true',
      ),
    );
  });

  it('clears the busy state when a mutation fails', async () => {
    vi.mocked(api.removeTags).mockRejectedValueOnce(new Error('nope'));
    const onerror = vi.fn();

    render(Inspector, { file: aFile, onopen: vi.fn(), onerror, onsearchtag: () => {} });
    await vi.waitFor(() => screen.getByText('character:samus'));

    await triggerRemoveViaMenu('character:samus');
    await vi.waitFor(() => expect(onerror).toHaveBeenCalled());
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'Add' })).not.toHaveAttribute('aria-disabled', 'true'),
    );
  });

  it('stale remove on old file does not clear the new file busy row', async () => {
    const tagA: TagDetail = { tag: 'character:samus', presence: 'local', services: [], relations: false };
    const tagB: TagDetail = { tag: 'series:metroid', presence: 'local', services: [], relations: false };

    let resolveRemoveA!: () => void;
    let resolveRemoveB!: () => void;
    vi.mocked(api.removeTags)
      .mockImplementationOnce(() => new Promise<void>((r) => (resolveRemoveA = r)))
      .mockImplementationOnce(() => new Promise<void>((r) => (resolveRemoveB = r)));
    vi.mocked(api.tagsDetailed).mockImplementation((hash) =>
      Promise.resolve(hash === 'a' ? [tagA] : [tagB]),
    );

    const { rerender } = render(Inspector, { file: file('a'), onopen: vi.fn(), onerror: vi.fn(), onsearchtag: () => {} });
    await vi.waitFor(() => screen.getByText('character:samus'));

    // Start remove on file A — stays pending
    await triggerRemoveViaMenu('character:samus');
    await tick();
    expect(screen.getByRole('button', { name: 'Add' })).toHaveAttribute('aria-disabled', 'true');

    // Switch to file B — $effect resets UI state but inflight['a'] persists
    await rerender({ file: file('b'), onopen: vi.fn(), onerror: vi.fn(), onsearchtag: () => {} });
    await vi.waitFor(() => screen.getByText('series:metroid'));

    // Start remove on file B — also stays pending
    await triggerRemoveViaMenu('series:metroid');
    await tick();
    expect(screen.getByRole('button', { name: 'Add' })).toHaveAttribute('aria-disabled', 'true'); // B is busy

    // Resolve file A's stale remove — the fix must prevent it from touching B
    resolveRemoveA();
    await tick();
    await tick();

    // B's mutation must still be in flight
    expect(screen.getByRole('button', { name: 'Add' })).toHaveAttribute('aria-disabled', 'true');

    // Resolve B's remove so the test finishes cleanly
    resolveRemoveB();
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'Add' })).not.toHaveAttribute('aria-disabled', 'true'),
    );
  });

  it('a round trip through another file does not reopen the guard on a pending add', async () => {
    let resolveAdd!: () => void;
    vi.mocked(api.addTags).mockImplementation(() => new Promise<void>((r) => (resolveAdd = r)));

    const props = { file: aFile, onopen: vi.fn(), onerror: vi.fn(), onsearchtag: () => {} };
    const { rerender } = render(Inspector, props);
    await vi.advanceTimersByTimeAsync(100);

    // Start an add on file A — stays pending.
    await fireEvent.input(screen.getByLabelText('add inspector tag'), {
      target: { value: 'series:metroid' },
    });
    await fireEvent.submit(screen.getByLabelText('add inspector tag').closest('form')!);
    await tick();
    expect(api.addTags).toHaveBeenCalledTimes(1);

    // Switch away and back while A's add is still in flight.
    await rerender({ ...props, file: file('b') });
    await vi.advanceTimersByTimeAsync(100);
    await rerender({ ...props, file: aFile });
    await vi.advanceTimersByTimeAsync(100);

    // A's add is still running, so the guard must still hold. Submitting again
    // must not put a second addTags for the same hash in flight.
    await fireEvent.input(screen.getByLabelText('add inspector tag'), {
      target: { value: 'another:tag' },
    });
    await fireEvent.submit(screen.getByLabelText('add inspector tag').closest('form')!);
    await tick();
    expect(api.addTags).toHaveBeenCalledTimes(1);

    resolveAdd();
    await vi.waitFor(() => expect(screen.getByLabelText('add inspector tag')).toBeTruthy());
  });

  it('restores the add form guard when returning to a file whose mutation still runs', async () => {
    let resolveRemove!: () => void;
    vi.mocked(api.removeTags).mockImplementation(() => new Promise<void>((r) => (resolveRemove = r)));

    const props = { file: aFile, onopen: vi.fn(), onerror: vi.fn(), onsearchtag: () => {} };
    const { rerender } = render(Inspector, props);
    await vi.waitFor(() => screen.getByText('character:samus'));

    await triggerRemoveViaMenu('character:samus');
    await tick();
    expect(screen.getByRole('button', { name: 'Add' })).toHaveAttribute('aria-disabled', 'true');

    await rerender({ ...props, file: file('b') });
    await vi.advanceTimersByTimeAsync(100);
    await rerender({ ...props, file: aFile });
    await vi.advanceTimersByTimeAsync(100);

    // The mutation never stopped, so the Add form must still be guarded.
    expect(screen.getByRole('button', { name: 'Add' })).toHaveAttribute('aria-disabled', 'true');

    resolveRemove();
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'Add' })).not.toHaveAttribute('aria-disabled', 'true'),
    );
  });

  it('pull remote is hidden when no repos are subscribed', async () => {
    vi.mocked(api.listRepos).mockResolvedValue([]);
    render(Inspector, { file: aFile, onopen: vi.fn(), onerror: vi.fn(), onsearchtag: () => {} });
    await vi.waitFor(() => expect(api.listRepos).toHaveBeenCalled());
    await tick();
    expect(screen.queryByRole('button', { name: 'pull remote tags' })).toBeNull();
  });

  it('pull remote pulls the focused file and shows the result', async () => {
    vi.mocked(api.listRepos).mockResolvedValue([{ name: 'r', url: 'http://x' }]);
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      handlers.onSummary({
        results: [{ repo: 'r', matched_files: 1, mappings: 5, missing_sha256: 0 }],
        matched_files: 1,
        mappings: 5,
      });
      return () => {};
    });

    render(Inspector, { file: aFile, onopen: vi.fn(), onerror: vi.fn(), onsearchtag: () => {} });
    const btn = await screen.findByRole('button', { name: 'pull remote tags' });

    await fireEvent.click(btn);
    expect(api.pullFileTagsStream).toHaveBeenCalledWith([aFile.hash], expect.anything());
    await vi.waitFor(() =>
      expect(screen.getByRole('button', { name: 'pull remote tags' })).toHaveTextContent(
        '5 mappings',
      ),
    );
  });

  it('pull remote shows count and sends all hashes when focused file is in the selection', async () => {
    vi.mocked(api.listRepos).mockResolvedValue([{ name: 'r', url: 'http://x' }]);
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      handlers.onSummary({
        results: [{ repo: 'r', matched_files: 2, mappings: 2, missing_sha256: 0 }],
        matched_files: 2,
        mappings: 2,
      });
      return () => {};
    });

    render(Inspector, {
      file: aFile,
      onopen: vi.fn(),
      onerror: vi.fn(),
      selectedHashes: [aFile.hash, 'b'],
      onsearchtag: () => {},
    });

    const btn = await screen.findByRole('button', { name: 'pull remote tags' });
    expect(btn).toHaveTextContent('pull remote (2)');

    await fireEvent.click(btn);
    expect(api.pullFileTagsStream).toHaveBeenCalledWith([aFile.hash, 'b'], expect.anything());
  });
});
