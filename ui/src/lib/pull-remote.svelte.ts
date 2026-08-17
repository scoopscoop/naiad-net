/**
 * Shared pull-remote adapter — thin wrapper over the streamed pull runner so
 * Inspector and DetailView mirror the SAME activity job instead of each
 * issuing its own request (#106, issue #110 part 3).
 *
 * Origin-hash suppression contract: `originHash` is captured synchronously
 * before the stream begins, then re-read whenever a callback fires. If the
 * caller's "current file" has changed, the result flash and the refresh are
 * suppressed — they belong to the file the user has already navigated away
 * from. Passing `() => file?.hash ?? null` makes a null return the "no file
 * open" sentinel (mirrors the original `!file` early-exit guard).
 *
 * Pull failures (per-repo error, fatal stream failure) are reported through
 * `pull-failure.svelte.ts` (#228), not through `report`. `report` is retained
 * for the refresh failure in `onSettled`.
 */

import { listRepos } from './api';
import { runPullTags } from './pull-runner';

export interface PullRemoteOptions {
  /** Hashes a pull acts on right now; empty means the button does nothing. */
  targets: () => string[];
  /** Hash the result flash belongs to. If it no longer matches when the pull
   *  lands, the result and refresh are suppressed. */
  originHash: () => string | null;
  /** Re-fetch tags for the file currently on screen after a successful pull. */
  refresh: () => Promise<void>;
  /** Surface a refresh failure after a successful pull. Pull failures themselves
   *  are reported through the `pullFailure` store (#228) rather than here. */
  report: (e: unknown) => void;
}

/** Pull-remote state machine shared by Inspector and DetailView. */
export function createPullRemote(opts: PullRemoteOptions) {
  let repoCount = $state(0);
  let pulling = $state(false);
  let result = $state<string | null>(null);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let close: (() => void) | undefined;

  listRepos()
    .then((r) => (repoCount = r.length))
    .catch(() => (repoCount = 0));

  function run() {
    const origin = opts.originHash();
    const targets = opts.targets();
    if (origin === null || pulling || targets.length === 0) return;
    pulling = true;
    result = null;
    const fresh = () => opts.originHash() === origin;
    close = runPullTags({
      hashes: targets,
      onResult: (line) => {
        if (fresh()) result = line;
      },
      onSettled: (ok) => {
        pulling = false;
        close = undefined;
        if (ok && fresh()) void opts.refresh().catch((e) => opts.report(e));
        clearTimeout(timer);
        timer = setTimeout(() => (result = null), 2500);
      },
    });
  }

  function destroy() {
    clearTimeout(timer);
    // Aborting skips onSettled, so clear the in-flight flag here.
    close?.();
    pulling = false;
  }

  return {
    get repoCount() {
      return repoCount;
    },
    get pulling() {
      return pulling;
    },
    get result() {
      return result;
    },
    run,
    destroy,
  };
}
