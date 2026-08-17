import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import QuickLook from './QuickLook.svelte';
import type { FileDto } from '../lib/types';

const file: FileDto = {
  hash: 'abc',
  name: 'cat.png',
  size: 1,
  path: '/cat.png',
  imported_at: 100,
  created_at: 80,
  modified_at: 90,
  mime: 'image/png',
};

describe('QuickLook', () => {
  it('renders the file in an image stage', () => {
    render(QuickLook, { file, onclose: vi.fn() });
    expect(screen.getAllByRole('img', { name: 'cat.png' })).not.toHaveLength(0);
  });

  it('scrim click closes, frame click does not', async () => {
    const onclose = vi.fn();
    const { container } = render(QuickLook, { file, onclose });
    await fireEvent.click(container.querySelector('.frame')!);
    expect(onclose).not.toHaveBeenCalled();
    await fireEvent.click(container.querySelector('.scrim')!);
    expect(onclose).toHaveBeenCalledOnce();
  });

  // --- a11y: focus management ---

  it('has aria-modal="true" on the dialog element', () => {
    const { container } = render(QuickLook, { file, onclose: vi.fn() });
    expect(container.querySelector('[role="dialog"]')).toHaveAttribute('aria-modal', 'true');
  });

  it('moves focus to the frame element on mount', async () => {
    const { container } = render(QuickLook, { file, onclose: vi.fn() });
    const frame = container.querySelector('.frame') as HTMLElement;
    await vi.waitFor(() => {
      expect(document.activeElement).toBe(frame);
    });
  });

  it('Tab keeps focus on the frame when there are no focusable children', async () => {
    const { container } = render(QuickLook, { file, onclose: vi.fn() });
    const frame = container.querySelector('.frame') as HTMLElement;
    await vi.waitFor(() => expect(document.activeElement).toBe(frame));

    const e = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true });
    frame.dispatchEvent(e);

    expect(e.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(frame);
  });

  it('Shift+Tab keeps focus on the frame when there are no focusable children', async () => {
    const { container } = render(QuickLook, { file, onclose: vi.fn() });
    const frame = container.querySelector('.frame') as HTMLElement;
    await vi.waitFor(() => expect(document.activeElement).toBe(frame));

    const e = new KeyboardEvent('keydown', {
      key: 'Tab',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    frame.dispatchEvent(e);

    expect(e.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(frame);
  });

  it('restores focus to the previously focused element on unmount', async () => {
    const trigger = document.createElement('button');
    trigger.textContent = 'opener';
    document.body.appendChild(trigger);
    trigger.focus();
    expect(document.activeElement).toBe(trigger);

    const { container, unmount } = render(QuickLook, { file, onclose: vi.fn() });
    const frame = container.querySelector('.frame') as HTMLElement;
    await vi.waitFor(() => expect(document.activeElement).toBe(frame));

    unmount();
    expect(document.activeElement).toBe(trigger);

    document.body.removeChild(trigger);
  });
});
