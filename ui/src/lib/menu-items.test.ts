import { describe, expect, it, vi } from 'vitest';
import {
  buildTileMenu,
  buildTagMenu,
  buildBackgroundMenu,
  type MenuItem,
  type MenuList,
} from './menu-items';
import type { FileDto } from './types';

const file = (h: string): FileDto => ({
  hash: h, name: `${h}.png`, size: 1, path: `C:/img/${h}.png`,
  imported_at: 0, created_at: null, modified_at: null, mime: null,
});
const ids = (m: MenuList) => m.map((e) => (e === 'separator' ? 'separator' : e.id));
const item = (m: MenuList, id: string) => m.find((e): e is MenuItem => e !== 'separator' && e.id === id)!;

describe('buildTileMenu', () => {
  const actions = {
    onOpen: vi.fn(), onQuickLook: vi.fn(), onPullTags: vi.fn(),
    onCopyHashes: vi.fn(), onCopyPaths: vi.fn(),
  };

  it('singular labels for one target, no reject item', () => {
    const m = buildTileMenu([file('a')], actions);
    expect(ids(m)).toEqual(['open', 'quick-look', 'pull-tags', 'separator', 'copy-hash', 'copy-path']);
    expect(item(m, 'pull-tags').label).toBe('Pull tags');
    expect(item(m, 'copy-hash').label).toBe('Copy hash');
    expect(item(m, 'copy-path').label).toBe('Copy path');
    expect(m.some((e) => e !== 'separator' && e.id.includes('reject'))).toBe(false);
  });

  it('plural count labels for many targets', () => {
    const m = buildTileMenu([file('a'), file('b'), file('c')], actions);
    expect(item(m, 'pull-tags').label).toBe('Pull tags — 3 files');
    expect(item(m, 'copy-hash').label).toBe('Copy 3 hashes');
    expect(item(m, 'copy-path').label).toBe('Copy 3 paths');
  });

  it('wires onselect to the actions', () => {
    const m = buildTileMenu([file('a')], actions);
    item(m, 'open').onselect();
    item(m, 'copy-path').onselect();
    expect(actions.onOpen).toHaveBeenCalledOnce();
    expect(actions.onCopyPaths).toHaveBeenCalledOnce();
  });
});

describe('buildTagMenu file-scoped', () => {
  const actions = { onSearch: vi.fn(), onCopy: vi.fn(), onHide: vi.fn(), onRemove: vi.fn() };

  it('pulled presence: search, copy, separator, hide, remove(danger,last)', () => {
    const m = buildTagMenu('series:x', 'pulled', 'file', false, actions);
    expect(ids(m)).toEqual(['tag-search', 'tag-copy', 'separator', 'tag-hide', 'tag-remove']);
    expect(item(m, 'tag-remove').danger).toBe(true);
    expect(item(m, 'tag-hide').disabled).toBeFalsy();
    expect(item(m, 'tag-remove').disabled).toBeFalsy();
  });

  it('non-pulled presence omits hide', () => {
    const m = buildTagMenu('character:x', 'local', 'file', false, actions);
    expect(ids(m)).toEqual(['tag-search', 'tag-copy', 'separator', 'tag-remove']);
    expect(m.some((e) => e !== 'separator' && e.id === 'tag-hide')).toBe(false);
  });

  it("presence 'both' omits hide (hide is pulled-only per spec §6.2)", () => {
    const actions = { onSearch: vi.fn(), onCopy: vi.fn(), onHide: vi.fn(), onRemove: vi.fn() };
    const m = buildTagMenu('series:x', 'both', 'file', false, actions);
    expect(ids(m)).toEqual(['tag-search', 'tag-copy', 'separator', 'tag-remove']);
  });

  it('while mutating, hide and remove are disabled but search/copy are not', () => {
    const m = buildTagMenu('series:x', 'pulled', 'file', true, actions);
    expect(item(m, 'tag-hide').disabled).toBe(true);
    expect(item(m, 'tag-remove').disabled).toBe(true);
    expect(item(m, 'tag-search').disabled).toBeFalsy();
    expect(item(m, 'tag-copy').disabled).toBeFalsy();
  });

  it('wires hide/remove/search/copy to the actions', () => {
    const m = buildTagMenu('series:x', 'pulled', 'file', false, actions);
    item(m, 'tag-search').onselect();
    item(m, 'tag-copy').onselect();
    item(m, 'tag-hide').onselect();
    item(m, 'tag-remove').onselect();
    expect(actions.onSearch).toHaveBeenCalledOnce();
    expect(actions.onCopy).toHaveBeenCalledOnce();
    expect(actions.onHide).toHaveBeenCalledOnce();
    expect(actions.onRemove).toHaveBeenCalledOnce();
  });

  it('file tag menu includes a Relations… row before the separator; catalog omits it', () => {
    const relActions = {
      onSearch: () => {}, onCopy: () => {}, onHide: () => {}, onRemove: () => {},
      onRelations: () => {},
    };
    const fileMenu = buildTagMenu('character:samus', 'local', 'file', false, relActions);
    const fileIds = ids(fileMenu);
    expect(fileIds).toContain('tag-relations');
    expect(fileIds.indexOf('tag-relations')).toBeLessThan(fileIds.indexOf('separator'));
    const catalog = buildTagMenu('character:samus', 'local', 'catalog', false, relActions);
    expect(ids(catalog)).not.toContain('tag-relations');
  });
});

describe('buildTagMenu catalog-scoped', () => {
  it('is exactly search + copy', () => {
    const actions = { onSearch: vi.fn(), onCopy: vi.fn() };
    const m = buildTagMenu('series:x', 'both', 'catalog', false, actions);
    expect(ids(m)).toEqual(['tag-search', 'tag-copy']);
  });
});

describe('buildBackgroundMenu', () => {
  it('select-all disabled with no files, refresh always enabled', () => {
    const actions = { onSelectAll: vi.fn(), onRefresh: vi.fn() };
    const empty = buildBackgroundMenu(0, actions);
    expect(ids(empty)).toEqual(['select-all', 'refresh']);
    expect(item(empty, 'select-all').disabled).toBe(true);
    const some = buildBackgroundMenu(5, actions);
    expect(item(some, 'select-all').disabled).toBeFalsy();
    item(some, 'refresh').onselect();
    expect(actions.onRefresh).toHaveBeenCalledOnce();
    item(some, 'select-all').onselect();
    expect(actions.onSelectAll).toHaveBeenCalledOnce();
  });
});
