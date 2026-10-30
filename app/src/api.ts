import type {
  Account,
  Action,
  ActionResult,
  Addr,
  FileInfo,
  NextUnread,
  ReplyContext,
  ReplyMode,
  SendQueued,
  SendRequest,
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
  TagDef,
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
  /** Is message `id` still in `folderId` (null = still exists at all)? */
  /** Row index of a message / conversation in the folder's list (null = not in it). */
  indexOf: (folderId: number, id: number, threaded: boolean): Promise<number | null> =>
    call('index_of', { folderId, id, threaded }),
  /** The next unread item below `after` in the folder's list, wrapping to the top (null = none). */
  nextUnread: (folderId: number, after: number | null, threaded: boolean): Promise<NextUnread | null> =>
    call('next_unread', { folderId, after, threaded }),
  /** Queue a message; it goes out after the undo window unless cancelSend(id) takes it back. */
  sendMessage: (request: SendRequest): Promise<SendQueued> => call('send_message', { request }),
  /** Save the composer's state as draft `id` (new when null); shown in Drafts. Returns its id. */
  saveDraft: (id: number | null, request: SendRequest): Promise<number> => call('save_draft', { id, request }),
  openDraft: (id: number): Promise<unknown> => call('open_draft', { id }),
  deleteDraft: (id: number): Promise<void> => call('delete_draft', { id }),
  /** People matching what's typed in To/Cc/Bcc, best first (from the mail you've exchanged). */
  suggestAddresses: (query: string, limit: number | null = null): Promise<Addr[]> => call('suggest_addresses', { query, limit }),
  /** Name and size of files to attach (missing files and folders are left out). */
  fileInfo: (paths: string[]): Promise<FileInfo[]> => call('file_info', { paths }),
  /** Open (or focus) the compose window of draft `draftId` (at the last compose window's size). */
  openCompose: (draftId: number): Promise<void> => call('open_compose', { draftId }),
  /** Prefill a reply / reply-all / forward of a message (downloads it first if needed). */
  replyContext: (id: number, mode: ReplyMode): Promise<ReplyContext> => call('reply_context', { id, mode }),
  /** Take back a queued (or failed) message: its draft, to reopen the composer. */
  cancelSend: (id: number): Promise<unknown | null> => call('cancel_send', { id }),
  /** Reveal the app's log file in the file manager. */
  showLog: (): Promise<void> => call('show_log', undefined),
  messageInFolder: (id: number, folderId: number | null): Promise<boolean> =>
    call('message_in_folder', { id, folderId }),
  openAttachment: (messageId: number, index: number): Promise<void> =>
    call('open_attachment', { messageId, index }),
  /** Returns the absolute path written in Downloads. */
  saveAttachment: (messageId: number, index: number): Promise<string> =>
    call('save_attachment', { messageId, index }),
  /** Saves every attachment into a new Downloads folder; returns its path. */
  saveAllAttachments: (messageId: number): Promise<string[]> => call('save_all_attachments', { messageId }),
  /** Applies locally at once; moves reach the server after a 5 s undo window. */
  applyAction: (action: Action, messageIds: number[], folderId: number): Promise<ActionResult> =>
    call('apply_action', { action, messageIds, folderId }),
  undoAction: (opId: number): Promise<void> => call('undo_action', { opId }),
  listTags: (): Promise<TagDef[]> => call('list_tags', undefined),
  /** Create (keyword null) or rename/recolour; returns the keyword. */
  setTag: (keyword: string | null, name: string, color: string): Promise<string> => call('set_tag', { keyword, name, color }),
  deleteTag: (keyword: string): Promise<void> => call('delete_tag', { keyword }),
  appVersion: (): Promise<string> => call('app_version', undefined),
  revealPath: (path: string): Promise<void> => call('reveal_path', { path }),
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
  onMessageSent: (cb: (p: EventMap['message-sent']) => void) => on('message-sent', cb),
  onSendFailed: (cb: (p: EventMap['send-failed']) => void) => on('send-failed', cb),
  onActionFailed: (cb: (p: EventMap['action-failed']) => void) => on('action-failed', cb),
  onMenuAction: (cb: (p: EventMap['menu-action']) => void) => on('menu-action', cb),
};
