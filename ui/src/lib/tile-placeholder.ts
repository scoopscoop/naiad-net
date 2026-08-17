/** Deterministic per-file loading placeholder: a soft "candy pastel" gradient
 *  seeded from the file hash so a tile keeps its colour across scroll/re-render
 *  rather than flickering. Pure - no DOM. Uses var() tokens so it tracks the
 *  theme.
 *
 *  Atelier note: this deliberately reverses the original "calm grid, not
 *  confetti" intent - brighter loading tiles are wanted now (spec decision 9).
 *  The color-mix ratios keep the candy mid-tone against the pastel-dusk ground
 *  rather than neon. */

const STOPS: ReadonlyArray<readonly [string, string]> = [
  ['--cat-rose', '--cat-butter'],
  ['--cat-sage', '--cat-sky'],
  ['--cat-peri', '--cat-lilac'],
  ['--cat-butter', '--cat-sage'],
  ['--cat-lilac', '--cat-rose'],
  ['--cat-sky', '--cat-peri'],
];

export function tilePlaceholder(hash: string): string {
  const seed = Math.abs(parseInt(hash.slice(0, 8), 16) || 0);
  const [a, b] = STOPS[seed % STOPS.length];
  const angle = 90 + (seed % 120);
  return (
    `linear-gradient(${angle}deg, ` +
    `color-mix(in srgb, var(${a}) 38%, var(--ink-800)), ` +
    `color-mix(in srgb, var(${b}) 26%, var(--ink-850)))`
  );
}
