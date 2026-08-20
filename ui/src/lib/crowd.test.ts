import { describe, it, expect } from 'vitest';
import {
  bitsForCrowd, crowdForBits, effectiveBits, softCapBits,
  bytesPerLookup, CROWD_FLOOR, SERVER_FLOOR_BITS, MAX_BITS,
} from './crowd';

describe('crowd/bits conversion', () => {
  it('bigger crowd => fewer bits', () => {
    const N = 1_000_000_000;
    expect(bitsForCrowd(N, 1000)).toBeLessThan(bitsForCrowd(N, 10));
  });
  it('crowdForBits inverts bitsForCrowd within a power-of-two factor', () => {
    const N = 1_000_000_000;
    const bits = bitsForCrowd(N, 1000);
    const crowd = crowdForBits(N, bits);
    expect(crowd).toBeGreaterThanOrEqual(1000);
    expect(crowd).toBeLessThan(2000);
  });
  it('clamps to server floor and max bits', () => {
    expect(bitsForCrowd(1_000_000_000, 1e15)).toBe(SERVER_FLOOR_BITS);
    expect(bitsForCrowd(1_000_000_000, 1)).toBeLessThanOrEqual(MAX_BITS);
  });
  it('softCapBits gives crowd >= CROWD_FLOOR', () => {
    const N = 1_000_000_000;
    expect(crowdForBits(N, softCapBits(N))).toBeGreaterThanOrEqual(CROWD_FLOOR);
  });
  it('effectiveBits = min(advertised, ceiling)', () => {
    expect(effectiveBits(18, 24)).toBe(18);
    expect(effectiveBits(24, 18)).toBe(18);
    expect(effectiveBits(null, 20)).toBe(20);
  });
  it('effectiveBits respects the server floor', () => {
    expect(effectiveBits(24, 10, 16)).toBe(16);   // ceiling below floor => clamped up
    expect(effectiveBits(null, 10, 16)).toBe(16);
    expect(effectiveBits(24, 20, 16)).toBe(20);   // ceiling above floor => unchanged
  });
  it('bytesPerLookup scales with crowd', () => {
    const N = 1_000_000_000;
    expect(bytesPerLookup(N, 18)).toBeGreaterThan(bytesPerLookup(N, 24)); // fewer bits = bigger crowd = more bytes
    expect(bytesPerLookup(N, 18)).toBe(crowdForBits(N, 18) * 300);
  });
});
