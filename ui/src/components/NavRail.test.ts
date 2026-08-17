import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import NavRail from './NavRail.svelte';
import * as api from '../lib/api';
import { loadSaved } from '../lib/rail-saved';

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return {
    ...actual,
    listNamespaces: vi.fn(),
    getAppVersion: vi.fn(),
  };
});

describe('NavRail', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.clearAllMocks();
    vi.mocked(api.listNamespaces).mockResolvedValue([{ namespace: 'artist', tag_count: 3 }]);
    vi.mocked(api.getAppVersion).mockResolvedValue(null);
  });

  it('runs all-media and namespace queries', async () => {
    const onrun = vi.fn();
    render(NavRail, { activeQuery: '', onrun, onerror: () => {} });
    await fireEvent.click(screen.getByText('all media'));
    expect(onrun).toHaveBeenCalledWith('');

    await screen.findByText('artist');
    await fireEvent.click(screen.getByText('artist'));
    expect(onrun).toHaveBeenCalledWith('artist:*');
  });

  it('pins and unpins the active query', async () => {
    render(NavRail, { activeQuery: 'creator:mika', onrun: () => {}, onerror: () => {} });
    await fireEvent.click(screen.getByLabelText('pin current search'));
    expect(loadSaved()).toEqual([{ name: 'creator:mika', query: 'creator:mika' }]);
    expect(screen.getByText('creator:mika')).toBeInTheDocument();

    await fireEvent.click(screen.getByLabelText('unpin creator:mika'));
    expect(loadSaved()).toEqual([]);
  });

  it('marks the matching row active', async () => {
    const { container } = render(NavRail, {
      activeQuery: 'artist:*',
      onrun: () => {},
      onerror: () => {},
    });
    await screen.findByText('artist');
    expect(container.querySelector('.row.active')?.textContent).toContain('artist');
  });

  it('displays the version stamp after getAppVersion resolves', async () => {
    vi.mocked(api.getAppVersion).mockResolvedValue('0.2.18');
    render(NavRail, { activeQuery: '', onrun: () => {}, onerror: () => {} });
    expect(await screen.findByLabelText('app version')).toHaveTextContent('0.2.18');
  });

  it('renders nothing for the version stamp when getAppVersion returns null', async () => {
    vi.mocked(api.getAppVersion).mockResolvedValue(null);
    render(NavRail, { activeQuery: '', onrun: () => {}, onerror: () => {} });
    // Give any pending microtasks a chance to flush before asserting absence.
    await screen.findByText('all media');
    expect(screen.queryByLabelText('app version')).toBeNull();
  });
});
