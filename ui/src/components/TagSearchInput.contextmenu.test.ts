import { describe, expect, it, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import TagSearchInput from './TagSearchInput.svelte';
import { contextMenu } from '../lib/context-menu.svelte';
import * as api from '../lib/api';
import type { MenuItem } from '../lib/menu-items';

afterEach(() => {
  contextMenu.close();
  vi.restoreAllMocks();
});

beforeEach(() => {
  vi.spyOn(api, 'completeTags').mockResolvedValue({
    namespaces: [],
    tags: [{ namespace: 'series', subtag: 'metroid', count: 3 }],
  });
});

/** Render a TagSearchInput, type a query that yields the mock 'series:metroid'
 *  suggestion, and wait for it to appear in the DOM. */
async function setup() {
  const onsearch = vi.fn();
  const onsearchtag = vi.fn();
  render(TagSearchInput, { onsearch, onsearchtag });
  const input = screen.getByLabelText('search') as HTMLInputElement;
  await fireEvent.input(input, { target: { value: 'series:me' } });
  await screen.findByText('series:metroid');
  const option = screen.getAllByRole('option')[0];
  return { input, option, onsearch, onsearchtag };
}

describe('TagSearchInput catalog menu', () => {
  it('right-click on a tag row opens a search+copy catalog menu', async () => {
    const { option } = await setup();
    await fireEvent.contextMenu(option);
    expect(contextMenu.open).toBe(true);
    const ids = contextMenu.items
      .filter((e): e is MenuItem => e !== 'separator')
      .map((e) => e.id);
    expect(ids).toEqual(['tag-search', 'tag-copy']);
    // Invoker must be the search input so focus is restored there on menu close.
    const input = screen.getByLabelText('search');
    expect(contextMenu.invoker).toBe(input);
  });

  it('tag-search calls onsearchtag with the full tag', async () => {
    const { option, onsearchtag } = await setup();
    await fireEvent.contextMenu(option);
    const searchItem = contextMenu.items.find(
      (e): e is MenuItem => e !== 'separator' && e.id === 'tag-search',
    )!;
    searchItem.onselect();
    expect(onsearchtag).toHaveBeenCalledWith('series:metroid');
  });

  it('right-click does not complete the suggestion', async () => {
    const { input, option, onsearch } = await setup();
    await fireEvent.contextMenu(option);
    expect(input.value).toBe('series:me');
    expect(onsearch).not.toHaveBeenCalled();
  });

  it('namespace rows get no context menu', async () => {
    vi.mocked(api.completeTags).mockResolvedValueOnce({
      namespaces: [{ namespace: 'series', tag_count: 5 }],
      tags: [],
    });
    render(TagSearchInput, { onsearch: () => {}, onsearchtag: () => {} });
    const input = screen.getByLabelText('search') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'ser' } });
    await screen.findByText('series:');
    const option = screen.getAllByRole('option')[0];
    const prevented = await fireEvent.contextMenu(option);
    expect(prevented).toBe(false); // e.preventDefault() was called
    expect(contextMenu.open).toBe(false);
  });

  it('left mousedown completes the suggestion', async () => {
    const { input, option } = await setup();
    await fireEvent.mouseDown(option, { button: 0 });
    expect(input.value).toBe('series:metroid ');
  });

  it('non-left mousedown does not complete the suggestion', async () => {
    const { input, option } = await setup();
    await fireEvent.mouseDown(option, { button: 2 });
    expect(input.value).toBe('series:me');
  });
});
