/** Global hotkey keymap (#27). Pure: KeyboardEvent -> action | null, so the
 *  table is unit-testable without a DOM.
 */

export type HotkeyAction =
  | { kind: 'cycle'; dir: 1 | -1 }
  | { kind: 'close-tab' }
  | { kind: 'new-gallery' }
  | { kind: 'activate-index'; n: number }
  | { kind: 'activate-last' }
  | { kind: 'focus-search' }
  | { kind: 'escape' }
  | { kind: 'select-all' }
  | { kind: 'open-focused' }
  | { kind: 'quick-look' };

/** `inEditable`: the event target is an input-like element. Plain Esc belongs
 *  to the input there; ctrl-chords still apply.
 */
export function matchHotkey(e: KeyboardEvent, inEditable: boolean): HotkeyAction | null {
  if (!e.ctrlKey && !e.altKey && !e.metaKey && e.key === 'Escape') {
    return inEditable ? null : { kind: 'escape' };
  }
  if (!e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey && e.key === 'Enter') {
    return inEditable ? null : { kind: 'open-focused' };
  }
  if (!e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey && e.key === ' ') {
    return inEditable ? null : { kind: 'quick-look' };
  }
  if (!e.ctrlKey || e.altKey || e.metaKey) return null;

  const key = e.key.toLowerCase();
  if (key === 'tab') return { kind: 'cycle', dir: e.shiftKey ? -1 : 1 };
  if (e.shiftKey) return null;
  if (key === 'w') return { kind: 'close-tab' };
  if (key === 'n' || key === 't') return { kind: 'new-gallery' };
  if (key === 'f') return { kind: 'focus-search' };
  // In editable targets ctrl+a stays native text select-all.
  if (key === 'a') return inEditable ? null : { kind: 'select-all' };
  if (key >= '1' && key <= '8') return { kind: 'activate-index', n: Number(key) - 1 };
  if (key === '9') return { kind: 'activate-last' };
  return null;
}
