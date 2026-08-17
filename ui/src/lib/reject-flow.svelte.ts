/**
 * Shared reject-flow logic — extracted from Inspector and DetailView (#74, #92-UI).
 *
 * One instance per component. Owns: flash state, auto-dismiss timer, report-offer
 * state, and the Esc keyboard handler. The host component keeps: inflight map
 * (serialises add/remove/rate/reject mutations), busy guard, begin/end helpers,
 * and calls opts.refresh() + opts.onRejectedChanged() via the injected callbacks.
 */

import { rejectTag, undoReject } from './api';
import type { RejectResponse } from './types';

/** Single source of truth for the flash animation duration. Consumed by
 *  createRejectFlow (timer) and by RejectFlash.svelte (CSS custom property). */
export const FLASH_MS = 2500;

interface RejectFlowOpts {
  /** Re-fetch tags for the current file. Called after every mutating operation. */
  refresh: () => Promise<void>;
  /** Bump the host's rejectedSectionTick so RejectedSection re-fetches. */
  onRejectedChanged: () => void;
}

export function createRejectFlow(opts: RejectFlowOpts) {
  /** Transient flash shown after a successful reject. Cleared by Esc, Undo, or
   *  auto-dismiss at FLASH_MS. Null when no recent reject. */
  let flash = $state<{ tag: string; services: string[]; hash: string } | null>(null);
  /** When set, the report modal is shown for this offer. Clearing without Undo
   *  leaves the rejection in place — cancelling a report MUST NOT undo the reject. */
  let reportOffer = $state<{ repo: string; tag: string; hash: string } | null>(null);
  let timerId: ReturnType<typeof setTimeout> | null = null;
  /** True for 4 s after a report is successfully sent — drives the "Report sent" notice. */
  let reportSent = $state(false);
  let reportSentTimer: ReturnType<typeof setTimeout> | null = null;

  function clearFlash() {
    if (timerId !== null) {
      clearTimeout(timerId);
      timerId = null;
    }
    flash = null;
  }

  function dismissOffer() {
    reportOffer = null;
  }

  /**
   * Execute a reject for tag across all services, then refresh + show the Undo
   * flash. If exactly one service responds with reports=true, also open the
   * report-offer modal.
   *
   * The caller is responsible for: busy guard, begin(hash, tag), error handling,
   * and end(hash). This function is called from inside that try block.
   */
  async function reject(tag: string, services: string[], hash: string): Promise<void> {
    const resps: RejectResponse[] = [];
    for (const s of services) {
      resps.push(await rejectTag(hash, tag, s));
    }
    await opts.refresh();
    opts.onRejectedChanged();
    clearFlash();
    flash = { tag, services, hash };
    timerId = setTimeout(clearFlash, FLASH_MS);
    if (services.length === 1 && resps[0].reports) {
      reportOffer = { repo: services[0], tag, hash };
    }
  }

  /**
   * Undo the most recent reject: calls undoReject per service then refreshes.
   * Clears both flash and reportOffer synchronously before any awaits so the
   * modal cannot be filed after an undo that is already in flight (#86 fix).
   */
  async function undoFlash(): Promise<void> {
    const fl = flash;
    if (!fl) return;
    clearFlash();
    reportOffer = null;
    for (const s of fl.services) {
      await undoReject(fl.hash, fl.tag, s);
    }
    await opts.refresh();
    opts.onRejectedChanged();
  }

  /**
   * Restore a rejection from the RejectedSection — follows the remove()
   * serialisation contract (caller handles begin/end + error handling).
   */
  async function restore(tag: string, service: string, hash: string): Promise<void> {
    await undoReject(hash, tag, service);
    await opts.refresh();
    opts.onRejectedChanged();
  }

  /**
   * Attach a window keydown listener that dismisses the flash on Escape,
   * UNLESS the report-offer modal is open (it owns Escape in that state).
   * Returns a cleanup function — call it from the $effect's return value.
   */
  function attachEsc(): () => void {
    function onEsc(e: KeyboardEvent) {
      if (e.key === 'Escape' && !reportOffer) {
        e.preventDefault();
        clearFlash();
      }
    }
    window.addEventListener('keydown', onEsc);
    return () => window.removeEventListener('keydown', onEsc);
  }

  /**
   * Show the "Report sent" notice for 4 s, then auto-clear. Replaces the
   * duplicated `showReportNotice` block that used to live in Inspector and
   * DetailView — absorbed here so both components share one implementation.
   */
  function notifyReportSent(): void {
    if (reportSentTimer !== null) clearTimeout(reportSentTimer);
    reportSent = true;
    reportSentTimer = setTimeout(() => { reportSent = false; reportSentTimer = null; }, 4000);
  }

  /**
   * Cancel any pending auto-dismiss timers. Call from onDestroy to prevent
   * timers from firing against a destroyed component's $state.
   */
  function destroy(): void {
    if (timerId !== null) {
      clearTimeout(timerId);
      timerId = null;
    }
    if (reportSentTimer !== null) {
      clearTimeout(reportSentTimer);
      reportSentTimer = null;
    }
  }

  return {
    get flash() { return flash; },
    get reportOffer() { return reportOffer; },
    get reportSent() { return reportSent; },
    reject,
    undoFlash,
    restore,
    clearFlash,
    dismissOffer,
    notifyReportSent,
    attachEsc,
    destroy,
  };
}
