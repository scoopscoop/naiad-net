import { describe, expect, it } from 'vitest';
import { MANUAL_ORIGIN, originKey } from './origin-visibility';
import type { TagDetail } from './types';

const makeTag = (tag: string, origin?: string): TagDetail => ({
  tag,
  presence: 'local',
  services: [],
  relations: false,
  origin,
});

describe('originKey', () => {
  it('returns the tag origin when present', () => {
    expect(originKey(makeTag('a:foo', 'hydrus'))).toBe('hydrus');
    expect(originKey(makeTag('b:bar', 'wd14-tagger'))).toBe('wd14-tagger');
  });

  it('returns MANUAL_ORIGIN when origin is absent (undefined)', () => {
    expect(originKey(makeTag('c:baz'))).toBe(MANUAL_ORIGIN);
  });

  it('MANUAL_ORIGIN is the NUL-prefixed sentinel \\0manual', () => {
    expect(MANUAL_ORIGIN).toBe('\0manual');
    // The first character must be NUL (U+0000), not a space or any printable char.
    expect(MANUAL_ORIGIN.charCodeAt(0)).toBe(0);
  });

  it('MANUAL_ORIGIN never collides with the string "manual"', () => {
    // A producer that (incorrectly) names an origin "manual" gets key "manual",
    // not MANUAL_ORIGIN — they are distinct strings.
    expect(MANUAL_ORIGIN).not.toBe('manual');
    expect(originKey(makeTag('x:y', 'manual'))).toBe('manual');
    expect(originKey(makeTag('x:y', 'manual'))).not.toBe(MANUAL_ORIGIN);
  });

  it('MANUAL_ORIGIN round-trips: origin-less tags hidden/shown by the sentinel key', () => {
    const tag = makeTag('series:x'); // no origin
    const key = originKey(tag);
    expect(key).toBe(MANUAL_ORIGIN);
    // Simulating: hiding/showing by the sentinel correctly targets origin-less tags
    const hidden = new Set([MANUAL_ORIGIN]);
    expect(hidden.has(originKey(tag))).toBe(true);
    hidden.delete(MANUAL_ORIGIN);
    expect(hidden.has(originKey(tag))).toBe(false);
  });
});
