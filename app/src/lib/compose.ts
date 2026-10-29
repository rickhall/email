import { createSignal } from 'solid-js';
import { accounts, folderById, selectedFolderId } from '../state';
import { api, errorMessage, events, type Addr, type Attachment, type ReplyMode, type SendRequest } from '../api';
import { hideToast, showToast } from '../components/Toast';
import { broadcast, closeThisWindow, composeDraftId, isComposeWindow, onBroadcast, openComposeWindow } from './windows';

// Writing mail happens in compose windows, one per draft (see lib/windows.ts). The main window
// starts them (new message, reply, forward) and shows "Sending… Undo" once one sends; the core
// keeps each draft, saved as it changes and shown in Drafts.

export type ComposeMode = 'new' | 'reply' | 'replyAll' | 'forward';

export interface ComposeState {
  /** changes when a different message is loaded into the composer (remounts the editor) */
  key: number;
  mode: ComposeMode;
  accountId: number;
  to: Addr[];
  cc: Addr[];
  bcc: Addr[];
  /** Cc/Bcc rows shown */
  showCc: boolean;
  subject: string;
  /** the editor's HTML */
  html: string;
  replyingTo: number | null;
  forwarding: number | null;
  /** the original, sent under the new text (Gmail-style quote / forward block); null for new */
  quoteHtml: string | null;
  /** forward: the original's attachments still to send along */
  fwdAttachments: Attachment[];
  /** the user has changed something (only then is it a draft worth keeping or listing) */
  edited?: boolean;
}

let keySeq = 0;

/** What the core needs to build (save or send) the message this state describes. */
function request(c: ComposeState, draftId: number | null): SendRequest {
  return {
    accountId: c.accountId,
    to: c.to,
    cc: c.cc,
    bcc: c.bcc,
    subject: c.subject,
    // Top-posting: the new text, then the original.
    html: c.html + (c.quoteHtml ?? ''),
    attachments: [],
    replyingTo: c.replyingTo,
    forwarding: c.forwarding,
    forwardAttachments: c.fwdAttachments.map((a) => a.index),
    draft: c,
    draftId,
    listed: !!c.edited,
  };
}

/** Has the user written anything worth keeping? (A reply opened and closed again hasn't.) */
export function hasContent(c: ComposeState): boolean {
  if (!c.edited) return false;
  const text = c.html.replace(/<[^>]*>/g, '').replace(/&nbsp;/g, ' ').trim();
  return text.length > 0 || c.subject.trim().length > 0 || c.to.length + c.cc.length + c.bcc.length > 0;
}

// ------------------------------------------------------------- main window: starting one

/** Default account for a new message: the folder on screen's, else the first. */
function defaultAccount(): number | null {
  const f = folderById(selectedFolderId());
  return f?.accountId ?? accounts()?.[0]?.id ?? null;
}

/** Save `state` as a new draft and open it in its own window. */
async function openDraftWindow(state: Omit<ComposeState, 'key'>): Promise<void> {
  try {
    const id = await api.saveDraft(null, request({ ...state, key: 0 }, null));
    await openComposeWindow(id);
  } catch (e) {
    showToast({ kind: 'error', text: errorMessage(e) });
  }
}

export function newMessage(): void {
  const accountId = defaultAccount();
  if (accountId == null) return;
  void openDraftWindow({
    mode: 'new',
    accountId,
    to: [],
    cc: [],
    bcc: [],
    showCc: false,
    subject: '',
    html: '',
    replyingTo: null,
    forwarding: null,
    quoteHtml: null,
    fwdAttachments: [],
  });
}

/** Reply, reply all or forward message `id` (prefilled by the core) in a new compose window. */
export async function startReply(id: number, mode: ReplyMode): Promise<void> {
  try {
    const r = await api.replyContext(id, mode);
    await openDraftWindow({
      mode,
      accountId: r.accountId,
      to: r.to,
      cc: r.cc,
      bcc: [],
      showCc: r.cc.length > 0,
      subject: r.subject,
      html: '',
      replyingTo: r.replyingTo,
      forwarding: r.forwarding,
      quoteHtml: r.quoteHtml,
      fwdAttachments: r.attachments,
    });
  } catch (e) {
    showToast({ kind: 'error', text: errorMessage(e) });
  }
}

/** Open an existing draft (from Drafts) in its compose window. */
export function editDraft(draftId: number): void {
  void openComposeWindow(draftId).catch((e) => showToast({ kind: 'error', text: errorMessage(e) }));
}

/** A message taken back (undo, or edit after a failure): back into a compose window. */
function reopen(draft: unknown): void {
  const d = draft as ComposeState | null;
  if (d && typeof d === 'object' && 'accountId' in d) void openDraftWindow(d);
}

