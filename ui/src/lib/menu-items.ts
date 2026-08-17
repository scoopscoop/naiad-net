/** Item model for the shared context menu (spec §4). DOM-free; builders in
 *  this module stay unit-testable exactly like selection.ts. */
import type { FileDto, TagDetail } from './types';

/** A single actionable row. */
export interface MenuItem {
  /** Stable id, unique within one menu; used as the {#each} key and in tests. */
  id: string;
  /** The row text; carries live counts where relevant. */
  label: string;
  /** Optional right-aligned muted hint (e.g. 'r'); never interactive. */
  hint?: string;
  /** Danger styling (err-colored hover). At most one per menu, rendered last. */
  danger?: boolean;
  /** Inert row: opacity 0.5, skipped by keyboard nav, not activatable. */
  disabled?: boolean;
  /** Invoked on activate; the menu closes immediately after. */
  onselect: () => void;
}

/** A visual divider. Non-focusable, skipped by all navigation. */
export type MenuSeparator = 'separator';

export type MenuEntry = MenuItem | MenuSeparator;
export type MenuList = MenuEntry[];

/** Callback bundle for the gallery tile menu (spec §6.1). */
export interface TileActions {
  onOpen: () => void;
  onQuickLook: () => void;
  onPullTags: () => void;
  onCopyHashes: () => void;
  onCopyPaths: () => void;
}

/** Tag menu context: file-scoped (TagGroupList) vs catalog (file-agnostic). */
export type TagMenuContext = 'file' | 'catalog';

/** Callback bundle for the tag menu (spec §6.2 / §6.3). Hide/Remove are only
 *  ever invoked from the file context. */
export interface TagActions {
  onSearch: () => void;
  onCopy: () => void;
  onHide?: () => void;
  onRemove?: () => void;
  /** Opens the relations popover (spec §6); file context only. */
  onRelations?: () => void;
}

/** Callback bundle for the empty-background menu (spec §6.4). */
export interface BackgroundActions {
  onSelectAll: () => void;
  onRefresh: () => void;
}

/** Tile menu (spec §6.1). `targets` is the already-resolved target set
 *  (Explorer selection semantics live in the surface, §5.3). No reject item.
 *  `targets` must be non-empty (a tile menu only opens from a clicked tile). */
export function buildTileMenu(targets: FileDto[], actions: TileActions): MenuList {
  const n = targets.length;
  return [
    { id: 'open', label: 'Open', onselect: actions.onOpen },
    { id: 'quick-look', label: 'Quick Look', onselect: actions.onQuickLook },
    { id: 'pull-tags', label: n === 1 ? 'Pull tags' : `Pull tags — ${n} files`, onselect: actions.onPullTags },
    'separator',
    { id: 'copy-hash', label: n === 1 ? 'Copy hash' : `Copy ${n} hashes`, onselect: actions.onCopyHashes },
    { id: 'copy-path', label: n === 1 ? 'Copy path' : `Copy ${n} paths`, onselect: actions.onCopyPaths },
  ];
}

/** Tag menu (spec §6.2 file / §6.3 catalog). `mutating` gates hide/remove in
 *  the file context (spec §7.2). `_tag` is part of the stable public signature
 *  used by later tasks (labels are static in v1). */
export function buildTagMenu(
  _tag: TagDetail['tag'],
  presence: TagDetail['presence'],
  context: TagMenuContext,
  mutating: boolean,
  actions: TagActions,
): MenuList {
  const items: MenuList = [
    { id: 'tag-search', label: 'Search with tag', onselect: actions.onSearch },
    { id: 'tag-copy', label: 'Copy tag', onselect: actions.onCopy },
  ];
  if (context === 'catalog') return items;

  if (actions.onRelations) {
    const onRelations = actions.onRelations;
    items.push({ id: 'tag-relations', label: 'Relations…', onselect: onRelations });
  }
  items.push('separator');
  if (presence === 'pulled') {
    items.push({
      id: 'tag-hide',
      label: 'Hide from repo ⊘',
      disabled: mutating,
      onselect: () => actions.onHide?.(),
    });
  }
  items.push({
    id: 'tag-remove',
    label: 'Remove ×',
    danger: true,
    disabled: mutating,
    onselect: () => actions.onRemove?.(),
  });
  return items;
}

/** Empty-background menu (spec §6.4). */
export function buildBackgroundMenu(fileCount: number, actions: BackgroundActions): MenuList {
  return [
    { id: 'select-all', label: 'Select all', disabled: fileCount === 0, onselect: actions.onSelectAll },
    { id: 'refresh', label: 'Refresh', onselect: actions.onRefresh },
  ];
}
