/** Shared tag-pull runner: every pull entry point (grid bulk, Inspector,
 *  DetailView) goes through here so a pull always drives ONE announcing
 *  activity job. On success or advisory (sha256 skips, floor clamp) the toast
 *  runs as before; on failure, the toast is suppressed and a blocking modal
 *  notice is raised instead (#228) — see pull-failure.svelte.ts. */

import { activity } from './activity.svelte';
import { pullFailure } from './pull-failure.svelte';
import { pullFileTagsStream } from './api';
import type { PullRepoOutcome, PullStage } from './types';

export interface PullRunOptions {
  /** Files to pull tags for. */
  hashes: string[];
  /** A short result line ('pulled 3 files, 8 mappings' / 'no new tags'). */
  onResult?: (line: string) => void;
  /** Fires once the pull settles; `ok` is false only on a fatal failure. */
  onSettled?: (ok: boolean) => void;
}

/** Files the daemon never asked any repo about, for want of a SHA-256 interop
 *  hash. Every SHA-256 repo reports the *same* un-resolvable files, so the
 *  maximum is the count of distinct affected files; summing would multiply one
 *  problem by the number of repos. */
function maxMissingSha256(results: PullRepoOutcome[]): number {
  // Coalesce missing_sha256 with ?? 0: the field is optional on the wire
  // (older daemon builds omit it), and Math.max(0, undefined) is NaN.
  return results.reduce((most, r) => Math.max(most, r.missing_sha256 ?? 0), 0);
}

/** Format the terminal result line from a summary. Skipped files are named
 *  explicitly: without that, "no new tags" is indistinguishable from "we never
 *  asked upstream about this file" (#144). */
function resultLine(matchedFiles: number, mappings: number, missingSha256: number): string {
  const files = `${matchedFiles} file${matchedFiles === 1 ? '' : 's'}`;
  const maps = `${mappings} mapping${mappings === 1 ? '' : 's'}`;
  const head = mappings <= 0 ? 'no new tags' : `pulled ${files}, ${maps}`;
  if (missingSha256 <= 0) return head;
  const skipped = `${missingSha256} file${missingSha256 === 1 ? '' : 's'}`;
  // "sha256 unavailable" covers all three root causes (NULL, unparseable stored
  // value, hash not in library). "rescan to fix" was only true for the NULL
  // case; a corrupt or absent sha256 is never repaired by a library rescan.
  return `${head} — ${skipped} skipped (sha256 unavailable)`;
}

/** Decimal (1000-based) byte formatting for the live pull label. Pinned here so
 *  the runner test can assert exact strings ("842 KB", "1.2 MB"). */