if (!isComposeWindow) {
  // A compose window sent a message (and closed): the undo window plays out here.
  onBroadcast('compose-sent', ({ id, sendAt }) => {
    const toast = showToast({
      kind: 'ok',
      text: 'Sending…',
      action: {
        label: 'Undo',
        run: () => {
          hideToast(toast);
          void api.cancelSend(id).then(reopen, (e) => showToast({ kind: 'error', text: errorMessage(e) }));
        },
      },
      ms: Math.max(1000, sendAt - Date.now()),
    });
  });
  void events.onMessageSent(() => showToast({ kind: 'ok', text: 'Message sent', ms: 2500 }));
  void events.onSendFailed(({ id, message }) =>
    showToast({
      kind: 'error',
      text: `Couldn't send: ${message ?? 'unknown error'}`,
      action: {
        label: 'Edit',
        run: () => void api.cancelSend(id).then(reopen, (e) => showToast({ kind: 'error', text: errorMessage(e) })),
      },
    }),
  );
}

// ------------------------------------------------------------- compose window: the draft

const [compose, setCompose] = createSignal<ComposeState | null>(null);
export { compose };

/** Load this window's draft. */
export async function loadDraft(): Promise<void> {
  if (composeDraftId == null) return;
  const d = (await api.openDraft(composeDraftId)) as ComposeState;
  setCompose({ ...d, key: ++keySeq });
}

let saveTimer: ReturnType<typeof setTimeout> | undefined;
/** Saved shortly after the last change (and right away before sending or closing). */
function scheduleSave() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => void saveNow(), 800);
}
export async function saveNow(): Promise<void> {
  clearTimeout(saveTimer);
  const c = compose();
  if (!c || composeDraftId == null) return;
  try {
    await api.saveDraft(composeDraftId, request(c, composeDraftId));
  } catch (e) {
    console.warn('saving draft', e);
  }
}

export function updateCompose(patch: Partial<ComposeState>): void {
  const c = compose();
  if (!c) return;
  setCompose({ ...c, ...patch, edited: true });
  scheduleSave();
}

/** Queue the message; the window closes and the main window shows "Sending… Undo". */
export async function send(c: ComposeState): Promise<void> {
  const bad = [...c.to, ...c.cc, ...c.bcc].find((a) => !isAddress(a.addr));
  if (bad) {
    showToast({ kind: 'error', text: `“${bad.addr}” isn't an email address.` });
    return;
  }
  if (c.to.length + c.cc.length + c.bcc.length === 0) {
    showToast({ kind: 'error', text: 'Add at least one recipient.' });
    return;
  }
  clearTimeout(saveTimer);
  try {
    const q = await api.sendMessage(request(c, composeDraftId));
    broadcast('compose-sent', { id: q.id, sendAt: q.sendAt });
    await closeThisWindow();
  } catch (e) {
    showToast({ kind: 'error', text: errorMessage(e) });
  }
}

/** Throw the draft away and close. */
export async function discardAndClose(): Promise<void> {
  clearTimeout(saveTimer);
  if (composeDraftId != null) await api.deleteDraft(composeDraftId).catch(() => {});
  await closeThisWindow();
}

/** Keep the draft (in Drafts) and close. */
export async function saveAndClose(): Promise<void> {
  await saveNow();
  await closeThisWindow();
}

// ------------------------------------------------------------- addresses

/** Plausible single email address (the server has the last word). */
export function isAddress(s: string): boolean {
  return /^[^\s@<>(),;:"]+@[^\s@<>(),;:"]+\.[^\s@<>(),;:"]+$/.test(s.trim());
}

/**
 * Addresses typed or pasted into a To/Cc field: "Name <addr>", "addr", or several separated by
 * commas, semicolons or new lines (commas inside quotes belong to the name).
 */
export function parseAddresses(text: string): Addr[] {
  const parts: string[] = [];
  let cur = '';
  let quoted = false;
  let angle = false;
  for (const ch of text) {
    if (ch === '"') quoted = !quoted;
    else if (ch === '<') angle = true;
    else if (ch === '>') angle = false;
    if (!quoted && !angle && (ch === ',' || ch === ';' || ch === '\n')) {
      parts.push(cur);
      cur = '';
    } else cur += ch;
  }
  parts.push(cur);
  const out: Addr[] = [];
  for (const raw of parts) {
    const p = raw.trim();
    if (!p) continue;
    const m = /^(.*?)<([^<>]+)>\s*$/.exec(p);
    if (m) {
      const name = m[1]!.trim().replace(/^"(.*)"$/, '$1').trim();
      out.push({ name: name || null, addr: m[2]!.trim() });
    } else {
      out.push({ name: null, addr: p.replace(/^mailto:/i, '') });
    }
  }
  return out;
}
