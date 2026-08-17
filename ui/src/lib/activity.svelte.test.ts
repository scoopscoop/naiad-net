import { describe, it, expect } from 'vitest';
import { createActivity } from './activity.svelte';

describe('activity store', () => {
  it('starts idle and empty', () => {
    const a = createActivity();
    expect(a.activities).toEqual([]);
    expect(a.current).toBeNull();
    expect(a.status).toBe('idle');
  });

  it('begin pushes a running activity and goes working', () => {
    const a = createActivity();
    const job = a.begin({ label: 'Library scan', kind: 'scan' });
    expect(a.activities).toHaveLength(1);
    expect(a.current?.id).toBe(job.id);
    expect(a.current?.status).toBe('running');
    expect(a.status).toBe('working');
  });

  it('progress updates detail/done/total without changing status', () => {
    const a = createActivity();
    const job = a.begin({ label: 'Library scan', kind: 'scan' });
    job.progress({ detail: 'indexed 10 · 0 skipped', done: 10, total: 100 });
    const cur = a.byId(job.id)!;
    expect(cur.detail).toBe('indexed 10 · 0 skipped');
    expect(cur.done).toBe(10);
    expect(cur.total).toBe(100);
    expect(cur.status).toBe('running');
    expect(a.status).toBe('working');
  });

  it('succeed / warn / fail set the right status and message', () => {
    const a = createActivity();
    const s = a.begin({ label: 'A', kind: 'a' });
    s.succeed({ detail: 'done' });
    expect(a.byId(s.id)?.status).toBe('success');

    const w = a.begin({ label: 'B', kind: 'b' });
    w.warn('3 skipped', { detail: 'indexed 7 · 3 skipped' });
    expect(a.byId(w.id)?.status).toBe('warning');
    expect(a.byId(w.id)?.message).toBe('3 skipped');

    const f = a.begin({ label: 'C', kind: 'c' });
    f.fail('connection lost');
    expect(a.byId(f.id)?.status).toBe('error');
    expect(a.byId(f.id)?.message).toBe('connection lost');
  });

  it('aggregate status is error > warning > working > idle', () => {
    const a = createActivity();
    a.begin({ label: 'run', kind: 'x' }); // running → working
    expect(a.status).toBe('working');

    const w = a.begin({ label: 'warn', kind: 'y' });
    w.warn('skipped');
    expect(a.status).toBe('warning'); // warning beats working

    const e = a.begin({ label: 'err', kind: 'z' });
    e.fail('boom');
    expect(a.status).toBe('error'); // error beats warning
  });

  it('begin replaces a terminal activity of the same kind but not a running one', () => {
    const a = createActivity();
    const first = a.begin({ label: 'scan', kind: 'scan' });
    first.succeed({ detail: 'done' });
    a.begin({ label: 'scan', kind: 'scan' }); // replaces the terminal one
    expect(a.activities.filter((x) => x.kind === 'scan')).toHaveLength(1);

    const running = a.begin({ label: 'imp', kind: 'import' }); // stays running
    a.begin({ label: 'imp', kind: 'import' }); // does NOT replace a running one
    expect(a.activities.filter((x) => x.kind === 'import')).toHaveLength(2);
    expect(a.byId(running.id)?.status).toBe('running');
  });

  it('dismiss removes an entry; empty → idle', () => {
    const a = createActivity();
    const job = a.begin({ label: 'scan', kind: 'scan' });
    job.succeed({ detail: 'done' });
    a.dismiss(job.id);
    expect(a.activities).toEqual([]);
    expect(a.status).toBe('idle');
  });

  it('byId returns null for an unknown id', () => {
    const a = createActivity();
    expect(a.byId(999)).toBeNull();
  });

  it('dismiss of an unknown id is a no-op', () => {
    const a = createActivity();
    const job = a.begin({ label: 'scan', kind: 'scan' });
    a.dismiss(999);
    expect(a.activities).toHaveLength(1);
    expect(a.byId(job.id)).not.toBeNull();
  });

  it('records the announce flag when set', () => {
    const a = createActivity();
    const announced = a.begin({ label: 'Pull tags', kind: 'pull-tags', announce: true });
    expect(a.byId(announced.id)?.announce).toBe(true);

    const quiet = a.begin({ label: 'Library scan', kind: 'scan' });
    expect(a.byId(quiet.id)?.announce).toBeUndefined();
  });
});
