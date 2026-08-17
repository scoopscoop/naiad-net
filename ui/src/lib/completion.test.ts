import { describe, it, expect } from 'vitest';
import {
  lastToken,
  stripQuotes,
  applyCompletion,
  mergeNamespaces,
  quoteTagForQuery,
} from './completion';

describe('lastToken', () => {
  it('returns the whole string for a single token', () => {
    expect(lastToken('samu')).toEqual({ token: 'samu', start: 0 });
  });
  it('returns the trailing token, preserving earlier ones', () => {
    expect(lastToken('cat dog art')).toEqual({ token: 'art', start: 8 });
  });
  it('treats a quoted value as part of one token', () => {
    expect(lastToken('character:"zero mis')).toEqual({
      token: 'character:"zero mis',
      start: 0,
    });
  });
  it('after a trailing space the token is empty', () => {
    expect(lastToken('artist:foo ')).toEqual({ token: '', start: 11 });
  });
  it('a closed quote then a second token returns the second token', () => {
    expect(lastToken('character:"zero mission" art')).toEqual({ token: 'art', start: 25 });
  });
});

describe('stripQuotes', () => {
  it('strips a leading opening quote so a quoted fragment queries bare', () => {
    expect(stripQuotes('"some')).toBe('some');
  });
  it('keeps the namespace on a quoted namespaced fragment', () => {
    expect(stripQuotes('creator:"some')).toBe('creator:some');
  });
  it('preserves internal spaces of a multi-word fragment', () => {
    expect(stripQuotes('"some ta')).toBe('some ta');
  });
  it('a bare quote collapses to empty', () => {
    expect(stripQuotes('"')).toBe('');
  });
  it('leaves an unquoted fragment untouched', () => {
    expect(stripQuotes('some_')).toBe('some_');
  });
});

describe('quoteTagForQuery', () => {
  it('leaves a spaceless tag untouched', () => {
    expect(quoteTagForQuery('character:samus_aran')).toBe('character:samus_aran');
  });
  it('quotes the subtag when it contains spaces', () => {
    expect(quoteTagForQuery('character:zero mission')).toBe('character:"zero mission"');
  });
  it('quotes a whole unnamespaced tag containing spaces', () => {
    expect(quoteTagForQuery('zero mission')).toBe('"zero mission"');
  });
  it('leaves a spaceless unnamespaced tag untouched', () => {
    expect(quoteTagForQuery('mushroom')).toBe('mushroom');
  });
});

describe('applyCompletion', () => {
  it('namespace suggestion appends a colon, no space', () => {
    expect(applyCompletion('art', { kind: 'namespace', namespace: 'artist' })).toBe('artist:');
  });
  it('tag suggestion writes namespace:subtag + trailing space', () => {
    expect(
      applyCompletion('character:sam', {
        kind: 'tag',
        namespace: 'character',
        subtag: 'samus_aran',
      }),
    ).toBe('character:samus_aran ');
  });
  it('unnamespaced tag writes a bare subtag', () => {
    expect(applyCompletion('mush', { kind: 'tag', namespace: '', subtag: 'mushroom' })).toBe(
      'mushroom ',
    );
  });
  it('quotes a subtag containing spaces', () => {
    expect(
      applyCompletion('character:zero', {
        kind: 'tag',
        namespace: 'character',
        subtag: 'zero mission',
      }),
    ).toBe('character:"zero mission" ');
  });
  it('preserves earlier tokens', () => {
    expect(
      applyCompletion('cat character:sam', {
        kind: 'tag',
        namespace: 'character',
        subtag: 'samus',
      }),
    ).toBe('cat character:samus ');
  });
});

describe('mergeNamespaces', () => {
  it('puts matching category namespaces first, then library, deduped', () => {
    const lib = [
      { namespace: 'art', tag_count: 3 },
      { namespace: 'artist', tag_count: 10 },
    ];
    const out = mergeNamespaces('art', lib, ['artist', 'character']);
    expect(out.map((n) => n.namespace)).toEqual(['artist', 'art']);
    expect(out[0].tag_count).toBe(10);
  });
  it('includes a category namespace the library lacks, with count 0', () => {
    const out = mergeNamespaces('cr', [], ['creator']);
    expect(out).toEqual([{ namespace: 'creator', tag_count: 0 }]);
  });
  it('drops library namespaces that do not match the token', () => {
    const lib = [
      { namespace: 'artist', tag_count: 10 },
      { namespace: 'character', tag_count: 4 },
    ];
    const out = mergeNamespaces('art', lib, []);
    expect(out.map((n) => n.namespace)).toEqual(['artist']);
  });
});
