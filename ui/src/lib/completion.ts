/**
 * Search-token completion helpers. The search box holds a whitespace-separated,
 * Hydrus-style query; completion operates on the trailing token only. Pure — no
 * store/DOM access — so it is exhaustively unit-tested.
 */
import type { NamespaceSuggestion } from './types';

export interface NamespacePick {
  kind: 'namespace';
  namespace: string;
}
export interface TagPick {
  kind: 'tag';
  namespace: string;
  subtag: string;
}
export type CompletionPick = NamespacePick | TagPick;

/** The trailing token of `query` and the index where it starts. Honors a quoted
 *  value so `character:"zero mission"` stays one token; mirrors the daemon's
 *  tokenize() trailing-token behavior. */
export function lastToken(query: string): { token: string; start: number } {
  let inQuote = false;
  let start = 0;
  for (let i = 0; i < query.length; i++) {
    const ch = query[i];
    if (ch === '"') inQuote = !inQuote;
    else if (!inQuote && /\s/.test(ch)) start = i + 1;
  }
  return { token: query.slice(start), start };
}

/** Strip double-quote phrase delimiters from a completion fragment and trim.
 *  Quotes are grouping-only (mirrors the daemon's tokenize()/completion path),
 *  so a fragment like `"some` or `creator:"some` must query as `some` /
 *  `creator:some` — otherwise typeahead dies on the opening quote. Used for the
 *  API query and namespace matching; `lastToken.start` still drives replacement,
 *  so the raw quote stays in the box until a suggestion is applied. (#49) */
export function stripQuotes(fragment: string): string {
  return fragment.replace(/"/g, '').trim();
}

function needsQuote(s: string): boolean {
  return /\s/.test(s);
}

/** Render a full `namespace:subtag` tag as one search-query token: the subtag
 *  gets double-quoted when it contains whitespace, so a context-menu "Search
 *  with tag" on `character:zero mission` queries as a single tag instead of
 *  tokenizing into two. */
export function quoteTagForQuery(tag: string): string {
  const i = tag.indexOf(':');
  const namespace = i < 0 ? '' : tag.slice(0, i);
  const subtag = i < 0 ? tag : tag.slice(i + 1);
  const value = needsQuote(subtag) ? `"${subtag}"` : subtag;
  return namespace ? `${namespace}:${value}` : value;
}

/** Replace the trailing token of `query` with `pick`. A namespace pick ends with
 *  `:` and keeps the dropdown open (no trailing space); a tag pick writes the full
 *  `namespace:subtag` (bare subtag when unnamespaced) plus a trailing space. */
export function applyCompletion(query: string, pick: CompletionPick): string {
  const { start } = lastToken(query);
  const head = query.slice(0, start);
  if (pick.kind === 'namespace') {
    return `${head}${pick.namespace}:`;
  }
  const value = needsQuote(pick.subtag) ? `"${pick.subtag}"` : pick.subtag;
  const full = pick.namespace ? `${pick.namespace}:${value}` : value;
  return `${head}${full} `;
}

/** Namespace suggestion list for a no-colon token: configured-category namespaces
 *  matching `token` first (in config order), then library namespaces, deduped.
 *  Category namespaces the library lacks get tag_count 0. */
export function mergeNamespaces(
  token: string,
  library: NamespaceSuggestion[],
  categoryNamespaces: string[],
): NamespaceSuggestion[] {
  const t = token.toLowerCase();
  const counts = new Map(library.map((n) => [n.namespace, n.tag_count]));
  const seen = new Set<string>();
  const out: NamespaceSuggestion[] = [];
  for (const ns of categoryNamespaces) {
    if (ns.toLowerCase().startsWith(t) && !seen.has(ns)) {
      seen.add(ns);
      out.push({ namespace: ns, tag_count: counts.get(ns) ?? 0 });
    }
  }
  for (const n of library) {
    if (!seen.has(n.namespace) && n.namespace.toLowerCase().startsWith(t)) {
      seen.add(n.namespace);
      out.push(n);
    }
  }
  return out;
}
