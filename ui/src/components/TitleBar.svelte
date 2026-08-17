<script lang="ts">
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { tick } from 'svelte';
  import { tabs, type GalleryTab, type Tab } from '../lib/tabs.svelte';
  import { thumbUrl } from '../lib/api';
  import Icon from './Icon.svelte';
  import Logo from './Logo.svelte';
  import ActivityIndicator from './ActivityIndicator.svelte';

  let strip: HTMLDivElement;
  let tabNav: HTMLDivElement;
  let menuTrigger = $state<HTMLButtonElement>();
  let menuEl = $state<HTMLDivElement>();
  let overflowing = $state(false);
  let canScrollLeft = $state(false);
  let canScrollRight = $state(false);
  let menuOpen = $state(false);
  let menuFocusIndex = $state(0);
  let menuFocusClose = $state(false);

  /** One menu row per tab: the row itself is the menuitem in the roving
   *  sequence, its close button is reachable with ArrowRight. */
  const menuRows = $derived(
    tabs.list.map((tab) => ({
      tab,
      closable: tab.kind === 'detail' || tabs.galleryCount > 1,
    })),
  );

  /** Roving-focus target, clamped so exactly one menu item always carries
   *  tabindex=0 even while the tab list shrinks under an open menu. */
  const menuFocusRow = $derived(Math.min(menuFocusIndex, Math.max(0, menuRows.length - 1)));
  const menuFocusOnClose = $derived(menuFocusClose && (menuRows[menuFocusRow]?.closable ?? false));

  function updateOverflow() {
    if (!strip) return;
    const maxScroll = strip.scrollWidth - strip.clientWidth;
    // Compare content with the whole navigation allotment, not just the strip's
    // current width. Otherwise the overflow controls can consume enough room to
    // keep themselves visible after a window resize would let every tab fit.
    overflowing = strip.scrollWidth - (tabNav?.clientWidth ?? strip.clientWidth) > 1;
    canScrollLeft = strip.scrollLeft > 1;
    canScrollRight = strip.scrollLeft < maxScroll - 1;
    if (!overflowing && menuOpen) {
      const held = focusBelongsToNav();
      menuOpen = false;
      if (held) void restoreFocus();
    }
  }

  /** True while the menu still owns focus, including the moment after a focused
   *  row was removed and the browser has dropped focus onto <body>. */
  function focusBelongsToNav() {
    const active = document.activeElement;
    if (!active || active === document.body) return true;
    return tabNav?.contains(active) ?? false;
  }

  /** The trigger unmounts with the overflow it belongs to, so fall back to the
   *  strip rather than dropping focus on <body>. */
  async function restoreFocus() {
    await tick();
    const fallback =
      menuTrigger ??
      strip?.querySelector<HTMLButtonElement>(`[data-tab-id="${tabs.activeId}"] .tab-main`) ??
      strip?.querySelector<HTMLButtonElement>('.tab-main');
    fallback?.focus();
  }

  function observeSize(node: HTMLElement) {
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(updateOverflow);
    observer.observe(node);
    return { destroy: () => observer.disconnect() };
  }

  function scrollTabs(direction: -1 | 1) {
    strip.scrollBy({ left: direction * Math.max(120, strip.clientWidth * 0.75), behavior: 'auto' });
  }

  function scrollTabsWithWheel(event: WheelEvent) {
    if (!overflowing) return;
    const delta = Math.abs(event.deltaX) > Math.abs(event.deltaY) ? event.deltaX : event.deltaY;
    if (!delta) return;
    const scale = event.deltaMode === WheelEvent.DOM_DELTA_LINE
      ? 16
      : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
        ? strip.clientWidth
        : 1;
    event.preventDefault();
    strip.scrollLeft += delta * scale;
    updateOverflow();
  }

  async function focusMenuItem(index: number, onClose = false) {
    menuFocusIndex = index;
    menuFocusClose = onClose && (menuRows[index]?.closable ?? false);
    await tick();
    const selector = menuFocusClose ? '.menu-x' : '.menu-open';
    tabNav
      ?.querySelector<HTMLButtonElement>(`[data-menu-row="${index}"] ${selector}`)
      ?.focus();
  }

  function toggleMenu() {
    menuOpen = !menuOpen;
    if (!menuOpen) return;
    menuFocusIndex = 0;
    menuFocusClose = false;
  }

  /** The menu's dismissal scope: the popover itself, its trigger, and the
   *  scroll arrows (arrow-scrolling to find a tab must not dismiss the menu).
   *  The strip is deliberately outside — interacting with a strip tab while
   *  the menu is open dismisses it. */
  function withinMenuScope(target: EventTarget | null): boolean {
    if (!(target instanceof Node)) return false;
    if (menuEl?.contains(target) || menuTrigger?.contains(target)) return true;
    if (!(tabNav?.contains(target) ?? false)) return false;
    const el = target instanceof Element ? target : target.parentElement;
    return el?.closest('.scroll-arrow') != null;
  }

  function activateFromStrip(id: number, event: MouseEvent) {
    tabs.activate(id);
    if (!menuOpen) return;
    menuOpen = false;
    (event.currentTarget as HTMLElement | null)?.focus();
  }

  async function closeFromStrip(id: number) {
    const wasOpen = menuOpen;
    tabs.close(id);
    if (!wasOpen) return;
    await tick();
    updateOverflow();
    menuOpen = false;
    await tick();
    if (!document.activeElement || document.activeElement === document.body) {
      await restoreFocus();
    }
  }

  function activateFromMenu(id: number) {
    tabs.activate(id);
    menuOpen = false;
    menuTrigger?.focus();
  }

  async function closeFromMenu(id: number) {
    const position = tabs.list.findIndex((t) => t.id === id);
    tabs.close(id);
    await tick();
    updateOverflow();
    await tick();
    // updateOverflow owns the focus restore when it collapses the menu itself.
    if (!menuOpen) return;
    if (!menuRows.length) {
      menuOpen = false;
      await restoreFocus();
      return;
    }
    await focusMenuItem(Math.min(position, menuRows.length - 1));
  }

  /** Tabbing (or clicking) out of the popover dismisses it; a null
   *  relatedTarget means the row under focus was just removed, not a tab-out. */
  function handleMenuFocusOut(event: FocusEvent) {
    const next = event.relatedTarget;
    if (!(next instanceof Node) || withinMenuScope(next)) return;
    menuOpen = false;
  }

  /** ArrowDown/ArrowUp enter the menu from the trigger whether it is collapsed
   *  or already expanded, landing on the first/last row. */
  function handleTriggerKeydown(event: KeyboardEvent) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      menuOpen = true;
      void focusMenuItem(event.key === 'ArrowDown' ? 0 : menuRows.length - 1);
      return;
    }
    if (event.key === 'Escape' && menuOpen) {
      menuOpen = false;
      event.stopPropagation();
    }
  }

  function handleMenuKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      menuOpen = false;
      menuTrigger?.focus();
      event.stopPropagation();
      return;
    }
    const count = menuRows.length;
    if (!count) return;
    const active = document.activeElement as HTMLElement | null;
    const from = Number(active?.dataset?.menuRow ?? menuFocusRow);
    const onClose = active?.dataset?.menuClose !== undefined;
    const row = menuRows[from];

    switch (event.key) {
      case 'ArrowDown':
      case 'ArrowUp':
        event.preventDefault();
        void focusMenuItem((from + (event.key === 'ArrowDown' ? 1 : -1) + count) % count);
        return;
      case 'Home':
      case 'End':
        event.preventDefault();
        void focusMenuItem(event.key === 'Home' ? 0 : count - 1);
        return;
      case 'ArrowRight':
        if (onClose || !row?.closable) return;
        event.preventDefault();
        void focusMenuItem(from, true);
        return;
      case 'ArrowLeft':
        if (!onClose) return;
        event.preventDefault();
        void focusMenuItem(from);
        return;
      case 'Delete':
      case 'Backspace':
        if (!row?.closable) return;
        event.preventDefault();
        void closeFromMenu(row.tab.id);
        return;
      default:
    }
  }

  function tabLabel(tab: Tab): string {
    return tab.kind === 'gallery' ? galleryLabel(tab) : tab.file.name;
  }

  $effect(() => {
    if (!menuOpen) return;
    const outside = (event: PointerEvent) => {
      if (withinMenuScope(event.target)) return;
      const held = menuEl?.contains(document.activeElement) ?? false;
      menuOpen = false;
      if (held) void restoreFocus();
    };
    window.addEventListener('pointerdown', outside, true);
    return () => window.removeEventListener('pointerdown', outside, true);
  });

  $effect(() => {
    const activeId = tabs.activeId;
    tabs.list.length;
    void tick().then(() => {
      updateOverflow();
      strip?.querySelector<HTMLElement>(`[data-tab-id="${activeId}"]`)?.scrollIntoView?.({
        block: 'nearest',
        inline: 'nearest',
      });
      updateOverflow();
    });
  });

  // Window controls. Guarded so a plain-browser dev session (no Tauri context)
  // degrades quietly instead of throwing.
  function winCall(method: 'minimize' | 'toggleMaximize' | 'close') {
    try {
      void getCurrentWindow()[method]();
    } catch {
      // not running inside a Tauri window
    }
  }

  function galleryLabel(tab: GalleryTab): string {
    return tab.query.trim() || 'all media';
  }
