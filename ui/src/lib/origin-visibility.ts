import type { TagDetail } from './types';

/** Sentinel key for origin-less (manual) tags in `hiddenOrigins`. The leading
 *  U+0000 NUL can never appear in a real interned origin name (ADR 0026 forbids
 *  control characters ≤ U+001F in origin names), so this value is
 *  collision-proof against every possible real origin, including one that a
 *  producer might literally name `"manual"`. */
export const MANUAL_ORIGIN = '\0manual';

/** Stable key for a tag's generation origin, suitable for use as a
 *  `hiddenOrigins` entry. Returns `MANUAL_ORIGIN` when the tag carries no
 *  recorded origin (i.e. `t.origin` is `undefined`). */
export function originKey(t: TagDetail): string {
  return t.origin ?? MANUAL_ORIGIN;
}
