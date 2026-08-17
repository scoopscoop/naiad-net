/** Latest catch-up scan status observed by the daemon health poll. Written by
 *  ActivityIndicator (which owns the poll) and read by App.svelte to stream
 *  freshly-indexed files into an empty gallery while the scan runs (#119). A
 *  module-level rune store, like the daemon-liveness poll it rides on. */
import type { CatchupStatus } from './types';

let status = $state<CatchupStatus | null>(null);

export const catchup = {
  get status(): CatchupStatus | null {
    return status;
  },
  set(next: CatchupStatus | null): void {
    status = next;
  },
};
