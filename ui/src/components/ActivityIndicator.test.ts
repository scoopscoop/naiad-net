import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { tick } from 'svelte';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import ActivityIndicator from './ActivityIndicator.svelte';
import { activity } from '../lib/activity.svelte';
import { catchup } from '../lib/catchup.svelte';
import * as api from '../lib/api';

vi.mock('../lib/api', () => ({ health: vi.fn() }));

beforeEach(() => {
  vi.mocked(api.health).mockResolvedValue({ ok: true, watch: null, scan: null, warmup: null });
  // jsdom lacks matchMedia. Stub it so reduceMotion=true → fadeMs=0. Svelte's
  // transition animate() fast-paths for 0-duration by calling on_finish()
  // synchronously without touching element.animate (also absent in jsdom).
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    configurable: true,
    value: () => ({ matches: true }),
  });
});

afterEach(() => {
  for (const a of [...activity.activities]) activity.dismiss(a.id);
  catchup.set(null);
  vi.useRealTimers();
  vi.restoreAllMocks();
});

const dot = () => screen.getByRole('button', { name: /daemon/i });

describe('ActivityIndicator', () => {
  it('reports idle when the daemon is healthy and nothing is running', async () => {
    render(ActivityIndicator);
    await waitFor(() => expect(dot().getAttribute('data-state')).toBe('idle'));
  });

  it('offline outranks a store error', async () => {
    vi.mocked(api.health).mockResolvedValue({ ok: false, watch: null, scan: null, warmup: null });
    activity.begin({ label: 'Scan', kind: 'scan' }).fail('bad');
    render(ActivityIndicator);
    await waitFor(() => expect(dot().getAttribute('data-state')).toBe('offline'));
    expect(dot().getAttribute('aria-label')).toMatch(/offline/i);
  });

  it('an error colors the dot while other work keeps the ring on', async () => {
    activity.begin({ label: 'Import', kind: 'import' });
    activity.begin({ label: 'Scan', kind: 'scan' }).fail('bad');
    render(ActivityIndicator);
    await waitFor(() => expect(dot().getAttribute('data-state')).toBe('error'));
    expect(dot().getAttribute('aria-label')).not.toMatch(/offline/i);
    // The channels are independent (#133): the failure does not hide the fact
    // that the import is still running.
    expect(dot().hasAttribute('data-busy')).toBe(true);
    expect(dot().getAttribute('aria-label')).toMatch(/failed, working/i);
  });

  it('a terminal status decays to idle after the linger window', async () => {
    // Fake timers must be installed before render: the linger setTimeout is
    // armed by an effect during mount, and a real-timer callback is invisible
    // to advanceTimersByTime.
    vi.useFakeTimers();
    activity.begin({ label: 'Scan', kind: 'scan' }).warn('partial');
    render(ActivityIndicator);

    await vi.advanceTimersByTimeAsync(0); // flush the initial health() poll
    expect(dot().getAttribute('data-state')).toBe('warning');

    await vi.advanceTimersByTimeAsync(6000);
    await tick();
    expect(dot().getAttribute('data-state')).toBe('idle');
  });

  it('a later activity still lights the dot after an earlier error lingered out', async () => {
    vi.useFakeTimers();
    activity.begin({ label: 'Scan', kind: 'scan' }).fail('bad');
    render(ActivityIndicator);

    await vi.advanceTimersByTimeAsync(0);
    expect(dot().getAttribute('data-state')).toBe('error');

    await vi.advanceTimersByTimeAsync(6000);
    await tick();
    expect(dot().getAttribute('data-state')).toBe('idle');

    // The errored entry is still in the store — a different kind does not evict
    // it. The new run shows on the ring; the dot stays at its lingered-out idle.
    activity.begin({ label: 'Import', kind: 'import' });
    await tick();
    expect(dot().getAttribute('data-state')).toBe('idle');
    expect(dot().hasAttribute('data-busy')).toBe(true);
  });

  it('a second failure of another kind re-lights the dot', async () => {
    vi.useFakeTimers();
    activity.begin({ label: 'Scan', kind: 'scan' }).fail('bad');
    render(ActivityIndicator);

    await vi.advanceTimersByTimeAsync(6000);
    await tick();
    expect(dot().getAttribute('data-state')).toBe('idle');

    activity.begin({ label: 'Import', kind: 'import' }).fail('worse');
    await tick();
    expect(dot().getAttribute('data-state')).toBe('error');
  });

  // The two-channel pip (#133): the dot's color is health, the ring is
  // activity. Neither hides the other.
  it('shows no ring when nothing is running', async () => {
    render(ActivityIndicator);
    await waitFor(() => expect(dot().getAttribute('data-state')).toBe('idle'));
    expect(dot().hasAttribute('data-busy')).toBe(false);
    expect(dot().querySelector('.ring')).toBeNull();
  });

  it('rings around a healthy dot while a job runs', async () => {
    activity.begin({ label: 'Import', kind: 'import' });
    render(ActivityIndicator);
    await waitFor(() => expect(dot().hasAttribute('data-busy')).toBe(true));
    expect(dot().getAttribute('data-state')).toBe('idle');
    expect(dot().querySelector('.ring')).not.toBeNull();
    expect(dot().getAttribute('aria-label')).toBe('daemon ok, working');
  });

  it('keeps a warning visible on the dot while other work continues', async () => {
    activity.begin({ label: 'Scan', kind: 'scan' }).warn('partial');
    activity.begin({ label: 'Import', kind: 'import' });
    render(ActivityIndicator);
    await waitFor(() => expect(dot().getAttribute('data-state')).toBe('warning'));
    expect(dot().hasAttribute('data-busy')).toBe(true);
    expect(dot().getAttribute('aria-label')).toBe('daemon ok, an activity warned, working');
  });

  it('never rings while offline, even with a running entry in the store', async () => {
    vi.mocked(api.health).mockResolvedValue({ ok: false, watch: null, scan: null, warmup: null });
    activity.begin({ label: 'Import', kind: 'import' });
    render(ActivityIndicator);
    await waitFor(() => expect(dot().getAttribute('data-state')).toBe('offline'));
    expect(dot().hasAttribute('data-busy')).toBe(false);
    expect(dot().querySelector('.ring')).toBeNull();
  });

  it('keeps the panel open when a non-focusable region is pressed', async () => {
    activity.begin({ label: 'Library scan', kind: 'scan' });
    render(ActivityIndicator);
    await fireEvent.click(dot());

    // The press blurs the trigger to <body>: focusout carries a null
    // relatedTarget, which must not close the panel. mousedown is not
    // suppressed, so the panel's text stays selectable.
    const label = screen.getByText('Library scan');
    const down = new MouseEvent('mousedown', { bubbles: true, cancelable: true });
    label.dispatchEvent(down);
    expect(down.defaultPrevented).toBe(false);
    await fireEvent.focusOut(dot(), { relatedTarget: null });
    await tick();
    expect(screen.queryByRole('region', { name: /activity/i })).toBeTruthy();
  });

  it('closes the panel on a pointer press outside the wrapper', async () => {
    activity.begin({ label: 'Library scan', kind: 'scan' });
    render(ActivityIndicator);
    await fireEvent.click(dot());
    expect(screen.getByRole('region', { name: /activity/i })).toBeTruthy();

    document.body.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
    await tick();
    expect(screen.queryByRole('region', { name: /activity/i })).toBeNull();
  });

  it('closes the panel when focus moves to an element outside', async () => {
    const outside = document.createElement('button');
    document.body.append(outside);
    render(ActivityIndicator);
    await fireEvent.click(dot());
    await fireEvent.focusOut(dot(), { relatedTarget: outside });
    await tick();
    expect(screen.queryByRole('region', { name: /activity/i })).toBeNull();
    outside.remove();
  });

  it('opens a panel listing activities, and dismisses one', async () => {
    const job = activity.begin({ label: 'Library scan', kind: 'scan' });
    job.progress({ detail: 'indexed 12', done: 12, total: 100 });
    render(ActivityIndicator);

    await fireEvent.click(dot());
    expect(screen.getByText('Library scan')).toBeTruthy();
    expect(screen.getByText('indexed 12')).toBeTruthy();
    expect(screen.getByRole('progressbar').getAttribute('aria-valuenow')).toBe('12');

    await fireEvent.click(screen.getByLabelText('dismiss Library scan'));
    expect(screen.queryByText('Library scan')).toBeNull();
  });

  it('returns focus to the trigger after a dismiss, so Escape still closes', async () => {
    activity.begin({ label: 'Library scan', kind: 'scan' });
    activity.begin({ label: 'Import', kind: 'import' });
    render(ActivityIndicator);

    await fireEvent.click(dot());
    const dismissBtn = screen.getByLabelText('dismiss Library scan');
    dismissBtn.focus();
    await fireEvent.click(dismissBtn);
    await tick();

    expect(document.activeElement).toBe(dot());
    expect(screen.getByRole('region', { name: /activity/i })).toBeTruthy();

    await fireEvent.keyDown(document.activeElement!, { key: 'Escape' });
    expect(screen.queryByRole('region', { name: /activity/i })).toBeNull();
  });

  it('closes the panel on Escape from the trigger', async () => {
    render(ActivityIndicator);
    await fireEvent.click(dot());
    expect(screen.getByRole('region', { name: /activity/i })).toBeTruthy();
    await fireEvent.keyDown(dot(), { key: 'Escape' });
    expect(screen.queryByRole('region', { name: /activity/i })).toBeNull();
  });

  it('closes the panel on Escape from a dismiss button inside the panel', async () => {
    activity.begin({ label: 'Library scan', kind: 'scan' });
    render(ActivityIndicator);
    await fireEvent.click(dot());
    const dismissBtn = screen.getByLabelText('dismiss Library scan');
    await fireEvent.keyDown(dismissBtn, { key: 'Escape' });
    expect(screen.queryByRole('region', { name: /activity/i })).toBeNull();
  });

  it('opens a watch-register job from health and completes it on success', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      .mockResolvedValueOnce({
        ok: true,
        watch: { total: 2, done: 1, current: 'D:/img/newstuff', failed: [], complete: false },
        scan: null,
        warmup: null,
      })
      .mockResolvedValue({
        ok: true,
        watch: { total: 2, done: 2, current: null, failed: [], complete: true },
        scan: null,
        warmup: null,
      });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0); // first poll → job begins
    const running = activity.activities.find((a) => a.kind === 'watch-register');
    expect(running?.status).toBe('running');
    expect(running?.detail).toContain('root 1/2');
    expect(running?.announce).toBeUndefined(); // panel-only

    await vi.advanceTimersByTimeAsync(5000); // second poll → complete
    const done = activity.activities.find((a) => a.kind === 'watch-register');
    expect(done?.status).toBe('success');
  });

  it('warns the watch-register job when a root fails', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      .mockResolvedValueOnce({
        ok: true,
        watch: { total: 1, done: 0, current: 'E:/gone', failed: [], complete: false },
        scan: null,
        warmup: null,
      })
      .mockResolvedValue({
        ok: true,
        watch: {
          total: 1,
          done: 0,
          current: null,
          failed: [{ path: 'E:/gone', error: 'path not found' }],
          complete: true,
        },
        scan: null,
        warmup: null,
      });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(5000);
    const job = activity.activities.find((a) => a.kind === 'watch-register');
    expect(job?.status).toBe('warning');
    expect(job?.message).toContain('gone');
  });

  it('shows a toast for an announced running job', async () => {
    activity.begin({ label: 'Syncing tags', kind: 'pull-tags', announce: true });
    render(ActivityIndicator);
    await tick();
    expect(screen.getByRole('status')).toBeTruthy();
    expect(screen.getByText('Syncing tags')).toBeTruthy();
  });

  it('does not show a toast for a non-announced job', async () => {
    activity.begin({ label: 'Library scan', kind: 'scan' });
    render(ActivityIndicator);
    await tick();
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('toast stays after succeed() then disappears after the 2s grace', async () => {
    vi.useFakeTimers();
    const job = activity.begin({ label: 'Syncing tags', kind: 'pull-tags', announce: true });
    render(ActivityIndicator);

    await vi.advanceTimersByTimeAsync(0);
    expect(screen.getByRole('status')).toBeTruthy();

    job.succeed({ detail: 'done' });
    await tick();
    // Still visible immediately after terminal state.
    expect(screen.getByRole('status')).toBeTruthy();

    await vi.advanceTimersByTimeAsync(2000);
    await tick();
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('resets the watch-register latch on daemon offline so a new run appears on reconnect', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      // Poll 1: watch run starts.
      .mockResolvedValueOnce({
        ok: true,
        watch: { total: 1, done: 0, current: 'D:/a', failed: [], complete: false },
        scan: null,
        warmup: null,
      })
      // Poll 2: watch run completes; latch is set.
      .mockResolvedValueOnce({
        ok: true,
        watch: { total: 1, done: 1, current: null, failed: [], complete: true },
        scan: null,
        warmup: null,
      })
      // Poll 3: daemon offline; latch must be cleared.
      .mockResolvedValueOnce({ ok: false, watch: null, scan: null, warmup: null })
      // Poll 4: daemon back with a fresh in-progress watch run.
      .mockResolvedValue({
        ok: true,
        watch: { total: 1, done: 0, current: 'D:/b', failed: [], complete: false },
        scan: null,
        warmup: null,
      });

    render(ActivityIndicator);

    await vi.advanceTimersByTimeAsync(0); // poll 1 → job begins
    expect(activity.activities.find((a) => a.kind === 'watch-register')?.status).toBe('running');

    await vi.advanceTimersByTimeAsync(5000); // poll 2 → job completes
    expect(activity.activities.find((a) => a.kind === 'watch-register')?.status).toBe('success');

    await vi.advanceTimersByTimeAsync(5000); // poll 3 → daemon offline, latch reset

    await vi.advanceTimersByTimeAsync(5000); // poll 4 → reconnect, new watch run
    // begin() replaces the terminal 'success' entry; a fresh running job must appear.
    expect(activity.activities.find((a) => a.kind === 'watch-register')?.status).toBe('running');
  });

  it('hides the toast when the panel is open', async () => {
    activity.begin({ label: 'Syncing tags', kind: 'pull-tags', announce: true });
    render(ActivityIndicator);
    await tick();
    expect(screen.getByRole('status')).toBeTruthy();

    await fireEvent.click(dot());
    await tick();
    expect(screen.queryByRole('status')).toBeNull();
    expect(screen.getByRole('region', { name: /activity/i })).toBeTruthy();
  });

  const idleScan = {
    running: false,
    imported: 0,
    errors: 0,
    roots_total: 0,
    roots_done: 0,
    current: null,
    complete: false,
  };

  it('opens a catchup-scan job from health and completes it on success', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: {
          running: true,
          imported: 12000,
          errors: 0,
          roots_total: 1,
          roots_done: 0,
          current: 'D:/img/newstuff',
          complete: false,
        },
        warmup: null,
      })
      .mockResolvedValue({
        ok: true,
        watch: null,
        scan: {
          running: false,
          imported: 95000,
          errors: 0,
          roots_total: 1,
          roots_done: 1,
          current: null,
          complete: true,
        },
        warmup: null,
      });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    const running = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(running?.status).toBe('running');
    expect(running?.label).toBe('Library scan');
    expect(running?.detail).toContain('12,000 files');
    expect(running?.detail).toContain('newstuff');
    expect(running?.total).toBeUndefined();
    expect(running?.announce).toBeUndefined();
    expect(running?.queued).toBeFalsy(); // actively scanning, so it gets a bar

    await vi.advanceTimersByTimeAsync(5000);
    const done = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(done?.status).toBe('success');
    expect(done?.detail).toContain('95,000 files');
    // Under the announce threshold (5s elapsed < 10s) → stays panel-only.
    expect(done?.announce).toBe(false);
  });

  it('warns the catchup-scan job when files could not be indexed', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: {
          running: true,
          imported: 100,
          errors: 0,
          roots_total: 1,
          roots_done: 0,
          current: 'D:/a',
          complete: false,
        },
        warmup: null,
      })
      .mockResolvedValue({
        ok: true,
        watch: null,
        scan: {
          running: false,
          imported: 100,
          errors: 4,
          roots_total: 1,
          roots_done: 1,
          current: null,
          complete: true,
        },
        warmup: null,
      });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(5000);
    const job = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(job?.status).toBe('warning');
    expect(job?.message).toContain('4');
  });

  it('shows nothing for a scan already complete before the first poll', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health).mockResolvedValue({
      ok: true,
      watch: null,
      scan: {
        running: false,
        imported: 95000,
        errors: 0,
        roots_total: 1,
        roots_done: 1,
        current: null,
        complete: true,
      },
      warmup: null,
    });
    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(activity.activities.find((a) => a.kind === 'catchup-scan')).toBeUndefined();
  });

  it('shows nothing for a default never-run scan', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health).mockResolvedValue({
      ok: true,
      watch: null,
      scan: idleScan,
      warmup: null,
    });
    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(activity.activities.find((a) => a.kind === 'catchup-scan')).toBeUndefined();
  });

  it('publishes the latest scan status to the shared store, null when offline', async () => {
    vi.useFakeTimers();
    const runningScan = {
      running: true,
      imported: 5000,
      errors: 0,
      roots_total: 1,
      roots_done: 0,
      current: 'D:/a',
      complete: false,
    };
    vi.mocked(api.health)
      .mockResolvedValueOnce({ ok: true, watch: null, scan: runningScan, warmup: null })
      .mockResolvedValue({ ok: false, watch: null, scan: null, warmup: null });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(catchup.status?.imported).toBe(5000);

    await vi.advanceTimersByTimeAsync(5000);
    expect(catchup.status).toBeNull();
  });

  it('resets the catchup-scan latch on daemon offline so a new run appears on reconnect', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: { running: true, imported: 10, errors: 0, roots_total: 1, roots_done: 0, current: 'D:/a', complete: false },
        warmup: null,
      })
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: { running: false, imported: 20, errors: 0, roots_total: 1, roots_done: 1, current: null, complete: true },
        warmup: null,
      })
      .mockResolvedValueOnce({ ok: false, watch: null, scan: null, warmup: null })
      .mockResolvedValue({
        ok: true,
        watch: null,
        scan: { running: true, imported: 5, errors: 0, roots_total: 1, roots_done: 0, current: 'D:/b', complete: false },
        warmup: null,
      });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(activity.activities.find((a) => a.kind === 'catchup-scan')?.status).toBe('running');
    await vi.advanceTimersByTimeAsync(5000);
    expect(activity.activities.find((a) => a.kind === 'catchup-scan')?.status).toBe('success');
    await vi.advanceTimersByTimeAsync(5000);
    await vi.advanceTimersByTimeAsync(5000);
    expect(activity.activities.find((a) => a.kind === 'catchup-scan')?.status).toBe('running');
  });

  // ---- startup warmup visibility (#130) --------------------------------
  //
  // The regression these cover: on a cold start the daemon marks the catch-up
  // scan running but defers it behind the cache warmup (#126), so the scan
  // reports all-zero counters for the whole warmup. The panel used to drop that
  // on the floor and read "Nothing running." for ~96 seconds.

  const warming = (phase: 'graph' | 'completion') => ({ phase, complete: false }) as const;
  const warm = { phase: 'done', complete: true } as const;
  /** A deferred scan: running, but nothing indexed and no roots walked yet. */
  const deferredScan = {
    running: true,
    imported: 0,
    errors: 0,
    roots_total: 0,
    roots_done: 0,
    current: null,
    complete: false,
  };

  it('shows a warmup job and a queued scan while the daemon is still warming', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health).mockResolvedValue({
      ok: true,
      watch: null,
      scan: deferredScan,
      warmup: warming('graph'),
    });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);

    const prep = activity.activities.find((a) => a.kind === 'warmup');
    expect(prep?.status).toBe('running');
    expect(prep?.label).toBe('Preparing library');
    expect(prep?.detail).toBe('building tag relations');

    // The scan is visible from the first poll, and marked queued so it renders
    // no progress bar — it has not started yet.
    const scan = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(scan?.status).toBe('running');
    expect(scan?.detail).toBe('waiting for cache warmup');
    expect(scan?.queued).toBe(true);

    // Both running → the ring spins around a healthy dot instead of sitting
    // idle-static.
    await tick();
    expect(dot().getAttribute('data-state')).toBe('idle');
    expect(dot().hasAttribute('data-busy')).toBe(true);
  });

  it('advances the warmup detail from graph to completion, then settles it', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: deferredScan,
        warmup: warming('graph'),
      })
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: deferredScan,
        warmup: warming('completion'),
      })
      .mockResolvedValue({ ok: true, watch: null, scan: deferredScan, warmup: warm });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(activity.activities.find((a) => a.kind === 'warmup')?.detail).toBe(
      'building tag relations',
    );

    // While warming the poll runs at the startup cadence, not the 5s one.
    await vi.advanceTimersByTimeAsync(2000);
    expect(activity.activities.find((a) => a.kind === 'warmup')?.detail).toBe(
      'warming tag completion',
    );

    await vi.advanceTimersByTimeAsync(2000);
    const prep = activity.activities.find((a) => a.kind === 'warmup');
    expect(prep?.status).toBe('success');
    expect(prep?.detail).toBe('ready');
  });

  it('shows no warmup job when the daemon is already warm', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health).mockResolvedValue({
      ok: true,
      watch: null,
      scan: idleScan,
      warmup: warm,
    });
    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(activity.activities.find((a) => a.kind === 'warmup')).toBeUndefined();
  });

  it('shows no warmup job when no warmup was spawned', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health).mockResolvedValue({
      ok: true,
      watch: null,
      scan: idleScan,
      warmup: { phase: 'idle', complete: true },
    });
    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(activity.activities.find((a) => a.kind === 'warmup')).toBeUndefined();
  });

  it('drops the queued flag and reports a rate once the scan actually runs', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: deferredScan,
        warmup: warming('graph'),
      })
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: { ...deferredScan, imported: 1000, roots_total: 1, current: 'D:/img/newstuff' },
        warmup: warm,
      })
      .mockResolvedValue({
        ok: true,
        watch: null,
        scan: { ...deferredScan, imported: 6000, roots_total: 1, current: 'D:/img/newstuff' },
        warmup: warm,
      });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(activity.activities.find((a) => a.kind === 'catchup-scan')?.queued).toBe(true);

    await vi.advanceTimersByTimeAsync(2000); // startup cadence: warmup still incomplete
    const started = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(started?.queued).toBe(false);
    expect(started?.detail).toContain('1,000 files');

    // Warmup is complete now, so the poll has backed off to the 5s cadence.
    await vi.advanceTimersByTimeAsync(5000);
    const running = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(running?.detail).toContain('6,000 files');
    expect(running?.detail).toMatch(/\d,?\d*\/s/); // 5,000 files in 5s → ~1,000/s
    expect(running?.detail).toContain('newstuff');
  });

  it('announces completion only when the startup was slow enough to notice', async () => {
    vi.useFakeTimers();
    const t0 = Date.now();
    // Warm for 11s — past the 10s announce threshold — then report a finished
    // scan. Driven off the clock rather than a fixed queue of responses so the
    // test asserts the threshold, not a particular number of polls.
    vi.mocked(api.health).mockImplementation(async () =>
      Date.now() - t0 < 11_000
        ? { ok: true, watch: null, scan: deferredScan, warmup: warming('graph') }
        : {
            ok: true,
            watch: null,
            scan: {
              running: false,
              imported: 96737,
              errors: 0,
              roots_total: 1,
              roots_done: 1,
              current: null,
              complete: true,
            },
            warmup: warm,
          },
    );

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0); // startup observed here
    await vi.advanceTimersByTimeAsync(12_000); // …and finishes past the threshold

    const done = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(done?.status).toBe('success');
    expect(done?.detail).toBe('96,737 files');
    expect(done?.announce).toBe(true);
    expect(screen.getByRole('status')).toBeTruthy(); // the toast is up
  });

  it('does not announce a startup that finished quickly', async () => {
    vi.useFakeTimers();
    const t0 = Date.now();
    vi.mocked(api.health).mockImplementation(async () =>
      Date.now() - t0 < 2000
        ? { ok: true, watch: null, scan: deferredScan, warmup: warming('graph') }
        : {
            ok: true,
            watch: null,
            scan: {
              running: false,
              imported: 40,
              errors: 0,
              roots_total: 1,
              roots_done: 1,
              current: null,
              complete: true,
            },
            warmup: warm,
          },
    );

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(4000);

    const done = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(done?.status).toBe('success');
    expect(done?.announce).toBe(false);
    expect(screen.queryByRole('status')).toBeNull(); // no toast on a quick start
  });

  it('shows no warmup job while the warmup is parked, then shows it when it starts', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      // Parked on the startup gate: incomplete, but nothing is being read.
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: deferredScan,
        warmup: { phase: 'queued', complete: false },
      })
      .mockResolvedValue({
        ok: true,
        watch: null,
        scan: deferredScan,
        warmup: warming('graph'),
      });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(activity.activities.find((a) => a.kind === 'warmup')).toBeUndefined();

    // Crucially the queued poll must not latch the job away for good.
    await vi.advanceTimersByTimeAsync(2000);
    const prep = activity.activities.find((a) => a.kind === 'warmup');
    expect(prep?.status).toBe('running');
    expect(prep?.detail).toBe('building tag relations');
  });

  it('does not call a started scan "waiting" just because nothing is indexed yet', async () => {
    vi.useFakeTimers();
    // The backstop-overlap case: warmup still incomplete, but the scan thread
    // has started — `roots_total` is set before the first file lands.
    vi.mocked(api.health).mockResolvedValue({
      ok: true,
      watch: null,
      scan: { ...deferredScan, roots_total: 1, current: 'D:/img/newstuff' },
      warmup: warming('completion'),
    });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);

    const scan = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(scan?.queued).toBe(false);
    expect(scan?.detail).not.toContain('waiting');
  });

  it('keeps a terminal-only announcement up long enough to read', async () => {
    vi.useFakeTimers();
    const t0 = Date.now();
    vi.mocked(api.health).mockImplementation(async () =>
      Date.now() - t0 < 11_000
        ? { ok: true, watch: null, scan: deferredScan, warmup: warming('graph') }
        : {
            ok: true,
            watch: null,
            scan: {
              running: false,
              imported: 96737,
              errors: 0,
              roots_total: 1,
              roots_done: 1,
              current: null,
              complete: true,
            },
            warmup: warm,
          },
    );

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(12_000); // completion observed, toast armed
    expect(screen.getByRole('status')).toBeTruthy();

    // The 2s pull-job grace would have dropped it by now; this toast is the
    // user's only signal that a three-minute startup finished.
    await vi.advanceTimersByTimeAsync(4000);
    expect(screen.getByRole('status')).toBeTruthy();

    // …but it does eventually go.
    await vi.advanceTimersByTimeAsync(6000);
    await tick();
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('still uses the short grace for a job that announced while running', async () => {
    vi.useFakeTimers();
    const job = activity.begin({ label: 'Syncing tags', kind: 'pull-tags', announce: true });
    render(ActivityIndicator);

    await vi.advanceTimersByTimeAsync(0);
    expect(screen.getByRole('status')).toBeTruthy();

    job.succeed({ detail: 'done' });
    await tick();
    await vi.advanceTimersByTimeAsync(2000);
    await tick();
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('always announces a scan that could not index everything', async () => {
    vi.useFakeTimers();
    vi.mocked(api.health)
      .mockResolvedValueOnce({
        ok: true,
        watch: null,
        scan: { ...deferredScan, imported: 10, roots_total: 1 },
        warmup: warm,
      })
      .mockResolvedValue({
        ok: true,
        watch: null,
        scan: {
          running: false,
          imported: 20,
          errors: 3,
          roots_total: 1,
          roots_done: 1,
          current: null,
          complete: true,
        },
        warmup: warm,
      });

    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(5000); // well under the announce threshold

    const job = activity.activities.find((a) => a.kind === 'catchup-scan');
    expect(job?.status).toBe('warning');
    expect(job?.announce).toBe(true);
  });

  // F1 (#228): announce:false on a pull failure must not resurrect an older
  // terminal announced toast whose grace had already expired.
  it('stale toast does not reappear when a newer pull sets announce:false', async () => {
    vi.useFakeTimers();

    // Step 1: an older job warns (terminal, announce:true). It was NOT running
    // while announced so it gets TERMINAL_ANNOUNCE_GRACE_MS (9 s).
    const oldJob = activity.begin({ label: 'Catch-up', kind: 'catchup-scan', announce: true });
    render(ActivityIndicator);
    await vi.advanceTimersByTimeAsync(0);
    expect(screen.getByRole('status')).toBeTruthy(); // toast visible

    // Let the 9 s grace expire so the old toast is gone.
    oldJob.warn('4 files could not be indexed');
    await tick();
    await vi.advanceTimersByTimeAsync(9000);
    await tick();
    expect(screen.queryByRole('status')).toBeNull(); // old toast gone

    // Step 2: a new pull job starts (announce:true → running → becomes candidate).
    const pullJob = activity.begin({ label: 'Pull tags', kind: 'pull-tags', announce: true });
    await tick();
    expect(screen.getByRole('status')).toBeTruthy(); // pull toast visible

    // Step 3: pull fails with announce:false, clearing it from the candidate set.
    pullJob.warn('repo failed', { announce: false });
    await tick();

    // The old catchup job should NOT re-show its toast — its grace already ran.
    expect(screen.queryByRole('status')).toBeNull();
  });
});
