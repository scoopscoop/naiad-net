import { beforeEach, describe, expect, it } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { createRawSnippet } from 'svelte';
import TagDrawer from './TagDrawer.svelte';
import { drawer } from '../lib/detail-drawer.svelte';

const children = createRawSnippet(() => ({
  render: () => '<p>drawer body</p>',
}));

describe('TagDrawer', () => {
  beforeEach(() => {
    localStorage.clear();
    drawer.open = true;
    drawer.height = 280;
  });

  it('renders children while open and collapses to the minimized bar', async () => {
    render(TagDrawer, { name: 'a.png', tagCount: 3, paneHeight: 800, children });
    expect(screen.getByText('drawer body')).toBeInTheDocument();

    const collapse = screen.getByRole('button', { name: 'minimize tag drawer' });
    expect(collapse).toHaveAttribute('aria-expanded', 'true');
    expect(screen.queryByText('v')).toBeNull();

    await fireEvent.click(collapse);
    expect(screen.getByText('a.png')).toBeInTheDocument();
    expect(screen.getByText('TAGS - 3')).toBeInTheDocument();
    expect(drawer.open).toBe(false);

    const expand = screen.getByRole('button', { name: 'expand tag drawer' });
    expect(expand).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByText('^')).toBeNull();

    await fireEvent.click(expand);
    expect(drawer.open).toBe(true);
  });

  it('does not start a resize when the collapse button is pressed', async () => {
    // Regression: the collapse button sits inside the resize handle. If its
    // pointerdown reaches the handle, the handle captures the pointer and the
    // browser retargets the click away from the button - so the drawer never
    // closes. The button must stop pointerdown from bubbling to the handle.
    render(TagDrawer, { name: 'a.png', tagCount: 3, paneHeight: 800, children });
    const collapse = screen.getByRole('button', { name: 'minimize tag drawer' });

    collapse.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, clientY: 400 }));
    collapse.dispatchEvent(new MouseEvent('pointermove', { bubbles: true, clientY: 200 }));
    collapse.dispatchEvent(new MouseEvent('pointerup', { bubbles: true, clientY: 200 }));

    // The press must not have been interpreted as a drag.
    expect(drawer.height).toBe(280);
  });

  it('resizes by dragging the handle', async () => {
    const { container } = render(TagDrawer, { name: 'a.png', tagCount: 3, paneHeight: 800, children });
    const handle = screen.getByLabelText('resize tag drawer');
    handle.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, clientY: 400 }));
    handle.dispatchEvent(new MouseEvent('pointermove', { bubbles: true, clientY: 300 }));
    handle.dispatchEvent(new MouseEvent('pointerup', { bubbles: true, clientY: 300 }));

    expect(drawer.height).toBe(380);
    await waitFor(() => expect((container.querySelector('.drawer') as HTMLElement).style.height).toBe('380px'));
  });
});
