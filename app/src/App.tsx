import { createEffect, createMemo, createSignal, Match, onCleanup, onMount, Show, Switch } from 'solid-js';
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
import { events } from './api';
import { ViewMenuPopover } from './components/ViewMenu';
import { ConfirmHost } from './components/Actions';
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
        <div class="app" style={{ '--sidebar-w': `${sidebarW()}px`, '--list-w': `${listW()}px` }}>
          <div class="pane pane-sidebar">
            <Sidebar onAddAccount={() => setShowAdd(true)} />
            <Resizer
              label="Resize sidebar"
              width={sidebarW}
              onResize={(w) => setSidebarW(clamp(w, SIDEBAR.min, SIDEBAR.max))}
              onDone={() => savePref('ui.sidebarWidth', sidebarW())}
            />
          </div>
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
            <Show when={conversationReader()} fallback={<Reader ref={(el) => (readerEl = el)} />}>
              <ConversationReader ref={(el) => (readerEl = el)} />
            </Show>
            <AttachmentToast />
          </div>
          <ViewMenuPopover />
          <ConfirmHost />
          <Show when={showAdd()}>
            <AddAccountModal onAdded={added} onClose={() => setShowAdd(false)} />
          </Show>
        </div>
      </Match>
    </Switch>
  );
}
