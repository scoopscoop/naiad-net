/** Tests for the pull-failure notice store (#228). Uses createPullFailure()
 *  for isolation — the `.svelte.test.ts` suffix enables rune compilation. */

import { describe, it, expect } from 'vitest';
import { createPullFailure } from './pull-failure.svelte';

describe('createPullFailure', () => {
  it('current starts null', () => {
    const pf = createPullFailure();
    expect(pf.current).toBeNull();
  });

  it('raise() sets current', () => {
    const pf = createPullFailure();
    pf.raise({ kind: 'repo', repos: ['ptr'], message: 'connection refused' });
    expect(pf.current).not.toBeNull();
    expect(pf.current?.kind).toBe('repo');
    // Narrow to the repo branch before accessing repos.
    expect(pf.current).toMatchObject({ repos: ['ptr'], message: 'connection refused' });
  });

  it('a second raise() replaces the first (latest wins, decision 5)', () => {
    const pf = createPullFailure();
    pf.raise({ kind: 'repo', repos: ['a'], message: 'err a' });
    pf.raise({ kind: 'fatal', message: 'fatal err' });
    expect(pf.current?.kind).toBe('fatal');
    expect(pf.current?.message).toBe('fatal err');
  });

  it('dismiss() clears to null', () => {
    const pf = createPullFailure();
    pf.raise({ kind: 'fatal', message: 'boom' });
    pf.dismiss();
    expect(pf.current).toBeNull();
  });
});
