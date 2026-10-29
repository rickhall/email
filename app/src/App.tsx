import { createEffect, createMemo, createSignal, lazy, Match, onCleanup, onMount, Show, Switch } from 'solid-js';
import type { Account } from './api';
import {
  accounts,
  folderById,
  initApp,
  onAccountAdded,
  searchActive,
  searchQuery,
  searchScope,
  searchFieldsArg,
  selectedFolderId,
  threaded,
  setThreaded,
  conversationScope,
  setConversationScope,
} from './state';
import { focusSearch, SearchBar } from './components/SearchBar';
import { Rail } from './components/Rail';
import { Flyout } from './components/Flyout';
import { FolderSwitcherHost, openFolderSwitcher } from './components/FolderSwitcher';
import { flyout, RAIL_W, setFlyout, sidebarMode, toggleSidebar } from './lib/layout';
import { api, errorMessage, events } from './api';
import { showToast } from './components/Toast';
import { ViewMenuPopover } from './components/ViewMenu';
import { ConfirmHost } from './components/Actions';
import { isTextInput, openShortcuts, ShortcutsHost } from './components/ShortcutsSheet';
import { openSettings, SettingsHost } from './components/Settings';
import { commandFor } from './lib/shortcuts';
import { compose, newMessage } from './lib/compose';
// The editor (TipTap) is the app's biggest dependency: loaded the first time it's needed.
const Composer = lazy(() => import('./components/Composer').then((m) => ({ default: m.Composer })));
import { ManageTagsHost } from './components/Tags';
import { AttachmentToast } from './components/Attachments';
import { loadPref, savePref } from './lib/format';
import { Sidebar } from './components/Sidebar';
import { MessageList, type SearchSpec } from './components/MessageList';
import { Reader } from './components/Reader';
import { ConversationReader } from './components/Conversation';
import { Resizer } from './components/Resizer';
import { AddAccountModal, Onboarding } from './components/AddAccount';
import { DRAG } from './lib/platform';

const SIDEBAR = { min: 180, max: 420, def: 236 };
const LIST = { min: 300, max: 760, def: 400 };
const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

