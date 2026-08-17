import { describe, it, expect } from 'vitest';
import { render } from '@testing-library/svelte';
import Spinner from './Spinner.svelte';

describe('Spinner', () => {
  it('defaults to a 12px ring', () => {
    const { container } = render(Spinner);
    const el = container.querySelector('.spinner') as HTMLElement;
    expect(el).not.toBeNull();
    expect(el.style.getPropertyValue('--sz')).toBe('12px');
  });

  it('honours an explicit size', () => {
    const { container } = render(Spinner, { props: { size: 16 } });
    const el = container.querySelector('.spinner') as HTMLElement;
    expect(el.style.getPropertyValue('--sz')).toBe('16px');
  });

  it('is decorative: the accessible name comes from the caller', () => {
    const { container } = render(Spinner);
    expect(container.querySelector('.spinner')!.getAttribute('aria-hidden')).toBe('true');
  });
});
