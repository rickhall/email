import { createSignal } from 'solid-js';
import { accounts, folderById, selectedFolderId } from '../state';
import { api, errorMessage, events, type Addr, type Attachment, type ReplyMode } from '../api';
import { hideToast, showToast } from '../components/Toast';
import { confirmDialog } from '../components/Actions';

// The composer: one message at a time, written in the reader pane. Sending queues it in the
// core, which sends after the undo window; Undo (or a failure's "Edit") reopens it as it was.

export type ComposeMode = 'new' | 'reply' | 'replyAll' | 'forward';

export interface ComposeState {
  /** changes when a different message is opened in the composer (remounts the editor) */
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
}

let keySeq = 0;
const [compose, setCompose] = createSignal<ComposeState | null>(null);
export { compose };

/** Default account for a new message: the folder on screen's, else the first. */
function defaultAccount(): number | null {
  const f = folderById(selectedFolderId());
  return f?.accountId ?? accounts()?.[0]?.id ?? null;
}

/** Open the composer for a new message (no-op if one is open: it's kept). */
export function newMessage(): void {
  if (compose()) return;
  const accountId = defaultAccount();
  if (accountId == null) return;
  setCompose({
    key: ++keySeq,
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

/** Reply, reply all or forward message `id` (prefilled by the core). A message already being
 *  written is only replaced after asking. */
export async function startReply(id: number, mode: ReplyMode): Promise<void> {
  const cur = compose();
  if (cur && hasContent(cur) && !(await confirmDialog('Discard the message you’re writing?', 'Discard'))) return;
  try {
    const r = await api.replyContext(id, mode);
    setCompose({
      key: ++keySeq,
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

/** Keep the composer's latest fields (editor typing, address edits). */
export function updateCompose(patch: Partial<ComposeState>): void {
  const c = compose();
  if (c) setCompose({ ...c, ...patch });
}

export function closeCompose(): void {
  setCompose(null);
}

/** Has the user written anything worth confirming before discarding? */
export function hasContent(c: ComposeState): boolean {
  const text = c.html.replace(/<[^>]*>/g, '').replace(/&nbsp;/g, ' ').trim();
  return text.length > 0 || c.subject.trim().length > 0 || c.to.length + c.cc.length + c.bcc.length > 0;
}

function reopen(draft: unknown): void {
  const d = draft as ComposeState | null;
  if (d && typeof d === 'object' && 'accountId' in d) setCompose({ ...d, key: ++keySeq });
}

/** Queue the message; the composer closes and a "Sending… Undo" toast covers the window. */
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
  try {
    const q = await api.sendMessage({
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
    });
    if (compose()?.key === c.key) setCompose(null);
    const toast = showToast({
      kind: 'ok',
      text: 'Sending…',
      action: {
        label: 'Undo',
        run: () => {
          hideToast(toast);
          void api.cancelSend(q.id).then(reopen, (e) => showToast({ kind: 'error', text: errorMessage(e) }));
        },
      },
      ms: Math.max(1000, q.sendAt - Date.now()),
    });
  } catch (e) {
    showToast({ kind: 'error', text: errorMessage(e) });
  }
}

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
