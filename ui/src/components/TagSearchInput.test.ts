import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import TagSearchInput from './TagSearchInput.svelte';
import * as api from '../lib/api';
import { view } from '../lib/settings.svelte';

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return { ...actual, completeTags: vi.fn() };
});

const empty = { namespaces: [], tags: [] };

beforeEach(() => {
  vi.mocked(api.completeTags).mockReset();
  vi.mocked(api.completeTags).mockResolvedValue(empty);
});

async function type(value: string): Promise<HTMLInputElement> {
  const input = screen.getByLabelText('search') as HTMLInputElement;
  await fireEvent.input(input, { target: { value } });
  return input;
}

describe('TagSearchInput', () => {
  it('shows tag suggestions for a typed token', async () => {
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus_aran', count: 3 }],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    await type('character:sam');
    expect(await screen.findByText('character:samus_aran')).toBeInTheDocument();
  });

  it('ArrowDown + Enter completes the highlighted tag without submitting', async () => {
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus_aran', count: 3 }],
    });
    const onsearch = vi.fn();
    render(TagSearchInput, { onsearch, onsearchtag: () => {} });
    const input = await type('character:sam');
    await screen.findByText('character:samus_aran');
    await fireEvent.keyDown(input, { key: 'ArrowDown' });
    await fireEvent.keyDown(input, { key: 'Enter' });
    expect(input.value).toBe('character:samus_aran ');
    expect(onsearch).not.toHaveBeenCalled();
  });

  it('Tab completes the highlighted namespace', async () => {
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [{ namespace: 'artist', tag_count: 5 }],
      tags: [],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = await type('art');
    await screen.findByText('artist:');
    await fireEvent.keyDown(input, { key: 'ArrowDown' });
    await fireEvent.keyDown(input, { key: 'Tab' });
    expect(input.value).toBe('artist:');
    expect(api.completeTags).toHaveBeenCalledWith(
      'artist:',
      20,
      expect.any(AbortSignal),
      'prefix',
    );
  });

  it('Escape closes the dropdown', async () => {
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus_aran', count: 3 }],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = await type('character:sam');
    await screen.findByText('character:samus_aran');
    await fireEvent.keyDown(input, { key: 'Escape' });
    await waitFor(() =>
      expect(screen.queryByText('character:samus_aran')).not.toBeInTheDocument(),
    );
  });

  it('clearing the field searches the empty query', async () => {
    const onsearch = vi.fn();
    render(TagSearchInput, { onsearch, onsearchtag: () => {} });
    await type('cat');
    onsearch.mockClear();
    await type('');
    expect(onsearch).toHaveBeenCalledWith('');
  });

  it('Enter with no highlight submits the query', async () => {
    const onsearch = vi.fn();
    render(TagSearchInput, { onsearch, onsearchtag: () => {} });
    const input = await type('character:samus_aran');
    await fireEvent.submit(input.closest('form')!);
    expect(onsearch).toHaveBeenCalledWith('character:samus_aran');
  });

  it('blurring the input (Tab away) closes the dropdown immediately', async () => {
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus_aran', count: 3 }],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = await type('character:sam');
    await screen.findByText('character:samus_aran');
    // Simulate Tab-away (focus leaves the input)
    await fireEvent.blur(input);
    await waitFor(() =>
      expect(screen.queryByText('character:samus_aran')).not.toBeInTheDocument(),
    );
  });

  it('clicking a suggestion completes it', async () => {
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus_aran', count: 3 }],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = await type('character:sam');
    const opt = await screen.findByText('character:samus_aran');
    await fireEvent.mouseDown(opt);
    expect(input.value).toBe('character:samus_aran ');
  });

  it('renders the server-provided canonical row with its merged count (no alias row)', async () => {
    // Server already merged: the alias `samus_aran` is gone; only the canonical
    // `character:samus` remains, carrying the merged count.
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus', count: 42 }],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    await type('character:sam');
    expect(await screen.findByText('character:samus')).toBeInTheDocument();
    expect(screen.queryByText('character:samus_aran')).toBeNull();
  });
});

