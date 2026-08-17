import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { tick } from 'svelte';
import { render, screen, fireEvent } from '@testing-library/svelte';
import OriginVisibilityMenu from './OriginVisibilityMenu.svelte';
import Inspector from './Inspector.svelte';
import { MANUAL_ORIGIN } from '../lib/origin-visibility';
import { view } from '../lib/settings.svelte';
import * as api from '../lib/api';
import type { TagDetail } from '../lib/types';
import type { FileDto } from '../lib/types';
import { thumbQueue } from '../lib/thumb-queue';
import { thumbStream } from '../lib/thumb-stream';

// Re-export the key through the module so we can reference without ambiguity
// (HIDDEN_ORIGINS_KEY is exported from settings too).
const STORAGE_KEY = 'naiad.view.hidden_origins';

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return {
    ...actual,
    tagsDetailed: vi.fn(),
    addTags: vi.fn(),
    removeTags: vi.fn(),
    rejectTag: vi.fn(),
    undoReject: vi.fn(),
    listRejections: vi.fn(),
    report: vi.fn(),
    listRepos: vi.fn(),
    pullFileTagsStream: vi.fn(),
  };
});

// ── helpers ────────────────────────────────────────────────────────────────

const makeTag = (tag: string, origin?: string): TagDetail => ({
  tag,
  presence: 'local',
  services: [],
  relations: false,
  origin,
});

const file = (hash = 'a', name = 'a.png'): FileDto => ({
  hash,
  name,
  size: 1024,
  path: `/${name}`,
  imported_at: 100,
  created_at: 80,
  modified_at: 90,
  mime: 'image/png',
});

/** Tags used in most tests: two 'hydrus' origins, one 'wd14', one manual. */
const sampleTags: TagDetail[] = [
  makeTag('character:samus', 'hydrus'),
  makeTag('series:metroid', 'hydrus'),
  makeTag('rating:safe', 'wd14'),
  makeTag('meta:solo'), // no origin → MANUAL_ORIGIN
];

/** Open the Origins menu and return the trigger button. */
async function openMenu(): Promise<HTMLElement> {
  const trigger = screen.getByRole('button', { name: /Origins/i });
  await fireEvent.click(trigger);
  return trigger;
}

/** Clear any hidden origins left over from a previous test. */
function clearHiddenOrigins(): void {
  for (const key of view.hiddenOrigins) {
    view.toggleHiddenOrigin(key);
  }
}

// ── setup / teardown ────────────────────────────────────────────────────────

beforeEach(() => {
  vi.useFakeTimers();
  Object.defineProperty(window, 'innerWidth', { configurable: true, value: 1280 });
  localStorage.clear();
  clearHiddenOrigins();
  view.inspectorCollapsed = false;
  vi.clearAllMocks();
  vi.spyOn(thumbQueue, 'request').mockImplementation(() => () => {});
  vi.spyOn(thumbStream, 'request').mockImplementation(() => () => {});
  vi.mocked(api.tagsDetailed).mockResolvedValue(sampleTags);
  vi.mocked(api.addTags).mockResolvedValue();
  vi.mocked(api.removeTags).mockResolvedValue();
  vi.mocked(api.rejectTag).mockResolvedValue({ reports: false });
  vi.mocked(api.undoReject).mockResolvedValue();
  vi.mocked(api.listRejections).mockResolvedValue([]);
  vi.mocked(api.report).mockResolvedValue();
  vi.mocked(api.listRepos).mockResolvedValue([]);
  vi.mocked(api.pullFileTagsStream).mockImplementation((_hashes, handlers) => {
    handlers.onSummary({ results: [], matched_files: 0, mappings: 0 });
    return () => {};
  });
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  clearHiddenOrigins();
});

// ── OriginVisibilityMenu unit tests ─────────────────────────────────────────

