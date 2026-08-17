/** Tests for runPullTags abort / settle semantics. */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { activity } from './activity.svelte';
import { pullFailure } from './pull-failure.svelte';
import { runPullTags } from './pull-runner';
import type { PullStreamHandlers } from './api';
import type { PullRepoOutcome } from './types';

vi.mock('./api', () => ({ pullFileTagsStream: vi.fn() }));
import * as api from './api';

// Shared cleanup: the tests use the global activity singleton; dismiss
// everything between runs so one test's leftovers cannot bleed into another.
// Also clear any open pull-failure notice (#228) so notices cannot bleed.
afterEach(() => {
  for (const a of [...activity.activities]) activity.dismiss(a.id);
  pullFailure.dismiss();
  vi.restoreAllMocks();
});

describe('runPullTags abort', () => {
  it('abort mid-stream dismisses the running pull-tags job so the indicator clears', () => {
    // pullFileTagsStream returns a close function; we never call any handler,
    // simulating a mid-stream abort before any terminal event arrives.
    const closeMock = vi.fn();
    vi.mocked(api.pullFileTagsStream).mockReturnValue(closeMock);

    const abort = runPullTags({ hashes: ['aabbccdd'] });

    // Job must be running before the abort.
    expect(activity.activities.find((a) => a.kind === 'pull-tags')?.status).toBe('running');

    abort();

    // No running pull-tags activity may remain after an intentional abort.
    expect(activity.activities.find((a) => a.kind === 'pull-tags')).toBeUndefined();
    // The underlying stream was closed.
    expect(closeMock).toHaveBeenCalledTimes(1);
  });

  it('calling the abort handle after summary leaves the terminal job intact', () => {
    // Capture the handlers so the test can deliver the onSummary terminal event.
    let captured: PullStreamHandlers | undefined;
    const closeMock = vi.fn();
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      captured = handlers;
      return closeMock;
    });

    const abort = runPullTags({ hashes: ['aabbccdd'] });
    expect(captured).toBeDefined();

    // Deliver the summary: job transitions to 'success' (or 'warning').
    captured!.onSummary({ matched_files: 1, mappings: 2, results: [] });
    const jobAfterSummary = activity.activities.find((a) => a.kind === 'pull-tags');
    expect(jobAfterSummary?.status).toBe('success');

    // Calling the abort handle now (e.g. component teardown racing with summary)
    // must NOT dismiss the terminal job — it should stay for the toast/panel.
    abort();
    expect(activity.activities.find((a) => a.kind === 'pull-tags')).not.toBeUndefined();
  });
});

describe('runPullTags missing_sha256 surfacing (#144)', () => {
  /** Run a pull and deliver one summary; returns the result line and the job. */
  function pullWithSummary(results: PullRepoOutcome[], matched = 0, mappings = 0) {
    let captured: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      captured = handlers;
      return vi.fn();
    });
    let line: string | undefined;
    runPullTags({ hashes: ['aabbccdd'], onResult: (l) => (line = l) });
    captured!.onSummary({ matched_files: matched, mappings, results });
    return { line, job: activity.activities.find((a) => a.kind === 'pull-tags') };
  }

  it('settles as a warning naming the skipped files, not a bare success', () => {
    const { line, job } = pullWithSummary(
      [{ repo: 'r', matched_files: 0, mappings: 0, missing_sha256: 2 }],
      0,
      0,
    );
    // 'no new tags' alone would read as "upstream has nothing" — the whole bug.
    expect(line).toContain('2 files skipped');
    // Must name the cause without promising a specific remedy: there are three
    // root causes (NULL sha256, corrupt stored value, hash not in library) and
    // only one is repaired by a rescan.
    expect(line).toContain('sha256 unavailable');
    expect(job?.status).toBe('warning');
  });

  it('takes the max across repos, never the sum — the same files are missing everywhere', () => {
    const { line } = pullWithSummary(
      [
        { repo: 'a', matched_files: 0, mappings: 1, missing_sha256: 3 },
        { repo: 'b', matched_files: 0, mappings: 1, missing_sha256: 3 },
      ],
      1,
      2,
    );
    expect(line).toContain('3 files skipped');
    expect(line).not.toContain('6 files');
  });

  it('a repo failure still wins the job message; onResult is NOT called (#228 F5)', () => {
    const { line, job } = pullWithSummary(
      [
        { repo: 'a', matched_files: 0, mappings: 0, missing_sha256: 0, error: 'unreachable' },
        { repo: 'b', matched_files: 1, mappings: 4, missing_sha256: 1 },
      ],
      1,
      4,
    );
    expect(job?.message).toContain('a failed');
    // onResult must NOT fire when there are failed repos — Inspector/DetailView
    // would otherwise flash "no new tags" beside the modal (#228 F5).
    expect(line).toBeUndefined();
  });

  it('says nothing extra when every file resolved', () => {
    const { line, job } = pullWithSummary(
      [{ repo: 'r', matched_files: 1, mappings: 3, missing_sha256: 0 }],
      1,
      3,
    );
    expect(line).toBe('pulled 1 file, 3 mappings');
    expect(job?.status).toBe('success');
  });
});