describe('TagSearchInput alias-source display', () => {
  afterEach(() => {
    view.showAliasSource = false;
  });

  it('pref ON: renders alias_source and arrow before canonical', async () => {
    view.showAliasSource = true;
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus', count: 3, alias_source: 'samus_aran' }],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    await type('character:sam');
    const item = await screen.findByRole('option');
    expect(item.textContent).toContain('alias:');
    expect(item.textContent).toContain('samus_aran');
    expect(item.textContent).toContain('→');
    expect(item.textContent).toContain('character:samus');
  });

  it('pref OFF: hides alias_source and arrow even when field is present', async () => {
    view.showAliasSource = false;
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus', count: 3, alias_source: 'samus_aran' }],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    await type('character:sam');
    const item = await screen.findByRole('option');
    expect(item.textContent).toContain('character:samus');
    expect(item.textContent).not.toContain('alias:');
    expect(item.textContent).not.toContain('samus_aran');
    expect(item.textContent).not.toContain('→');
  });

  it('pref ON: applying the row inserts canonical, not alias', async () => {
    view.showAliasSource = true;
    vi.mocked(api.completeTags).mockResolvedValue({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus', count: 3, alias_source: 'samus_aran' }],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = await type('character:sam');
    await screen.findByRole('option');
    await fireEvent.keyDown(input, { key: 'ArrowDown' });
    await fireEvent.keyDown(input, { key: 'Enter' });
    expect(input.value).toBe('character:samus ');
    expect(input.value).not.toContain('samus_aran');
  });
});

describe('TagSearchInput spinner', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('a fast response (< 150ms) never shows the spinner', async () => {
    let resolve!: (v: typeof empty) => void;
    vi.mocked(api.completeTags).mockReturnValue(new Promise((r) => { resolve = r; }));

    vi.useFakeTimers();
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = screen.getByLabelText('search') as HTMLInputElement;
    fireEvent.input(input, { target: { value: 'cat' } });

    await vi.advanceTimersByTimeAsync(120); // debounce fires → completeTags called
    resolve(empty);
    await vi.advanceTimersByTimeAsync(0); // flush microtasks — resolve path must clear the grace timer

    expect(screen.getByRole('status')).toHaveTextContent('');

    // Advance past the 150ms grace: a leaked grace timer would paint the
    // spinner late, so this asserts the resolve path actually cleared it.
    await vi.advanceTimersByTimeAsync(300);
    expect(screen.getByRole('status')).toHaveTextContent('');
  });

  it('a slow response (> 150ms) paints the spinner then clears it on resolve', async () => {
    let resolve!: (v: typeof empty) => void;
    vi.mocked(api.completeTags).mockReturnValue(new Promise((r) => { resolve = r; }));

    vi.useFakeTimers();
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = screen.getByLabelText('search') as HTMLInputElement;
    fireEvent.input(input, { target: { value: 'cat' } });

    await vi.advanceTimersByTimeAsync(120); // debounce fires → completeTags called
    await vi.advanceTimersByTimeAsync(150); // grace fires → busy = true

    expect(screen.getByRole('status')).toHaveTextContent('loading suggestions');

    resolve(empty);
    await vi.runAllTimersAsync(); // flush promise microtasks → busy = false

    expect(screen.getByRole('status')).toHaveTextContent('');
  });

  it('a superseded request resolving late does not clear the newer spinner', async () => {
    let resolve1!: (v: typeof empty) => void;
    let resolve2!: (v: typeof empty) => void;
    vi.mocked(api.completeTags)
      .mockReturnValueOnce(new Promise((r) => { resolve1 = r; }))
      .mockReturnValueOnce(new Promise((r) => { resolve2 = r; }));

    vi.useFakeTimers();
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = screen.getByLabelText('search') as HTMLInputElement;

    // First keystroke: debounce fires, first request in flight.
    fireEvent.input(input, { target: { value: 'ca' } });
    await vi.advanceTimersByTimeAsync(120);

    // Second keystroke: supersedes first (aborts ac1, starts ac2).
    fireEvent.input(input, { target: { value: 'cat' } });
    await vi.advanceTimersByTimeAsync(120); // second debounce fires
    await vi.advanceTimersByTimeAsync(150); // second grace fires → busy = true

    expect(screen.getByRole('status')).toHaveTextContent('loading suggestions');

    // Stale request resolves — identity guard must keep spinner alive.
    resolve1(empty);
    await vi.runAllTimersAsync();
    expect(screen.getByRole('status')).toHaveTextContent('loading suggestions');

    // Current request resolves — spinner clears.
    resolve2(empty);
    await vi.runAllTimersAsync();
    expect(screen.getByRole('status')).toHaveTextContent('');
  });

  it('Escape (closeDropdown) clears the spinner and grace timer', async () => {
    vi.mocked(api.completeTags).mockReturnValue(new Promise(() => {})); // never resolves

    vi.useFakeTimers();
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = screen.getByLabelText('search') as HTMLInputElement;

    fireEvent.input(input, { target: { value: 'cat' } });
    await vi.advanceTimersByTimeAsync(120); // debounce
    await vi.advanceTimersByTimeAsync(150); // grace → busy = true

    expect(screen.getByRole('status')).toHaveTextContent('loading suggestions');

    fireEvent.keyDown(input, { key: 'Escape' });
    await vi.runAllTimersAsync();

    expect(screen.getByRole('status')).toHaveTextContent('');
  });

  it('Escape during a slow request aborts it so the late response cannot reopen', async () => {
    let resolve!: (v: { namespaces: never[]; tags: { namespace: string; subtag: string; count: number }[] }) => void;
    vi.mocked(api.completeTags).mockReturnValue(new Promise((r) => { resolve = r; }));

    vi.useFakeTimers();
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = screen.getByLabelText('search') as HTMLInputElement;

    fireEvent.input(input, { target: { value: 'character:sam' } });
    await vi.advanceTimersByTimeAsync(120); // debounce → request in flight
    await vi.advanceTimersByTimeAsync(150); // grace → busy = true

    fireEvent.keyDown(input, { key: 'Escape' });
    await vi.runAllTimersAsync();
    expect(screen.getByRole('status')).toHaveTextContent('');

    // Late response for the aborted request must not repopulate or reopen.
    resolve({
      namespaces: [],
      tags: [{ namespace: 'character', subtag: 'samus_aran', count: 3 }],
    });
    await vi.runAllTimersAsync();

    expect(screen.queryByText('character:samus_aran')).not.toBeInTheDocument();
    expect(screen.getByRole('status')).toHaveTextContent('');
  });

  it('stale rows remain clickable while the spinner is active', async () => {
    vi.mocked(api.completeTags)
      .mockResolvedValueOnce({
        namespaces: [],
        tags: [{ namespace: 'character', subtag: 'samus_aran', count: 3 }],
      })
      .mockReturnValue(new Promise(() => {})); // second request never resolves

    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = screen.getByLabelText('search') as HTMLInputElement;

    // First request: populate suggestions.
    await fireEvent.input(input, { target: { value: 'character:sam' } });
    await screen.findByText('character:samus_aran');

    // Second request: pending — stale rows remain visible.
    await fireEvent.input(input, { target: { value: 'character:sama' } });
    await waitFor(() => expect(api.completeTags).toHaveBeenCalledTimes(2));

    // Click the stale row — it must still complete.
    await fireEvent.mouseDown(screen.getByText('character:samus_aran'));
    expect(input.value).toBe('character:samus_aran ');
  });
});
