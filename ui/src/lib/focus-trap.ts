/**
 * Focus-trap Svelte action.
 *
 * Attaches a keydown listener to `node` that intercepts Tab / Shift+Tab and
 * keeps focus cycling among the node's focusable descendants.  If no focusable
 * children exist, Tab keeps focus on the node itself (which must carry
 * tabindex="-1" so it can be focused programmatically).
 *
 * Usage:
 *   <div tabindex="-1" use:trapFocus …>…</div>
 */

const FOCUSABLE_SELECTOR = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(',');

/**
 * Returns all tabbable descendants of `container` in DOM order.
 * Elements with tabindex="-1" are excluded because they are not reachable via
 * the Tab key under normal browser behaviour.
 *
 * Visibility filter: elements with the `hidden` attribute (or a hidden
 * ancestor) are always excluded.  CSS-display-none / visibility-hidden
 * detection is intentionally omitted: jsdom returns 0 for offsetWidth/Height
 * and an empty DOMRectList for getClientRects(), so any layout-based check
 * would incorrectly filter out all elements in tests.
 */
export function getFocusableElements(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
    (el) => !el.hidden && !el.closest('[hidden]'),
  );
}

/**
 * Svelte action: trap Tab / Shift+Tab within `node`.
 *
 * The handler is attached to `node` so it captures both direct keydown events
 * (when `node` itself has focus) and bubbled events from focused children.
 *
 * Interception rules:
 * - Forward Tab on the last focusable (or on the container itself) → wrap to first.
 * - Shift+Tab on the first focusable (or on the container itself) → wrap to last.
 * - All other Tab keypresses are left for the browser to handle natively.
 * - When there are no focusable children, all Tab presses stay on the container.
 */
export function trapFocus(node: HTMLElement): { destroy(): void } {
  function handleKeydown(e: KeyboardEvent) {
    if (e.key !== 'Tab') return;

    const focusables = getFocusableElements(node);

    if (focusables.length === 0) {
      e.preventDefault();
      node.focus();
      return;
    }

    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    const active = document.activeElement;

    if (e.shiftKey) {
      // Shift+Tab: wrap backward when on the first focusable, the container, or
      // outside the trap (defensive guard).
      if (active === first || active === node || !node.contains(active)) {
        e.preventDefault();
        last.focus();
      }
    } else {
      // Tab: wrap forward when on the last focusable, the container, or outside.
      if (active === last || active === node || !node.contains(active)) {
        e.preventDefault();
        first.focus();
      }
    }
  }

  node.addEventListener('keydown', handleKeydown);

  return {
    destroy() {
      node.removeEventListener('keydown', handleKeydown);
    },
  };
}