describe('runPullTags sub-repo bar (#172)', () => {
  it('single-domain bar is monotonic, snaps at progress, hits 100% at summary', () => {
    let h: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      h = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['a'] });
    const job = () => activity.activities.find((a) => a.kind === 'pull-tags')!;
    const pct = () => Math.round(((job().done ?? 0) / (job().total ?? 1)) * 100);

    h!.onConnecting!({ repo: 'r1', index: 1, total: 1 });
    let last = pct();
    for (const [chunk, bytes] of [[1, 100_000], [2, 250_000], [3, 842_000]] as const) {
      h!.onStage!({ repo: 'r1', index: 1, total: 1, phase: 'chunk', chunk, chunk_total: 3, bytes });
      expect(pct()).toBeGreaterThanOrEqual(last);
      last = pct();
    }
    // Label shows humanBytes of the last stage.
    expect(job().detail).toBe('pulling r1 (1/1) · 842 KB');
    h!.onProgress!({ repos_done: 1, repos_total: 1, repo: 'r1', matched_files: 1, mappings: 2 });
    expect(pct()).toBe(100);
    h!.onSummary!({ results: [], matched_files: 1, mappings: 2 });
    expect(pct()).toBe(100);
  });

  it('dual-domain leg reset never lowers the byte label', () => {
    let h: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      h = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['a'] });
    const job = () => activity.activities.find((a) => a.kind === 'pull-tags')!;
    const bytesOf = () => {
      const m = /· ([\d.]+) (B|KB|MB)/.exec(job().detail);
      if (!m) return 0;
      const n = Number(m[1]);
      const u = m[2];
      return u === 'MB' ? n * 1_000_000 : u === 'KB' ? n * 1_000 : n;
    };

    h!.onConnecting!({ repo: 'r1', index: 1, total: 1 });
    h!.onStage!({ repo: 'r1', index: 1, total: 1, phase: 'chunk', chunk: 2, chunk_total: 2, bytes: 500_000, domain: 'blake3' });
    const b1 = bytesOf();
    // sha256 leg: chunk index RESETS to 1 but cumulative bytes keep climbing.
    h!.onStage!({ repo: 'r1', index: 1, total: 1, phase: 'chunk', chunk: 1, chunk_total: 3, bytes: 700_000, domain: 'sha256' });
    expect(bytesOf()).toBeGreaterThanOrEqual(b1);
  });
});

