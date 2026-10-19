// Types mirror src/api-contract.md exactly. All JSON is camelCase, timestamps are unix seconds.

export type Provider = 'gmail' | 'outlook' | 'imap';

export interface Account {
  id: number;
  email: string;
  displayName: string;
  provider: Provider;
}

export type FolderRole =
  | 'inbox'
  | 'sent'
  | 'drafts'
  | 'trash'
  | 'junk'
  | 'archive'
  | 'all'
  | 'flagged'
  | 'important'
  | null;

export interface Folder {
  id: number;
  accountId: number;
  path: string[];
  displayName: string;
  role: FolderRole;
  selectable: boolean;
  total: number;
  unread: number;
  syncedCount: number;
}

export interface MessageSummary {
  id: number;
  folderId: number;
  subject: string;
  fromName: string;
  fromAddr: string;
  date: number;
  snippet: string | null;
  unread: boolean;
  flagged: boolean;
  hasAttachments: boolean;
  hasBody: boolean;
}

export interface Addr {
  name: string | null;
  addr: string;
}

export interface Attachment {
  name: string;
  mime: string;
  size: number;
}

export interface MessageDetail {
  id: number;
  subject: string;
  from: Addr;
  to: Addr[];
  cc: Addr[];
  date: number;
  html: string | null;
  text: string | null;
  attachments: Attachment[];
  hasRemoteContent: boolean;
  /** The user chose "Load images" for this message before. */
  remoteAllowed: boolean;
}

/**
 * Which of a conversation's messages to show (list summaries and the reader alike):
 * messages in the current folder always; with 'all', also those in any other folder except
 * Trash/Spam; with includeTrash, also those in Trash/Spam.
 */
export type ConversationScope = 'folder' | 'all';

/** Fields plain (unprefixed) quick-search terms look in; null/empty = all three. */
export type SearchField = 'sender' | 'recipients' | 'subject';

/** One conversation as it appears in a folder. */
export interface ThreadSummary {
  /** thread id */
  id: number;
  folderId: number;
  /** subject of the first message in the thread */
  subject: string;
  /** sender display names (or addresses), most recent first, unique, max 4 */
  participants: string[];
  /** messages in the whole conversation (all folders) */
  messageCount: number;
  unread: boolean;
  flagged: boolean;
  hasAttachments: boolean;
  /** newest message of the thread in this folder */
  date: number;
  snippet: string | null;
  latestMessageId: number;
}

/** get_thread result: the selected messages plus how many the scope hid. */
export interface ThreadView {
  messages: MessageSummary[];
  /** hidden messages in other folders (not Trash/Spam) */
  hiddenOther: number;
  /** hidden messages only in Trash/Spam */
  hiddenTrash: number;
}

export interface ThreadPage {
  total: number;
  threads: ThreadSummary[];
}

export type SyncPhase = 'idle' | 'connecting' | 'folders' | 'headers' | 'bodies' | 'error' | 'auth' | 'limited';

export interface SyncStatus {
  accountId: number;
  phase: SyncPhase;
  folderId: number | null;
  done: number;
  total: number;
  message: string | null;
}

export interface AddAccountArgs {
  email: string;
  password: string;
  displayName?: string | null;
  imapHost?: string | null;
  imapPort?: number | null;
}

export interface MessagePage {
  total: number;
  messages: MessageSummary[];
}

export interface FoldersChangedPayload {
  accountId: number;
}
export interface MessagesChangedPayload {
  folderIds: number[];
}
export interface MessageReadyPayload {
  id: number;
}

export interface EventMap {
  'folders-changed': FoldersChangedPayload;
  'messages-changed': MessagesChangedPayload;
  'message-ready': MessageReadyPayload;
  'sync-status': SyncStatus;
}

export interface CommandMap {
  list_accounts: { args: undefined; ret: Account[] };
  add_account: { args: AddAccountArgs; ret: Account };
  list_folders: { args: { accountId: number }; ret: Folder[] };
  open_folder: { args: { folderId: number }; ret: void };
  list_messages: { args: { folderId: number; offset: number; limit: number }; ret: MessagePage };
  get_message: { args: { id: number; allowRemote: boolean }; ret: MessageDetail | null };
  search_messages: {
    args: {
      accountId: number;
      folderId: number | null;
      query: string;
      offset: number;
      limit: number;
      fields: SearchField[] | null;
    };
    ret: MessagePage;
  };
  list_threads: {
    args: { folderId: number; offset: number; limit: number; scope: ConversationScope; includeTrash: boolean };
    ret: ThreadPage;
  };
  get_thread: {
    args: { threadId: number; folderId: number; scope: ConversationScope; includeTrash: boolean };
    ret: ThreadView;
  };
  get_sync_status: { args: undefined; ret: SyncStatus[] };
  set_read: { args: { id: number; read: boolean }; ret: void };
  set_allow_remote: { args: { id: number; allow: boolean }; ret: void };
  update_password: { args: { accountId: number; password: string }; ret: void };
}

export type CommandName = keyof CommandMap;
export type EventName = keyof EventMap;

/** The minimal surface both the Tauri bridge and the in-browser mock implement. */
export interface Backend {
  invoke<K extends CommandName>(cmd: K, args: CommandMap[K]['args']): Promise<CommandMap[K]['ret']>;
  listen<K extends EventName>(event: K, cb: (payload: EventMap[K]) => void): Promise<() => void>;
}
