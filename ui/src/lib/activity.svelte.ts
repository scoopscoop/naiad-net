/** In-memory model of "what the app is doing right now". Every long-running
 *  operation (library scan, Hydrus import) reports into this store via a job
 *  handle from `begin()`. The derived `status` is the single value the title-bar
 *  `ActivityIndicator` reads (#34). Not persisted across restarts, like `tabs`. */

export type ActivityStatus = 'running' | 'success' | 'warning' | 'error';
export type AggregateStatus = 'idle' | 'working' | 'warning' | 'error';

export interface Activity {
  id: number;
  /** Human label, e.g. "Library scan", "Hydrus import". */
  label: string;
  /** Producer kind, e.g. "scan" | "import". A free string: new producers are
   *  zero-touch on the store. Used for terminal-replacement grouping. */
  kind: string;
  status: ActivityStatus;
  /** Caller-formatted line, e.g. "indexed 1,234 · 5 skipped". */
  detail: string;
  /** Present with `total` → determinate (drives a bar). */
  done?: number;
  /** Absent (or 0) → indeterminate. */
  total?: number;
  /** Set on warn/fail. */
  message?: string;
  /** Opt-in: pull jobs set this so the indicator auto-reveals a transient toast.
   *  Settable at `begin()` (announce for the whole run, as pulls do) or on a
   *  terminal patch (announce only the outcome, as the startup scan does when it
   *  was slow enough to be worth interrupting for — #130). */
  announce?: boolean;
  /** Running but not yet started — queued behind another job. Renders no
   *  progress bar: an indeterminate bar means "working, duration unknown", and
   *  a job that has not begun is not working. The deferred catch-up scan sits
   *  here while the cache warmup runs (#126, #130). */
  queued?: boolean;
}

/** Collapse a set of activities to one status, priority error > warning >
 *  working > idle. Exported so presentation code that needs the same
 *  precedence over a filtered subset does not restate it. */
export function aggregate(items: readonly Pick<Activity, 'status'>[]): AggregateStatus {
  let agg: AggregateStatus = 'idle';
  for (const a of items) {
    if (a.status === 'error') return 'error';
    if (a.status === 'warning') agg = 'warning';
    else if (a.status === 'running' && agg !== 'warning') agg = 'working';
  }
  return agg;
}

/** What a producer holds after `begin()`. Never mutate the store array directly. */
export interface JobHandle {
  readonly id: number;
  progress(patch: { detail?: string; done?: number; total?: number; queued?: boolean }): void;
  succeed(patch?: { detail?: string; announce?: boolean }): void;
  warn(message: string, patch?: { detail?: string; announce?: boolean }): void;
  fail(message: string, patch?: { announce?: boolean }): void;
}

/** Create an isolated activity store. The app uses the shared `activity`
 *  singleton; tests call this for a fresh instance. */
export function createActivity() {
  let nextId = 1;
  let items = $state<Activity[]>([]);

  function find(id: number): Activity | undefined {
    return items.find((a) => a.id === id);
  }

  // `id`/`kind` are invariant after construction, so they are not patchable.
  function patch(id: number, fields: Partial<Omit<Activity, 'id' | 'kind'>>): void {
    const a = find(id);
    if (a) Object.assign(a, fields);
  }

  function begin(init: {
    label: string;
    kind: string;
    detail?: string;
    announce?: boolean;
    queued?: boolean;
  }): JobHandle {
    // Terminal-replacement: drop any finished activity of the same kind so the
    // panel/indicator shows one entry per kind. A still-running one is left be.
    items = items.filter((a) => !(a.kind === init.kind && a.status !== 'running'));
    const entry: Activity = {
      id: nextId++,
      label: init.label,
      kind: init.kind,
      status: 'running',
      detail: init.detail ?? '',
      announce: init.announce,
      queued: init.queued,
    };
    items = [...items, entry];
    const id = entry.id;
    return {
      get id() {
        return id;
      },
      progress(p) {
        patch(id, { ...p, status: 'running' });
      },
      // Terminal patches clear `queued`: a finished job is no longer waiting on
      // anything, and a stale flag would suppress nothing but read as a lie.
      succeed(p) {
        patch(id, { ...p, status: 'success', queued: false });
      },
      warn(message, p) {
        patch(id, { ...p, status: 'warning', message, queued: false });
      },
      fail(message, p) {
        patch(id, { ...p, status: 'error', message, queued: false });
      },
    };
  }

  return {
    begin,
    get activities(): Activity[] {
      return items;
    },
    /** The most-recently-started still-running activity (compact display). */
    get current(): Activity | null {
      for (let i = items.length - 1; i >= 0; i--) {
        if (items[i].status === 'running') return items[i];
      }
      return null;
    },
    /** Derived aggregate, priority error > warning > working > idle. */
    get status(): AggregateStatus {
      return aggregate(items);
    },
    byId(id: number): Activity | null {
      return find(id) ?? null;
    },
    dismiss(id: number): void {
      items = items.filter((a) => a.id !== id);
    },
  };
}

/** App-wide singleton. */
export const activity = createActivity();
