# UI <-> Core contract (Tauri v2)

All JSON is camelCase. Timestamps are unix seconds. IDs are numbers.

## Types

```ts
type Provider = 'gmail' | 'outlook' | 'imap';
interface Account { id: number; email: string; displayName: string; provider: Provider }

type FolderRole = 'inbox' | 'sent' | 'drafts' | 'trash' | 'junk' | 'archive'
                | 'all' | 'flagged' | 'important' | null;
interface Folder {
  id: number; accountId: number;
  path: string[];          // decoded hierarchy segments, e.g. ["Work", "Clients"]
  displayName: string;     // last segment (or friendly name for roles, e.g. "Inbox")
  role: FolderRole;
  selectable: boolean;     // false = container only
  total: number; unread: number;
  syncedCount: number;     // headers stored locally for this folder
}

interface MessageSummary {
  id: number; folderId: number;
  subject: string; fromName: string; fromAddr: string;
  date: number; snippet: string | null;
  unread: boolean; flagged: boolean; hasAttachments: boolean;
  hasBody: boolean;        // full message downloaded locally
  tags: string[];          // tag keywords, e.g. ["$label2"] (see TagDef)
}

interface Addr { name: string | null; addr: string }
interface Attachment {
  index: number;       // stable position in this message's attachment list (pass back to commands)
  name: string; mime: string;
  size: number;        // bytes (estimate for attachments not downloaded yet)
  downloaded: boolean; // available locally; otherwise Open/Save first fetch it from the server
  openable: boolean;   // false for executable/script types: offer Save only
}
interface MessageDetail {
  id: number; subject: string; from: Addr; to: Addr[]; cc: Addr[]; date: number;
  html: string | null;     // already sanitized by core; remote content stripped unless allowRemote
  text: string | null;
  attachments: Attachment[];
  hasRemoteContent: boolean; // true if remote images/css were blocked
  remoteAllowed: boolean;    // user chose "Load images" for this message before
  tags: string[];
}

/**
 * Which of a conversation's messages to show (list summaries and the reader alike):
 * messages in the current folder always; with 'all', also those in any other folder except
 * Trash/Spam; with includeTrash, also those in Trash/Spam.
 */
type ConversationScope = 'folder' | 'all';

/** One conversation as it appears in a folder. */
interface ThreadSummary {
  id: number;               // thread id
  folderId: number;
  subject: string;          // subject of the first message in the thread
  participants: string[];   // sender display names (or addresses), most recent first, unique, max 4
  messageCount: number;     // messages selected by scope/includeTrash
  unread: boolean;          // any message unread
  flagged: boolean;         // any message flagged
  hasAttachments: boolean;  // any message has attachments
  date: number;             // newest message of the thread in this folder
  snippet: string | null;   // snippet of the newest message
  latestMessageId: number;
  tags: string[];           // tags on any selected message of the conversation
}

type SearchField = 'sender' | 'recipients' | 'subject';

interface ThreadView {
  messages: MessageSummary[];
  hiddenOther: number;  // messages the scope hid that are in other folders (not Trash/Spam)
  hiddenTrash: number;  // messages the scope hid that are only in Trash/Spam
}

/** Message actions. `folderId` passed with them is the folder being viewed. */
type Action =
  | { kind: 'archive' }        // Gmail: unfiled (drop Inbox + user labels); else move to Archive
  | { kind: 'trash' }
  | { kind: 'deleteForever' }  // only in Trash/Spam
  | { kind: 'move'; targetFolderId: number }
  | { kind: 'spam' } | { kind: 'notSpam' }
  | { kind: 'read' } | { kind: 'unread' } | { kind: 'flag' } | { kind: 'unflag' }
  | { kind: 'tag'; keyword: string; on: boolean };  // add/remove a tag (IMAP keyword), sent at once

/** Tag definition. Tags are IMAP keywords on messages (Thunderbird-compatible: $label1..$label5
 *  are Important/Work/Personal/To Do/Later); names/colours are local. `defined: false` = a keyword
 *  seen on messages without a definition (name = keyword, generated colour). */
interface TagDef { keyword: string; name: string; color: string; position: number; defined: boolean }

type SyncPhase = 'idle' | 'connecting' | 'folders' | 'headers' | 'bodies' | 'error'
               | 'auth'     // credentials rejected: sync paused until update_password
               | 'limited'; // server refused a connection temporarily (connection limit); retrying on its own
interface SyncStatus {
  accountId: number; phase: SyncPhase; folderId: number | null;
  done: number; total: number; message: string | null;
}
```

## Commands (`invoke(name, args)`)

