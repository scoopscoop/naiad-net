/**
 * Pure helpers translating a user-facing "cover crowd" (k-anonymity set: how
 * many files hide each hash you pull) to/from a bucket width in prefix bits,
 * plus a rough download estimate. Bits are the stored quantity
 * (max_query_bits); crowd is presentation only.
 */
export const CROWD_FLOOR = 1000;
export const SERVER_FLOOR_BITS = 8;
export const MAX_BITS = 256;
/** Rough average bytes per returned (hash, tags) entry. Tunable; approximate. */
export const EST_BYTES_PER_ENTRY = 300;

const log2 = (x: number) => Math.log(x) / Math.LN2;
const clamp = (n: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, n));

export function bitsForCrowd(count: number, desiredCrowd: number): number {
  if (count <= 0 || desiredCrowd <= 0) return SERVER_FLOOR_BITS;
  const raw = Math.floor(log2(count / desiredCrowd));
  return clamp(Number.isFinite(raw) ? raw : SERVER_FLOOR_BITS, SERVER_FLOOR_BITS, MAX_BITS);
}
export function crowdForBits(count: number, bits: number): number {
  return Math.max(1, Math.round(count / 2 ** bits));
}
export function effectiveBits(
  advertised: number | null | undefined,
  ceiling: number,
  floor?: number | null,
): number {
  const capped = advertised == null ? ceiling : Math.min(advertised, ceiling);
  const floored = floor == null ? capped : Math.max(capped, floor);
  return advertised == null ? floored : Math.min(advertised, floored);
}
export function softCapBits(count: number): number {
  return bitsForCrowd(count, CROWD_FLOOR);
}
/** Approx bytes downloaded per single hash you look up: the whole bucket
 *  (~crowd entries) comes back. This is the "size per hash" estimate. */
export function bytesPerLookup(count: number, bits: number): number {
  return crowdForBits(count, bits) * EST_BYTES_PER_ENTRY;
}
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}
