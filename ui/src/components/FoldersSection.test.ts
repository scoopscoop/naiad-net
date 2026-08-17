import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { tick } from 'svelte';
import { render, screen, fireEvent } from '@testing-library/svelte';
import FoldersSection from './FoldersSection.svelte';
import * as api from '../lib/api';
import { activity } from '../lib/activity.svelte';

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return { ...actual, scanStream: vi.fn(), listRoots: vi.fn(), removeRoot: vi.fn() };
});

vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn() }));

// The component now derives its progress bar from the shared activity store, so
// clear leftover (e.g. still-running) activities between tests to keep them isolated.
afterEach(() => {
  for (const a of [...activity.activities]) activity.dismiss(a.id);
});

describe('FoldersSection', () => {
  beforeEach(() => {
    vi.mocked(api.scanStream).mockReset();
    vi.mocked(api.listRoots).mockReset();
    vi.mocked(api.removeRoot).mockReset();
    vi.mocked(api.listRoots).mockResolvedValue([]);
  });

  it('shows a path field but no native picker in a browser', () => {
    render(FoldersSection, { onimported: () => {} });
    expect(screen.getByLabelText('folder path')).toBeInTheDocument();
    expect(screen.queryByText('Choose folder')).not.toBeInTheDocument();
  });

  it('streams the scan, fires onimported and onsaved on a clean run', async () => {
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onSummary({ imported: 2, marked_missing: 0, errors: [] });
      return () => {};
    });
    const onimported = vi.fn();
    const onsaved = vi.fn();
    render(FoldersSection, { onimported, onsaved });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    expect(api.scanStream).toHaveBeenCalledWith('/photos', expect.anything());
    expect(onimported).toHaveBeenCalledWith({ imported: 2, marked_missing: 0, errors: [] });
    expect(onsaved).toHaveBeenCalled();
  });

  it('lists skipped files when there are errors', async () => {
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onSummary({
        imported: 1,
        marked_missing: 0,
        errors: [{ path: '/photos/bad.xyz', message: 'unsupported type' }],
      });
      return () => {};
    });
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    expect(await screen.findByText(/unsupported type/)).toBeInTheDocument();
  });

  it('shows an inline error when the scan fails', async () => {
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onError('could not reach daemon');
      return () => {};
    });
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    expect(await screen.findByText('could not reach daemon')).toBeInTheDocument();
  });

  it('shows a determinate bar and live count while scanning', async () => {
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onProgress?.({ imported: 42, skipped: 3, total: 100 });
      return () => {};
    });
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    expect(await screen.findByText(/indexed 42\/100 · 3 skipped/)).toBeInTheDocument();
    const bar = screen.getByRole('progressbar');
    expect(bar).toHaveAttribute('max', '100'); // determinate
    expect(bar).toHaveAttribute('value', '45'); // done = min(42+3, 100)
  });

  it('shows an indeterminate bar when total is 0', async () => {
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onProgress?.({ imported: 0, skipped: 0, total: 0 });
      return () => {};
    });
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    const bar = await screen.findByRole('progressbar');
    expect(bar).not.toHaveAttribute('max'); // indeterminate
  });

  it('clears the progress bar after the scan completes', async () => {
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onProgress?.({ imported: 5, skipped: 0, total: 10 });
      h.onSummary({ imported: 10, marked_missing: 0, errors: [] });
      return () => {};
    });
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    expect(screen.queryByRole('progressbar')).not.toBeInTheDocument();
  });

  it('shows a "Preparing…" hint before the first progress tick (pre-count walk)', async () => {
    // No onProgress/onSummary yet — the daemon is still counting files.
    vi.mocked(api.scanStream).mockImplementation(() => () => {});
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    expect(await screen.findByText('Preparing…')).toBeInTheDocument();
    const bar = screen.getByRole('progressbar');
    expect(bar).not.toHaveAttribute('max'); // indeterminate until a total arrives
  });

  it('shows the scanned folder while preparing, before any progress tick', async () => {
    // Still in the pre-count walk: only the label/detail are set so far. The path
    // must already be on screen (and independent of the now-cleared path field).
    vi.mocked(api.scanStream).mockImplementation(() => () => {});
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    expect(await screen.findByText('Scanning /photos')).toBeInTheDocument();
  });

  it('keeps showing the scanned folder once progress ticks arrive', async () => {
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onProgress?.({ imported: 42, skipped: 3, total: 100 });
      return () => {};
    });
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), {
      target: { value: '/media/photos' },
    });
    await fireEvent.click(screen.getByText('Scan'));
    expect(await screen.findByText('Scanning /media/photos')).toBeInTheDocument();
    expect(screen.getByText(/indexed 42\/100 · 3 skipped/)).toBeInTheDocument();
  });

  it('a reopened panel shows the running scan\'s folder even though its path field is empty', async () => {
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onProgress?.({ imported: 5, skipped: 0, total: 10 });
      return () => {};
    });
    const first = render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    first.unmount();
    // Fresh mount (panel reopened): the path field is empty but the store still
    // carries the running scan, so the folder is visible from the label.
    render(FoldersSection, { onimported: () => {} });
    expect(await screen.findByText('Scanning /photos')).toBeInTheDocument();
    expect(screen.getByLabelText('folder path')).toHaveValue('');
  });

  it('re-attaches to a running scan when the panel is closed and reopened', async () => {
    vi.mocked(api.scanStream).mockImplementation((_folder, h) => {
      h.onProgress?.({ imported: 5, skipped: 0, total: 10 });
      return () => {};
    });
    const first = render(FoldersSection, { onimported: () => {} });
    await fireEvent.input(screen.getByLabelText('folder path'), { target: { value: '/photos' } });
    await fireEvent.click(screen.getByText('Scan'));
    expect(screen.getByRole('progressbar')).toBeInTheDocument();
    // Closing the settings panel unmounts the section; the scan keeps running in
    // the store. Reopening mounts a fresh section that must re-attach to it.
    first.unmount();
    render(FoldersSection, { onimported: () => {} });
    expect(await screen.findByRole('progressbar')).toBeInTheDocument();
    expect(screen.getByText(/indexed 5\/10 · 0 skipped/)).toBeInTheDocument();
  });

  it('disables Scan when the path is empty', () => {
    render(FoldersSection, { onimported: () => {} });
    expect(screen.getByText('Scan')).toBeDisabled();
  });
});