| name | args | returns |
|---|---|---|
| `list_accounts` | – | `Account[]` |
| `add_account` | `{ email, password, displayName?, imapHost?, imapPort? }` | `Account` (throws string on auth failure) |
| `list_folders` | `{ accountId }` | `Folder[]` |
| `open_folder` | `{ folderId }` | `void` — tells sync engine to prioritise this folder |
| `list_messages` | `{ folderId, offset, limit }` | `{ total: number, messages: MessageSummary[] }` newest first |
| `get_message` | `{ id, allowRemote }` | `MessageDetail \| null` — null means not downloaded yet; core queues it at top priority and emits `message-ready` |
| `list_threads` | `{ folderId, offset, limit, scope, includeTrash }` | `{ total: number, threads: ThreadSummary[] }` newest first — threaded counterpart of `list_messages`. A conversation is listed if any of its messages is in `folderId`; summary fields (count, participants, unread, flagged, attachments, snippet, latestMessageId) cover only the messages `scope`/`includeTrash` select. |
| `get_thread` | `{ threadId, folderId, scope, includeTrash }` | `ThreadView` — the conversation's messages selected by `scope`/`includeTrash`, oldest first, plus counts of hidden ones. A message's `folderId` is `folderId` when it's in that folder, else its most specific other folder (labels before All Mail) |
| `search_messages` | `{ accountId, folderId, query, offset, limit, fields }` | `{ total, messages: MessageSummary[] }` newest first. `folderId: null` = whole account. Each result's `folderId` is the most specific folder it's in (Inbox/label over All Mail). `fields: SearchField[] \| null` limits plain (unprefixed) terms to those fields; null or empty = all three. Query syntax below. |
| `open_attachment` | `{ messageId, index }` | `void` — fetches if needed, writes to the app cache, opens in the default app. Rejects (string) for non-openable types or on failure |
| `save_attachment` | `{ messageId, index }` | `string` — absolute path written in the Downloads folder (`name (1).ext` if taken) |
| `save_all_attachments` | `{ messageId }` | `string[]` — absolute paths of every attachment, each saved directly in Downloads (`name (1).ext` if taken) |
| `apply_action` | `{ action: Action, messageIds: number[], folderId }` | `{ opId: number \| null, undoable: boolean }` — applies locally at once (emits `messages-changed` / `folders-changed`); moves are sent to the server after a 5 s undo window, read/star at once. Rejects (string) when the action doesn't apply (e.g. no Archive folder) |
| `undo_action` | `{ opId }` | `void` — within the undo window; rejects "Too late to undo…" otherwise |
| `index_of` | `{ folderId, id, threaded }` | `number \| null` — the row index of a message (`threaded: false`) or conversation (`threaded: true`, `id` = thread id) in the folder's list (same ordering as `list_messages` / `list_threads`); `null` when it isn't in the folder |
| `list_tags` | – | `TagDef[]` — definitions (by position) then undefined keywords seen on messages |
| `set_tag` | `{ keyword: string \| null, name, color }` | `string` keyword — create (keyword null → Thunderbird-style keyword from the name) or rename/recolour |
| `delete_tag` | `{ keyword }` | `void` — forgets the definition only (messages keep the keyword) |
| `app_version` | – | `string` — e.g. "0.1.28" |
| `reveal_path` | `{ path }` | `void` — show the file in the system file manager |
| `get_sync_status` | – | `SyncStatus[]` |
| `update_password` | `{ accountId, password }` | `void` — verifies with the server (throws a readable string if rejected), saves to the keychain, restarts sync |
| `set_allow_remote` | `{ id, allow }` | `void` — remembers the "Load images" choice per message |
| `message_in_folder` | `{ id, folderId }` | `boolean` — whether message `id` is (still) in folder `folderId`; `folderId: null` = whether it still exists anywhere in its account. Used to drop a selection that was archived/deleted elsewhere |
| `set_read` | `{ id, read }` | `void` — updates locally at once (emits `messages-changed` + `folders-changed`), then syncs `\Seen` to the server |

## Events (`listen(name, cb)`)

| name | payload |
|---|---|
| `folders-changed` | `{ accountId }` |
| `messages-changed` | `{ folderIds: number[] }` (throttled ~250ms) |
| `message-ready` | `{ id }` |
| `action-failed` | `{ message }` — a queued action was rejected by the server and rolled back locally |
| `sync-status` | `SyncStatus` |

## Quick search query syntax

Matches sender (name + address), recipients (To/Cc names + addresses) and subject; every term must match (AND), case- and accent-insensitive, each word matched as a prefix (`inv` finds "invoice").

- `word` — any of the three fields
- `"exact phrase"` — phrase (last word still prefix-matched)
- `from:x`, `to:x`, `subject:x` — restrict a term to one field (`from:"Ada L"` works); these override `fields`
- `-word` — exclude
- `tag:work`, `tag:"to do"`, `tag:$label2` — messages with that tag (by tag name or keyword); may be used alone

## macOS menu events

| name | payload |
|---|---|
| `menu-action` | `{ id: 'search' \| 'toggle-threaded' \| 'toggle-scope' \| 'add-account' \| 'shortcuts' }` — sent by the native macOS menu bar (the window is shown/focused first) |