describe('OriginVisibilityMenu', () => {
  it('lists distinct origins with correct counts, named-alphabetical then manual last', async () => {
    render(OriginVisibilityMenu, { tags: sampleTags });
    await openMenu();

    const rows = screen.getAllByRole('menuitemcheckbox');
    // Sorted: 'hydrus' < 'wd14' alphabetically, then manual last
    expect(rows).toHaveLength(3);
    expect(rows[0]).toHaveTextContent('hydrus');
    expect(rows[0]).toHaveTextContent('2'); // two hydrus tags
    expect(rows[1]).toHaveTextContent('wd14');
    expect(rows[1]).toHaveTextContent('1');
    expect(rows[2]).toHaveTextContent('manual');
    expect(rows[2]).toHaveTextContent('1');
  });

  it('all rows are checked (visible) by default', async () => {
    render(OriginVisibilityMenu, { tags: sampleTags });
    await openMenu();

    for (const row of screen.getAllByRole('menuitemcheckbox')) {
      expect(row).toHaveAttribute('aria-checked', 'true');
    }
  });

  it('toggling a named origin unchecks it, persists to localStorage, and reflects in view', async () => {
    render(OriginVisibilityMenu, { tags: sampleTags });
    await openMenu();

    const hydrusRow = screen.getAllByRole('menuitemcheckbox')[0]; // 'hydrus' first
    await fireEvent.click(hydrusRow);
    await tick();

    expect(hydrusRow).toHaveAttribute('aria-checked', 'false');
    expect(view.hiddenOrigins).toContain('hydrus');

    // Persisted to localStorage
    const stored = localStorage.getItem(STORAGE_KEY);
    expect(stored).not.toBeNull();
    expect(JSON.parse(stored!)).toContain('hydrus');
  });

  it('toggling a hidden origin back shows it again and removes from localStorage', async () => {
    view.toggleHiddenOrigin('hydrus');
    render(OriginVisibilityMenu, { tags: sampleTags });
    await openMenu();

    const hydrusRow = screen.getAllByRole('menuitemcheckbox')[0];
    expect(hydrusRow).toHaveAttribute('aria-checked', 'false');

    await fireEvent.click(hydrusRow);
    await tick();

    expect(hydrusRow).toHaveAttribute('aria-checked', 'true');
    expect(view.hiddenOrigins).not.toContain('hydrus');
    const stored = JSON.parse(localStorage.getItem(STORAGE_KEY)!);
    expect(stored).not.toContain('hydrus');
  });

  it('MANUAL_ORIGIN sentinel hides origin-less tags correctly (round-trip)', async () => {
    render(OriginVisibilityMenu, { tags: sampleTags });
    await openMenu();

    // 'manual' row is last
    const rows = screen.getAllByRole('menuitemcheckbox');
    const manualRow = rows[rows.length - 1];
    expect(manualRow).toHaveTextContent('manual');
    expect(manualRow).toHaveAttribute('aria-checked', 'true');

    await fireEvent.click(manualRow);
    await tick();

    expect(manualRow).toHaveAttribute('aria-checked', 'false');
    expect(view.hiddenOrigins).toContain(MANUAL_ORIGIN);

    // Should persist the NUL-prefixed key
    const stored: string[] = JSON.parse(localStorage.getItem(STORAGE_KEY)!);
    expect(stored).toContain(MANUAL_ORIGIN);
  });

  it('MANUAL_ORIGIN never collides with a real origin named "manual"', async () => {
    const tagsWithLiteralManual: TagDetail[] = [
      makeTag('a:foo', 'manual'), // real origin literally named "manual"
      makeTag('b:bar'),            // truly origin-less → MANUAL_ORIGIN
    ];
    render(OriginVisibilityMenu, { tags: tagsWithLiteralManual });
    await openMenu();

    const rows = screen.getAllByRole('menuitemcheckbox');
    // 'manual' (the real origin) sorts alphabetically first,
    // then MANUAL_ORIGIN (displayed as 'manual') last
    expect(rows).toHaveLength(2);
    // Both may display the text "manual" but they are distinct keys
    await fireEvent.click(rows[0]); // hide the real 'manual' origin
    await tick();
    expect(view.hiddenOrigins).toContain('manual');
    expect(view.hiddenOrigins).not.toContain(MANUAL_ORIGIN);

    await fireEvent.click(rows[1]); // hide the sentinel (origin-less)
    await tick();
    expect(view.hiddenOrigins).toContain(MANUAL_ORIGIN);
    // Both are hidden but are distinct entries
    expect(view.hiddenOrigins).toHaveLength(2);
    expect(view.hiddenOrigins[0]).not.toBe(view.hiddenOrigins[1]);
  });

  it('shows N-hidden badge only when a hidden origin is present on the file', async () => {
    render(OriginVisibilityMenu, { tags: sampleTags });

    // No hidden origins → no badge
    expect(screen.queryByText(/hidden/)).toBeNull();

    view.toggleHiddenOrigin('hydrus');
    await tick();

    // 'hydrus' is present on this file → badge should appear
    expect(screen.getByText(/1 hidden/)).toBeInTheDocument();

    // Check aria-label describes the count
    const trigger = screen.getByRole('button', { name: /Origins: 1 hidden/i });
    expect(trigger).toBeInTheDocument();
  });

  it('N-hidden badge does NOT appear when hidden origin is absent from this file', async () => {
    // Hide 'nonexistent' — not present on sampleTags
    view.toggleHiddenOrigin('nonexistent');
    await tick();

    render(OriginVisibilityMenu, { tags: sampleTags });
    await tick();

    expect(screen.queryByText(/hidden/)).toBeNull();
    // aria-label should still be plain "Origins"
    expect(screen.getByRole('button', { name: 'Origins' })).toBeInTheDocument();
  });

  // ── Keyboard / focus ───────────────────────────────────────────────────────

  it('clicking the trigger toggles the menu open and closed', async () => {
    render(OriginVisibilityMenu, { tags: sampleTags });
    const trigger = screen.getByRole('button', { name: /Origins/i });

    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
    await fireEvent.click(trigger);
    expect(screen.getByRole('menu')).toBeInTheDocument();
    await fireEvent.click(trigger);
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });

  it('onfocusout closes the menu when focus leaves the component', async () => {
    const { container } = render(OriginVisibilityMenu, { tags: sampleTags });
    await openMenu();
    expect(screen.getByRole('menu')).toBeInTheDocument();

    const originsDiv = container.querySelector('.origins')!;
    await fireEvent.focusOut(originsDiv, { relatedTarget: document.body });
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });

  it('tabbing from trigger into a menu row keeps the menu open', async () => {
    render(OriginVisibilityMenu, { tags: sampleTags });
    const trigger = await openMenu();

    const row = screen.getAllByRole('menuitemcheckbox')[0];
    await fireEvent.focusOut(trigger, { relatedTarget: row });
    expect(screen.getByRole('menu')).toBeInTheDocument();
  });

  it('rows prevent default on mousedown to preserve trigger focus', async () => {
    render(OriginVisibilityMenu, { tags: sampleTags });
    await openMenu();

    const row = screen.getAllByRole('menuitemcheckbox')[0];
    const evt = new MouseEvent('mousedown', { bubbles: true, cancelable: true });
    row.dispatchEvent(evt);
    expect(evt.defaultPrevented).toBe(true);
  });
});

