import { describe, expect, it } from 'vitest';
import { tilePlaceholder } from './tile-placeholder';

describe('tilePlaceholder', () => {
  it('is deterministic for the same hash', () => {
    expect(tilePlaceholder('3f9a2c71deadbeef')).toBe(tilePlaceholder('3f9a2c71deadbeef'));
  });

  it('draws from the candy category palette, dimmed toward the ink ground', () => {
    const css = tilePlaceholder('00000000');
    expect(css).toMatch(/^linear-gradient\(\d+deg, color-mix\(in srgb, var\(--cat-/);
    expect(css).toContain('var(--ink-8');
  });

  it('references only category and ink tokens', () => {
    for (const hash of ['00', 'ff00ab', 'deadbeefcafe', '12345678', 'abc']) {
      const refs = [...tilePlaceholder(hash).matchAll(/var\((--[a-z0-9-]+)\)/g)].map((m) => m[1]);
      expect(refs.length).toBeGreaterThan(0);
      for (const r of refs) expect(r.startsWith('--cat-') || r.startsWith('--ink-')).toBe(true);
    }
  });

  it('keeps the angle in 90..209deg', () => {
    for (const h of ['00000000', 'ffffffff', '3f9a2c71']) {
      const angle = Number(tilePlaceholder(h).match(/^linear-gradient\((\d+)deg/)![1]);
      expect(angle).toBeGreaterThanOrEqual(90);
      expect(angle).toBeLessThan(210);
    }
  });

  it('handles a non-hex hash without throwing', () => {
    expect(() => tilePlaceholder('not-a-hash')).not.toThrow();
  });

  it('falls back to a stable gradient for a malformed hash', () => {
    const g = tilePlaceholder('zzzz');
    expect(g).toBe(tilePlaceholder('00000000'));
    expect(g).not.toMatch(/NaN/);
  });
});
