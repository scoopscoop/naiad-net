/** Tests for PullFailureModal — the blocking notice raised on a failed repo
 *  pull (#228). Mirrors ReportModal.test.ts structure. */

import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import { tick } from 'svelte';
import PullFailureModal from './PullFailureModal.svelte';

describe('PullFailureModal', () => {
  it('has role alertdialog and aria-modal="true"', async () => {
    render(PullFailureModal, { kind: 'repo', repos: ['ptr'], message: 'err', ondismiss: vi.fn() });
    await tick();
    const dialog = screen.getByRole('alertdialog');
    expect(dialog).toHaveAttribute('aria-modal', 'true');
  });

  it('kind repo, one repo: body names the repo; daemon message renders verbatim', async () => {
    render(PullFailureModal, {
      kind: 'repo',
      repos: ['ptr'],
      message: 'ptr: connection refused',
      ondismiss: vi.fn(),
    });
    await tick();
    const dialog = screen.getByRole('alertdialog');
    expect(dialog).toHaveTextContent('ptr');
    expect(screen.getByText(/ptr: connection refused/)).toBeInTheDocument();
  });

  it('kind repo, two repos: both names appear comma-joined', async () => {
    render(PullFailureModal, {
      kind: 'repo',
      repos: ['alpha', 'beta'],
      message: 'err',
      ondismiss: vi.fn(),
    });
    await tick();
    const dialog = screen.getByRole('alertdialog');
    expect(dialog).toHaveTextContent('alpha, beta');
  });

  it('kind fatal: body says the pull did not complete; no repo name required', async () => {
    render(PullFailureModal, {
      kind: 'fatal',
      message: 'stream error',
      ondismiss: vi.fn(),
    });
    await tick();
    const dialog = screen.getByRole('alertdialog');
    expect(dialog).toHaveTextContent('The pull did not complete');
  });

  it('Dismiss button has focus after mount', async () => {
    render(PullFailureModal, { kind: 'fatal', message: '', ondismiss: vi.fn() });
    await tick();
    expect(screen.getByRole('button', { name: 'Dismiss' })).toHaveFocus();
  });

  it('Escape on the modal calls ondismiss once', async () => {
    const ondismiss = vi.fn();
    render(PullFailureModal, { kind: 'fatal', message: '', ondismiss });
    await tick();
    const dialog = screen.getByRole('alertdialog');
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(ondismiss).toHaveBeenCalledTimes(1);
  });

  it('Enter on the modal calls ondismiss once', async () => {
    const ondismiss = vi.fn();
    render(PullFailureModal, { kind: 'fatal', message: '', ondismiss });
    await tick();
    const dialog = screen.getByRole('alertdialog');
    await fireEvent.keyDown(dialog, { key: 'Enter' });
    expect(ondismiss).toHaveBeenCalledTimes(1);
  });

  it('clicking Dismiss button calls ondismiss', async () => {
    const ondismiss = vi.fn();
    render(PullFailureModal, { kind: 'fatal', message: '', ondismiss });
    await tick();
    await fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    expect(ondismiss).toHaveBeenCalledTimes(1);
  });

  it('clicking the backdrop calls ondismiss', async () => {
    const ondismiss = vi.fn();
    render(PullFailureModal, { kind: 'fatal', message: '', ondismiss });
    await tick();
    await fireEvent.click(screen.getByLabelText('dismiss pull failure notice'));
    expect(ondismiss).toHaveBeenCalledTimes(1);
  });

  it('empty message renders no error block', async () => {
    const { container } = render(PullFailureModal, {
      kind: 'repo',
      repos: ['ptr'],
      message: '',
      ondismiss: vi.fn(),
    });
    await tick();
    expect(container.querySelector('.error-text')).toBeNull();
  });

  // F6: aria-describedby includes error id when message is non-empty.
  it('aria-describedby includes pull-failure-error when message is non-empty', async () => {
    render(PullFailureModal, {
      kind: 'repo',
      repos: ['ptr'],
      message: 'connection refused',
      ondismiss: vi.fn(),
    });
    await tick();
    const dialog = screen.getByRole('alertdialog');
    expect(dialog.getAttribute('aria-describedby')).toContain('pull-failure-error');
    expect(dialog.getAttribute('aria-describedby')).toContain('pull-failure-body');
  });

  it('aria-describedby is only pull-failure-body when message is empty', async () => {
    render(PullFailureModal, {
      kind: 'fatal',
      message: '',
      ondismiss: vi.fn(),
    });
    await tick();
    const dialog = screen.getByRole('alertdialog');
    expect(dialog.getAttribute('aria-describedby')).toBe('pull-failure-body');
  });

  // F3: focus restore on destroy.
  it('restores focus to the previously focused element on unmount', async () => {
    const trigger = document.createElement('button');
    trigger.textContent = 'opener';
    document.body.appendChild(trigger);
    trigger.focus();
    expect(document.activeElement).toBe(trigger);

    const { unmount } = render(PullFailureModal, {
      kind: 'fatal',
      message: '',
      ondismiss: vi.fn(),
    });
    await tick();
    // Modal focus moved to Dismiss button.
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Dismiss' }));

    unmount();
    expect(document.activeElement).toBe(trigger);
    document.body.removeChild(trigger);
  });

  it('does not error when previously focused element is detached before unmount', async () => {
    const trigger = document.createElement('button');
    document.body.appendChild(trigger);
    trigger.focus();

    const { unmount } = render(PullFailureModal, {
      kind: 'fatal',
      message: '',
      ondismiss: vi.fn(),
    });
    await tick();
    // Remove the trigger from the DOM before unmount.
    document.body.removeChild(trigger);

    // Should not throw; detached element guard prevents focusing a removed node.
    expect(() => unmount()).not.toThrow();
  });
});