// ── Inspector integration tests ────────────────────────────────────────────

describe('Inspector + OriginVisibilityMenu integration', () => {
  it('hides origin tags from groups and updates TAGS count when an origin is toggled off', async () => {
    render(Inspector, {
      file: file('a'),
      onopen: vi.fn(),
      onerror: vi.fn(),
      onsearchtag: () => {},
    });
    await vi.advanceTimersByTimeAsync(100);
    await vi.waitFor(() => screen.getByText('character:samus'));

    // Initial count: all 4 tags visible
    expect(screen.getByText(/^TAGS - 4$/)).toBeInTheDocument();

    // Open Origins menu and hide 'hydrus' (2 tags)
    await openMenu();
    const hydrusRow = screen.getAllByRole('menuitemcheckbox')[0];
    await fireEvent.click(hydrusRow);
    await tick();

    // After hiding hydrus: count should be 2, hydrus tags gone
    expect(screen.getByText(/^TAGS - 2$/)).toBeInTheDocument();
    expect(screen.queryByText('character:samus')).toBeNull();
    expect(screen.queryByText('series:metroid')).toBeNull();
    // wd14 and manual tags remain
    expect(screen.getByText('rating:safe')).toBeInTheDocument();
    expect(screen.getByText('meta:solo')).toBeInTheDocument();
  });

  it('shows the N-hidden badge in the inspector section header when an origin is hidden', async () => {
    render(Inspector, {
      file: file('a'),
      onopen: vi.fn(),
      onerror: vi.fn(),
      onsearchtag: () => {},
    });
    await vi.advanceTimersByTimeAsync(100);
    await vi.waitFor(() => screen.getByText('character:samus'));

    // No badge initially
    expect(screen.queryByText(/hidden/)).toBeNull();

    await openMenu();
    const wd14Row = screen.getAllByRole('menuitemcheckbox')[1]; // 'wd14'
    await fireEvent.click(wd14Row);
    await tick();

    expect(screen.getByText(/1 hidden/)).toBeInTheDocument();
  });

  it('restoring a hidden origin brings its tags back and updates the count', async () => {
    view.toggleHiddenOrigin('hydrus');

    render(Inspector, {
      file: file('a'),
      onopen: vi.fn(),
      onerror: vi.fn(),
      onsearchtag: () => {},
    });
    await vi.advanceTimersByTimeAsync(100);
    await vi.waitFor(() => screen.getByText('rating:safe'));

    expect(screen.getByText(/^TAGS - 2$/)).toBeInTheDocument();

    // Restore hydrus
    await openMenu();
    const hydrusRow = screen.getAllByRole('menuitemcheckbox')[0];
    expect(hydrusRow).toHaveAttribute('aria-checked', 'false');
    await fireEvent.click(hydrusRow);
    await tick();

    expect(screen.getByText(/^TAGS - 4$/)).toBeInTheDocument();
    expect(screen.getByText('character:samus')).toBeInTheDocument();
    expect(screen.getByText('series:metroid')).toBeInTheDocument();
  });
});
