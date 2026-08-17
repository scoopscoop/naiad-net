import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import * as api from '../lib/api';
import RejectedSection from './RejectedSection.svelte';

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return {
    ...actual,
    listRejections: vi.fn(),
  };
});

const mockRejection = {
  hash: 'abc',
  service: 'repo',
  tag: 'series:metroid',
  note: null,
  created_at: '2026-07-01T00:00:00Z',
};

describe('RejectedSection', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(api.listRejections).mockResolvedValue([]);
  });

  it('collapsed by default, lists rejections with restore only (no status badges)', async () => {
    vi.mocked(api.listRejections).mockResolvedValue([mockRejection]);
    const onrestore = vi.fn();
    const { container } = render(RejectedSection, { hash: 'abc', onrestore });

    // Wait for data to load and header to appear
    const header = await screen.findByRole('button', { name: 'Rejected tags' });
    expect(header).toBeInTheDocument();

    // Collapsed by default — rows not visible
    expect(screen.queryByText('series:metroid')).not.toBeInTheDocument();

    // Expand
    await fireEvent.click(header);

    // Row is visible
    expect(screen.getByText('series:metroid')).toBeInTheDocument();
    expect(screen.getByText('repo')).toBeInTheDocument();

    // No status badge present
    expect(container.querySelector('.badge')).not.toBeInTheDocument();

    // Restore button fires onrestore with (tag, service)
    await fireEvent.click(screen.getByRole('button', { name: 'Restore series:metroid from repo' }));
    expect(onrestore).toHaveBeenCalledWith('series:metroid', 'repo');
  });

  it('renders nothing when there are no rejections', async () => {
    vi.mocked(api.listRejections).mockResolvedValue([]);
    render(RejectedSection, { hash: 'abc', onrestore: vi.fn() });
    // Wait a tick for effects to run
    await new Promise((r) => setTimeout(r, 0));
    expect(screen.queryByRole('button', { name: 'Rejected tags' })).not.toBeInTheDocument();
  });

  it('reloads when refreshTick changes', async () => {
    vi.mocked(api.listRejections).mockResolvedValue([mockRejection]);
    const { rerender } = render(RejectedSection, { hash: 'abc', refreshTick: 0, onrestore: vi.fn() });

    await screen.findByRole('button', { name: 'Rejected tags' });
    expect(api.listRejections).toHaveBeenCalledTimes(1);

    await rerender({ hash: 'abc', refreshTick: 1, onrestore: vi.fn() });
    await vi.waitFor(() => expect(api.listRejections).toHaveBeenCalledTimes(2));
  });
});
