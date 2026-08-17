import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import { tick } from 'svelte';
import ReportModal from './ReportModal.svelte';

describe('ReportModal', () => {
  it('focus lands on the note input after mount', async () => {
    const onsend = vi.fn();
    const oncancel = vi.fn();
    render(ReportModal, { repo: 'repo', tag: 'series:metroid', onsend, oncancel });
    await tick();
    const note = screen.getByLabelText('note (optional)');
    expect(note).toHaveFocus();
  });

  it('Enter sends with the typed note', async () => {
    const onsend = vi.fn();
    const oncancel = vi.fn();
    render(ReportModal, { repo: 'repo', tag: 'series:metroid', onsend, oncancel });
    await tick();
    const note = screen.getByLabelText('note (optional)');
    await fireEvent.input(note, { target: { value: 'wrong char' } });
    await fireEvent.keyDown(note, { key: 'Enter' });
    expect(onsend).toHaveBeenCalledWith('wrong char');
    expect(oncancel).not.toHaveBeenCalled();
  });

  it('Send report button calls onsend with the note', async () => {
    const onsend = vi.fn();
    const oncancel = vi.fn();
    render(ReportModal, { repo: 'repo', tag: 'series:metroid', onsend, oncancel });
    await tick();
    const note = screen.getByLabelText('note (optional)');
    await fireEvent.input(note, { target: { value: 'bad tag' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Send report' }));
    expect(onsend).toHaveBeenCalledWith('bad tag');
    expect(oncancel).not.toHaveBeenCalled();
  });

  it('empty note calls onsend with null', async () => {
    const onsend = vi.fn();
    const oncancel = vi.fn();
    render(ReportModal, { repo: 'repo', tag: 'series:metroid', onsend, oncancel });
    await tick();
    await fireEvent.click(screen.getByRole('button', { name: 'Send report' }));
    expect(onsend).toHaveBeenCalledWith(null);
  });

  it('Esc cancels without sending', async () => {
    const onsend = vi.fn();
    const oncancel = vi.fn();
    render(ReportModal, { repo: 'repo', tag: 'series:metroid', onsend, oncancel });
    await tick();
    const note = screen.getByLabelText('note (optional)');
    await fireEvent.input(note, { target: { value: 'something' } });
    await fireEvent.keyDown(note, { key: 'Escape' });
    expect(oncancel).toHaveBeenCalled();
    expect(onsend).not.toHaveBeenCalled();
  });

  it('Cancel button calls oncancel without sending', async () => {
    const onsend = vi.fn();
    const oncancel = vi.fn();
    render(ReportModal, { repo: 'repo', tag: 'series:metroid', onsend, oncancel });
    await tick();
    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(oncancel).toHaveBeenCalled();
    expect(onsend).not.toHaveBeenCalled();
  });

  it('shows the repo name, tag, and hash-reveal warning in the body copy', async () => {
    render(ReportModal, {
      repo: 'my-repo',
      tag: 'series:metroid',
      onsend: vi.fn(),
      oncancel: vi.fn(),
    });
    const dialog = screen.getByRole('dialog', { name: 'Report series:metroid to my-repo?' });
    expect(dialog).toHaveTextContent(/my-repo/);
    expect(dialog).toHaveTextContent(/reveals the file's hash/);
  });

  it('dialog has role=dialog and correct accessible name', async () => {
    render(ReportModal, {
      repo: 'repo',
      tag: 'series:metroid',
      onsend: vi.fn(),
      oncancel: vi.fn(),
    });
    expect(screen.getByRole('dialog', { name: 'Report series:metroid to repo?' })).toBeInTheDocument();
  });
});
