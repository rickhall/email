import { batch, createEffect, createRoot, createSignal } from 'solid-js';
import { createStore, reconcile } from 'solid-js/store';
import { api, events, type Account, type Folder, type MessageSummary, type SyncStatus, type ThreadSummary, type ConversationScope, type SearchField } from './api';
import { loadPref, savePref } from './lib/format';

// ------------------------------------------------------------- accounts & folders

/** null while the initial load is in flight. */
const [accounts, setAccounts] = createSignal<Account[] | null>(null);
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

export function setThreaded(v: boolean): void {
  if (v === threaded()) return;
  batch(() => {
    setThreadedSig(v);
    selectMessage(null);
    selectThread(null);
  });
  savePref('ui.threaded', v);
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
        /* core without message_in_folder: keep the selection */
      });
  }, 60);
}

let started = false;
export async function initApp(): Promise<void> {
  if (started) return;
  started = true;

  void events.onFoldersChanged(({ accountId }) => {
    const known = (accounts() ?? []).some((a) => a.id === accountId);
    if (!known) {
      void reloadAccounts();
      return;
    }
    loadFolders(accountId)
      .then(() => ensureFolderSelected())
      .catch((e) => console.error('list_folders', e));
  });
  void events.onMessagesChanged(({ folderIds }) => checkSelectedMessage(folderIds));
  void events.onSyncStatus((s) => {
    setSyncByAccount(s.accountId, s);
  });

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