describe('runPullTags enriched stage label (#174)', () => {
  beforeEach(() => {
    for (const a of [...activity.activities]) activity.dismiss(a.id);
  });

  it('enriched stage shows bytes + file count in pinned format', () => {
    let h: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      h = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['a'] });
    const job = () => activity.activities.find((a) => a.kind === 'pull-tags')!;
    const pct = () => Math.round(((job().done ?? 0) / (job().total ?? 1)) * 100);

    h!.onConnecting!({ repo: 'artstation', index: 1, total: 3 });
    let last = pct();
    // Enriched stages — job/total monotonic, label matches pinned format.
    h!.onStage!({ repo: 'artstation', index: 1, total: 3, phase: 'chunk', chunk: 1, chunk_total: 3, bytes: 1_200_000, hashes: 3410 });
    expect(pct()).toBeGreaterThanOrEqual(last);
    last = pct();
    expect(job().detail).toBe('pulling artstation (1/3) · 1.2 MB · 3,410 files');

    h!.onStage!({ repo: 'artstation', index: 1, total: 3, phase: 'chunk', chunk: 2, chunk_total: 3, bytes: 2_400_000, hashes: 6820 });
    expect(pct()).toBeGreaterThanOrEqual(last);
    last = pct();
    expect(job().detail).toBe('pulling artstation (1/3) · 2.4 MB · 6,820 files');

    h!.onProgress!({ repos_done: 1, repos_total: 3, repo: 'artstation', matched_files: 6820, mappings: 10 });
    expect(pct()).toBeGreaterThanOrEqual(last);
    h!.onSummary!({ results: [], matched_files: 6820, mappings: 10 });
  });

  it('dual-domain leg reset never lowers bytes or file count in the label', () => {
    let h: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      h = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['a'] });
    const job = () => activity.activities.find((a) => a.kind === 'pull-tags')!;
    const filesOf = () => {
      const m = /· ([\d,]+) files/.exec(job().detail);
      return m ? Number(m[1].replace(/,/g, '')) : 0;
    };
    const bytesOf = () => {
      const m = /· ([\d.]+) (B|KB|MB)/.exec(job().detail);
      if (!m) return 0;
      const n = Number(m[1]);
      const u = m[2];
      return u === 'MB' ? n * 1_000_000 : u === 'KB' ? n * 1_000 : n;
    };

    h!.onConnecting!({ repo: 'r1', index: 1, total: 1 });
    // blake3 leg: chunk 2/2, 500 KB, 2000 files.
    h!.onStage!({ repo: 'r1', index: 1, total: 1, phase: 'chunk', chunk: 2, chunk_total: 2, bytes: 500_000, domain: 'blake3', hashes: 2000 });
    const b1 = bytesOf();
    const f1 = filesOf();
    expect(b1).toBeGreaterThan(0);
    expect(f1).toBe(2000);

    // sha256 leg: chunk index RESETS to 1/3, but cumulative bytes and hashes keep climbing.
    h!.onStage!({ repo: 'r1', index: 1, total: 1, phase: 'chunk', chunk: 1, chunk_total: 3, bytes: 700_000, domain: 'sha256', hashes: 3000 });
    expect(bytesOf()).toBeGreaterThanOrEqual(b1);
    expect(filesOf()).toBeGreaterThanOrEqual(f1);
  });

  it('degrade: stage with hashes 0 renders bytes-only label (no "· 0 files" noise)', () => {
    let h: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      h = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['a'] });
    const job = () => activity.activities.find((a) => a.kind === 'pull-tags')!;

    h!.onConnecting!({ repo: 'r1', index: 1, total: 1 });
    h!.onStage!({ repo: 'r1', index: 1, total: 1, phase: 'chunk', chunk: 1, chunk_total: 2, bytes: 842_000, hashes: 0 });
    expect(job().detail).toBe('pulling r1 (1/1) · 842 KB');
    expect(job().detail).not.toContain('files');
  });

  it('degrade: stage with hashes absent (old daemon) renders bytes-only label', () => {
    let h: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      h = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['a'] });
    const job = () => activity.activities.find((a) => a.kind === 'pull-tags')!;

    h!.onConnecting!({ repo: 'r1', index: 1, total: 1 });
    // No hashes field — old-daemon compat.
    h!.onStage!({ repo: 'r1', index: 1, total: 1, phase: 'chunk', chunk: 1, chunk_total: 2, bytes: 842_000 });
    expect(job().detail).toBe('pulling r1 (1/1) · 842 KB');
    expect(job().detail).not.toContain('files');
  });
});

