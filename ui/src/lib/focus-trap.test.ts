import { describe, it, expect, afterEach } from 'vitest';
import { trapFocus, getFocusableElements } from './focus-trap';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makeContainer(...children: HTMLElement[]): HTMLElement {
  const div = document.createElement('div');
  div.setAttribute('tabindex', '-1');
  if (children.length) div.append(...children);
  document.body.appendChild(div);
  return div;
}

function btn(label: string): HTMLButtonElement {
  const b = document.createElement('button');
  b.textContent = label;
  return b;
}

// ---------------------------------------------------------------------------
// getFocusableElements
// ---------------------------------------------------------------------------

describe('getFocusableElements', () => {
  it('includes buttons, inputs, anchors, and tabindex-0 elements', () => {
    const container = document.createElement('div');
    const button = btn('click');
    const input = document.createElement('input');
    const a = document.createElement('a');
    a.href = '#';
    const tab0 = document.createElement('span');
    tab0.setAttribute('tabindex', '0');
    container.append(button, input, a, tab0);

    const result = getFocusableElements(container);
    expect(result).toContain(button);
    expect(result).toContain(input);
    expect(result).toContain(a);
    expect(result).toContain(tab0);
  });

  it('excludes disabled buttons and inputs', () => {
    const container = document.createElement('div');
    const disabledBtn = document.createElement('button');
    disabledBtn.disabled = true;
    const disabledInput = document.createElement('input');
    disabledInput.disabled = true;
    container.append(disabledBtn, disabledInput);

    const result = getFocusableElements(container);
    expect(result).not.toContain(disabledBtn);
    expect(result).not.toContain(disabledInput);
  });

  it('excludes elements with tabindex="-1"', () => {
    const container = document.createElement('div');
    const tabNeg = document.createElement('span');
    tabNeg.setAttribute('tabindex', '-1');
    container.appendChild(tabNeg);

    expect(getFocusableElements(container)).not.toContain(tabNeg);
  });
});

// ---------------------------------------------------------------------------
// trapFocus action
// ---------------------------------------------------------------------------

describe('trapFocus', () => {
  let container: HTMLElement;

  afterEach(() => {
    container?.parentNode?.removeChild(container);
  });

  it('Tab on the last focusable wraps to the first', () => {
    const b1 = btn('first');
    const b2 = btn('last');
    container = makeContainer(b1, b2);
    trapFocus(container);

    b2.focus();
    b2.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true }));

    expect(document.activeElement).toBe(b1);
  });

  it('Shift+Tab on the first focusable wraps to the last', () => {
    const b1 = btn('first');
    const b2 = btn('last');
    container = makeContainer(b1, b2);
    trapFocus(container);

    b1.focus();
    b1.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true, cancelable: true }),
    );

    expect(document.activeElement).toBe(b2);
  });

  it('Tab on the container itself moves focus to the first focusable', () => {
    const b1 = btn('first');
    const b2 = btn('second');
    container = makeContainer(b1, b2);
    trapFocus(container);

    container.focus();
    container.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true }),
    );

    expect(document.activeElement).toBe(b1);
  });

  it('Shift+Tab on the container itself moves focus to the last focusable', () => {
    const b1 = btn('first');
    const b2 = btn('last');
    container = makeContainer(b1, b2);
    trapFocus(container);

    container.focus();
    container.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true, cancelable: true }),
    );

    expect(document.activeElement).toBe(b2);
  });

  it('Tab with no focusable children keeps focus on the container', () => {
    container = makeContainer(); // no children
    trapFocus(container);

    container.focus();
    container.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true }),
    );

    expect(document.activeElement).toBe(container);
  });

  it('Tab on a middle element is not intercepted (defaultPrevented stays false)', () => {
    const b1 = btn('first');
    const b2 = btn('middle');
    const b3 = btn('last');
    container = makeContainer(b1, b2, b3);
    trapFocus(container);

    b2.focus();
    const e = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true });
    b2.dispatchEvent(e);

    expect(e.defaultPrevented).toBe(false);
  });

  it('Shift+Tab on a middle element is not intercepted', () => {
    const b1 = btn('first');
    const b2 = btn('middle');
    const b3 = btn('last');
    container = makeContainer(b1, b2, b3);
    trapFocus(container);

    b2.focus();
    const e = new KeyboardEvent('keydown', {
      key: 'Tab',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    b2.dispatchEvent(e);

    expect(e.defaultPrevented).toBe(false);
  });

  it('non-Tab keys are ignored', () => {
    const b1 = btn('only');
    container = makeContainer(b1);
    trapFocus(container);

    b1.focus();
    const e = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
    b1.dispatchEvent(e);

    expect(e.defaultPrevented).toBe(false);
    expect(document.activeElement).toBe(b1);
  });

  it('Tab wraps to first focusable when focus is outside the container', () => {
    // Simulates the "focus escaped" defensive guard: active element is
    // document.body (outside the trap), Tab should wrap to the first focusable.
    const b1 = btn('first');
    const b2 = btn('last');
    container = makeContainer(b1, b2);
    trapFocus(container);

    document.body.focus();
    expect(document.activeElement).toBe(document.body);
    container.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true }),
    );

    expect(document.activeElement).toBe(b1);
  });

  it('destroy removes the listener so Tab is no longer intercepted', () => {
    const b1 = btn('first');
    const b2 = btn('last');
    container = makeContainer(b1, b2);
    const { destroy } = trapFocus(container);

    destroy();

    b2.focus();
    const e = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true });
    b2.dispatchEvent(e);

    // No wrapping happened — focus stays on b2
    expect(document.activeElement).toBe(b2);
    expect(e.defaultPrevented).toBe(false);
  });
});
