import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import TagCategoriesSection from './TagCategoriesSection.svelte';
import { categories, CATEGORIES_KEY } from '../lib/categories.svelte';

describe('TagCategoriesSection', () => {
  beforeEach(() => {
    localStorage.clear();
    categories.reset();
  });

  it('renders a row per category with name and color inputs', () => {
    render(TagCategoriesSection, {});
    expect((screen.getByLabelText('name for Artist') as HTMLInputElement).value).toBe('Artist');
    expect(screen.getByLabelText('color for Artist')).toBeInTheDocument();
  });

  it('shows aliased namespaces and labels the empty namespace as (general)', () => {
    render(TagCategoriesSection, {});
    expect(screen.getByText('creator')).toBeInTheDocument();
    expect(screen.getByText('copyright')).toBeInTheDocument();
    expect(screen.getByText('(general)')).toBeInTheDocument();
  });

  it('renaming a category persists and fires onsaved', async () => {
    const onsaved = vi.fn();
    render(TagCategoriesSection, { onsaved });
    const input = screen.getByLabelText('name for Artist');
    await fireEvent.change(input, { target: { value: 'Creators' } });
    expect(categories.list.find((c) => c.id === 'artist')?.name).toBe('Creators');
    expect(onsaved).toHaveBeenCalled();
  });

  it('adding a namespace appends a chip', async () => {
    render(TagCategoriesSection, {});
    const field = screen.getByLabelText('add namespace to Meta');
    await fireEvent.input(field, { target: { value: 'system' } });
    await fireEvent.submit(field.closest('form')!);
    expect(categories.list.find((c) => c.id === 'meta')?.namespaces).toContain('system');
  });

  it('the first row cannot move up and the last cannot move down', () => {
    render(TagCategoriesSection, {});
    expect((screen.getByLabelText('move Artist up') as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByLabelText('move Medium down') as HTMLButtonElement).disabled).toBe(true);
  });

  it('reset restores defaults after a delete', async () => {
    render(TagCategoriesSection, {});
    await fireEvent.click(screen.getByLabelText('delete Meta'));
    expect(categories.list.find((c) => c.id === 'meta')).toBeUndefined();
    await fireEvent.click(screen.getByText('Reset to defaults'));
    expect(categories.list.find((c) => c.id === 'meta')).toBeDefined();
  });
});