export function App() {
  const [sidebarW, setSidebarW] = createSignal(clamp(loadPref('ui.sidebarWidth', SIDEBAR.def), SIDEBAR.min, SIDEBAR.max));
  const [listW, setListW] = createSignal(clamp(loadPref('ui.listWidth', LIST.def), LIST.min, LIST.max));
  const [showAdd, setShowAdd] = createSignal(false);
  let readerEl: HTMLElement | undefined;

  onMount(() => void initApp());

  // Ctrl/⌘+1: full ↔ rail · Ctrl/⌘+Shift+1: folder flyout · Ctrl/⌘+G: folder switcher ·
  // Ctrl/⌘+,: settings — plus any single keys given to these in Settings.
  onMount(() => {
    const flyoutKey = () => {
      if (sidebarMode() === 'rail') setFlyout(flyout() ? false : 'keyboard');
    };
    const onKey = (e: KeyboardEvent) => {
      if (document.querySelector('.modal-backdrop')) return;
      const ready = (accounts()?.length ?? 0) > 0;
      if (!isTextInput(e.target)) {
        const run: Record<string, (() => void) | undefined> = {
          // `?`: the shortcuts sheet (the sheet itself handles `?` to close)
          shortcuts: openShortcuts,
          settings: openSettings,
          rail: ready ? toggleSidebar : undefined,
          compose: ready ? newMessage : undefined,
          flyout: ready ? flyoutKey : undefined,
          goto: ready ? openFolderSwitcher : undefined,
        };
        const f = run[commandFor(e) ?? ''];
        if (f) {
          e.preventDefault();
          f();
          return;
        }
      }
      if (!(e.metaKey || e.ctrlKey) || e.altKey) return;
      if (e.key === ',' && !e.shiftKey) {
        e.preventDefault();
        openSettings();
        return;
      }
      if (!ready) return;
      if (e.key.toLowerCase() === 'n' && !e.shiftKey) {
        e.preventDefault();
        newMessage();
        return;
      }
      if (e.code === 'Digit1') {
        e.preventDefault();
        if (e.shiftKey) flyoutKey();
        else toggleSidebar();
      } else if (!e.shiftKey && e.key.toLowerCase() === 'g') {
        e.preventDefault();
        openFolderSwitcher();
      }
    };
    window.addEventListener('keydown', onKey);
    onCleanup(() => window.removeEventListener('keydown', onKey));
  });

  // Native macOS menu bar (no-op elsewhere: the event is never sent).
  onMount(() => {
    const un = events.onMenuAction(({ id }) => {
      switch (id) {
        case 'search':
          focusSearch();
          break;
        case 'toggle-threaded':
          setThreaded(!threaded());
          break;
        case 'toggle-scope':
          setConversationScope(conversationScope() === 'all' ? 'folder' : 'all');
          break;
        case 'add-account':
          // With no accounts the onboarding form is already the whole window.
          if ((accounts()?.length ?? 0) > 0) setShowAdd(true);
          break;
        case 'shortcuts':
          openShortcuts();
          break;
        case 'settings':
          openSettings();
          break;
        case 'new-message':
          newMessage();
          break;
        case 'show-log':
          void api.showLog().catch((e) => showToast({ kind: 'error', text: errorMessage(e) }));
          break;
      }
    });
    onCleanup(() => void un.then((f) => f()));
  });

  const folder = createMemo(() => folderById(selectedFolderId()));

  // Quick search: debounce typing into a spec; the results view stays mounted while a query
  // is active and swaps results in place when the spec changes (no remount, no flicker).
  const [debouncedQ, setDebouncedQ] = createSignal('');
  let debounce: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    const q = searchQuery().trim();
    clearTimeout(debounce);
    if (!q) setDebouncedQ('');
    else debounce = setTimeout(() => setDebouncedQ(q), 120);
  });
  onCleanup(() => clearTimeout(debounce));
  const searchKey = createMemo(() => {
    const q = debouncedQ();
    const f = folder();
    if (!q || !f || !searchActive()) return null;
    const spec: SearchSpec = {
      accountId: f.accountId,
      folderId: searchScope() === 'folder' ? f.id : null,
      query: q,
      fields: searchFieldsArg(),
    };
    return JSON.stringify(spec);
  });
  const searchSpec = createMemo(() => {
    const k = searchKey();
    return k ? (JSON.parse(k) as SearchSpec) : null;
  });
  const conversationReader = () => threaded() && !searchActive();

  const added = (a: Account) => {
    setShowAdd(false);
    void onAccountAdded(a);
  };

  const openReader = () => {
    // Focus the reader so Space / arrows scroll the body; iframe takes focus if present.
    if (readerEl?.classList.contains('conv-scroll') && readerEl.isConnected) {
      readerEl.focus();
      return;
    }
    const frame = readerEl?.querySelector('iframe');
    const plain = readerEl?.querySelector<HTMLElement>('.plain');
    (frame ?? plain ?? readerEl)?.focus();
  };

  return (
    <Switch>
      <Match when={accounts() === null}>
        <div class="splash" data-tauri-drag-region={DRAG} />
      </Match>
      <Match when={accounts()?.length === 0}>
        <Onboarding onAdded={added} />
      </Match>
      <Match when={true}>
        <div
          class="app"
          classList={{ 'rail-mode': sidebarMode() === 'rail' }}
          style={{ '--sidebar-w': `${sidebarMode() === 'rail' ? RAIL_W : sidebarW()}px`, '--list-w': `${listW()}px` }}
        >
          <div class="pane pane-sidebar">
            <Show
              when={sidebarMode() === 'rail'}
              fallback={
                <>
                  <Sidebar onAddAccount={() => setShowAdd(true)} />
                  <Resizer
                    label="Resize sidebar"
                    width={sidebarW}
                    onResize={(w) => setSidebarW(clamp(w, SIDEBAR.min, SIDEBAR.max))}
                    onDone={() => savePref('ui.sidebarWidth', sidebarW())}
                  />
                </>
              }
            >
              <Rail />
            </Show>
          </div>
          <Show when={flyout()}>
            <Flyout width={sidebarW()} onAddAccount={() => setShowAdd(true)} />
          </Show>
          <div class="pane pane-list">
            <div class="list-pane">
              <SearchBar />
              <div class="list-stack">
                {/* The folder list stays mounted (just hidden) during search so clearing restores it exactly. */}
                <div class="list-layer folder-layer" classList={{ hidden: searchKey() != null }}>
                  <Show
                    when={folder()}
                    keyed
                    fallback={<div class="list-placeholder" data-tauri-drag-region={DRAG} />}
                  >
                    {(f) => (
                      <Show when={threaded()} fallback={<MessageList folder={f} threaded={false} onOpen={openReader} />}>
                        <MessageList folder={f} threaded onOpen={openReader} />
                      </Show>
                    )}
                  </Show>
                </div>
                <Show when={searchKey() != null && folder()}>
                  {(f) => (
                    <div class="list-layer search-layer">
                      <MessageList
                        folder={f()}
                        threaded={false}
                        search={searchSpec() ?? undefined}
                        onOpen={openReader}
                      />
                    </div>
                  )}
                </Show>
              </div>
            </div>
            <Resizer
              label="Resize message list"
              width={listW}
              onResize={(w) => setListW(clamp(w, LIST.min, LIST.max))}
              onDone={() => savePref('ui.listWidth', listW())}
            />
          </div>
          <div class="pane pane-reader">
            {/* The composer takes the reader's place while a message is being written. */}
            <Show
              when={compose()?.key}
              keyed
              fallback={
                <Show when={conversationReader()} fallback={<Reader ref={(el) => (readerEl = el)} />}>
                  <ConversationReader ref={(el) => (readerEl = el)} />
                </Show>
              }
            >
              {(_key) => <Composer />}
            </Show>
            <AttachmentToast />
          </div>
          <ViewMenuPopover />
          <ManageTagsHost />
          <ConfirmHost />
          <ShortcutsHost />
          <SettingsHost />
          <FolderSwitcherHost />
          <Show when={showAdd()}>
            <AddAccountModal onAdded={added} onClose={() => setShowAdd(false)} />
          </Show>
        </div>
      </Match>
    </Switch>
  );
}
