import { batch, createEffect, createMemo, createRoot, createSignal, on, untrack } from 'solid-js';
import { createStore, reconcile } from 'solid-js/store';
import { api, events, type Account, type Folder, type MessageSummary, type SyncStatus, type ThreadSummary, type ConversationScope, type SearchField } from './api';
import { loadPref, savePref } from './lib/format';

// ------------------------------------------------------------- accounts & folders

/** null while the initial load is in flight. In the order the core lists them; see `accounts`. */
const [accountList, setAccounts] = createSignal<Account[] | null>(null);

/** The user's order of accounts (ids; dragged in the sidebar). Persisted on this machine. */
const ORDER_KEY = 'ui.accountOrder';
const [accountOrder, setAccountOrder] = createSignal<number[]>(loadPref<number[]>(ORDER_KEY, []));

/** Accounts in the user's order (new ones after the ordered ones, oldest first): everywhere
 *  accounts are listed (sidebar, rail, flyout, switcher, default account). */
const accounts = createRoot(() =>
  createMemo(() => {
    const list = accountList();
    if (!list) return list;
    const order = accountOrder();
    const rank = (a: Account) => {
      const i = order.indexOf(a.id);
      return i < 0 ? order.length + a.id : i;
    };
    return [...list].sort((a, b) => rank(a) - rank(b));
  }),
);

/** Move account `id` to position `to` (0 = first). */
export function moveAccount(id: number, to: number): void {
  const ids = (accounts() ?? []).map((a) => a.id).filter((x) => x !== id);
  ids.splice(Math.max(0, Math.min(to, ids.length)), 0, id);
  setAccountOrder(ids);
  savePref(ORDER_KEY, ids);
}
const [foldersByAccount, setFoldersByAccount] = createStore<Record<number, Folder[]>>({});
const [syncByAccount, setSyncByAccount] = createStore<Record<number, SyncStatus | undefined>>({});
const [selectedFolderId, setSelectedFolderId] = createSignal<number | null>(null);

/** Selected message id plus the summary row it came from (for an instant reader header). */
const [selectedMessageId, setSelectedMessageId] = createSignal<number | null>(null);
const [selectedSummary, setSelectedSummary] = createSignal<MessageSummary | null>(null);

/** Conversation view: list shows threads, reader shows the whole conversation. Persisted. */
const [threaded, setThreadedSig] = createSignal<boolean>(loadPref<boolean>('ui.threaded', true) !== false);
const [selectedThread, setSelectedThread] = createSignal<ThreadSummary | null>(null);
const selectedThreadId = () => selectedThread()?.id ?? null;

export {
  threaded,
  selectedThread,
  selectedThreadId,
  accounts,
  foldersByAccount,
  syncByAccount,
  selectedFolderId,
  selectedMessageId,
  selectedSummary,
};

export function folderById(id: number | null): Folder | undefined {
  if (id == null) return undefined;
  for (const list of Object.values(foldersByAccount)) {
    const f = list.find((x) => x.id === id);
    if (f) return f;
  }
  return undefined;
}

/** Keep the selected row's copy in step after a local change (read/star). */
export function patchSelectedSummary(id: number, patch: Partial<MessageSummary>): void {
  const cur = selectedSummary();
  if (cur && cur.id === id) setSelectedSummary({ ...cur, ...patch });
}

export function selectMessage(m: MessageSummary | null): void {
  batch(() => {
    setSelectedMessageId(m ? m.id : null);
    setSelectedSummary(m ? { ...m } : null);
  });
}

// ------------------------------------------------------------- conversation scope

/** Which messages of a conversation to show; global, persisted. */
const [conversationScope, setConversationScopeSig] = createSignal<ConversationScope>(
  loadPref<ConversationScope>('ui.conversationScope', 'folder') === 'all' ? 'all' : 'folder',
);
const [includeTrash, setIncludeTrashSig] = createSignal<boolean>(loadPref<boolean>('ui.includeTrash', false) === true);
export { conversationScope, includeTrash };

export function setConversationScope(s: ConversationScope): void {
  setConversationScopeSig(s);
  savePref('ui.conversationScope', s);
}
export function setIncludeTrash(v: boolean): void {
  setIncludeTrashSig(v);
  savePref('ui.includeTrash', v);
}

// ------------------------------------------------------------- quick search

