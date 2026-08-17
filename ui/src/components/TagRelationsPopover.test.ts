import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import TagRelationsPopover from './TagRelationsPopover.svelte';
import { tagRelationsPopover } from '../lib/tag-relations.svelte';
import * as api from '../lib/api';

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return { ...actual, fetchTagRelations: vi.fn() };
});

afterEach(() => tagRelationsPopover.close());

const rel = {
  canonical: 'character:samus',
  count: 51,
  via_alias: true,
  aliases: { items: [{ tag: 'samus_aran', count: 7 }], total: 3 },
  parents: { items: [{ tag: 'series:metroid', count: 40 }], total: 1 },
  children: { items: [], total: 0 },
};

describe('TagRelationsPopover', () => {
  it('renders non-empty sections, the via-alias note, a "… N more" ghost row, and the header count', async () => {
    vi.mocked(api.fetchTagRelations).mockResolvedValue(rel);
    render(TagRelationsPopover, { onsearchtag: () => {} });
    tagRelationsPopover.openAt({ x: 0, y: 0 }, 'character:samus', 'abc', null);
    expect(await screen.findByText('samus_aran')).toBeInTheDocument();
    expect(screen.getByText('series:metroid')).toBeInTheDocument();
    expect(screen.getByText(/via an alias/i)).toBeInTheDocument();
    // aliases total 3, shown 1 -> "… 2 more".
    expect(screen.getByText(/…\s*2 more/)).toBeInTheDocument();
    // Implied by is empty -> heading absent.
    expect(screen.queryByText(/Implied by/i)).toBeNull();
    // Canonical count (51) is rendered in the "Shown as" header row.
    expect(screen.getByText('51')).toBeInTheDocument();
  });

  it('shows the section total in the header and hides a 0 alias count', async () => {
    const relZeroAlias = {
      canonical: 'character:samus',
      count: 51,
      via_alias: false,
      // Two alternate spellings, neither used as a raw mapping (count 0).
      aliases: {
        items: [
          { tag: 'samus_aran', count: 0 },
          { tag: 'samus', count: 0 },
        ],
        total: 2,
      },
      parents: { items: [{ tag: 'series:metroid', count: 40 }], total: 1 },
      children: { items: [], total: 0 },
    };
    vi.mocked(api.fetchTagRelations).mockResolvedValue(relZeroAlias);
    render(TagRelationsPopover, { onsearchtag: () => {} });
    tagRelationsPopover.openAt({ x: 0, y: 0 }, 'character:samus', 'abc', null);

    // Both alias rows render as spellings...
    const aliasRow = await screen.findByRole('menuitem', { name: /samus_aran/ });
    expect(aliasRow).toBeInTheDocument();
    // ...but with no visible count (a bare "0" would read as "zero files").
    expect(aliasRow.textContent).not.toMatch(/0/);
    // The Aliases header carries the total so you can tell how many exist.
    const header = screen.getByText('Aliases').closest('h5');
    expect(header?.textContent).toMatch(/2/);
    // Parent rows keep their (non-zero) merged count.
    expect(screen.getByText('40')).toBeInTheDocument();
  });

  it('row click fires onsearchtag and closes', async () => {
    vi.mocked(api.fetchTagRelations).mockResolvedValue(rel);
    const onsearchtag = vi.fn();
    render(TagRelationsPopover, { onsearchtag });
    tagRelationsPopover.openAt({ x: 0, y: 0 }, 'character:samus', 'abc', null);
    const row = await screen.findByRole('menuitem', { name: /samus_aran/ });
    await fireEvent.click(row);
    expect(onsearchtag).toHaveBeenCalledWith('samus_aran');
    expect(tagRelationsPopover.open).toBe(false);
  });

  it('Escape closes the popover when data loaded', async () => {
    vi.mocked(api.fetchTagRelations).mockResolvedValue(rel);
    render(TagRelationsPopover, { onsearchtag: () => {} });
    tagRelationsPopover.openAt({ x: 0, y: 0 }, 'character:samus', 'abc', null);
    await screen.findByText('samus_aran');
    await fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(tagRelationsPopover.open).toBe(false));
  });

  it('Escape closes the popover even when fetch failed (dialog not rendered)', async () => {
    vi.mocked(api.fetchTagRelations).mockRejectedValue(new Error('network error'));
    render(TagRelationsPopover, { onsearchtag: () => {} });
    tagRelationsPopover.openAt({ x: 0, y: 0 }, 'character:samus', 'abc', null);
    // Wait for the rejected promise to settle (data stays null, dialog not rendered).
    await waitFor(() => expect(api.fetchTagRelations).toHaveBeenCalled());
    // Escape via window keydown must still close.
    await fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(tagRelationsPopover.open).toBe(false));
  });

  it('click-away closes the popover even when fetch failed (el never rendered)', async () => {
    vi.mocked(api.fetchTagRelations).mockRejectedValue(new Error('network error'));
    render(TagRelationsPopover, { onsearchtag: () => {} });
    tagRelationsPopover.openAt({ x: 0, y: 0 }, 'character:samus', 'abc', null);
    // Wait for the rejected promise to settle (data stays null, dialog not rendered, el is undefined).
    await waitFor(() => expect(api.fetchTagRelations).toHaveBeenCalled());
    // Click-away on document.body must still close despite el being undefined.
    await fireEvent.pointerDown(document.body);
    await waitFor(() => expect(tagRelationsPopover.open).toBe(false));
  });
});
