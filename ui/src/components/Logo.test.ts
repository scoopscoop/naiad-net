import { describe, it, expect } from 'vitest';
import { render } from '@testing-library/svelte';
import Logo from './Logo.svelte';

describe('Logo', () => {
  it('renders the small cut stroke when cut=small', () => {
    const { container } = render(Logo, { props: { cut: 'small' } });
    const path = container.querySelector('path')!;
    expect(path.getAttribute('stroke-width')).toBe('11.5');
    expect(container.querySelectorAll('circle')).toHaveLength(2);
  });

  it('renders the full cut stroke by default', () => {
    const { container } = render(Logo);
    expect(container.querySelector('path')!.getAttribute('stroke-width')).toBe('8.5');
  });

  it('uses the accent token, never a gradient', () => {
    const { container } = render(Logo);
    expect(container.querySelector('path')!.getAttribute('stroke')).toBe('var(--accent)');
    expect(container.querySelector('linearGradient')).toBeNull();
  });
});