export type SearchScope = 'all' | 'folder';
const [searchQuery, setSearchQuerySig] = createSignal('');
const [searchScope, setSearchScopeSig] = createSignal<SearchScope>(
  loadPref<SearchScope>('ui.searchScope', 'all') === 'folder' ? 'folder' : 'all',
);
export const ALL_SEARCH_FIELDS: SearchField[] = ['sender', 'recipients', 'subject'];
function loadFields(): SearchField[] {
  const v = loadPref<unknown>('ui.searchFields', ALL_SEARCH_FIELDS);
  const ok = Array.isArray(v) ? ALL_SEARCH_FIELDS.filter((f) => v.includes(f)) : [];
  return ok.length ? ok : ALL_SEARCH_FIELDS;
}
const [searchFields, setSearchFieldsSig] = createSignal<SearchField[]>(loadFields());
export { searchQuery, searchScope, searchFields };

/** Fields for search_messages: null when all are on. */
export const searchFieldsArg = (): SearchField[] | null =>
  searchFields().length === ALL_SEARCH_FIELDS.length ? null : searchFields();

/** Toggle one field; never leaves zero. `only` makes it the sole field (alt/option-click). */
export function toggleSearchField(f: SearchField, only = false): void {
  const cur = searchFields();
  let next: SearchField[];
  if (only) next = cur.length === 1 && cur[0] === f ? ALL_SEARCH_FIELDS : [f];
  else if (cur.includes(f)) next = cur.length === 1 ? cur : cur.filter((x) => x !== f);
  else next = ALL_SEARCH_FIELDS.filter((x) => x === f || cur.includes(x));
  setSearchFieldsSig(next);
  savePref('ui.searchFields', next);
}
export const searchActive = () => searchQuery().trim() !== '';

/** Selection before the search started, restored when the query is cleared. */
let preSearchSelection: MessageSummary | null = null;

export function setSearchQuery(q: string): void {
  const was = searchActive();
  setSearchQuerySig(q);
  const now = searchActive();
  if (!was && now) preSearchSelection = selectedSummary();
  else if (was && !now) {
    selectMessage(preSearchSelection);
    preSearchSelection = null;
  }
}

export function setSearchScope(s: SearchScope): void {
  setSearchScopeSig(s);
  savePref('ui.searchScope', s);
}

export function selectThread(t: ThreadSummary | null): void {
  if (t && t.id === selectedThreadId()) return;
  setSelectedThread(t ? { ...t, participants: [...t.participants] } : null);
}

// ------------------------------------------------------------- what the reader shows

// The readers follow the selection a painted frame later: building a message (an HTML document
// per message of a conversation) and tearing down the last one can take a while in WebKit, and
// done in the same step it held up the highlight too. While the selection moves quickly (arrow
// keys held), the reader waits for a pause instead of building every message passed over.
// Actions use the selection itself: they apply to what's highlighted.
const [readerThread, setReaderThread] = createSignal<ThreadSummary | null>(null);
const [readerMessageId, setReaderMessageId] = createSignal<number | null>(null);
export { readerThread, readerMessageId };

/** Slower than this (ms) is written to the app log. */
const SLOW_MS = 250;
let lastSelect = 0;
let readerFrame = 0;
let readerTimer: ReturnType<typeof setTimeout> | undefined;
let readerPending: (() => void) | null = null;

function showSelection(selectedAt: number) {
  readerPending = null;
  const start = performance.now();
  const t = untrack(selectedThread);
  batch(() => {
    setReaderThread(t);
    setReaderMessageId(untrack(selectedMessageId));
  });
  // The reader's work and its layout are done by the second frame from here.
  requestAnimationFrame(() =>
    requestAnimationFrame(() => {
      const ms = Math.round(performance.now() - start);
      if (ms > SLOW_MS) {
        const what = t ? `conversation of ${t.messageCount} messages` : 'message';
        void api.uiLog(`slow reader: ${ms} ms to show a ${what} (${Math.round(start - selectedAt)} ms after selecting)`).catch(() => {});
      }
    }),
  );
}

createRoot(() =>
  createEffect(
    on([selectedThread, selectedMessageId], () => {
      const now = performance.now();
      const rapid = now - lastSelect < 150;
      lastSelect = now;
      cancelAnimationFrame(readerFrame);
      clearTimeout(readerTimer);
      readerPending = () => showSelection(now);
      if (rapid) {
        readerTimer = setTimeout(() => readerPending?.(), 120);
        return;
      }
      // After the highlight is painted (the frame's callback runs before its paint: wait for the
      // next task).
      readerFrame = requestAnimationFrame(() => {
        const painted = Math.round(performance.now() - now);
        if (painted > SLOW_MS) void api.uiLog(`slow highlight: ${painted} ms from selecting to the frame`).catch(() => {});
        readerTimer = setTimeout(() => readerPending?.(), 0);
      });
    }),
  ),
);

