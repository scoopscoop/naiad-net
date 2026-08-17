/** Module-level focus registry. TagSearchInput registers its focus fn on mount;
 *  the ctrl+F hotkey invokes it without prop-drilling an element ref.
 */

let focusFn: (() => void) | null = null;

export function registerSearchFocus(fn: (() => void) | null): void {
  focusFn = fn;
}

export function focusSearch(): boolean {
  if (!focusFn) return false;
  focusFn();
  return true;
}
