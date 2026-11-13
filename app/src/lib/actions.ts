import { createSignal } from 'solid-js';
import { api, errorMessage, events, type Action, type ActionKind, type MessageSummary, type ThreadSummary } from '../api';
import {
  folderById,
  patchSelectedSummary,
  searchActive,
  selectedMessageId,
  selectedSummary,
  selectedThread,
  selectedThreadId,
  selectMessage,
  selectThread,
  threaded,
} from '../state';
import { hideToast, showToast } from '../components/Toast';
import { commandFor } from './shortcuts';

// ------------------------------------------------------------- list navigation registry

/** Implemented by the mounted MessageList instances (folder list, search results). */
export interface ListNav {
  /** Drop the selected row (an own action is removing it) and select the next older row in its
   *  place (else the newer one, else nothing), scrolling only if needed. */
  advance(): Removed | null;
  /** Select the next older row (else the newer one), keeping the selected one: a search result
   *  acted on still matches (in its new folder), so it stays in the results. */
  step(): void;
  /** Undo of `advance`: the row back in its place, selected (whatever is selected now). */
  putBack(r: Removed): void;
  /** Refetch the rows on screen (puts back a row `advance` dropped when the action didn't go). */
  refresh(): void;
  /** Re-sync the list's keyboard index to the current selection. */
  sync(): void;
}
/** The row `advance` dropped. */
export interface Removed {
  id: number;
  index: number;
  row: MessageSummary | ThreadSummary;
}
const navs: { folder?: ListNav; search?: ListNav } = {};
export function registerListNav(kind: 'folder' | 'search', nav: ListNav): () => void {
  navs[kind] = nav;
  return () => {
    if (navs[kind] === nav) delete navs[kind];
  };
}
const activeNav = () => (searchActive() ? navs.search : navs.folder);

// ------------------------------------------------------------- selection snapshots

/** Conversation reader is showing (vs. a single message). */
export const conversationMode = () => threaded() && !searchActive();

type Selection = { thread: ThreadSummary } | { msg: MessageSummary } | null;
function snapshot(): Selection {
  if (conversationMode()) {
    const t = selectedThread();
    return t ? { thread: t } : null;
  }
  const m = selectedSummary();
  return m ? { msg: m } : null;
}
const currentId = () => (conversationMode() ? selectedThreadId() : selectedMessageId());
function restore(s: Selection) {
  if (!s) return;
  if ('thread' in s) selectThread(s.thread);
  else selectMessage(s.msg);
  activeNav()?.sync();
}

// ------------------------------------------------------------- run / undo

const REMOVING = new Set<ActionKind>(['archive', 'trash', 'deleteForever', 'move', 'spam', 'notSpam']);

function describe(a: Action, n: number, targetName?: string): string {
  const many = n > 1 ? ` ${n} messages` : '';
  switch (a.kind) {
    case 'archive':
      return `Archived${many}`;
    case 'trash':
      return `Deleted${many}`;
    case 'deleteForever':
      return n > 1 ? `Deleted ${n} messages forever` : 'Deleted forever';
    case 'spam':
      return n > 1 ? `Marked ${n} messages as spam` : 'Marked as spam';
    case 'notSpam':
      return `Moved${many} to Inbox`;
    case 'move':
      return `Moved${many} to ${targetName ?? 'folder'}`;
    case 'snooze': {
      const when = new Date(a.until * 1000).toLocaleString(undefined, { weekday: 'short', day: 'numeric', month: 'short', hour: 'numeric', minute: '2-digit' });
      return `Snoozed${many}: back ${when}`;
    }
    case 'unsnooze':
      return `Unsnoozed${many}`;
    default:
      return 'Done';
  }
}

interface UndoEntry {
  opId: number;
  /** selection before an auto-advance, and what we advanced to */
  prev: Selection;
  next: number | null;
  /** the row the advance dropped, and from which list */
  removed: Removed | null;
  nav: ListNav | undefined;
  toastId: number;
}
let lastUndo: UndoEntry | null = null;

export interface RunOpts {
  ids: number[];
  /** folder the messages are being viewed in (apply_action's folderId) */
  folderId: number;
  /** open the next older item first when this removes what's being viewed */
  advance: boolean;
}

/** Ids with an action in flight (buttons show a spinner, repeat presses are ignored). */
const [running, setRunning] = createSignal(0);
export { running };

