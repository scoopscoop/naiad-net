import { describe, it, expect } from 'vitest';
import { render } from '@testing-library/svelte';
import Icon from './Icon.svelte';

const NAMES = ['minimize', 'maximize', 'close', 'grid', 'search', 'settings'];

describe('Icon', () => {
  it('renders an svg with shape children for every known name', () => {
    for (const name of NAMES) {
      const { container, unmount } = render(Icon, { props: { name } });
      const svg = container.querySelector('svg');
      expect(svg, name).not.toBeNull();
      expect(svg!.children.length, name).toBeGreaterThan(0);
      unmount();
    }
  });

  it('renders an empty svg for an unknown name', () => {
    const { container } = render(Icon, { props: { name: 'nope' } });
    const svg = container.querySelector('svg');
    expect(svg).not.toBeNull();
    expect(svg!.children.length).toBe(0);
  });

  it('applies the size prop to width and height', () => {
    const { container } = render(Icon, { props: { name: 'close', size: 20 } });
    const svg = container.querySelector('svg')!;
    expect(svg.getAttribute('width')).toBe('20');
    expect(svg.getAttribute('height')).toBe('20');
  });
});