describe('FoldersSection under Tauri', () => {
  beforeEach(() => {
    vi.mocked(api.scanStream).mockReset();
    vi.mocked(api.listRoots).mockReset();
    vi.mocked(api.removeRoot).mockReset();
    vi.mocked(api.listRoots).mockResolvedValue([]);
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  });

  afterEach(() => {
    delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  });

  it('shows the native picker and fills the path from the dialog', async () => {
    const { open: openDialog } = await import('@tauri-apps/plugin-dialog');
    vi.mocked(openDialog).mockResolvedValue('/picked/folder');
    render(FoldersSection, { onimported: () => {} });
    const choose = screen.getByText('Choose folder');
    expect(choose).toBeInTheDocument();
    await fireEvent.click(choose);
    expect(await screen.findByDisplayValue('/picked/folder')).toBeInTheDocument();
  });
});

describe('FoldersSection watched roots', () => {
  beforeEach(() => {
    vi.mocked(api.scanStream).mockReset();
    vi.mocked(api.listRoots).mockReset();
    vi.mocked(api.removeRoot).mockReset();
  });

  it('lists watched roots on mount', async () => {
    vi.mocked(api.listRoots).mockResolvedValue(['/media/photos', '/media/art']);
    render(FoldersSection, { onimported: () => {} });
    expect(await screen.findByText('/media/photos')).toBeInTheDocument();
    expect(screen.getByText('/media/art')).toBeInTheDocument();
  });

  it('shows an empty state when nothing is watched', async () => {
    vi.mocked(api.listRoots).mockResolvedValue([]);
    render(FoldersSection, { onimported: () => {} });
    expect(await screen.findByText('No folders watched yet.')).toBeInTheDocument();
  });

  it('× opens a confirm modal that says files are not deleted', async () => {
    vi.mocked(api.listRoots).mockResolvedValue(['/media/photos']);
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.click(await screen.findByLabelText('stop watching /media/photos'));
    expect(screen.getByRole('dialog', { name: /stop watching/i })).toBeInTheDocument();
    expect(screen.getByText(/NOT deleted/i)).toBeInTheDocument();
    expect(api.removeRoot).not.toHaveBeenCalled();
  });

  it('Cancel closes the modal without removing', async () => {
    vi.mocked(api.listRoots).mockResolvedValue(['/media/photos']);
    render(FoldersSection, { onimported: () => {} });
    await fireEvent.click(await screen.findByLabelText('stop watching /media/photos'));
    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(api.removeRoot).not.toHaveBeenCalled();
  });

  it('Keep files unwatches without hiding and fires onsaved + onremoved', async () => {
    vi.mocked(api.listRoots)
      .mockResolvedValueOnce(['/media/photos', '/media/art'])
      .mockResolvedValueOnce(['/media/art']);
    vi.mocked(api.removeRoot).mockResolvedValue();
    const onsaved = vi.fn();
    const onremoved = vi.fn();
    render(FoldersSection, { onimported: () => {}, onsaved, onremoved });
    await fireEvent.click(await screen.findByLabelText('stop watching /media/photos'));
    await fireEvent.click(screen.getByRole('button', { name: 'Keep files' }));
    expect(api.removeRoot).toHaveBeenCalledWith('/media/photos', false);
    expect(await screen.findByText('/media/art')).toBeInTheDocument();
    expect(screen.queryByText('/media/photos')).not.toBeInTheDocument();
    expect(onsaved).toHaveBeenCalled();
    expect(onremoved).toHaveBeenCalled();
  });

  it('Hide files unwatches with hide=true and fires onremoved', async () => {
    vi.mocked(api.listRoots)
      .mockResolvedValueOnce(['/media/photos'])
      .mockResolvedValueOnce([]);
    vi.mocked(api.removeRoot).mockResolvedValue();
    const onremoved = vi.fn();
    render(FoldersSection, { onimported: () => {}, onremoved });
    await fireEvent.click(await screen.findByLabelText('stop watching /media/photos'));
    await fireEvent.click(screen.getByRole('button', { name: 'Hide files' }));
    expect(await screen.findByText('No folders watched yet.')).toBeInTheDocument();
    expect(api.removeRoot).toHaveBeenCalledWith('/media/photos', true);
    expect(onremoved).toHaveBeenCalled();
  });

  it('shows an inline error when removing a root fails, and does not fire onremoved', async () => {
    vi.mocked(api.listRoots).mockResolvedValue(['/media/photos']);
    vi.mocked(api.removeRoot).mockRejectedValue(new Error('not a watched root: /media/photos'));
    const onremoved = vi.fn();
    render(FoldersSection, { onimported: () => {}, onremoved });
    await fireEvent.click(await screen.findByLabelText('stop watching /media/photos'));
    await fireEvent.click(screen.getByRole('button', { name: 'Keep files' }));
    expect(await screen.findByText('not a watched root: /media/photos')).toBeInTheDocument();
    expect(onremoved).not.toHaveBeenCalled();
  });

  it('says why a second removal is refused while one is in flight', async () => {
    vi.mocked(api.listRoots).mockResolvedValue(['/a', '/b']);
    vi.mocked(api.removeRoot).mockImplementationOnce(() => new Promise<void>(() => {}));

    render(FoldersSection, { onimported: () => {} });
    await vi.waitFor(() => screen.getByLabelText('stop watching /a'));

    await fireEvent.click(screen.getByLabelText('stop watching /a'));
    await fireEvent.click(screen.getByRole('button', { name: 'Hide files' }));

    // The remaining buttons are only aria-disabled, so the click still lands.
    // Silently doing nothing would look like a broken button.
    await fireEvent.click(screen.getByLabelText('stop watching /b'));
    expect(await screen.findByRole('alert')).toHaveTextContent(/still being removed/i);
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('marks the removed row busy across removeRoot and refreshRoots', async () => {
    vi.mocked(api.listRoots).mockResolvedValue(['/a', '/b']);
    let resolveRemove!: () => void;
    vi.mocked(api.removeRoot).mockImplementationOnce(
      () => new Promise<void>((r) => (resolveRemove = r)),
    );

    const { container } = render(FoldersSection, { onimported: () => {} });
    await vi.waitFor(() => screen.getByLabelText('stop watching /a'));

    await fireEvent.click(screen.getByLabelText('stop watching /a'));
    await fireEvent.click(screen.getByRole('button', { name: 'Hide files' }));
    await tick();

    expect(screen.queryByLabelText('stop watching /a')).toBeNull();
    expect(screen.getByLabelText('stop watching /b')).toBeTruthy();
    expect(container.querySelectorAll('.spinner')).toHaveLength(1);

    resolveRemove();
    await vi.waitFor(() => expect(screen.getByLabelText('stop watching /a')).toBeTruthy());
  });
});