describe('runPullTags floor clamp-up notice (#179)', () => {
  /** Run a pull and deliver one summary; returns the job. */
  function pullWithSummary(results: PullRepoOutcome[], matched = 0, mappings = 0) {
    let captured: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      captured = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['aabbccdd'] });
    captured!.onSummary({ matched_files: matched, mappings, results });
    return activity.activities.find((a) => a.kind === 'pull-tags');
  }

  it('raises a non-fatal warn toast when a result carries a notice', () => {
    const results: PullRepoOutcome[] = [
      {
        repo: 'ptr',
        matched_files: 1,
        mappings: 5,
        missing_sha256: 0,
        notice: 'repo ptr: privacy ceiling (12) below floor (16); querying at 16 bits',
      },
    ];
    const job = pullWithSummary(results, 1, 5);
    // The pull itself succeeded (no error), so the job should not be failed.
    expect(job?.status).not.toBe('failed');
    // A warn call should have been made naming the notice.
    expect(job?.message).toContain('repo ptr: privacy ceiling');
  });

  it('raises no notice toast when notice is absent', () => {
    const results: PullRepoOutcome[] = [
      { repo: 'ptr', matched_files: 1, mappings: 3, missing_sha256: 0 },
    ];
    const job = pullWithSummary(results, 1, 3);
    expect(job?.status).toBe('success');
    expect(job?.message ?? '').not.toContain('ceiling');
  });

  it('settles ok=true even with a notice (advisory, not fatal)', () => {
    let settled: boolean | undefined;
    let captured: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      captured = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['aabbccdd'], onSettled: (ok) => { settled = ok; } });
    captured!.onSummary({
      matched_files: 2,
      mappings: 4,
      results: [
        {
          repo: 'ptr',
          matched_files: 1,
          mappings: 4,
          missing_sha256: 0,
          notice: 'floor clamp notice',
        },
      ],
    });
    expect(settled).toBe(true);
  });

  it('failure toast survives when another repo carries a notice (no clobber)', () => {
    // Repo A failed; Repo B succeeded but carries a notice. The failure warn
    // must remain on the job — the notice must not overwrite it.
    const results: PullRepoOutcome[] = [
      { repo: 'a', matched_files: 0, mappings: 0, missing_sha256: 0, error: 'timeout' },
      { repo: 'b', matched_files: 1, mappings: 3, missing_sha256: 0, notice: 'floor clamp notice from b' },
    ];
    const job = pullWithSummary(results, 0, 3);
    // Job must be in warning state (not error — a per-repo failure is a warn).
    expect(job?.status).toBe('warning');
    // Message must name the failed repo.
    expect(job?.message).toContain('a failed');
    // The notice must NOT have clobbered the failure message.
    expect(job?.message).not.toContain('floor clamp notice');
  });

  it('missing-files warn survives alongside a notice (no clobber)', () => {
    // One repo reports skipped files; another carries a floor-clamp notice.
    // The missing-files warn must remain — the notice must not clobber it.
    const results: PullRepoOutcome[] = [
      { repo: 'a', matched_files: 0, mappings: 0, missing_sha256: 2 },
      { repo: 'b', matched_files: 1, mappings: 1, missing_sha256: 0, notice: 'floor clamp notice from b' },
    ];
    const job = pullWithSummary(results, 1, 1);
    expect(job?.status).toBe('warning');
    expect(job?.message).toContain('skipped');
    expect(job?.message).not.toContain('floor clamp');
  });
});

describe('runPullTags absent missing_sha256 (#156)', () => {
  /** Run a pull and deliver one summary; returns the result line and the job. */
  function pullWithSummary(results: PullRepoOutcome[], matched = 0, mappings = 0) {
    let captured: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      captured = handlers;
      return vi.fn();
    });
    let line: string | undefined;
    runPullTags({ hashes: ['aabbccdd'], onResult: (l) => (line = l) });
    captured!.onSummary({ matched_files: matched, mappings, results });
    return { line, job: activity.activities.find((a) => a.kind === 'pull-tags') };
  }

  it('absent field yields 0, prints no NaN, and settles as success', () => {
    // Simulate a daemon build that omits missing_sha256 entirely.
    // Math.max(0, undefined) = NaN would make the summary branch fall through
    // to job.succeed while also printing "NaN files skipped" — both wrong.
    const result: PullRepoOutcome = { repo: 'r', matched_files: 1, mappings: 3 };
    const { line, job } = pullWithSummary([result], 1, 3);
    expect(line).not.toContain('NaN');
    expect(line).toBe('pulled 1 file, 3 mappings');
    expect(job?.status).toBe('success');
  });

  it('absent field on every repo in a multi-repo pull does not warn', () => {
    const results: PullRepoOutcome[] = [
      { repo: 'a', matched_files: 1, mappings: 2 },
      { repo: 'b', matched_files: 1, mappings: 1 },
    ];
    const { line, job } = pullWithSummary(results, 1, 3);
    expect(line).not.toContain('NaN');
    expect(job?.status).toBe('success');
  });
});

