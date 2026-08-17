import { describe, expect, it, vi, afterEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import TagGroupList from './TagGroupList.svelte';
import { contextMenu } from '../lib/context-menu.svelte';
import type { TagDetail } from '../lib/types';
import type { TagGroup } from '../lib/namespace';
import type { MenuItem, MenuList } from '../lib/menu-items';

const localTag: TagDetail = { tag: 'character:samus', presence: 'local', services: [], relations: false };
const pulledTag: TagDetail = { tag: 'series:metroid', presence: 'pulled', services: ['repo'], relations: false };
const groups: TagGroup<TagDetail>[] = [
  { id: 'character', name: 'Character', color: '#5f9e6b', tags: [localTag] },
  { id: 'series', name: 'Series', color: '#9a6fb0', tags: [pulledTag] },
];
const ids = (m: MenuList) => m.map((e) => (e === 'separator' ? 'separator' : e.id));
const item = (m: MenuList, id: string) => m.find((e): e is MenuItem => e !== 'separator' && e.id === id);
const chip = (text: string) => screen.getByText(text, { selector: 'span.label' }).closest('button')!;

afterEach(() => contextMenu.close());

describe('TagGroupList', () => {
  it('renders grouped tags with no inline reject/remove buttons', () => {
    render(TagGroupList, { groups, onremove: () => {}, onreject: () => {}, onsearchtag: () => {} });
    expect(screen.getByText('character:samus')).toBeInTheDocument();
    expect(screen.queryByLabelText('remove character:samus')).toBeNull();
    expect(screen.queryByLabelText('reject series:metroid')).toBeNull();
  });

  it('pulled chip menu has hide + remove; local chip menu omits hide', async () => {
    render(TagGroupList, { groups, onremove: () => {}, onreject: () => {}, onsearchtag: () => {} });

    await fireEvent.contextMenu(chip('series:metroid'));
    expect(ids(contextMenu.items)).toEqual(['tag-search', 'tag-copy', 'separator', 'tag-hide', 'tag-remove']);
    contextMenu.close();

    await fireEvent.contextMenu(chip('character:samus'));
    expect(ids(contextMenu.items)).toEqual(['tag-search', 'tag-copy', 'separator', 'tag-remove']);
  });

  it('menu hide/remove fire onreject/onremove; search fires onsearchtag', async () => {
    const onremove = vi.fn();
    const onreject = vi.fn();
    const onsearchtag = vi.fn();
    render(TagGroupList, { groups, onremove, onreject, onsearchtag });

    await fireEvent.contextMenu(chip('series:metroid'));
    item(contextMenu.items, 'tag-hide')!.onselect();
    item(contextMenu.items, 'tag-remove')!.onselect();
    item(contextMenu.items, 'tag-search')!.onselect();
    expect(onreject).toHaveBeenCalledWith('series:metroid', ['repo']);
    expect(onremove).toHaveBeenCalledWith('series:metroid');
    expect(onsearchtag).toHaveBeenCalledWith('series:metroid');
  });

  it('while mutating, hide and remove menu items are disabled', async () => {
    render(TagGroupList, { groups, mutating: true, onremove: () => {}, onreject: () => {}, onsearchtag: () => {} });
    await fireEvent.contextMenu(chip('series:metroid'));
    expect(item(contextMenu.items, 'tag-hide')!.disabled).toBe(true);
    expect(item(contextMenu.items, 'tag-remove')!.disabled).toBe(true);
  });

  it('Shift+F10 opens the menu on the focused chip', async () => {
    render(TagGroupList, { groups, onremove: () => {}, onreject: () => {}, onsearchtag: () => {} });
    await fireEvent.keyDown(chip('series:metroid'), { key: 'F10', shiftKey: true });
    expect(contextMenu.open).toBe(true);
  });

  it('accepts busyTag without rendering inline busy UI', () => {
    render(TagGroupList, { groups, busyTag: 'series:metroid', onremove: () => {}, onreject: () => {}, onsearchtag: () => {} });
    expect(screen.getByText('series:metroid')).toBeInTheDocument();
  });

  it('renders the ⇆ glyph only on related chips and opens the popover on click', async () => {
    const related: TagDetail = { tag: 'character:samus', presence: 'local', services: [], relations: true };
    const plain: TagDetail = { tag: 'meta:solo', presence: 'local', services: [], relations: false };
    const grp: TagGroup<TagDetail>[] = [
      { id: 'character', name: 'Character', color: '#5f9e6b', tags: [related] },
      { id: 'meta', name: 'Meta', color: '#888', tags: [plain] },
    ];
    render(TagGroupList, { groups: grp, onremove: () => {}, onreject: () => {}, onsearchtag: () => {} });
    const glyphs = screen.getAllByRole('button', { name: /relations/i });
    expect(glyphs).toHaveLength(1);
    await fireEvent.click(glyphs[0]);
    const { tagRelationsPopover } = await import('../lib/tag-relations.svelte');
    expect(tagRelationsPopover.open).toBe(true);
    expect(tagRelationsPopover.tag).toBe('character:samus');
    tagRelationsPopover.close();

    // Keyboard activation: Enter on the glyph also opens the popover.
    await fireEvent.keyDown(glyphs[0], { key: 'Enter' });
    expect(tagRelationsPopover.open).toBe(true);
    tagRelationsPopover.close();
  });

  it('renders an origin chip when tag.origin is set', () => {
    const tagWithOrigin: TagDetail = { tag: 'character:samus', presence: 'local', services: [], relations: false, origin: 'wd14-tagger' };
    const grp: TagGroup<TagDetail>[] = [
      { id: 'character', name: 'Character', color: '#5f9e6b', tags: [tagWithOrigin] },
    ];
    render(TagGroupList, { groups: grp, onremove: () => {}, onreject: () => {}, onsearchtag: () => {} });
    const chip = screen.getByTestId('origin-chip');
    expect(chip).toBeInTheDocument();
    expect(chip).toHaveTextContent('wd14-tagger');
    expect(chip.tagName.toLowerCase()).toBe('span');
    expect(chip.getAttribute('role')).toBeNull();
    expect(chip.getAttribute('tabindex')).toBeNull();
  });

  it('does not render an origin chip when tag.origin is absent', () => {
    render(TagGroupList, { groups, onremove: () => {}, onreject: () => {}, onsearchtag: () => {} });
    expect(screen.queryByTestId('origin-chip')).toBeNull();
  });

  it('r hotkey fires onreject for pulled tags, not with a modifier or while mutating', async () => {
    const onreject = vi.fn();
    const { rerender } = render(TagGroupList, { groups, onremove: () => {}, onreject, onsearchtag: () => {} });
    await fireEvent.keyDown(chip('series:metroid'), { key: 'r' });
    expect(onreject).toHaveBeenCalledWith('series:metroid', ['repo']);

    await fireEvent.keyDown(chip('series:metroid'), { key: 'r', ctrlKey: true });
    expect(onreject).toHaveBeenCalledTimes(1);

    await fireEvent.keyDown(chip('character:samus'), { key: 'r' }); // not pulled
    expect(onreject).toHaveBeenCalledTimes(1);

    await rerender({ groups, mutating: true, onremove: () => {}, onreject, onsearchtag: () => {} });
    await fireEvent.keyDown(chip('series:metroid'), { key: 'r' });
    expect(onreject).toHaveBeenCalledTimes(1);
  });
});