/** Show the selection in the reader now (before moving focus into it). */
export function flushReader(): void {
  if (!readerPending) return;
  cancelAnimationFrame(readerFrame);
  clearTimeout(readerTimer);
  readerPending();
}

export function setThreaded(v: boolean): void {
  if (v === threaded()) return;
  batch(() => {
    setThreadedSig(v);
    selectMessage(null);
    selectThread(null);
  });
  savePref('ui.threaded', v);
}

// N/P crossed into another folder: once its list is up, it selects the first (or, going back,
// the last) unread message there.
let unreadOnArrival: { folderId: number; back: boolean } | null = null;

export function selectFolderAtUnread(id: number, back: boolean): void {
  unreadOnArrival = { folderId: id, back };
  selectFolder(id);
}

/** For the folder's list, once mounted: whether to select an unread message (and which way). */
export function takeUnreadOnArrival(folderId: number): { back: boolean } | null {
  const u = unreadOnArrival;
  unreadOnArrival = null;
  return u?.folderId === folderId ? { back: u.back } : null;
}

// A notification was clicked: the folder's list opens this message once it's up (or, already
// showing that folder, right away). A new object each time, so the same message works twice.
const [revealRequest, setRevealRequest] = createSignal<{ folderId: number; id: number } | null>(null);

/** For the folder's list: the message to open, if one was asked for there (taken once). */
export function takeReveal(folderId: number): number | null {
  const r = untrack(revealRequest);
  if (r?.folderId !== folderId) return null;
  setRevealRequest(null);
  return r.id;
}
export { revealRequest };

/** Show the folder and, if given, select this message in it. */
export function revealMessage(folderId: number, id: number | null): void {
  if (searchActive()) setSearchQuery('');
  if (id != null) setRevealRequest({ folderId, id });
  selectFolder(folderId);
}

// New-mail notifications (Settings › Notifications); the core is told at startup and on change.
const [notifyNewMail, setNotifyNewMailSig] = createSignal<boolean>(loadPref<boolean>('notify.newMail', true) !== false);
export { notifyNewMail };
export function setNotifyNewMail(on: boolean): void {
  setNotifyNewMailSig(on);
  savePref('notify.newMail', on);
  api.setNotifications(on).catch((e) => console.warn('set_notifications failed', e));
}

export function selectFolder(id: number): void {
  if (selectedFolderId() === id) return;
  batch(() => {
    setSelectedFolderId(id);
    preSearchSelection = null;
    selectMessage(null);
    selectThread(null);
  });
  savePref('ui.folder', id);
  api.openFolder(id).catch((e) => console.warn('open_folder failed', e));
}

async function loadFolders(accountId: number): Promise<Folder[]> {
  const list = await api.listFolders(accountId);
  setFoldersByAccount(accountId, reconcile(list, { key: 'id' }));
  return list;
}

function firstInbox(): Folder | undefined {
  for (const a of accounts() ?? []) {
    const f = (foldersByAccount[a.id] ?? []).find((x) => x.role === 'inbox' && x.selectable);
    if (f) return f;
  }
  for (const a of accounts() ?? []) {
    const f = (foldersByAccount[a.id] ?? []).find((x) => x.selectable);
    if (f) return f;
  }
  return undefined;
}

function ensureFolderSelected(): void {
  const cur = selectedFolderId();
  if (cur != null && folderById(cur)?.selectable) return;
  const pref = loadPref<number | null>('ui.folder', null);
  const f = (pref != null && folderById(pref)?.selectable ? folderById(pref) : undefined) ?? firstInbox();
  if (f) selectFolder(f.id);
}

/** A compose window: just the accounts (for From), no folders or sync. */
export async function loadAccountsOnly(): Promise<void> {
  setAccounts(await api.listAccounts());
}

export async function reloadAccounts(): Promise<Account[]> {
  const list = await api.listAccounts();
  setAccounts(list);
  await Promise.all(list.map((a) => loadFolders(a.id).catch((e) => console.error('list_folders', e))));
  ensureFolderSelected();
  return list;
}

/** After add_account succeeds: refresh and jump to the new account's inbox. */
export async function onAccountAdded(acc: Account): Promise<void> {
  await reloadAccounts();
  const inbox = (foldersByAccount[acc.id] ?? []).find((f) => f.role === 'inbox');
  if (inbox) selectFolder(inbox.id);
}

/**
 * Single-message selection (flat list or search results) that was archived/deleted elsewhere:
 * clear it so the reader goes blank. Flat: must still be in the open folder. Search: results
 * span folders, so only a message that no longer exists at all is dropped.
 */
