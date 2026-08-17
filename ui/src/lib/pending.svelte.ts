/** A busy flag with hysteresis.
 *
 *  Two thresholds, both about human perception rather than correctness:
 *  a spinner that appears for a 40ms search reads as jitter, and a spinner
 *  that appears at 149ms and vanishes at 160ms reads as a glitch. So `busy`
 *  turns true only after DELAY_MS in flight, and once true it stays true for
 *  at least HOLD_MS.
 *
 *  `start()`/`end()` are refcounted, so a surface that fires two overlapping
 *  requests (search + a tag refresh) stays busy until both land.
 *
 *  Components read `busy` directly ($state). Callers that live outside a
 *  component — a per-tab map, say — pass `onchange` and mirror the flag into
 *  their own reactive store. */

const DELAY_MS = 150;
const HOLD_MS = 300;

export interface Pending {
  readonly busy: boolean;
  start(): void;
  end(): void;
  /** Cancels all in-flight bookkeeping. Safe on idle instances, and during
   *  teardown. If `onchange` is set, it will fire with `false` when this is
   *  called with `busy === true` — callers that mirror the flag into a map
   *  entry being discarded should null-guard their handler. */
  reset(): void;
}

export function createPending(onchange?: (busy: boolean) => void): Pending {
  let inFlight = 0;
  let shown = $state(false);
  let shownAt = 0;
  let delayTimer: ReturnType<typeof setTimeout> | null = null;
  let holdTimer: ReturnType<typeof setTimeout> | null = null;

  function clearTimers() {
    if (delayTimer !== null) clearTimeout(delayTimer);
    if (holdTimer !== null) clearTimeout(holdTimer);
    delayTimer = holdTimer = null;
  }

  function setShown(next: boolean) {
    if (shown === next) return;
    shown = next;
    onchange?.(next);
  }

  function show() {
    delayTimer = null;
    shownAt = Date.now();
    setShown(true);
  }

  /** Called when the last in-flight operation ends. Hides now if the spinner
   *  never appeared or has already served its minimum, else schedules it. */
  function settle() {
    if (!shown) {
      clearTimers();
      return;
    }
    const remaining = HOLD_MS - (Date.now() - shownAt);
    if (remaining <= 0) {
      setShown(false);
      return;
    }
    holdTimer = setTimeout(() => {
      holdTimer = null;
      // A new operation may have started during the hold; only hide if idle.
      if (inFlight === 0) setShown(false);
    }, remaining);
  }

  return {
    get busy() {
      return shown;
    },
    start() {
      inFlight += 1;
      if (holdTimer !== null) {
        // Re-armed during the hold window: keep the spinner up, drop the hide.
        clearTimeout(holdTimer);
        holdTimer = null;
      }
      if (inFlight === 1 && !shown && delayTimer === null) {
        delayTimer = setTimeout(show, DELAY_MS);
      }
    },
    end() {
      if (inFlight === 0) return;
      inFlight -= 1;
      if (inFlight > 0) return;
      if (delayTimer !== null) {
        clearTimeout(delayTimer);
        delayTimer = null;
      }
      settle();
    },
    reset() {
      inFlight = 0;
      clearTimers();
      setShown(false);
    },
  };
}
