import { describe, expect, it, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import GallerySortMenu from './GallerySortMenu.svelte';

describe('GallerySortMenu', () => {
  it('renders the current sort label', () => {
    render(GallerySortMenu, {
      sort: { key: 'name', direction: 'asc' },
      onchange: () => {},
    });
    expect(screen.getByRole('button', { name: /sort: name ascending/i })).toBeInTheDocument();
  });

  it('selects a new key with its default direction', async () => {
    const onchange = vi.fn();
    render(GallerySortMenu, {
      sort: { key: 'name', direction: 'asc' },
      onchange,
    });
    await fireEvent.click(screen.getByRole('button', { name: /sort:/i }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /size/i }));
    expect(onchange).toHaveBeenCalledWith({ key: 'size', direction: 'desc' });
  });

  it('selecting the current key toggles direction', async () => {
    const onchange = vi.fn();
    render(GallerySortMenu, {
      sort: { key: 'name', direction: 'asc' },
      onchange,
    });
    await fireEvent.click(screen.getByRole('button', { name: /sort:/i }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /name/i }));
    expect(onchange).toHaveBeenCalledWith({ key: 'name', direction: 'desc' });
  });

  it('does not open when disabled', async () => {
    render(GallerySortMenu, {
      sort: { key: 'name', direction: 'asc' },
      disabled: true,
      onchange: () => {},
    });
    await fireEvent.click(screen.getByRole('button', { name: /sort:/i }));
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });

  it('tabbing from trigger into a menu item keeps the menu open', async () => {
    render(GallerySortMenu, {
      sort: { key: 'name', direction: 'asc' },
      onchange: () => {},
    });
    const trigger = screen.getByRole('button', { name: /sort:/i });
    await fireEvent.click(trigger);
    expect(screen.getByRole('menu')).toBeInTheDocument();

    // focusOut bubbles to the .sort container; relatedTarget inside the component
    // means the containment check passes and the menu must stay open.
    const menuItem = screen.getByRole('menuitem', { name: /size/i });
    await fireEvent.focusOut(trigger, { relatedTarget: menuItem });

    expect(screen.getByRole('menu')).toBeInTheDocument();
  });

  it('focusOut from menu item to trigger keeps menu open; subsequent click closes it', async () => {
    render(GallerySortMenu, {
      sort: { key: 'name', direction: 'asc' },
      onchange: () => {},
    });
    const trigger = screen.getByRole('button', { name: /sort:/i });
    await fireEvent.click(trigger);
    expect(screen.getByRole('menu')).toBeInTheDocument();

    // Focus moving from a menu item back to the trigger stays within the component;
    // the containment check must pass and the menu must remain open.
    const menuItem = screen.getByRole('menuitem', { name: /size/i });
    await fireEvent.focusOut(menuItem, { relatedTarget: trigger });
    expect(screen.getByRole('menu')).toBeInTheDocument();

    // Now clicking the trigger should toggle (close) the menu, not reopen it.
    await fireEvent.click(trigger);
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });

  it('focus leaving the component closes the menu', async () => {
    const { container } = render(GallerySortMenu, {
      sort: { key: 'name', direction: 'asc' },
      onchange: () => {},
    });
    await fireEvent.click(screen.getByRole('button', { name: /sort:/i }));
    expect(screen.getByRole('menu')).toBeInTheDocument();

    // Focus moves outside the component entirely
    const sortDiv = container.querySelector('.sort')!;
    await fireEvent.focusOut(sortDiv, { relatedTarget: document.body });

    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });
});
