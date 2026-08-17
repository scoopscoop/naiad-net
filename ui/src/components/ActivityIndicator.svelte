<script lang="ts">
  import { onDestroy } from 'svelte';
  import { fade } from 'svelte/transition';
  import { activity, type JobHandle } from '../lib/activity.svelte';
  import {
    health,
    type WatchStatus,
    type CatchupStatus,
    type WarmupStatus,
    type WarmupPhase,
  } from '../lib/api';
  import { catchup } from '../lib/catchup.svelte';
  import Icon from './Icon.svelte';

  /** How long a finished error/warning keeps the dot lit. The activity store is
   *  a pure state container — terminal entries persist there until replaced by
   *  the next `begin()` of the same kind — so the decay to idle is presentation
   *  and lives here (#34). */
  const LINGER_MS = 6000;
  const POLL_MS = 5000;
  /** Cadence while the daemon is still warming its caches. The 5s steady-state
   *  poll means the first "something is happening" paint can land up to 5s after
   *  launch — squarely in the window where a cold start otherwise looks dead
   *  (#130). Reverts to `POLL_MS` the moment the warmup reports complete.
   *
   *  2s rather than 1s: startup is exactly the window where WebView2's ~6-socket
   *  cap is scarce (#115), shared with the first search and the thumbnail
   *  stream. Health does no DB work so it cannot be disk-starved, but there is
   *  no reason to take a socket five times as often to shave one second off a
   *  paint. */
  const STARTUP_POLL_MS = 2000;
  /** Only announce a startup that the user actually waited on. A warm start
   *  finishes in well under this and stays silent; a cold 95k-file start takes
   *  minutes and earns a toast. Failures announce regardless. */
  const STARTUP_ANNOUNCE_MS = 10_000;

  /** How long the toast lingers after a job reaches a terminal state, for a job
   *  that was announcing while it ran (pulls). Its toast has been on screen for
   *  the whole run, so this is only a fade-out tail. */
  const ANNOUNCE_GRACE_MS = 2000;
  /** How long the toast lingers for a job that only becomes announced at its
   *  terminal patch — the startup scan. This is the toast's *entire* lifetime,
   *  not a tail: it appears for the first time at that moment. Two seconds
   *  defeated the point, since we announce precisely because the user waited
   *  long enough (>10s) to have looked away (#130 review). */
  const TERMINAL_ANNOUNCE_GRACE_MS = 9000;
  /** Opacity-only fade duration; 0 under reduced motion (no fade, just hide). */
  const reduceMotion =
    typeof window !== 'undefined' &&
    !!window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
  const fadeMs = reduceMotion ? 0 : 200;

  /** The dot's color channel: daemon reachability + worst lingering outcome.
   *  "Busy" is no longer a dot state — it is the ring, an independent channel
   *  (#133), so a warning stays visible while other work is still running. */
  type DotState = 'offline' | 'error' | 'warning' | 'idle';
  type Health = Exclude<DotState, 'offline'>;

  let online = $state(true);
  let open = $state(false);
  let wrap = $state<HTMLElement | null>(null);
  let trigger = $state<HTMLButtonElement | null>(null);
  /** Keys (`id:status`) of terminal entries whose linger has run out. Keyed per
   *  entry, not on the aggregate: an errored entry lives in the store until the
   *  next `begin()` of its kind, so an aggregate-keyed timer could never re-arm
   *  and the dot would stay idle for every later activity. */
  let expired = $state<string[]>([]);
  const timers = new Map<string, ReturnType<typeof setTimeout>>();

  const key = (a: { id: number; status: string }) => `${a.id}:${a.status}`;
  const isTerminal = (s: string) => s === 'error' || s === 'warning';

  // Background watch-registration surfaced as a normal activity job (issue
  // #110 part 2). Driven by the health poll — per-root, seconds-scale work
  // fits the 5s cadence, so no separate stream. Panel-only (never announces).
  let watchJob: JobHandle | null = null;
  let watchJobSettled = false;

  function watchBasename(p: string): string {
    const parts = p.split(/[\\/]/).filter(Boolean);
    return parts[parts.length - 1] ?? p;
  }
  function watchDetail(w: WatchStatus): string {
    const where = w.current ? ` (${watchBasename(w.current)})` : '';
    return `root ${w.done}/${w.total}${where}`;
  }
  function applyWatchStatus(w: WatchStatus | null) {
    if (!w || w.total === 0 || watchJobSettled) return;
    if (!watchJob) {
      // Already finished before the first poll landed — nothing to show.
      if (w.complete) {
        watchJobSettled = true;
        return;
      }
      watchJob = activity.begin({
        label: 'Watching folders',
        kind: 'watch-register',
        detail: watchDetail(w),
      });
    }
    if (w.complete) {
      if (w.failed.length > 0) {
        const names = w.failed.map((f) => watchBasename(f.path)).join(', ');
        watchJob.warn(
          `${w.failed.length} folder${w.failed.length === 1 ? '' : 's'} could not be watched: ${names}`,
          { detail: `${w.done}/${w.total} registered` },
        );
      } else {
        watchJob.succeed({ detail: `${w.total} folder${w.total === 1 ? '' : 's'} watched` });
      }
      watchJob = null;
      watchJobSettled = true;
    } else {
      watchJob.progress({ detail: watchDetail(w), done: w.done, total: w.total });
    }
  }

  // Startup cache warmup surfaced as a normal activity job (#130). Before this
  // the ~96s warmup on a cold 95k-file library was invisible: the catch-up scan
  // defers behind it (#126) so its counters stay at zero, and the panel read
  // "Nothing running." for the whole window. Panel-only; the scan below owns the
  // completion announcement for the startup as a whole.
  let warmupJob: JobHandle | null = null;
  let warmupJobSettled = false;
  /** When the first incomplete warmup poll landed — the start of "startup" for
   *  the announce threshold. 0 until a warming daemon is observed. */
  let startupBeganAt = 0;

  const WARMUP_DETAIL: Record<WarmupPhase, string> = {
    idle: '',
    queued: '',
    graph: 'building tag relations',
    completion: 'warming tag completion',
    done: '',
  };

  function applyWarmupStatus(w: WarmupStatus | null) {
    // `idle` means no warmup was ever spawned (a daemon with no read pool), and
    // a pre-#130 daemon sends no warmup block at all. `queued` means the warmup
    // is parked on the startup gate with nothing read yet — incomplete, but not
    // yet work to show, and deliberately not latching `settled` so the job still
    // appears when it starts. None of the three is work to show.
    if (!w || w.phase === 'idle' || w.phase === 'queued' || warmupJobSettled) return;
    if (!warmupJob) {
      // Already warm before the first poll landed — the common warm-launch case.
      // Returning here is also what keeps a sub-second warmup from flickering a
      // row into the panel: the job only exists if a poll caught it incomplete,
      // and at the startup cadence that means it ran for at least ~1s.
      if (w.complete) {
        warmupJobSettled = true;
        return;
      }
      startupBeganAt = Date.now();
      warmupJob = activity.begin({
        label: 'Preparing library',
        kind: 'warmup',
        detail: WARMUP_DETAIL[w.phase],
      });
    }
    if (w.complete) {
      warmupJob.succeed({ detail: 'ready' });
      warmupJob = null;
      warmupJobSettled = true;
    } else {
      warmupJob.progress({ detail: WARMUP_DETAIL[w.phase] });
    }
  }

  // Background catch-up rescan surfaced as a normal activity job (issue #119),
  // exactly like watch-registration above. Driven by the same health poll.
  let catchupJob: JobHandle | null = null;
  let catchupJobSettled = false;
  /** Previous (imported, timestamp) sample, for the files/s readout. */
  let lastImported = 0;
  let lastImportedAt = 0;
  /** Smoothed indexing rate in files/s; 0 until two samples have advanced. */
  let importRate = 0;

  /** Fold one poll into the smoothed rate. Exponential smoothing keeps the
   *  readout steady when poll intervals jitter or a tick lands mid-batch. */
  function sampleRate(s: CatchupStatus): void {
    const now = Date.now();
    if (lastImportedAt > 0 && s.imported > lastImported) {
      const dt = (now - lastImportedAt) / 1000;
      if (dt > 0) {
        const instant = (s.imported - lastImported) / dt;
        importRate = importRate > 0 ? importRate * 0.6 + instant * 0.4 : instant;
      }
    }
    lastImportedAt = now;
    lastImported = s.imported;
  }

  function catchupDetail(s: CatchupStatus, deferred: boolean): string {
    // Nothing indexed yet: say why rather than showing a meaningless "0 files".
    if (s.imported === 0 && !s.complete) {
      return deferred ? 'waiting for cache warmup' : 'starting…';
    }
    const where = s.current ? ` · ${watchBasename(s.current)}` : '';
    const speed = importRate >= 1 ? ` · ${Math.round(importRate).toLocaleString()}/s` : '';
    return `${s.imported.toLocaleString()} files${speed}${where}`;
  }

  /** Whether the scan is pending rather than active: the daemon marks it running
   *  before deferring it behind the warmup, so counters stay at zero until the
   *  scan thread is actually spawned (`daemon/src/lib.rs`, #126).
   *
   *  `roots_total` is the discriminator, not `imported`: the first progress tick
   *  from `rescan_roots` sets it, so a scan that has started but not yet indexed
   *  anything reads as active. Keying on `imported` alone claimed "waiting" while
   *  the scan was genuinely running — visible whenever the warmup outlives
   *  `CATCHUP_SCAN_DEFER_TIMEOUT` and the two overlap. */
  function isDeferred(s: CatchupStatus, w: WarmupStatus | null): boolean {
    return (
      !!w && !w.complete && s.running && !s.complete && s.roots_total === 0 && s.imported === 0
    );
  }

  function applyCatchupStatus(s: CatchupStatus | null, w: WarmupStatus | null) {
    // No scan ran (default all-zero status, `running` false) → nothing to show.
    // A *deferred* scan also reports all-zero counters but sets `running`, and
    // that is precisely the case this used to drop on the floor (#130).
    if (!s || (!s.running && s.roots_total === 0 && s.imported === 0) || catchupJobSettled) return;
    const deferred = isDeferred(s, w);
    if (!catchupJob) {
      // Already finished before the first poll landed — nothing to show.
      if (s.complete) {
        catchupJobSettled = true;
        return;
      }
      if (startupBeganAt === 0) startupBeganAt = Date.now();
      catchupJob = activity.begin({
        label: 'Library scan',
        kind: 'catchup-scan',
        detail: catchupDetail(s, deferred),
        queued: deferred,
      });
    }
    if (s.complete) {
      // Announce only a startup the user actually waited on; always announce a
      // scan that could not index everything.
      const waited = startupBeganAt > 0 && Date.now() - startupBeganAt >= STARTUP_ANNOUNCE_MS;
      const detail = `${s.imported.toLocaleString()} files`;
      if (s.errors > 0) {
        catchupJob.warn(
          `${s.errors.toLocaleString()} file${s.errors === 1 ? '' : 's'} could not be indexed`,
          { detail, announce: true },
        );
      } else {
        catchupJob.succeed({ detail, announce: waited });
      }
      catchupJob = null;
      catchupJobSettled = true;
    } else {
      sampleRate(s);
      // Background scans report total=0 → leave done/total unset (indeterminate).
      catchupJob.progress({ detail: catchupDetail(s, deferred), queued: deferred });
    }
  }

  // Poll daemon liveness + watch-registration. Lifted out of TitleBar, which
  // had no business owning it.
  $effect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const tick = async () => {
      const res = await health();
      if (cancelled) return;
      online = res.ok;
      if (!res.ok) {
        // Daemon offline: settle any in-flight jobs so they do not remain
        // 'running' forever, then reset the latches so a fresh run on reconnect
        // is detected and surfaced as a new job.
        if (watchJob) {
          watchJob.fail('daemon offline');
          watchJob = null;
        }
        watchJobSettled = false;
        if (warmupJob) {
          // Announce, like the scan below: a daemon that dies mid-warmup is the
          // more alarming of the two, and staying silent about it because the
          // panel happens to be shut would be the #130 bug all over again.
          warmupJob.fail('daemon offline', { announce: true });
          warmupJob = null;
        }
        warmupJobSettled = false;
        if (catchupJob) {
          catchupJob.fail('daemon offline', { announce: true });
          catchupJob = null;
        }
        catchupJobSettled = false;
        startupBeganAt = 0;
        lastImportedAt = 0;
        lastImported = 0;
        importRate = 0;
        catchup.set(null);
      } else {
        applyWatchStatus(res.watch);
        // Warmup first: the scan reads it to tell "deferred" from "starting".
        applyWarmupStatus(res.warmup);
        applyCatchupStatus(res.scan, res.warmup);
        catchup.set(res.scan);
      }
      if (cancelled) return;
      // Poll fast only while the daemon says it is still warming. A pre-#130
      // daemon sends no warmup block, so it stays on the steady cadence.
      const warming = res.ok && !!res.warmup && !res.warmup.complete;
      timer = setTimeout(tick, warming ? STARTUP_POLL_MS : POLL_MS);
    };
    tick();
    return () => {
      cancelled = true;
      if (timer !== undefined) clearTimeout(timer);
    };
  });

  // Arm one linger per terminal entry, once. Entries that leave the store (or
  // transition to another status) drop their timer and their expiry, so the
  // same id lights the dot again the next time it fails.
  $effect(() => {
    const live = new Set(activity.activities.filter((a) => isTerminal(a.status)).map(key));

    for (const [k, t] of timers) {
      if (!live.has(k)) {
        clearTimeout(t);
        timers.delete(k);
      }
    }
    if (expired.some((k) => !live.has(k))) expired = expired.filter((k) => live.has(k));

    for (const k of live) {
      if (timers.has(k) || expired.includes(k)) continue;
      timers.set(
        k,
        setTimeout(() => {
          timers.delete(k);
          expired = [...expired, k];
        }, LINGER_MS),
      );
    }
  });

  // The entry to announce: the most-recently-started flagged one. A newer
  // announced pull replaces an older one (they share kind 'pull-tags', so the
  // store already collapses terminal ones).
  const announceCandidate = $derived.by(() => {
    for (let i = activity.activities.length - 1; i >= 0; i--) {
      if (activity.activities[i].announce) return activity.activities[i];
    }
    return null;
  });

  // Keys (`id:status`) of terminal announced entries whose grace has run out —
  // after which the toast fades. Keyed like the linger set so a re-run (new id)
  // re-announces.
  let announceExpired = $state<string[]>([]);
  const announceTimers = new Map<string, ReturnType<typeof setTimeout>>();

  // Ids seen announcing while still running. Their toast has already been on
  // screen for the whole job, so the short fade-out tail is right; anything else
  // is appearing for the first time at its terminal moment and needs long enough
  // to actually be read.
  const announcedWhileRunning = new Set<number>();
  $effect(() => {
    const liveIds = new Set(activity.activities.map((a) => a.id));
    for (const a of activity.activities) {
      if (a.announce && a.status === 'running') announcedWhileRunning.add(a.id);
    }
    // Entries leave the store on dismiss or terminal-replacement; drop their ids
    // so the set cannot grow without bound across a long session.
    for (const id of announcedWhileRunning) {
      if (!liveIds.has(id)) announcedWhileRunning.delete(id);
    }
  });

  $effect(() => {
    const c = announceCandidate;
    const liveKey = c && c.status !== 'running' ? key(c) : null;
    // Ids still present in the store — only prune timers/expiries for entries
    // that have left entirely. An entry that expired once must stay expired so
    // announce:false on a newer job cannot resurrect an older toast (#228 F1).
    const liveActivityIds = new Set(activity.activities.map((a) => a.id));

    // Drop timers/expiries only for entries whose activity is gone from the store.
    for (const [k, t] of announceTimers) {
      if (!liveActivityIds.has(parseInt(k))) {
        clearTimeout(t);
        announceTimers.delete(k);
      }
    }
    if (announceExpired.some((k) => !liveActivityIds.has(parseInt(k)))) {
      announceExpired = announceExpired.filter((k) => liveActivityIds.has(parseInt(k)));
    }

    // Arm the grace timer once for a terminal announced entry. A job that never
    // announced while running is showing its toast for the first time here, so
    // it gets the long grace rather than a 2s fade-out tail.
    if (liveKey && c && !announceTimers.has(liveKey) && !announceExpired.includes(liveKey)) {
      const graceMs = announcedWhileRunning.has(c.id)
        ? ANNOUNCE_GRACE_MS
        : TERMINAL_ANNOUNCE_GRACE_MS;
      announceTimers.set(
        liveKey,
        setTimeout(() => {
          announceTimers.delete(liveKey);
          announceExpired = [...announceExpired, liveKey];
        }, graceMs),
      );
    }
  });

  // What the toast shows: the candidate, unless the panel is open (it takes
  // over) or a terminal candidate has passed its grace (faded out).
  const toast = $derived.by(() => {
    const c = announceCandidate;
    if (!c || open) return null;
    if (c.status !== 'running' && announceExpired.includes(key(c))) return null;
    return c;
  });

  // A press on a non-focusable region of the panel blurs the trigger to <body>,
  // which gives `focusout` a null relatedTarget. That case is ignored below, so
  // dismissal by pointer is handled here instead of by suppressing mousedown —
  // the panel's error text stays selectable.
  $effect(() => {
    const el = wrap;
    if (!open || !el) return;
    const outside = (e: PointerEvent) => {
      if (e.target instanceof Node && !el.contains(e.target)) open = false;
    };
    window.addEventListener('pointerdown', outside, true);
    return () => window.removeEventListener('pointerdown', outside, true);
  });

  onDestroy(() => {
    for (const t of timers.values()) clearTimeout(t);
    timers.clear();
    for (const t of announceTimers.values()) clearTimeout(t);
    announceTimers.clear();
  });

  // Two channels instead of one aggregate (#133): the dot's color is *health*
  // (worst not-yet-lingered outcome), the ring is *activity* (anything
  // running). The old single channel forced them to fight — a warning raised
  // mid-scan was invisible until the scan finished.
  const visible = $derived(
    activity.activities.filter((a) => !(isTerminal(a.status) && expired.includes(key(a)))),
  );
  const busy = $derived(visible.some((a) => a.status === 'running'));
  const worst = $derived.by((): Health => {
    if (visible.some((a) => a.status === 'error')) return 'error';
    if (visible.some((a) => a.status === 'warning')) return 'warning';
    return 'idle';
  });

  const dotState = $derived.by((): DotState => (online ? worst : 'offline'));
  const ringing = $derived(online && busy);

  const labels: Record<DotState, string> = {
    offline: 'daemon offline',
    error: 'daemon ok, an activity failed',
    warning: 'daemon ok, an activity warned',
    idle: 'daemon ok, idle',
  };

  const dotLabel = $derived.by(() => {
    if (!ringing) return labels[dotState];
    return dotState === 'idle' ? 'daemon ok, working' : `${labels[dotState]}, working`;
  });

  // Newest first: the store appends, and the thing that just started is the
  // thing the user came to look at.
  const entries = $derived(activity.activities.toReversed());

  // Dismissing unmounts the button that holds focus, which would drop focus to
  // <body> — outside the wrapper that owns the Escape handler, and past the
  // focusout guard that ignores a null relatedTarget. Hand focus back first.
  function dismiss(id: number) {
    trigger?.focus();
    activity.dismiss(id);
  }

  function percent(done?: number, total?: number): number | null {
    if (!total || total <= 0 || done === undefined) return null;
    return Math.max(0, Math.min(100, Math.round((done / total) * 100)));
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- Escape must close the panel even when focus is on a dismiss button inside
     it — that is precisely where keyboard users land after tabbing in. The
     handler belongs on the wrapper so bubbled keydown events from panel
     children are caught. The wrapper itself is not interactive; all reachable
     targets (trigger button, dismiss buttons) are. -->
<div
  class="wrap"
  bind:this={wrap}
  onkeydown={(e) => {
    if (e.key === 'Escape' && open) {
      open = false;
      e.stopPropagation();
    }
  }}
  onfocusout={(e) => {
    // A null relatedTarget means focus went nowhere focusable (a press on the
    // panel's own text). Only a move to a real target outside closes.
    if (!(e.relatedTarget instanceof Node)) return;
    if (e.currentTarget instanceof HTMLElement && !e.currentTarget.contains(e.relatedTarget))
      open = false;
  }}
>
  <button
    class="trigger"
    type="button"
    bind:this={trigger}
    data-state={dotState}
    data-busy={ringing || undefined}
    aria-controls="activity-panel"
    aria-expanded={open}
    aria-label={dotLabel}
    onclick={() => (open = !open)}
  >
    <span class="text">daemon</span>
    <span class="mark" aria-hidden="true">
      {#if ringing}<span class="ring"></span>{/if}
      <span class="dot"></span>
    </span>
    <span class="caret" aria-hidden="true"><Icon name="chevron-down" size={12} /></span>
  </button>

  {#if toast}
    {@const pct = percent(toast.done, toast.total)}
    <div
      class="toast"
      role="status"
      aria-live="polite"
      data-status={toast.status}
      transition:fade={{ duration: fadeMs }}
    >
      <div class="toast-row">
        <span class="toast-label">{toast.label}</span>
        {#if toast.detail}<span class="toast-detail">{toast.detail}</span>{/if}
      </div>
      {#if toast.status === 'running' && pct !== null}
        <div
          class="toast-bar"
          role="progressbar"
          aria-label={toast.label}
          aria-valuemin="0"
          aria-valuemax={toast.total}
          aria-valuenow={toast.done}
        >
          <span style="width: {pct}%"></span>
        </div>
      {/if}
    </div>
  {/if}

  {#if open}
    <div id="activity-panel" class="panel" role="region" aria-label="activity">
      {#if entries.length === 0}
        <p class="empty">Nothing running.</p>
      {:else}
        <ul>
          {#each entries as a (a.id)}
            {@const pct = percent(a.done, a.total)}
            <li>
              <div class="row">
                <span class="label">{a.label}</span>
                <button
                  class="x"
                  type="button"
                  aria-label={`dismiss ${a.label}`}
                  onclick={() => dismiss(a.id)}>×</button>
              </div>
              {#if a.detail}<p class="detail">{a.detail}</p>{/if}
              {#if a.message}<p class="detail msg" data-status={a.status}>{a.message}</p>{/if}
              {#if a.status === 'running' && !a.queued}
                {#if pct === null}
                  <div class="bar indeterminate" role="progressbar" aria-label={a.label}></div>
                {:else}
                  <div
                    class="bar"
                    role="progressbar"
                    aria-label={a.label}
                    aria-valuemin="0"
                    aria-valuemax={a.total}
                    aria-valuenow={a.done}
                  >
                    <span style="width: {pct}%"></span>
                  </div>
                {/if}
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
</div>

<style>
  .wrap {
    position: relative;
    flex: none;
  }
  .trigger {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 26px;
    padding: 0 8px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-faint);
    font: 500 11px/1 var(--mono);
    white-space: nowrap;
    cursor: pointer;
  }
  .trigger:hover {
    background: var(--raise);
  }
  .trigger:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .mark {
    position: relative;
    display: inline-grid;
    place-items: center;
    width: 15px;
    height: 15px;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--ok);
  }
  .trigger[data-state='offline'] .dot,
  .trigger[data-state='error'] .dot {
    background: var(--err);
  }
  .trigger[data-state='warning'] .dot {
    background: var(--warn);
  }
  /* The activity ring: the canonical Spinner construction (1.5px line-soft
     border, accent top edge) slowed to an ambient 1.4s — the interactive 0.7s
     tempo nags from peripheral vision when it runs for minutes. Replaces the
     dot's opacity pulse in the DESIGN.md motion carve-out (#133). */
  .ring {
    position: absolute;
    inset: 0;
    border-radius: 50%;
    border: 1.5px solid var(--line-soft);
    border-top-color: var(--accent);
    animation: ring-spin 1.4s linear infinite;
  }
  @keyframes ring-spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    /* Static, not hidden: the ring's presence is the busy cue — a shape that
       reduced-motion users still get, where the pulse gave them nothing. */
    .ring {
      animation: none;
    }
  }
  /* Clickability affordance: a caret revealed at the moment of intent. The
     titlebar's grammar is invisible-at-rest controls, so nothing shows until
     hover/focus/open; the slot keeps its width so the pip never shifts. */
  .caret {
    display: inline-flex;
    margin-left: -2px;
    opacity: 0;
    transition: opacity 0.15s;
  }
  .trigger:hover .caret,
  .trigger:focus-visible .caret,
  .trigger[aria-expanded='true'] .caret {
    opacity: 1;
  }
  @media (prefers-reduced-motion: reduce) {
    .caret {
      transition: none;
    }
  }

  /* A true floating overlay, so the popover shadow applies (Overlay-Only Rule). */
  .panel {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    z-index: 30;
    width: 260px;
    max-height: 320px;
    overflow-y: auto;
    padding: 6px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    box-shadow: var(--shadow-popover);
  }
  .empty {
    margin: 6px;
    font: 500 11px/1 var(--mono);
    color: var(--text-faint);
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  li {
    padding: 6px 6px 8px;
    border-radius: 6px;
  }
  li + li {
    border-top: 1px solid var(--line);
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .label {
    font: 500 12px/1 var(--mono);
    color: var(--text);
  }
  .detail {
    margin: 4px 0 0;
    font: 500 11px/1.3 var(--mono);
    color: var(--text-faint);
  }
  .msg[data-status='error'] {
    color: var(--err);
  }
  .msg[data-status='warning'] {
    color: var(--warn);
  }
  .x {
    flex: none;
    width: 20px;
    height: 20px;
    border: 0;
    border-radius: 5px;
    background: transparent;
    color: var(--text-mute);
    cursor: pointer;
  }
  .x:hover {
    background: var(--err-bg);
    color: var(--err);
  }
  .bar {
    position: relative;
    height: 3px;
    margin-top: 6px;
    border-radius: 2px;
    background: var(--raise);
    overflow: hidden;
  }
  .bar span {
    display: block;
    height: 100%;
    background: var(--accent);
  }
  .bar.indeterminate::after {
    content: '';
    position: absolute;
    inset: 0 auto 0 0;
    width: 40%;
    background: var(--accent);
    animation: slide 1.2s ease-in-out infinite;
  }
  @keyframes slide {
    from {
      transform: translateX(-100%);
    }
    to {
      transform: translateX(250%);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .bar.indeterminate::after {
      animation: none;
      width: 100%;
      opacity: 0.35;
    }
  }

  /* Transient announce toast — a floating overlay, so the popover shadow is
     licensed by the Overlay-Only Rule. Toasts rung (50) of the z-ladder: above
     the activity panel (30). Tokens only; motion is opacity via transition:fade. */
  .toast {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    z-index: 50;
    min-width: 200px;
    max-width: 280px;
    padding: 6px 8px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    box-shadow: var(--shadow-popover);
  }
  .toast-row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
  }
  .toast-label {
    font: 500 12px/1 var(--mono);
    color: var(--text);
    white-space: nowrap;
  }
  .toast[data-status='running'] .toast-label {
    color: var(--accent);
  }
  .toast[data-status='success'] .toast-label {
    color: var(--ok);
  }
  .toast[data-status='warning'] .toast-label {
    color: var(--warn);
  }
  .toast[data-status='error'] .toast-label {
    color: var(--err);
  }
  .toast-detail {
    font: 500 11px/1.3 var(--mono);
    color: var(--text-faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .toast-bar {
    position: relative;
    height: 3px;
    margin-top: 6px;
    border-radius: 2px;
    background: var(--raise);
    overflow: hidden;
  }
  .toast-bar span {
    display: block;
    height: 100%;
    background: var(--accent);
  }
</style>
