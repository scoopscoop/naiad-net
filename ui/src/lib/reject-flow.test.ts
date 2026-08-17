import { describe, it, expect, vi, beforeEach, afterEach, type Mock } from 'vitest';
import { createRejectFlow, FLASH_MS } from './reject-flow.svelte';
import * as api from './api';

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return {
    ...actual,
    rejectTag: vi.fn(),
    undoReject: vi.fn(),
  };
});

describe('createRejectFlow', () => {
  let refresh: Mock<() => Promise<void>>;
  let onRejectedChanged: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    refresh = vi.fn().mockResolvedValue(undefined);
    onRejectedChanged = vi.fn();
    vi.mocked(api.rejectTag).mockResolvedValue({ reports: false });
    vi.mocked(api.undoReject).mockResolvedValue(undefined);
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  // --- FLASH_MS export ---
  it('FLASH_MS is 2500', () => {
    expect(FLASH_MS).toBe(2500);
  });

  // --- reject: basic flash ---
  it('flash is null before any reject', () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    expect(flow.flash).toBeNull();
  });

  it('flash appears after a successful reject', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    expect(flow.flash).toEqual({ tag: 'tag:foo', services: ['repo1'], hash: 'hash1' });
  });

  it('flash auto-clears after FLASH_MS', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    expect(flow.flash).not.toBeNull();
    vi.advanceTimersByTime(FLASH_MS);
    expect(flow.flash).toBeNull();
  });

  it('reject calls refresh and onRejectedChanged', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    expect(refresh).toHaveBeenCalledTimes(1);
    expect(onRejectedChanged).toHaveBeenCalledTimes(1);
  });

  it('reject calls rejectTag for each service', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1', 'repo2'], 'hash1');
    expect(api.rejectTag).toHaveBeenCalledWith('hash1', 'tag:foo', 'repo1');
    expect(api.rejectTag).toHaveBeenCalledWith('hash1', 'tag:foo', 'repo2');
  });

  it('a second reject replaces the first flash (clearFlash called first)', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    await flow.reject('tag:bar', ['repo1'], 'hash1');
    expect(flow.flash).toEqual({ tag: 'tag:bar', services: ['repo1'], hash: 'hash1' });
  });

  // --- reportOffer ---
  it('reportOffer is null before any reject', () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    expect(flow.reportOffer).toBeNull();
  });

  it('reportOffer is set when single service + reports=true', async () => {
    vi.mocked(api.rejectTag).mockResolvedValue({ reports: true });
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    expect(flow.reportOffer).toEqual({ repo: 'repo1', tag: 'tag:foo', hash: 'hash1' });
  });

  it('no reportOffer when reports=false', async () => {
    vi.mocked(api.rejectTag).mockResolvedValue({ reports: false });
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    expect(flow.reportOffer).toBeNull();
  });

  it('no reportOffer when multiple services (even if all report reports=true)', async () => {
    vi.mocked(api.rejectTag).mockResolvedValue({ reports: true });
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1', 'repo2'], 'hash1');
    expect(flow.reportOffer).toBeNull();
  });

  // --- clearFlash ---
  it('clearFlash removes the flash and cancels the auto-dismiss timer', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    flow.clearFlash();
    expect(flow.flash).toBeNull();
    // Timer should be gone — advancing time does not throw or re-clear
    vi.advanceTimersByTime(FLASH_MS + 100);
    expect(vi.getTimerCount()).toBe(0);
  });

  // --- dismissOffer ---
  it('dismissOffer clears reportOffer without touching flash', async () => {
    vi.mocked(api.rejectTag).mockResolvedValue({ reports: true });
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    expect(flow.reportOffer).not.toBeNull();
    expect(flow.flash).not.toBeNull();
    flow.dismissOffer();
    expect(flow.reportOffer).toBeNull();
    expect(flow.flash).not.toBeNull();
  });

  // --- undoFlash ---
  it('undoFlash clears flash synchronously, then calls undoReject per service', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1', 'repo2'], 'hash1');

    let resolveUndo!: () => void;
    vi.mocked(api.undoReject).mockImplementationOnce(
      () => new Promise<void>((r) => { resolveUndo = r; }),
    );
    vi.mocked(api.undoReject).mockResolvedValue(undefined);

    const undoPromise = flow.undoFlash();
    // Flash and offer both null synchronously
    expect(flow.flash).toBeNull();
    resolveUndo();
    await undoPromise;

    expect(api.undoReject).toHaveBeenCalledWith('hash1', 'tag:foo', 'repo1');
    expect(api.undoReject).toHaveBeenCalledWith('hash1', 'tag:foo', 'repo2');
  });

  it('undoFlash clears reportOffer synchronously before the first await (#86 fix)', async () => {
    vi.mocked(api.rejectTag).mockResolvedValue({ reports: true });
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    expect(flow.reportOffer).not.toBeNull();

    let resolveUndo!: () => void;
    vi.mocked(api.undoReject).mockImplementationOnce(
      () => new Promise<void>((r) => { resolveUndo = r; }),
    );

    const undoPromise = flow.undoFlash();
    // reportOffer must be null before the first await returns
    expect(flow.reportOffer).toBeNull();

    resolveUndo();
    await undoPromise;
  });

  it('undoFlash calls refresh and onRejectedChanged after undoing', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    refresh.mockClear();
    onRejectedChanged.mockClear();

    await flow.undoFlash();

    expect(refresh).toHaveBeenCalledTimes(1);
    expect(onRejectedChanged).toHaveBeenCalledTimes(1);
  });

  it('undoFlash is a no-op when flash is null', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.undoFlash();
    expect(api.undoReject).not.toHaveBeenCalled();
    expect(refresh).not.toHaveBeenCalled();
  });

  // --- restore ---
  it('restore calls undoReject, refresh, and onRejectedChanged', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.restore('tag:foo', 'repo1', 'hash1');
    expect(api.undoReject).toHaveBeenCalledWith('hash1', 'tag:foo', 'repo1');
    expect(refresh).toHaveBeenCalledTimes(1);
    expect(onRejectedChanged).toHaveBeenCalledTimes(1);
  });

  // --- Esc handler ---
  it('attachEsc: Escape dismisses flash when reportOffer is null', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    const cleanup = flow.attachEsc();

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));

    expect(flow.flash).toBeNull();
    cleanup();
  });

  it('attachEsc: Escape does NOT dismiss flash while reportOffer is set', async () => {
    vi.mocked(api.rejectTag).mockResolvedValue({ reports: true });
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    expect(flow.reportOffer).not.toBeNull();
    const cleanup = flow.attachEsc();

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));

    expect(flow.flash).not.toBeNull();
    cleanup();
  });

  it('attachEsc cleanup removes the listener', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    const cleanup = flow.attachEsc();
    cleanup();

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));

    // Listener removed — flash still present
    expect(flow.flash).not.toBeNull();
  });

  // --- destroy: timer cleanup, no leaked timers ---
  it('destroy clears the pending auto-dismiss timer (no leak)', async () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    await flow.reject('tag:foo', ['repo1'], 'hash1');
    // Timer is armed; destroying must defuse it
    flow.destroy();
    expect(vi.getTimerCount()).toBe(0);
  });

  it('destroy on idle flow is a no-op', () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    expect(() => flow.destroy()).not.toThrow();
  });

  // --- notifyReportSent ---
  it('reportSent is false before any call', () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    expect(flow.reportSent).toBe(false);
  });

  it('reportSent is true immediately after notifyReportSent()', () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    flow.notifyReportSent();
    expect(flow.reportSent).toBe(true);
  });

  it('reportSent auto-clears after 4000 ms', () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    flow.notifyReportSent();
    vi.advanceTimersByTime(4000);
    expect(flow.reportSent).toBe(false);
  });

  it('a second notifyReportSent() resets the auto-clear timer', () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    flow.notifyReportSent();
    vi.advanceTimersByTime(3000);
    // Call again — should restart the 4 s window
    flow.notifyReportSent();
    vi.advanceTimersByTime(3000); // 3 s into the new window — still true
    expect(flow.reportSent).toBe(true);
    vi.advanceTimersByTime(1100); // past 4 s — clears
    expect(flow.reportSent).toBe(false);
  });

  it('destroy clears the reportSent timer too (no leak)', () => {
    const flow = createRejectFlow({ refresh, onRejectedChanged });
    flow.notifyReportSent();
    flow.destroy();
    expect(vi.getTimerCount()).toBe(0);
  });
});