let selCheckTimer: ReturnType<typeof setTimeout> | undefined;
function checkSelectedMessage(changed: number[]) {
  if (threaded() && !searchActive()) return; // conversations check themselves (Conversation.tsx)
  const id = selectedMessageId();
  const folderId = selectedFolderId();
  if (id == null || folderId == null) return;
  const inSearch = searchActive();
  if (!inSearch && !changed.includes(folderId)) return;
  clearTimeout(selCheckTimer);
  selCheckTimer = setTimeout(() => {
    api
      .messageInFolder(id, inSearch ? null : folderId)
      .then((still) => {
        // Only act if nothing changed meanwhile (same message, folder and mode).
        if (still || selectedMessageId() !== id || selectedFolderId() !== folderId || searchActive() !== inSearch) return;
        selectMessage(null);
      })
      .catch(() => {
        /* couldn't tell: keep the selection */
      });
  }, 60);
}

let started = false;
export async function initApp(): Promise<void> {
  if (started) return;
  started = true;

  // Listening before the first loads: a change between a load and its listener would be lost.
  const listening = [
    events.onFoldersChanged(({ accountId }) => {
      const known = (accounts() ?? []).some((a) => a.id === accountId);
      if (!known) {
        void reloadAccounts();
        return;
      }
      loadFolders(accountId)
        .then(() => ensureFolderSelected())
        .catch((e) => console.error('list_folders', e));
    }),
    events.onMessagesChanged(({ folderIds }) => checkSelectedMessage(folderIds)),
    events.onSyncStatus((s) => {
      setSyncByAccount(s.accountId, s);
    }),
    events.onOpenMessage(({ folderId, id }) => revealMessage(folderId, id)),
  ];
  api.setNotifications(notifyNewMail()).catch((e) => console.warn('set_notifications failed', e));
  await Promise.all(listening).catch((e) => console.warn('subscribing to core events', e));

  try {
    await reloadAccounts();
  } catch (e) {
    console.error('list_accounts failed', e);
    setAccounts([]);
  }
  try {
    const statuses = await api.getSyncStatus();
    // Events received meanwhile are newer than the snapshot; don't clobber them.
    batch(() => statuses.forEach((s) => syncByAccount[s.accountId] || setSyncByAccount(s.accountId, s)));
  } catch (e) {
    console.warn('get_sync_status failed', e);
  }
}

// ------------------------------------------------------------- per-folder remembered selection

/** folder id -> last selected thread (t) / message (f) id, with a recency stamp. Persisted. */
type SelMemory = Record<string, { t?: number; f?: number; at: number }>;
const SEL_KEY = 'ui.lastSelection';
const SEL_MAX = 500;
const selMem: SelMemory = (() => {
  const v = loadPref<unknown>(SEL_KEY, {});
  return v && typeof v === 'object' ? (v as SelMemory) : {};
})();
let saveTimer: ReturnType<typeof setTimeout> | undefined;
function persistSelMem() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    const keys = Object.keys(selMem);
    if (keys.length > SEL_MAX) {
      keys
        .sort((a, b) => (selMem[a]!.at ?? 0) - (selMem[b]!.at ?? 0))
        .slice(0, keys.length - SEL_MAX)
        .forEach((k) => delete selMem[k]);
    }
    savePref(SEL_KEY, selMem);
  }, 200);
}

export function rememberedSelection(folderId: number, threadedMode: boolean): number | null {
  const e = selMem[folderId];
  const v = threadedMode ? e?.t : e?.f;
  return typeof v === 'number' ? v : null;
}

export function rememberSelection(folderId: number, threadedMode: boolean, id: number | null): void {
  const e = selMem[folderId] ?? { at: 0 };
  if (threadedMode) {
    if (id == null) delete e.t;
    else e.t = id;
  } else if (id == null) delete e.f;
  else e.f = id;
  e.at = Date.now();
  if (e.t == null && e.f == null) delete selMem[folderId];
  else selMem[folderId] = e;
  persistSelMem();
}

/** True while a folder's remembered item is being verified (the empty reader hides its hint). */
const [restorePending, setRestorePending] = createSignal(false);
export { restorePending, setRestorePending };

// Record selection changes made *within* a folder+mode (not the reset that comes with
// switching folder / mode / entering or leaving search, and nothing while searching).
createRoot(() => {
  let prevKey = '';
  createEffect(() => {
    const f = selectedFolderId();
    const th = threaded();
    const s = searchActive();
    const id = th ? selectedThreadId() : selectedMessageId();
    const key = `${f}|${th}|${s}`;
    if (key !== prevKey) {
      prevKey = key;
      return;
    }
    if (f == null || s) return;
    rememberSelection(f, th, id);
  });
});