describe('runPullTags failure modal (#228)', () => {
  /** Run a pull and deliver one summary; returns the job from the activity store. */
  function pullWithSummary(results: PullRepoOutcome[], matched = 0, mappings = 0) {
    let captured: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      captured = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['aabbccdd'] });
    captured!.onSummary({ matched_files: matched, mappings, results });
    return activity.activities.find((a) => a.kind === 'pull-tags');
  }

  it('per-repo failure raises a repo notice with the failing repo and daemon error', () => {
    pullWithSummary([
      { repo: 'ptr', matched_files: 0, mappings: 0, error: 'connection refused' },
    ]);
    expect(pullFailure.current).not.toBeNull();
    expect(pullFailure.current?.kind).toBe('repo');
    // Use toMatchObject to narrow the union (repos exists only on the repo branch).
    expect(pullFailure.current).toMatchObject({ repos: expect.arrayContaining(['ptr']) });
    expect(pullFailure.current?.message).toContain('connection refused');
  });

  it('two failing repos — both names in repos, both errors in message', () => {
    pullWithSummary([
      { repo: 'a', matched_files: 0, mappings: 0, error: 'timeout' },
      { repo: 'b', matched_files: 0, mappings: 0, error: 'auth expired' },
    ]);
    expect(pullFailure.current).toMatchObject({ repos: ['a', 'b'] });
    expect(pullFailure.current?.message).toContain('timeout');
    expect(pullFailure.current?.message).toContain('auth expired');
  });

  it('failure job.warn clears announce flag (toast suppressed, panel row kept)', () => {
    const job = pullWithSummary([
      { repo: 'a', matched_files: 0, mappings: 0, error: 'unreachable' },
    ]);
    // Panel history preserved — message still names the failed repo.
    expect(job?.message).toContain('a failed');
    expect(job?.status).toBe('warning');
    // announce: false suppresses the transient toast while keeping the panel row.
    expect(job?.announce).toBe(false);
  });

  it('fatal onError raises kind fatal (no repos field) with the fatal message', () => {
    let captured: PullStreamHandlers | undefined;
    vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
      captured = handlers;
      return vi.fn();
    });
    runPullTags({ hashes: ['aabbccdd'] });
    captured!.onError!('daemon disconnected');
    expect(pullFailure.current?.kind).toBe('fatal');
    // Fatal notices carry no repos (discriminated union — fatal is not repo-specific).
    expect('repos' in (pullFailure.current ?? {})).toBe(false);
    expect(pullFailure.current?.message).toBe('daemon disconnected');
    const job = activity.activities.find((a) => a.kind === 'pull-tags');
    expect(job?.status).toBe('error');
    expect(job?.announce).toBe(false);
  });

  it('no notice on clean success', () => {
    pullWithSummary([{ repo: 'r', matched_files: 1, mappings: 3, missing_sha256: 0 }], 1, 3);
    expect(pullFailure.current).toBeNull();
  });

  it('no notice when missing_sha256 > 0 only (advisory, not a failure)', () => {
    pullWithSummary([{ repo: 'r', matched_files: 0, mappings: 0, missing_sha256: 2 }]);
    expect(pullFailure.current).toBeNull();
  });

  it('no notice on a clamp-up notice only (advisory, pull succeeded)', () => {
    pullWithSummary([
      {
        repo: 'ptr',
        matched_files: 1,
        mappings: 5,
        missing_sha256: 0,
        notice: 'privacy ceiling (12) below floor (16); querying at 16 bits',
      },
    ], 1, 5);
    expect(pullFailure.current).toBeNull();
  });

  it('a second failing pull replaces the first notice rather than stacking', () => {
    pullWithSummary([{ repo: 'first', matched_files: 0, mappings: 0, error: 'err 1' }]);
    expect(pullFailure.current).toMatchObject({ repos: expect.arrayContaining(['first']) });
    // Dismiss activities so the second pull can register a fresh job.
    for (const a of [...activity.activities]) activity.dismiss(a.id);
    pullWithSummary([{ repo: 'second', matched_files: 0, mappings: 0, error: 'err 2' }]);
    expect(pullFailure.current).toMatchObject({ repos: expect.arrayContaining(['second']) });
    expect(pullFailure.current).toMatchObject({ repos: expect.not.arrayContaining(['first']) });
  });
});