function humanBytes(n: number): string {
  if (n < 1000) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1000;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** Start one streamed pull. Returns a function that aborts it. */
export function runPullTags(opts: PullRunOptions): () => void {
  const job = activity.begin({ label: 'Pull tags', kind: 'pull-tags', announce: true });
  const SUB = 1000;
  let lastFrac = 0;
  const close = pullFileTagsStream(opts.hashes, {
    onConnecting: (c) => {
      lastFrac = 0; // new repo slice
      job.progress({
        detail: `pulling ${c.repo} (${c.index}/${c.total})`,
        done: (c.index - 1) * SUB,
        total: c.total * SUB,
      });
    },
    onStage: (s) => {
      // Subdivide THIS repo's slice by chunk fraction. `merging` holds the last
      // chunk fraction (network done, merge running); `done` fills the slice.
      // A dual-domain leg reset rewinds lastFrac within the slice (accepted:
      // §4.6) but the byte figure is monotonic (daemon never resets it).
      if (s.chunk_total > 0) lastFrac = s.chunk / s.chunk_total;
      const frac = s.phase === 'done' ? 1 : lastFrac;
      // `hashes` is additive (#174): only append " · N files" when non-zero.
      // Omitting it when zero avoids "· 0 files" noise on old-daemon responses.
      const filesLabel = (s.hashes ?? 0) > 0
        ? ` · ${s.hashes!.toLocaleString('en-US')} files`
        : '';
      job.progress({
        detail: `pulling ${s.repo} (${s.index}/${s.total}) · ${humanBytes(s.bytes)}${filesLabel}`,
        done: (s.index - 1) * SUB + Math.round(frac * SUB),
        total: s.total * SUB,
      });
    },
    onProgress: (p) => {
      // Fires once a repo has finished; snap the slice full and report totals.
      job.progress({
        detail: `pulled ${p.mappings} mappings (${p.repos_done}/${p.repos_total} repos)`,
        done: p.repos_done * SUB,
        total: p.repos_total * SUB,
      });
    },
    onSummary: (s) => {
      const missing = maxMissingSha256(s.results);
      const line = resultLine(s.matched_files, s.mappings, missing);
      const failed = s.results.filter((r) => r.error);
      // Track whether a failure/missing warn already settled the job so the
      // §179 notice loop does not clobber it — job.warn() REPLACES the message.
      let warnSettled = false;
      if (failed.length > 0) {
        const names = failed.map((r) => r.repo).join(', ');
        // #228: the failure is raised as a modal notice instead of a toast.
        // `announce: false` clears the flag set at begin() so the transient
        // toast does not double-report — the panel row still keeps the full
        // message as history. The daemon's error string is used as the panel
        // detail fallback (failed[0].error ?? line) because it names the actual
        // cause (timeout, #169 privacy-ceiling hint, etc.) where the generic
        // result line ("no new tags") would hide it. The modal body uses its own
        // fallback (r.error ?? 'unknown error') so it never reads as a result line.
        job.warn(`${names} failed`, { detail: failed[0].error ?? line, announce: false });
        warnSettled = true;
        // Single-repo failure: the modal body already names the repo in its
        // header, so the error block only shows the bare daemon string.
        // Multi-repo: prefix each error with its repo name for legibility.
        const failureMessage =
          failed.length === 1
            ? (failed[0].error ?? 'unknown error')
            : failed.map((r) => `${r.repo}: ${r.error ?? 'unknown error'}`).join('\n');
        pullFailure.raise({ kind: 'repo', repos: failed.map((r) => r.repo), message: failureMessage });
      } else if (missing > 0) {
        // Not an error — everything that could be pulled was pulled — but a
        // plain success would let the skipped files pass for "upstream has no
        // tags". Settle as a warning so the real cause is visible in the
        // activity panel. The warning does not promise a rescan will help:
        // there are three root causes and only one of them is rescan-fixable.
        job.warn(`${missing} file${missing === 1 ? '' : 's'} skipped (sha256 unavailable)`, { detail: line });
        warnSettled = true;
      } else {
        job.succeed({ detail: line });
      }
      // §7.3 / #179: surface any floor clamp-up advisory as a non-fatal toast,
      // but only when no failure/missing warn already settled the job. The
      // warn-once dedup happens server-side, so a notice appears at most once
      // per repo+domain per session. The pull succeeded; this is advisory only.
      if (!warnSettled) {
        for (const r of s.results) {
          if (r.notice) {
            job.warn(r.notice, { detail: line });
            break; // server-side dedup guarantees at most one notice per session
          }
        }
      }
      // #228: on a per-repo failure the modal already names the cause; calling
      // onResult would flash "no new tags" in Inspector/DetailView beside a modal
      // that says the pull failed. Keep onSettled — partial results were applied.
      if (failed.length === 0) opts.onResult?.(line);
      opts.onSettled?.(true); // partial (warn) still applied its mappings
    },
    onError: (message) => {
      // #228: fatal stream failure is announced as a modal notice; `announce: false`
      // suppresses the transient toast while the panel row retains the error as history.
      job.fail(message, { announce: false });
      pullFailure.raise({ kind: 'fatal', message });
      opts.onSettled?.(false);
    },
  });
  // The stream's close sets `settled = true` before aborting, so the stream's
  // error path early-returns and onError (→ job.fail) never fires. An aborted
  // job would therefore remain 'running' forever. Dismiss instead — an
  // intentional abort is not an error from the user's perspective.
  return () => {
    close();
    if (activity.byId(job.id)?.status === 'running') {
      activity.dismiss(job.id);
    }
  };
}
