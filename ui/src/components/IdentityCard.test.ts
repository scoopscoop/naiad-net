import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import IdentityCard from './IdentityCard.svelte';
import type { FileDto } from '../lib/types';

const file: FileDto = {
  hash: 'abc',
  name: 'a.png',
  size: 1536,
  path: '/a.png',
  imported_at: 100,
  created_at: 80,
  modified_at: 90,
  mime: 'image/png',
};

describe('IdentityCard', () => {
  it('renders hash, path, human size, format, and dates', () => {
    render(IdentityCard, { file });
    expect(screen.getByText('BLAKE3')).toBeInTheDocument();
    expect(screen.getByText('abc')).toBeInTheDocument();
    expect(screen.getByText('/a.png')).toBeInTheDocument();
    expect(screen.getByText('1.5 KB')).toBeInTheDocument();
    expect(screen.getByText('image/png')).toBeInTheDocument();
    expect(screen.getAllByText('1970-01-01 00:01')).toHaveLength(3);
  });

  it('uses a dash for missing mime and omits null filesystem dates', () => {
    render(IdentityCard, {
      file: { ...file, mime: null, created_at: null, modified_at: null },
    });

    expect(screen.getByText('-')).toBeInTheDocument();
    expect(screen.queryByText('CREATED')).toBeNull();
    expect(screen.queryByText('MODIFIED')).toBeNull();
  });
});