</script>

<header class="titlebar" data-tauri-drag-region>
  <div class="brand" aria-hidden="true"><Logo cut="small" size={20} /></div>

  <div
    class="tab-nav"
    role="navigation"
    aria-label="tabs"
    bind:this={tabNav}
    use:observeSize
  >
    {#if overflowing}
      <button
        class="scroll-arrow"
        aria-label="scroll tabs left"
        disabled={!canScrollLeft}
        onclick={() => scrollTabs(-1)}
      ><Icon name="chevron-left" size={14} /></button>
    {/if}

    <div class="strip" bind:this={strip} onscroll={updateOverflow} onwheel={scrollTabsWithWheel}>
      {#each tabs.list as tab (tab.id)}
        {#if tab.kind === 'gallery'}
        <div class="tab gallery" class:active={tabs.activeId === tab.id} data-tab-id={tab.id}>
          <button class="tab-main" onclick={(e) => activateFromStrip(tab.id, e)}>
            <span class="ico"><Icon name="grid" size={14} /></span>
            <span class="name">{galleryLabel(tab)}</span>
          </button>
          {#if tabs.galleryCount > 1}
            <button
              class="x"
              aria-label={`close ${galleryLabel(tab)}`}
              onclick={() => closeFromStrip(tab.id)}
            >
              <Icon name="close" size={16} />
            </button>
          {/if}
        </div>
      {:else}
        <div class="tab detail" class:active={tabs.activeId === tab.id} data-tab-id={tab.id}>
          <button class="tab-main" onclick={(e) => activateFromStrip(tab.id, e)}>
            <img class="chip" src={thumbUrl(tab.file.hash)} alt="" />
            <span class="name">{tab.file.name}</span>
          </button>
          <button class="x" aria-label={`close ${tab.file.name}`} onclick={() => closeFromStrip(tab.id)}>
            <Icon name="close" size={16} />
          </button>
        </div>
        {/if}
      {/each}
    </div>

    {#if overflowing}
      <button
        class="scroll-arrow"
        aria-label="scroll tabs right"
        disabled={!canScrollRight}
        onclick={() => scrollTabs(1)}
      ><Icon name="chevron-right" size={14} /></button>
      <button
        class="tab-menu-trigger"
        bind:this={menuTrigger}
        aria-label="show all tabs"
        aria-haspopup="menu"
        aria-expanded={menuOpen}
        aria-controls={menuOpen ? 'tab-overflow-menu' : undefined}
        onclick={() => toggleMenu()}
        onkeydown={handleTriggerKeydown}
      ><Icon name="chevron-down" size={14} /></button>

      {#if menuOpen}
        <div
          class="tab-menu"
          bind:this={menuEl}
          id="tab-overflow-menu"
          role="menu"
          aria-label="open tabs"
          tabindex="-1"
          onkeydown={handleMenuKeydown}
          onfocusout={handleMenuFocusOut}
        >
          {#each menuRows as row, index (row.tab.id)}
            <div class="menu-row" role="none" data-menu-row={index}>
              <button
                class="menu-open"
                role="menuitem"
                data-menu-row={index}
                tabindex={menuFocusRow === index && !menuFocusOnClose ? 0 : -1}
                class:active={tabs.activeId === row.tab.id}
                onclick={() => activateFromMenu(row.tab.id)}
              >
                {#if row.tab.kind === 'gallery'}
                  <span class="menu-icon"><Icon name="grid" size={14} /></span>
                {:else}
                  <img class="chip" src={thumbUrl(row.tab.file.hash)} alt="" />
                {/if}
                <span class="menu-name">{tabLabel(row.tab)}</span>
              </button>
              {#if row.closable}
                <button
                  class="menu-x"
                  role="menuitem"
                  data-menu-row={index}
                  data-menu-close=""
                  tabindex={menuFocusRow === index && menuFocusOnClose ? 0 : -1}
                  aria-label={`close ${tabLabel(row.tab)} from menu`}
                  onclick={() => closeFromMenu(row.tab.id)}
                ><Icon name="close" size={14} /></button>
              {/if}
            </div>
          {/each}
        </div>
      {/if}
    {/if}
  </div>

  <div class="spacer"></div>

  <ActivityIndicator />

  <div class="winctl">
    <button aria-label="minimize" onclick={() => winCall('minimize')}><Icon name="minimize" size={15} /></button>
    <button aria-label="maximize" onclick={() => winCall('toggleMaximize')}><Icon name="maximize" size={14} /></button>
    <button class="danger" aria-label="close window" onclick={() => winCall('close')}><Icon name="close" size={15} /></button>
  </div>
</header>

<style>
  .titlebar {
    display: flex;
    align-items: center;
    gap: 14px;
    height: 48px;
    padding: 0 6px 0 14px;
    background: var(--ink-750);
    border-bottom: 1px solid var(--line);
  }
  .brand {
    flex: none;
    display: flex;
    align-items: center;
  }
  .tab-nav {
    position: relative;
    display: flex;
    align-items: center;
    min-width: 0;
  }
  .strip {
    display: flex;
    align-items: center;
    gap: 3px;
    min-width: 0;
    overflow-x: auto;
    overflow-y: hidden;
    scrollbar-width: none;
    overscroll-behavior-x: contain;
  }
  .strip::-webkit-scrollbar {
    display: none;
  }
  .scroll-arrow,
  .tab-menu-trigger {
    display: flex;
    align-items: center;
    justify-content: center;
    flex: none;
    width: 24px;
    height: 26px;
    padding: 0;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-mute);
    cursor: pointer;
  }
  .scroll-arrow:hover:not(:disabled),
  .tab-menu-trigger:hover,
  .tab-menu-trigger[aria-expanded='true'] {
    background: var(--raise);
    color: var(--accent);
  }
  .scroll-arrow:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .tab-menu {
    position: absolute;
    top: calc(100% + 5px);
    right: 0;
    z-index: 25;
    width: min(320px, 70vw);
    max-height: min(420px, 70vh);
    overflow-y: auto;
    padding: 4px;
    background: var(--ink-900);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
    box-shadow: var(--shadow-popover);
  }
  .menu-row {
    display: flex;
    align-items: center;
    border-radius: 6px;
  }
  .menu-row:hover,
  .menu-row:has(.menu-open.active) {
    background: var(--raise);
  }
  .tab-menu button {
    display: flex;
    align-items: center;
    gap: 8px;
    height: 30px;
    padding: 0 8px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-mute);
    font: 500 12px/1 var(--mono);
    text-align: left;
    cursor: pointer;
  }
  .tab-menu .menu-open {
    flex: 1;
    min-width: 0;
  }
  .tab-menu .menu-row:hover .menu-open,
  .tab-menu .menu-open:hover {
    color: var(--text);
  }
  .tab-menu .menu-open.active {
    color: var(--accent);
  }
  .tab-menu .menu-x {
    flex: none;
    justify-content: center;
    width: 26px;
    padding: 0;
  }
  .tab-menu .menu-x:hover {
    background: var(--err-bg);
    color: var(--err);
  }
  .menu-icon {
    display: flex;
    flex: none;
  }
  .menu-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 0;
    flex: none;
    min-width: 120px;
    height: 30px;
    padding: 0 6px 0 0;
    border: 1px solid transparent;
    border-radius: 7px;
    background: transparent;
    color: var(--text-mute);
    font: 500 12px/1 var(--mono);
    white-space: nowrap;
  }
  .tab .ico {
    color: var(--text-faint);
  }
  .tab.active {
    background: var(--ink-900);
    border-color: var(--line-soft);
    color: var(--text);
    box-shadow: inset 0 -2px 0 var(--accent);
  }
  .tab.active .ico {
    color: var(--accent);
  }
  .tab.gallery:not(:has(.x)) {
    padding-right: 12px;
  }
  .tab-main {
    display: flex;
    align-items: center;
    gap: 7px;
    flex: 1;
    min-width: 0;
    height: 100%;
    padding: 0 6px 0 10px;
    border: 0;
    border-radius: 6px 0 0 6px;
    background: transparent;
    color: inherit;
    font: inherit;
    cursor: pointer;
    white-space: nowrap;
  }
  .tab.gallery:not(:has(.x)) .tab-main {
    border-radius: 6px;
  }
  .chip {
    width: 14px;
    height: 14px;
    border-radius: 4px;
    object-fit: cover;
    flex: none;
  }
  .name {
    font: 500 12px/1 var(--mono);
    max-width: 160px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .x {
    display: flex;
    align-items: center;
    justify-content: center;
    flex: none;
    width: 24px;
    height: 24px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-mute);
    cursor: pointer;
  }
  .x:hover {
    background: var(--err-bg);
    color: var(--err);
  }
  .spacer {
    flex: 1;
  }
  .winctl {
    display: flex;
    align-items: center;
    gap: 1px;
  }
  .winctl button {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-mute);
    cursor: pointer;
  }
  .winctl button:hover {
    background: var(--raise);
  }
  .winctl button.danger:hover {
    background: var(--err-bg);
    color: var(--err);
  }
</style>
