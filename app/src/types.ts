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
  /** tag keywords, e.g. ["$label2"] (see TagDef) */
  tags: string[];
  /** replied to (\\Answered, from any client) */
  answered: boolean;
  /** forwarded (Thunderbird's $Forwarded keyword) */
  forwarded: boolean;
  /** our message, waiting to be sent (undo window, or retrying) */
  sending: boolean;
  /** our message, refused by the server: not sent */
  sendFailed: boolean;
}

export interface Addr {
  name: string | null;
  addr: string;
}

export interface Attachment {
  /** stable position in this message's attachment list (pass back to commands) */
  index: number;
  name: string;
  mime: string;
  /** bytes (estimate when not downloaded yet) */
  size: number;
  /** available locally; otherwise Open/Save fetch it from the server first */
  downloaded: boolean;
  /** false for executable/script types: Save only */
  openable: boolean;
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
  tags: string[];
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
  /** tags on any selected message of the conversation */
  tags: string[];
  /** the newest message from someone else was replied to / forwarded */
  answered: boolean;
  forwarded: boolean;
  /** the newest message is ours, waiting to be sent / refused by the server */
  sending: boolean;
  sendFailed: boolean;
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

/** A user action on messages (api-contract `Action`). */
export type Action =
  | { kind: 'archive' }
  | { kind: 'trash' }
  | { kind: 'deleteForever' }
  | { kind: 'move'; targetFolderId: number }
  | { kind: 'spam' }
  | { kind: 'notSpam' }
  | { kind: 'read' }
  | { kind: 'unread' }
  | { kind: 'flag' }
  | { kind: 'unflag' }
  | { kind: 'tag'; keyword: string; on: boolean };
export type ActionKind = Action['kind'];

/** Tag definition. Tags are IMAP keywords on messages (Thunderbird-compatible: $label1..$label5).
 *  `defined: false` = a keyword seen on messages without a definition. */
export interface TagDef {
  keyword: string;
  name: string;
  color: string;
  position: number;
  defined: boolean;
}

/** A message to send, as the composer has it (see compose in lib/compose.ts). */
export interface SendRequest {
  accountId: number;
  to: Addr[];
  cc: Addr[];
  bcc: Addr[];
  subject: string;
  /** body HTML (editor output, plus any quoted original) */
  html: string;
  /** file paths */
  attachments: string[];
  /** message being replied to (threading; marked replied once sent) */
  replyingTo: number | null;
  /** message being forwarded (marked forwarded once sent) */
  forwarding: number | null;
  /** attachments of `forwarding` to send along (their index) */
  forwardAttachments: number[];
  /** the composer's own state, handed back by cancel_send to reopen it */
  draft: unknown;
}

export type ReplyMode = 'reply' | 'replyAll' | 'forward';

/** A reply / forward, prefilled by the core (recipients, subject, quoted original). */
export interface ReplyContext {
  accountId: number;
  to: Addr[];
  cc: Addr[];
  subject: string;
  /** HTML sent under the new text (Gmail-style quote or forward block) */
  quoteHtml: string;
  replyingTo: number | null;
  forwarding: number | null;
  /** forward: the original's attachments (sent along unless removed) */
  attachments: Attachment[];
}

/** queued: sent at `sendAt` (ms) unless taken back with cancel_send(id) */
export interface SendQueued {
  id: number;
  sendAt: number;
}

export interface SendEventPayload {
  accountId: number;
  id: number;
  /** why it failed (send-failed only) */
  message: string | null;
}

/** Where `next_unread` found the next unread item: its id and row index in the folder list. */
export interface NextUnread {
  id: number;
  index: number;
}

export interface ActionResult {
  /** null when nothing was queued (e.g. no-op, or read/star which apply at once) */
  opId: number | null;
  undoable: boolean;
}

export interface ActionFailedPayload {
  message: string;
}

/** Native macOS menu bar commands. */
export type MenuActionId = 'search' | 'toggle-threaded' | 'toggle-scope' | 'add-account' | 'shortcuts' | 'settings' | 'show-log' | 'new-message';
export interface MenuActionPayload {
  id: MenuActionId;
}

export interface EventMap {
  'folders-changed': FoldersChangedPayload;
  'messages-changed': MessagesChangedPayload;
  'message-ready': MessageReadyPayload;
  'sync-status': SyncStatus;
  'menu-action': MenuActionPayload;
  'message-sent': SendEventPayload;
  'send-failed': SendEventPayload;
  'action-failed': ActionFailedPayload;
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
  open_attachment: { args: { messageId: number; index: number }; ret: void };
  save_attachment: { args: { messageId: number; index: number }; ret: string };
  save_all_attachments: { args: { messageId: number }; ret: string[] };
  apply_action: { args: { action: Action; messageIds: number[]; folderId: number }; ret: ActionResult };
  undo_action: { args: { opId: number }; ret: void };
  list_tags: { args: undefined; ret: TagDef[] };
  set_tag: { args: { keyword: string | null; name: string; color: string }; ret: string };
  delete_tag: { args: { keyword: string }; ret: void };
  app_version: { args: undefined; ret: string };
  reveal_path: { args: { path: string }; ret: void };
  index_of: { args: { folderId: number; id: number; threaded: boolean }; ret: number | null };
  next_unread: { args: { folderId: number; after: number | null; threaded: boolean }; ret: NextUnread | null };
  show_log: { args: undefined; ret: void };
  send_message: { args: { request: SendRequest }; ret: SendQueued };
  cancel_send: { args: { id: number }; ret: unknown | null };
  reply_context: { args: { id: number; mode: ReplyMode }; ret: ReplyContext };
  message_in_folder: { args: { id: number; folderId: number | null }; ret: boolean };
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
