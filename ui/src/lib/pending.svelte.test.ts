import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { createPending } from './pending.svelte';

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

describe('createPending', () => {
  it('stays quiet for operations under the 150ms delay', () => {
    const p = createPending();
    p.start();
    vi.advanceTimersByTime(149);
    p.end();
    vi.advanceTimersByTime(1000);
    expect(p.busy).toBe(false);
  });

  it('shows once the delay elapses', () => {
    const p = createPending();
    p.start();
    expect(p.busy).toBe(false);
    vi.advanceTimersByTime(150);
    expect(p.busy).toBe(true);
  });

  it('holds for 300ms after showing, even if the work ends immediately', () => {
    const p = createPending();
    p.start();
    vi.advanceTimersByTime(150);
    p.end();
    expect(p.busy).toBe(true);
    vi.advanceTimersByTime(299);
    expect(p.busy).toBe(true);
    vi.advanceTimersByTime(1);
    expect(p.busy).toBe(false);
  });

  it('refcounts overlapping operations', () => {
    const p = createPending();
    p.start();
    p.start();
    vi.advanceTimersByTime(150);
    p.end();
    vi.advanceTimersByTime(500);
    expect(p.busy).toBe(true);
    p.end();
    vi.advanceTimersByTime(500);
    expect(p.busy).toBe(false);
  });

  it('reset() clears the flag and any armed timer', () => {
    const p = createPending();
    p.start();
    p.reset();
    vi.advanceTimersByTime(1000);
    expect(p.busy).toBe(false);
  });

  it('reports every transition to onchange', () => {
    const seen: boolean[] = [];
    const p = createPending((busy) => seen.push(busy));
    p.start();
    vi.advanceTimersByTime(150);
    p.end();
    vi.advanceTimersByTime(300);
    expect(seen).toEqual([true, false]);
  });

  it('start() during the hold window cancels the pending hide', () => {
    const p = createPending();
    p.start();
    vi.advanceTimersByTime(150);   // delay fires, shown = true
    p.end();                       // schedules the hold timer
    p.start();                     // must cancel the hold, not schedule a new delay
    vi.advanceTimersByTime(350);   // the hold window would have expired by now
    expect(p.busy).toBe(true);     // still shown: a new operation is in flight
    p.end();
    vi.advanceTimersByTime(300);
    expect(p.busy).toBe(false);
  });

  it('end() on an idle instance is a no-op', () => {
    const p = createPending();
    p.end(); // underflow guard: inFlight is already 0
    vi.advanceTimersByTime(1000);
    expect(p.busy).toBe(false);
  });

  it('reset() mid-hold clears busy immediately and keeps it clear', () => {
    const p = createPending();
    p.start();
    vi.advanceTimersByTime(150);   // spinner shown
    p.end();                       // hold timer armed
    p.reset();                     // must clear busy immediately
    expect(p.busy).toBe(false);
    vi.advanceTimersByTime(1000);  // hold timer would have fired here
    expect(p.busy).toBe(false);
  });
});
