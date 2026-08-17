import { beforeEach, describe, expect, it } from 'vitest';
import { RAIL_SAVED_KEY, loadSaved, saveSaved } from './rail-saved';

beforeEach(() => localStorage.clear());

describe('rail saved searches', () => {
  it('round-trips a list', () => {
    saveSaved([{ name: 'mika', query: 'creator:mika' }]);
    expect(loadSaved()).toEqual([{ name: 'mika', query: 'creator:mika' }]);
  });

  it('returns [] on missing or malformed storage', () => {
    expect(loadSaved()).toEqual([]);
    localStorage.setItem(RAIL_SAVED_KEY, '{not json');
    expect(loadSaved()).toEqual([]);
    localStorage.setItem(RAIL_SAVED_KEY, '[{"name":1}]');
    expect(loadSaved()).toEqual([]);
  });
});
