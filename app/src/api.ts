import type {
  Account,
  AddAccountArgs,
  Backend,
  CommandMap,
  CommandName,
  ConversationScope,
  SearchField,
  EventMap,
  EventName,
  Folder,
  MessageDetail,
  MessagePage,
  SyncStatus,
  ThreadPage,
  ThreadView,
} from './types';

export * from './types';

export const isTauri: boolean = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

async function createTauriBackend(): Promise<Backend> {
  const [{ invoke }, { listen }] = await Promise.all([
    import('@tauri-apps/api/core'),
    import('@tauri-apps/api/event'),
  ]);
  return {
    invoke(cmd, args) {
      return invoke(cmd, (args ?? {}) as Record<string, unknown>);
    },
    async listen(event, cb) {
      return listen(event, (e) => cb(e.payload as never));
    },
  };
}

const backendPromise: Promise<Backend> = isTauri
  ? createTauriBackend()
  : import('./mock').then((m) => m.createMockBackend());

async function call<K extends CommandName>(
  cmd: K,
  args: CommandMap[K]['args'],
): Promise<CommandMap[K]['ret']> {
  const b = await backendPromise;
  return b.invoke(cmd, args);
}

/** Normalise anything thrown across the bridge into a readable string. */
export function errorMessage(e: unknown): string {
  if (typeof e === 'string') return e;
  if (e instanceof Error) return e.message;
  if (e && typeof e === 'object' && 'message' in e) return String((e as { message: unknown }).message);
  try {
    return JSON.stringify(e);
  } catch {
    return 'Unknown error';
  }
}

export const api = {
  listAccounts: (): Promise<Account[]> => call('list_accounts', undefined),
  addAccount: (a: AddAccountArgs): Promise<Account> =>
    call('add_account', {
      email: a.email,
      password: a.password,
      // Explicit nulls so Rust `Option<T>` params always receive a key.
      displayName: a.displayName || null,
      imapHost: a.imapHost || null,
      imapPort: a.imapPort ?? null,
    }),
  listFolders: (accountId: number): Promise<Folder[]> => call('list_folders', { accountId }),
  openFolder: (folderId: number): Promise<void> => call('open_folder', { folderId }),
  listMessages: (folderId: number, offset: number, limit: number): Promise<MessagePage> =>
    call('list_messages', { folderId, offset, limit }),
  getMessage: (id: number, allowRemote: boolean): Promise<MessageDetail | null> =>
    call('get_message', { id, allowRemote }),
  /** Quick search; `folderId: null` searches the whole account. Newest first. */
  searchMessages: (
    accountId: number,
    folderId: number | null,
    query: string,
    offset: number,
    limit: number,
    /** null = all fields */
    fields: SearchField[] | null,
  ): Promise<MessagePage> => call('search_messages', { accountId, folderId, query, offset, limit, fields }),
  listThreads: (
    folderId: number,
    offset: number,
    limit: number,
    scope: ConversationScope,
    includeTrash: boolean,
  ): Promise<ThreadPage> => call('list_threads', { folderId, offset, limit, scope, includeTrash }),
  /** Every message in the conversation, all folders, oldest first. */
  getThread: (
    threadId: number,
    folderId: number,
    scope: ConversationScope,
    includeTrash: boolean,
  ): Promise<ThreadView> => call('get_thread', { threadId, folderId, scope, includeTrash }),
  getSyncStatus: (): Promise<SyncStatus[]> => call('get_sync_status', undefined),
  setRead: (id: number, read: boolean): Promise<void> => call('set_read', { id, read }),
  setAllowRemote: (id: number, allow: boolean): Promise<void> => call('set_allow_remote', { id, allow }),
  updatePassword: (accountId: number, password: string): Promise<void> =>
    call('update_password', { accountId, password }),
};

async function on<K extends EventName>(event: K, cb: (p: EventMap[K]) => void): Promise<() => void> {
  const b = await backendPromise;
  return b.listen(event, cb);
}

export const events = {
  onFoldersChanged: (cb: (p: EventMap['folders-changed']) => void) => on('folders-changed', cb),
  onMessagesChanged: (cb: (p: EventMap['messages-changed']) => void) => on('messages-changed', cb),
  onMessageReady: (cb: (p: EventMap['message-ready']) => void) => on('message-ready', cb),
  onSyncStatus: (cb: (p: EventMap['sync-status']) => void) => on('sync-status', cb),
};