export async function runAction(action: Action, o: RunOpts): Promise<void> {
  if (o.ids.length === 0) return;
  const removing = REMOVING.has(action.kind);
  let prev: Selection = null;
  let next: number | null = null;
  let removed: Removed | null = null;
  const nav = activeNav();
  if (removing && o.advance) {
    // Own action: open the next older item right away (the "left this folder → blank pane"
    // rule is for changes made elsewhere).
    prev = snapshot();
    if (searchActive()) nav?.step();
    else removed = nav?.advance() ?? null;
    next = currentId();
  }
  const putBack = () => {
    if (!prev) return;
    nav?.refresh();
    if (currentId() === next) restore(prev);
  };
  setRunning((n) => n + 1);
  try {
    const res = await api.applyAction(action, o.ids, o.folderId);
    if (action.kind === 'read' || action.kind === 'unread') {
      for (const id of o.ids) patchSelectedSummary(id, { unread: action.kind === 'unread' });
    } else if (action.kind === 'flag' || action.kind === 'unflag') {
      for (const id of o.ids) patchSelectedSummary(id, { flagged: action.kind === 'flag' });
    } else if (action.kind === 'tag') {
      const s = selectedSummary();
      if (s && o.ids.includes(s.id)) {
        const k = action.keyword;
        patchSelectedSummary(s.id, { tags: action.on ? (s.tags.includes(k) ? s.tags : [...s.tags, k]) : s.tags.filter((x) => x !== k) });
      }
    }
    if (!removing) return;
    if (res.opId == null) {
      // Nothing changed (e.g. Archive inside All Mail): stay on what was open.
      putBack();
      return;
    }
    if (res.undoable) {
      const target = action.kind === 'move' ? folderById(action.targetFolderId)?.displayName : undefined;
      const entry: UndoEntry = { opId: res.opId, prev, next, removed, nav, toastId: 0 };
      entry.toastId = showToast({
        kind: 'ok',
        text: describe(action, o.ids.length, target),
        action: { label: 'Undo', run: () => void undo(entry) },
        ms: 5000,
      });
      lastUndo = entry;
    }
  } catch (e) {
    putBack();
    showToast({ kind: 'error', text: errorMessage(e) });
  } finally {
    setRunning((n) => n - 1);
  }
}

async function undo(entry: UndoEntry) {
  if (lastUndo === entry) lastUndo = null;
  hideToast(entry.toastId);
  try {
    await api.undoAction(entry.opId);
  } catch (e) {
    entry.nav?.refresh();
    showToast({ kind: 'error', text: errorMessage(e) });
    return;
  }
  // Then the message back in its place and selected again, even if the user has moved on since;
  // not if they've left that list (another folder, a search). Only once the core has it back:
  // a conversation opened earlier would find nothing in this folder and close again.
  if (entry.removed && entry.nav && entry.nav === activeNav()) entry.nav.putBack(entry.removed);
  else if (entry.prev != null && currentId() === entry.next) restore(entry.prev);
}

/** `z`: undo the most recent undoable action. */
export function undoLast(): boolean {
  if (!lastUndo) return false;
  void undo(lastUndo);
  return true;
}

// ------------------------------------------------------------- reader header shortcuts

/** `tag-N`: toggle the Nth tag (1–9) · `tag-clear`: remove all · `tag-menu`: open the Tag menu */
export type BarCommand =
  | 'archive'
  | 'delete'
  | 'move'
  | 'spam'
  | 'unread'
  | 'star'
  | 'reply'
  | 'replyAll'
  | 'forward'
  | 'tag-menu'
  | 'tag-clear'
  | 'snooze'
  | `tag-${number}`;
let headerHandler: ((c: BarCommand) => void) | null = null;
/** The reader header's action bar registers itself; shortcuts drive it. */
export function setHeaderHandler(fn: ((c: BarCommand) => void) | null): void {
  headerHandler = fn;
}
export function clearHeaderHandler(fn: (c: BarCommand) => void): void {
  if (headerHandler === fn) headerHandler = null;
}

/** Keymap command → reader bar command. */
const BAR_COMMANDS: Record<string, BarCommand> = {
  archive: 'archive',
  delete: 'delete',
  move: 'move',
  spam: 'spam',
  unread: 'unread',
  star: 'star',
  reply: 'reply',
  'reply-all': 'replyAll',
  forward: 'forward',
  'tag-menu': 'tag-menu',
  snooze: 'snooze',
};

/** Archive, delete, move, spam, read/unread, star, undo and the tag digits (keys from the
 *  keymap) — returns true when handled. */
export function runShortcut(e: KeyboardEvent): boolean {
  // Thunderbird: 1–9 toggle the tag in that position, 0 removes all tags (fixed keys).
  if (!e.metaKey && !e.ctrlKey && !e.altKey && /^[0-9]$/.test(e.key)) {
    headerHandler?.(e.key === '0' ? 'tag-clear' : (`tag-${Number(e.key)}` as BarCommand));
    return true;
  }
  const cmd = commandFor(e);
  if (cmd === 'undo') return undoLast() || true;
  const bar = cmd ? BAR_COMMANDS[cmd] : undefined;
  if (!bar) return false;
  headerHandler?.(bar);
  return true;
}

// ------------------------------------------------------------- server rejections

void events.onActionFailed(({ message }) => showToast({ kind: 'error', text: message }));
