import { describe, it, expect } from 'vitest';
import { matchHotkey } from './hotkeys';
import { registerSearchFocus, focusSearch } from './search-focus';

const ev = (key: string, mods: Partial<KeyboardEventInit> = {}) =>
  new KeyboardEvent('keydown', { key, ...mods });

describe('matchHotkey', () => {
  it('ctrl+tab cycles forward, ctrl+shift+tab backward', () => {
    expect(matchHotkey(ev('Tab', { ctrlKey: true }), false)).toEqual({
      kind: 'cycle',
      dir: 1,
    });
    expect(matchHotkey(ev('Tab', { ctrlKey: true, shiftKey: true }), false)).toEqual({
      kind: 'cycle',
      dir: -1,
    });
  });

  it('ctrl+w closes the active tab', () => {
    expect(matchHotkey(ev('w', { ctrlKey: true }), false)).toEqual({ kind: 'close-tab' });
  });

  it('ctrl+n and ctrl+t open a new gallery tab', () => {
    expect(matchHotkey(ev('n', { ctrlKey: true }), false)).toEqual({ kind: 'new-gallery' });
    expect(matchHotkey(ev('t', { ctrlKey: true }), false)).toEqual({ kind: 'new-gallery' });
  });

  it('ctrl+1..8 activate by index, ctrl+9 activates last', () => {
    expect(matchHotkey(ev('1', { ctrlKey: true }), false)).toEqual({
      kind: 'activate-index',
      n: 0,
    });
    expect(matchHotkey(ev('8', { ctrlKey: true }), false)).toEqual({
      kind: 'activate-index',
      n: 7,
    });
    expect(matchHotkey(ev('9', { ctrlKey: true }), false)).toEqual({ kind: 'activate-last' });
    expect(matchHotkey(ev('0', { ctrlKey: true }), false)).toBeNull();
  });

  it('ctrl+f focuses search', () => {
    expect(matchHotkey(ev('f', { ctrlKey: true }), false)).toEqual({ kind: 'focus-search' });
  });

  it('esc closes the active detail tab but not while typing in an input', () => {
    expect(matchHotkey(ev('Escape'), false)).toEqual({ kind: 'escape' });
    expect(matchHotkey(ev('Escape'), true)).toBeNull();
  });

  it('maps ctrl+a to select-all outside editable targets', () => {
    const e = new KeyboardEvent('keydown', { key: 'a', ctrlKey: true });
    expect(matchHotkey(e, false)).toEqual({ kind: 'select-all' });
  });

  it('leaves ctrl+a to the input inside editable targets', () => {
    const e = new KeyboardEvent('keydown', { key: 'a', ctrlKey: true });
    expect(matchHotkey(e, true)).toBeNull();
  });

  it('ctrl chords still fire while typing in an input', () => {
    expect(matchHotkey(ev('Tab', { ctrlKey: true }), true)).toEqual({ kind: 'cycle', dir: 1 });
    expect(matchHotkey(ev('w', { ctrlKey: true }), true)).toEqual({ kind: 'close-tab' });
  });

  it('uppercase key from a chord still matches', () => {
    expect(matchHotkey(ev('W', { ctrlKey: true }), false)).toEqual({ kind: 'close-tab' });
  });

  it('enter opens the focused file but not while typing or with modifiers', () => {
    expect(matchHotkey(ev('Enter'), false)).toEqual({ kind: 'open-focused' });
    expect(matchHotkey(ev('Enter'), true)).toBeNull();
    expect(matchHotkey(ev('Enter', { ctrlKey: true }), false)).toBeNull();
    expect(matchHotkey(ev('Enter', { shiftKey: true }), false)).toBeNull();
  });

  it('space toggles quick-look but not while typing or with modifiers', () => {
    expect(matchHotkey(ev(' '), false)).toEqual({ kind: 'quick-look' });
    expect(matchHotkey(ev(' '), true)).toBeNull();
    expect(matchHotkey(ev(' ', { ctrlKey: true }), false)).toBeNull();
  });

  it('alt/meta disqualify; shift only valid on ctrl+tab; bare keys do not match', () => {
    expect(matchHotkey(ev('w', { ctrlKey: true, altKey: true }), false)).toBeNull();
    expect(matchHotkey(ev('w', { ctrlKey: true, metaKey: true }), false)).toBeNull();
    expect(matchHotkey(ev('w', { ctrlKey: true, shiftKey: true }), false)).toBeNull();
    expect(matchHotkey(ev('w'), false)).toBeNull();
    expect(matchHotkey(ev('x', { ctrlKey: true }), false)).toBeNull();
  });
});

describe('search focus registry', () => {
  it('invokes a registered focus fn, reports absence, unregisters cleanly', () => {
    let called = 0;
    registerSearchFocus(() => called++);
    expect(focusSearch()).toBe(true);
    expect(called).toBe(1);
    registerSearchFocus(null);
    expect(focusSearch()).toBe(false);
    expect(called).toBe(1);
  });
});
