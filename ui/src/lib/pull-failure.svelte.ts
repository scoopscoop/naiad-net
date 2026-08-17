/** Pull failure notices raised by the shared pull runner (#228). A failed pull
 *  against a configured repo is significant enough to interrupt for: toasts are
 *  transient and were being missed entirely. The runner raises here; App hosts
 *  the single PullFailureModal that renders `current`. */

/** Discriminated union: repo failures carry the failing repo names; a fatal
 *  stream failure is not attributable to a single repo, so it carries no repos. */
export type PullFailureNotice =
  | { kind: 'repo'; repos: string[]; message: string }
  | { kind: 'fatal'; message: string };

/** Create an isolated notice store. The app uses the `pullFailure` singleton;
 *  tests call this for a fresh instance. */
export function createPullFailure() {
  let current = $state<PullFailureNotice | null>(null);
  return {
    get current(): PullFailureNotice | null {
      return current;
    },
    /** Raise a notice, replacing any open one (#228 decision 5: latest wins). */
    raise(notice: PullFailureNotice): void {
      current = notice;
    },
    dismiss(): void {
      current = null;
    },
  };
}

/** App-wide singleton. */
export const pullFailure = createPullFailure();
