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

// ------------------------------------------------------------- list navigation registry

/** Implemented by the mounted MessageList instances (folder list, search results). */
export interface ListNav {
  /** Select the next older row (else the newer one, else nothing), scrolling only if needed. */
  advance(): void;
  /** Re-sync the list's keyboard index to the current selection. */
  sync(): void;
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
    default:
      return 'Done';
  }
}

interface UndoEntry {
  opId: number;
  /** selection before an auto-advance, and what we advanced to */
  prev: Selection;
  next: number | null;
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
  if (removing && o.advance) {
    // Own action: open the next older item right away (the "left this folder → blank pane"
    // rule is for changes made elsewhere).
    prev = snapshot();
    activeNav()?.advance();
    next = currentId();
  }
  const putBack = () => {
    if (prev && currentId() === next) restore(prev);
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
      const entry: UndoEntry = { opId: res.opId, prev, next, toastId: 0 };
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
    // Back to what was open, unless the user has since picked something else.
    if (entry.prev && currentId() === entry.next) restore(entry.prev);
  } catch (e) {
    showToast({ kind: 'error', text: errorMessage(e) });
  }
}

/** `z`: undo the most recent undoable action. */
export function undoLast(): boolean {
  if (!lastUndo) return false;
  void undo(lastUndo);
  return true;
}

// ------------------------------------------------------------- reader header shortcuts

/** `tag-N`: toggle the Nth tag (1–9) · `tag-clear`: remove all · `tag-menu`: open the Tag menu */
export type BarCommand = 'archive' | 'delete' | 'move' | 'spam' | 'unread' | 'star' | 'tag-menu' | 'tag-clear' | `tag-${number}`;
let headerHandler: ((c: BarCommand) => void) | null = null;
/** The reader header's action bar registers itself; shortcuts drive it. */
export function setHeaderHandler(fn: ((c: BarCommand) => void) | null): void {
  headerHandler = fn;
}
export function clearHeaderHandler(fn: (c: BarCommand) => void): void {
  if (headerHandler === fn) headerHandler = null;
}

/** e, #, Delete, v, !, Shift+U, s, z — returns true when handled. */
export function runShortcut(e: KeyboardEvent): boolean {
  if (e.key === 'z') return undoLast() || true;
  // Thunderbird: 1–9 toggle the tag in that position, 0 removes all tags.
  if (/^[0-9]$/.test(e.key)) {
    headerHandler?.(e.key === '0' ? 'tag-clear' : (`tag-${Number(e.key)}` as BarCommand));
    return true;
  }
  const cmd: BarCommand | null =
    e.key === 'e'
      ? 'archive'
      : e.key === '#' || e.key === 'Delete'
        ? 'delete'
        : e.key === 'v'
          ? 'move'
          : e.key === '!'
            ? 'spam'
            : e.key === 'U'
              ? 'unread'
              : e.key === 's'
                ? 'star'
                : null;
  if (!cmd) return false;
  headerHandler?.(cmd);
  return true;
}

// ------------------------------------------------------------- server rejections

void events.onActionFailed(({ message }) => showToast({ kind: 'error', text: message }));
