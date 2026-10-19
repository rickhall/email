// In-memory fake backend so `npm run dev` works in a plain browser.
// Implements the exact command/event surface from api-contract.md.
// Append `?empty` to the URL to start with no accounts (onboarding flow).
// Append `?auth` to simulate the server rejecting the second account's saved password.

import type {
  Account,
  AddAccountArgs,
  Attachment,
  Backend,
  CommandMap,
  CommandName,
  EventMap,
  EventName,
  Folder,
  FolderRole,
  MessageDetail,
  MessageSummary,
  ConversationScope,
  Provider,
  SearchField,
  ThreadSummary,
  SyncPhase,
  SyncStatus,
} from './types';

import { fieldMatches, parseQuery, tokenize, type Term } from './lib/query';

// ---------------------------------------------------------------- utilities

function mulberry32(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

function pick<T>(rnd: () => number, arr: readonly T[]): T {
  return arr[Math.floor(rnd() * arr.length)] as T;
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) =>
    c === '&' ? '&amp;' : c === '<' ? '&lt;' : c === '>' ? '&gt;' : c === '"' ? '&quot;' : '&#39;',
  );
}

// ---------------------------------------------------------------- content pools

interface Person {
  name: string;
  addr: string;
}

const PEOPLE: Person[] = [
  { name: 'Sarah Chen', addr: 'sarah.chen@northwind.io' },
  { name: 'Marcus Okafor', addr: 'marcus@okafor.design' },
  { name: 'Priya Raman', addr: 'priya.raman@acme-corp.com' },
  { name: 'Tom Becker', addr: 'tbecker@globex.com' },
  { name: 'Elena Rossi', addr: 'elena.rossi@studio-rossi.it' },
  { name: 'James Whitfield', addr: 'james@whitfield.law' },
  { name: 'Aiko Tanaka', addr: 'aiko.tanaka@kumo.jp' },
  { name: 'Daniel Park', addr: 'dpark@initech.com' },
  { name: 'Hannah Müller', addr: 'hannah.mueller@bergwerk.de' },
  { name: 'Luis Fernández', addr: 'luis@fernandez.dev' },
  { name: 'Grace Hall', addr: 'grace.hall@gmail.com' },
  { name: 'Olivia Brooks', addr: 'olivia@brooksandco.com' },
  { name: 'Noah Williams', addr: 'noah.w@umbrella.health' },
  { name: 'Fatima Al-Sayed', addr: 'fatima@sayed-partners.ae' },
  { name: 'Ben Carter', addr: 'ben.carter@hey.com' },
  { name: 'Chloé Martin', addr: 'chloe.martin@lune.fr' },
  { name: 'Ravi Patel', addr: 'ravi@patel.consulting' },
  { name: 'Zoe Lindqvist', addr: 'zoe.lindqvist@nordlys.se' },
];

type Kind = 'person' | 'personHtml' | 'brief' | 'promo' | 'receipt' | 'notify';

interface Sender extends Person {
  kind: Kind;
}

const SENDERS: Sender[] = [
  { name: 'The Morning Brief', addr: 'hello@morningbrief.news', kind: 'brief' },
  { name: 'Lumen Goods', addr: 'news@lumengoods.com', kind: 'promo' },
  { name: 'Fieldnotes Weekly', addr: 'editor@fieldnotes.email', kind: 'brief' },
  { name: 'Northwind Air', addr: 'no-reply@northwind-air.com', kind: 'receipt' },
  { name: 'Parcel & Co.', addr: 'orders@parcelco.shop', kind: 'receipt' },
  { name: 'Invoicely Billing', addr: 'invoices@invoicely.io', kind: 'receipt' },
  { name: 'GitHub', addr: 'notifications@github.com', kind: 'notify' },
  { name: 'Linear', addr: 'notifications@linear.app', kind: 'notify' },
  { name: 'Figma', addr: 'no-reply@figma.com', kind: 'notify' },
  { name: 'Atelier Supply', addr: 'hi@ateliersupply.co', kind: 'promo' },
];

const PERSON_SUBJECTS = [
  'Quick question about the Q3 roadmap',
  'Re: Contract draft v4',
  'Dinner on Saturday?',
  'Notes from today’s sync',
  'Re: Re: Invoice #{n}',
  'Updated proposal attached',
  'Can we move our 1:1?',
  'Photos from the weekend',
  'Feedback on the onboarding flow',
  'Re: Hiring plan for next year',
  'Intro: you two should talk',
  'Flight details for Lisbon',
  'Fwd: Office lease renewal',
  'Draft blog post — thoughts?',
  'Re: Budget review',
  'Welcome to the team!',
  'Weekly status — week {n}',
  'Following up on our call',
  'Re: Design review for v2.{n}',
  'Lunch tomorrow?',
];

const PERSON_SNIPPETS = [
  'Hey — I took another pass at the numbers and I think we can make it work if we push the launch by two weeks.',
  'Thanks for sending this over. A few comments inline, mostly small wording changes on section three.',
  'Are you around later today? I’d love fifteen minutes to talk through the open questions before Friday.',
  'Attached is the latest version. The main change is the payment schedule, which now lines up with milestones.',
  'Loved seeing everyone last weekend! Here are the photos I promised — the sunset ones turned out great.',
  'Quick heads up: the client asked for a revised estimate, so I’ve put a placeholder on the calendar for Tuesday.',
  'I’ve been thinking about what you said in the retro, and I think you’re right about simplifying the setup.',
  'Following up on the thread below — did we land on a decision for the vendor? Happy to make the call.',
];

const NOTIFY_SUBJECTS = [
  '[email/core] Fix IMAP IDLE reconnect loop (#{n})',
  '[email/app] Virtualized message list (#{n})',
  'ENG-{n} moved to In Review',
  'Rick, you were mentioned in “Inbox redesign”',
  'Your weekly digest: {n} new comments',
];

const BRIEF_SUBJECTS = [
  'The Morning Brief: markets, Mars, and a very good dog',
  'Fieldnotes #{n}: the quiet craft of small software',
  'This week: what we learned shipping {n} releases',
  'Your Sunday reading list',
];
const PROMO_SUBJECTS = [
  'The Autumn Collection is here',
  'Last chance: 20% off everything',
  'New arrivals, picked for you',
  'Your cart misses you',
];
const RECEIPT_SUBJECTS = [
  'Your order #{n} has shipped',
  'Booking confirmed — ref {n}',
  'Receipt for your purchase',
];

const ATTACHMENTS: Attachment[] = [
  { name: 'Q3-roadmap.pdf', mime: 'application/pdf', size: 482_113 },
  { name: 'IMG_2041.jpg', mime: 'image/jpeg', size: 2_841_002 },
  { name: 'budget-2027.xlsx', mime: 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet', size: 61_440 },
  { name: 'contract-v4.docx', mime: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document', size: 118_272 },
  { name: 'wireframes.fig', mime: 'application/octet-stream', size: 9_420_800 },
  { name: 'invoice-2291.pdf', mime: 'application/pdf', size: 88_120 },
];

// ---------------------------------------------------------------- bodies

const LOGO_SVG =
  'data:image/svg+xml;base64,' +
  btoa(
    '<svg xmlns="http://www.w3.org/2000/svg" width="120" height="32" viewBox="0 0 120 32"><rect rx="8" width="32" height="32" fill="#ff5a36"/><path d="M9 22l7-12 7 12z" fill="#fff"/><text x="40" y="22" font-family="Helvetica,Arial" font-size="16" font-weight="700" fill="#1d1d1f">Parcel</text></svg>',
  );

function briefHtml(subject: string, seed: number): string {
  const img = (w: number, h: number, k: string) =>
    `<img src="https://picsum.photos/seed/${k}${seed}/${w}/${h}" width="${w}" height="${h}" alt="" style="display:block;width:100%;max-width:${w}px;height:auto;border-radius:6px">`;
  const story = (kicker: string, title: string, body: string, k: string) => `
    <tr><td style="padding:24px 32px 0">
      ${img(536, 220, k)}
      <p style="margin:16px 0 4px;font:600 11px/1.4 Helvetica,Arial,sans-serif;letter-spacing:.08em;text-transform:uppercase;color:#c2410c">${kicker}</p>
      <h2 style="margin:0 0 8px;font:700 22px/1.25 Georgia,serif;color:#111">${title}</h2>
      <p style="margin:0;font:16px/1.6 Georgia,serif;color:#333">${body} <a href="https://example.com/read" style="color:#c2410c">Read more →</a></p>
    </td></tr>`;
  return `<!doctype html><html><head><style>
    .wrap{background:#f4f1ea;padding:24px 0}
    @media (max-width:620px){ .card{width:100%!important} }
  </style></head><body style="margin:0;background:#f4f1ea">
  <div class="wrap"><table role="presentation" align="center" width="600" class="card" cellpadding="0" cellspacing="0" style="background:#fffdf8;border:1px solid #e7e1d4;border-radius:10px;border-collapse:separate">
    <tr><td style="padding:28px 32px 20px;border-bottom:3px double #e7e1d4">
      <table width="100%" role="presentation"><tr>
        <td style="font:800 26px/1 Georgia,serif;color:#111">${escapeHtml(subject.replace(/^((re|fwd?):\s*)+/i, '').split(':')[0] ?? 'Brief')}</td>
        <td align="right" style="font:12px Helvetica,Arial,sans-serif;color:#8a8170">Issue ${(seed % 400) + 12}</td>
      </tr></table>
    </td></tr>
    <tr><td style="padding:20px 32px 0;font:17px/1.6 Georgia,serif;color:#333">
      Good morning. Here’s what you need to know today, in about four minutes.
    </td></tr>
    ${story('Markets', 'Stocks drift higher as rate fears ease', 'Investors shrugged off a soft jobs report as bond yields fell for a third straight session, lifting tech shares and small caps alike.', 'a')}
    ${story('Science', 'A new map of Mars’ ancient shorelines', 'Researchers stitched together a decade of orbital imagery to trace what may be the edges of an ocean that vanished three billion years ago.', 'b')}
    <tr><td style="padding:28px 32px">
      <table width="100%" role="presentation" cellpadding="0" cellspacing="0" style="background:#fff4e5;border-radius:8px">
        <tr><td style="padding:16px 20px;font:15px/1.5 Helvetica,Arial,sans-serif;color:#5b3a12">
          <strong>Quick hits</strong>
          <ul style="margin:8px 0 0;padding-left:18px">
            <li>The city council approved the new waterfront park.</li>
            <li>A record 41% of commuters now cycle at least once a week.</li>
            <li>The very good dog of the week is named Biscuit.</li>
          </ul>
        </td></tr>
      </table>
    </td></tr>
    <tr><td align="center" style="padding:0 32px 28px">
      <a href="https://example.com/subscribe" style="display:inline-block;background:#111;color:#fff;text-decoration:none;font:600 14px Helvetica,Arial,sans-serif;padding:12px 22px;border-radius:999px">Share the Brief</a>
    </td></tr>
    <tr><td style="padding:16px 32px;border-top:1px solid #eee6d6;font:12px/1.5 Helvetica,Arial,sans-serif;color:#9a917f" align="center">
      You’re receiving this because you subscribed. <a href="https://example.com/unsub" style="color:#9a917f">Unsubscribe</a>
    </td></tr>
  </table></div></body></html>`;
}

function promoHtml(seed: number): string {
  const product = (k: string, name: string, price: string) => `
    <td width="50%" valign="top" style="padding:8px">
      <img src="https://picsum.photos/seed/${k}${seed}/260/300" width="260" alt="${name}" style="display:block;width:100%;height:auto;border-radius:4px;background:#eee">
      <p style="margin:10px 0 2px;font:600 15px Helvetica,Arial,sans-serif;color:#1a1a1a">${name}</p>
      <p style="margin:0;font:14px Helvetica,Arial,sans-serif;color:#6b6b6b">${price}</p>
    </td>`;
  return `<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#ffffff">
  <tr><td align="center" style="padding:24px 0">
    <table role="presentation" width="600" cellpadding="0" cellspacing="0" style="max-width:600px">
      <tr><td align="center" style="padding:8px 0 24px;font:700 13px Helvetica,Arial,sans-serif;letter-spacing:.3em;color:#1a1a1a">LUMEN GOODS</td></tr>
      <tr><td style="background:#2d3a2e url('https://picsum.photos/seed/hero${seed}/600/320') center/cover;height:320px;border-radius:2px" valign="bottom">
        <div style="padding:28px;font:300 34px/1.15 Georgia,serif;color:#fff">The Autumn<br>Collection</div>
      </td></tr>
      <tr><td style="padding:28px 24px 8px;font:16px/1.6 Helvetica,Arial,sans-serif;color:#333" align="center">
        Wool, waxed canvas and warm light. Pieces made to be used every day and kept for years.
      </td></tr>
      <tr><td align="center" style="padding:12px 0 24px">
        <a href="https://example.com/shop" style="display:inline-block;border:1px solid #1a1a1a;color:#1a1a1a;text-decoration:none;font:600 13px Helvetica,Arial,sans-serif;letter-spacing:.12em;padding:12px 28px">SHOP NOW</a>
      </td></tr>
      <tr><td><table role="presentation" width="100%" cellpadding="0" cellspacing="0">
        <tr>${product('p1', 'Merino Crew', '$128')}${product('p2', 'Canvas Tote', '$64')}</tr>
        <tr>${product('p3', 'Ceramic Lamp', '$210')}${product('p4', 'Field Notebook', '$18')}</tr>
      </table></td></tr>
      <tr><td align="center" style="padding:32px 24px;font:12px/1.6 Helvetica,Arial,sans-serif;color:#999">
        Lumen Goods · 12 Harbor St, Portland, ME<br><a href="https://example.com/unsub" style="color:#999">Unsubscribe</a> · <a href="https://example.com/prefs" style="color:#999">Preferences</a>
      </td></tr>
    </table>
  </td></tr></table>`;
}

function receiptHtml(subject: string, seed: number): string {
  const rnd = mulberry32(seed);
  const items = [
    ['Wireless Charging Pad', 1, 39],
    ['USB-C Cable (2m)', 2, 14],
    ['Desk Mat — Charcoal', 1, 29],
  ] as const;
  const rows = items
    .map(
      ([n, q, p]) => `<tr>
        <td style="padding:10px 0;border-bottom:1px solid #eee">${n}</td>
        <td style="padding:10px 0;border-bottom:1px solid #eee;text-align:center;color:#666">${q}</td>
        <td style="padding:10px 0;border-bottom:1px solid #eee;text-align:right">$${(q * p).toFixed(2)}</td></tr>`,
    )
    .join('');
  const subtotal = items.reduce((s, [, q, p]) => s + q * p, 0);
  const tax = Math.round(subtotal * 0.0825 * 100) / 100;
  return `<div style="background:#f6f7f9;padding:32px 12px;font-family:-apple-system,Helvetica,Arial,sans-serif">
  <table role="presentation" align="center" width="560" cellpadding="0" cellspacing="0" style="background:#fff;border-radius:12px;box-shadow:0 1px 3px rgba(0,0,0,.08)">
    <tr><td style="padding:28px 32px 8px"><img src="${LOGO_SVG}" width="120" height="32" alt="Parcel"></td></tr>
    <tr><td style="padding:8px 32px 0"><h1 style="margin:0;font-size:22px;color:#111">${escapeHtml(subject)}</h1>
      <p style="color:#555;font-size:15px;line-height:1.5">Good news — your package is on its way. Estimated delivery: <strong>${['Tuesday', 'Wednesday', 'Thursday'][Math.floor(rnd() * 3)]}</strong>.</p></td></tr>
    <tr><td style="padding:8px 32px">
      <table width="100%" cellpadding="0" cellspacing="0" style="font-size:14px;color:#222;border-collapse:collapse">
        <tr><th align="left" style="padding-bottom:8px;color:#888;font-weight:500">Item</th><th style="padding-bottom:8px;color:#888;font-weight:500">Qty</th><th align="right" style="padding-bottom:8px;color:#888;font-weight:500">Price</th></tr>
        ${rows}
        <tr><td colspan="2" style="padding:10px 0;color:#666">Subtotal</td><td style="text-align:right">$${subtotal.toFixed(2)}</td></tr>
        <tr><td colspan="2" style="padding:4px 0;color:#666">Tax</td><td style="text-align:right">$${tax.toFixed(2)}</td></tr>
        <tr><td colspan="2" style="padding:12px 0;font-weight:700;font-size:16px">Total</td><td style="text-align:right;font-weight:700;font-size:16px">$${(subtotal + tax).toFixed(2)}</td></tr>
      </table>
    </td></tr>
    <tr><td style="padding:16px 32px 32px"><a href="https://example.com/track" style="display:block;text-align:center;background:#ff5a36;color:#fff;text-decoration:none;font-weight:600;padding:14px;border-radius:8px">Track package</a></td></tr>
  </table></div>`;
}

function notifyHtml(subject: string, seed: number): string {
  return `<div style="font-family:-apple-system,Segoe UI,Helvetica,Arial,sans-serif;font-size:14px;line-height:1.5;color:#24292f;max-width:640px;padding:16px">
    <p><strong>@sarah-chen</strong> commented on <a href="https://github.com/example/email/pull/${seed % 900}">${escapeHtml(subject)}</a>:</p>
    <blockquote style="margin:0;padding:0 1em;color:#57606a;border-left:.25em solid #d0d7de">
      Looks good overall! One nit: we should debounce the <code style="background:#f6f8fa;padding:.2em .4em;border-radius:6px;font-size:85%">messages-changed</code> refetch so we don't thrash on large syncs.
    </blockquote>
    <pre style="background:#f6f8fa;border-radius:6px;padding:12px;overflow:auto;font-size:12px"><code>- let page = fetch(offset);
+ let page = cache.get(offset) ?? fetch(offset);</code></pre>
    <p style="color:#57606a;font-size:12px">— Reply to this email directly or <a href="https://github.com/notifications">view it on GitHub</a>.</p>
  </div>`;
}

function personText(p: Person, me: Person, snippet: string, seed: number): string {
  const rnd = mulberry32(seed);
  const first = me.name.split(' ')[0];
  const extra = pick(rnd, [
    'I put the shared doc here: https://docs.example.com/d/1A2b3C4d — feel free to comment directly.',
    'There’s a short recording of the walkthrough at https://video.example.com/w/8f2k if that’s easier.',
    'No rush on this, end of week is totally fine.',
    'Let me know what works and I’ll send an invite.',
  ]);
  const quoted = rnd() < 0.4
    ? `\n\nOn ${new Date(Date.now() - 86400_000 * 3).toDateString()}, ${me.name} <${me.addr}> wrote:\n> Sounds good — can you send over the latest when you have it?\n> Thanks!`
    : '';
  return `Hi ${first},\n\n${snippet}\n\n${extra}\n\nA couple of other things while I have you:\n  • The staging environment is back up.\n  • I moved the review to Thursday at 2pm.\n\nBest,\n${p.name.split(' ')[0]}${quoted}`;
}

function personHtml(p: Person, me: Person, snippet: string): string {
  return `<div style="font-family:Helvetica,Arial,sans-serif;font-size:14px;line-height:1.55;color:#222">
  <p>Hi ${escapeHtml(me.name.split(' ')[0] ?? '')},</p>
  <p>${escapeHtml(snippet)}</p>
  <p>Here's the summary table we discussed:</p>
  <table style="border-collapse:collapse;font-size:13px">
    <tr style="background:#f2f4f7"><th style="border:1px solid #d9dde3;padding:6px 10px;text-align:left">Milestone</th><th style="border:1px solid #d9dde3;padding:6px 10px">Owner</th><th style="border:1px solid #d9dde3;padding:6px 10px">Due</th></tr>
    <tr><td style="border:1px solid #d9dde3;padding:6px 10px">Sync engine beta</td><td style="border:1px solid #d9dde3;padding:6px 10px">Rick</td><td style="border:1px solid #d9dde3;padding:6px 10px">Nov 4</td></tr>
    <tr><td style="border:1px solid #d9dde3;padding:6px 10px">UI polish pass</td><td style="border:1px solid #d9dde3;padding:6px 10px">${escapeHtml(p.name)}</td><td style="border:1px solid #d9dde3;padding:6px 10px">Nov 11</td></tr>
  </table>
  <p>Thanks!<br><b>${escapeHtml(p.name)}</b><br><span style="color:#888">${escapeHtml(p.addr)}</span></p>
</div>`;
}

const REMOTE_IMG_RE = /<img\b[^>]*\bsrc=["']https?:[^"']*["'][^>]*>/gi;
const REMOTE_URL_RE = /url\(['"]?https?:[^)'"]*['"]?\)/gi;
function stripRemote(html: string): { html: string; had: boolean } {
  let had = false;
  const out = html
    .replace(REMOTE_IMG_RE, () => {
      had = true;
      return '';
    })
    .replace(REMOTE_URL_RE, () => {
      had = true;
      return 'none';
    });
  return { html: out, had };
}

// ---------------------------------------------------------------- backend state

interface Meta {
  kind: Kind;
  seed: number;
  accountId: number;
  attachments: Attachment[];
  cc: boolean;
  threadId: number;
}

interface FolderSpec {
  path: string[];
  role: FolderRole;
  count: number;
  selectable?: boolean;
  name?: string;
}

const GMAIL_FOLDERS: FolderSpec[] = [
  { path: ['INBOX'], role: 'inbox', count: 20_000, name: 'Inbox' },
  { path: ['[Gmail]'], role: null, count: 0, selectable: false },
  { path: ['[Gmail]', 'Starred'], role: 'flagged', count: 86, name: 'Starred' },
  { path: ['[Gmail]', 'Important'], role: 'important', count: 1_400 },
  { path: ['[Gmail]', 'Sent Mail'], role: 'sent', count: 2_400, name: 'Sent' },
  { path: ['[Gmail]', 'Drafts'], role: 'drafts', count: 7 },
  { path: ['[Gmail]', 'All Mail'], role: 'all', count: 6_000 },
  { path: ['[Gmail]', 'Spam'], role: 'junk', count: 143 },
  { path: ['[Gmail]', 'Trash'], role: 'trash', count: 312 },
  { path: ['Newsletters'], role: null, count: 900 },
  { path: ['Personal'], role: null, count: 50 },
  { path: ['Personal', 'Family'], role: null, count: 70 },
  { path: ['Personal', 'Travel'], role: null, count: 44 },
  { path: ['Receipts'], role: null, count: 380 },
  { path: ['Work'], role: null, count: 240 },
  { path: ['Work', 'Clients'], role: null, count: 120 },
  { path: ['Work', 'Clients', 'Acme'], role: null, count: 64 },
  { path: ['Work', 'Clients', 'Globex'], role: null, count: 38 },
  { path: ['Work', 'Invoices'], role: null, count: 90 },
];

const IMAP_FOLDERS: FolderSpec[] = [
  { path: ['INBOX'], role: 'inbox', count: 1_800, name: 'Inbox' },
  { path: ['Sent'], role: 'sent', count: 300 },
  { path: ['Drafts'], role: 'drafts', count: 2 },
  { path: ['Archive'], role: 'archive', count: 4_000 },
  { path: ['Junk'], role: 'junk', count: 20 },
  { path: ['Trash'], role: 'trash', count: 40 },
  { path: ['Projects'], role: null, count: 0, selectable: false },
  { path: ['Projects', 'Atlas'], role: null, count: 120 },
  { path: ['Projects', 'Borealis'], role: null, count: 75 },
  { path: ['Lists'], role: null, count: 0, selectable: false },
  { path: ['Lists', 'rust-users'], role: null, count: 600 },
];

const SMALL_FOLDERS: FolderSpec[] = [
  { path: ['INBOX'], role: 'inbox', count: 400, name: 'Inbox' },
  { path: ['Sent'], role: 'sent', count: 60 },
  { path: ['Drafts'], role: 'drafts', count: 0 },
  { path: ['Archive'], role: 'archive', count: 200 },
  { path: ['Junk'], role: 'junk', count: 4 },
  { path: ['Trash'], role: 'trash', count: 9 },
];

export function createMockBackend(): Backend {
  const listeners = new Map<string, Set<(p: unknown) => void>>();
  const emit = <K extends EventName>(event: K, payload: EventMap[K]) => {
    if (event === 'messages-changed') searchCache = null;
    const set = listeners.get(event);
    if (!set) return;
    setTimeout(() => set.forEach((cb) => cb(payload)), 0);
  };

  const accounts: Account[] = [];
  const folders = new Map<number, Folder>();
  const folderMsgs = new Map<number, MessageSummary[]>();
  const byId = new Map<number, MessageSummary>();
  const remoteAllowed = new Set<number>();
  /** Last search result set; any messages-changed invalidates it. */
  let searchCache: { key: string; results: MessageSummary[] } | null = null;
  /** threadId -> every message of the conversation (any folder) */
  const threads = new Map<number, MessageSummary[]>();
  const meta = new Map<number, Meta>();
  const downloaded = new Set<number>();
  const pendingDownloads = new Set<number>();
  const statuses = new Map<number, SyncStatus>();

  let nextFolderId = 1;
  let nextMsgId = 1;
  let nextAccountId = 1;

  function makeMessage(
    folder: Folder,
    me: Person,
    rnd: () => number,
    date: number,
    index: number,
    forceUnread = false,
  ): MessageSummary {
    const id = nextMsgId++;
    const isSent = folder.role === 'sent' || folder.role === 'drafts';
    const r = rnd();
    let kind: Kind;
    let from: Person;
    if (isSent) {
      kind = rnd() < 0.85 ? 'person' : 'personHtml';
      from = me;
    } else if (r < 0.55) {
      kind = rnd() < 0.75 ? 'person' : 'personHtml';
      from = pick(rnd, PEOPLE);
    } else {
      const s = pick(rnd, SENDERS);
      kind = s.kind;
      from = s;
    }
    const n = Math.floor(rnd() * 9000) + 100;
    const tmpl =
      kind === 'person' || kind === 'personHtml'
        ? pick(rnd, PERSON_SUBJECTS)
        : kind === 'brief'
          ? pick(rnd, BRIEF_SUBJECTS)
          : kind === 'promo'
            ? pick(rnd, PROMO_SUBJECTS)
            : kind === 'receipt'
              ? pick(rnd, RECEIPT_SUBJECTS)
              : pick(rnd, NOTIFY_SUBJECTS);
    const subject = tmpl.replace('{n}', String(n));
    const snippet =
      kind === 'person' || kind === 'personHtml'
        ? pick(rnd, PERSON_SNIPPETS)
        : kind === 'brief'
          ? 'Good morning. Here’s what you need to know today, in about four minutes. Markets, science, and quick hits.'
          : kind === 'promo'
            ? 'Wool, waxed canvas and warm light. Pieces made to be used every day and kept for years.'
            : kind === 'receipt'
              ? 'Good news — your package is on its way. Track your delivery and view your receipt.'
              : '@sarah-chen commented: Looks good overall! One nit: we should debounce the refetch.';
    const hasAttachments = (kind === 'person' || kind === 'personHtml') && rnd() < 0.12;
    const unread = forceUnread || (folder.role !== 'sent' && (index < 40 ? rnd() < 0.5 : rnd() < 0.03));
    const flagged = folder.role === 'flagged' || rnd() < 0.02;
    const m: MessageSummary = {
      id,
      folderId: folder.id,
      subject,
      fromName: from.name,
      fromAddr: from.addr,
      date,
      snippet: rnd() < 0.97 ? snippet : null,
      unread,
      flagged,
      hasAttachments,
      hasBody: false,
    };
    const attachments: Attachment[] = [];
    if (hasAttachments) {
      const k = 1 + Math.floor(rnd() * 3);
      for (let i = 0; i < k; i++) attachments.push(pick(rnd, ATTACHMENTS));
    }
    meta.set(id, { kind, seed: Math.floor(rnd() * 1e9), accountId: folder.accountId, attachments, cc: rnd() < 0.3, threadId: id });
    // Most bodies are already local; ~25% must be "downloaded" on demand.
    if (rnd() < 0.75) {
      downloaded.add(id);
      m.hasBody = true;
    }
    byId.set(id, m);
    return m;
  }

  function createAccount(email: string, displayName: string, provider: Provider, specs: FolderSpec[]): Account {
    const acc: Account = { id: nextAccountId++, email, displayName, provider };
    accounts.push(acc);
    const me: Person = { name: displayName, addr: email };
    const rnd = mulberry32(acc.id * 7919);
    const now = Math.floor(Date.now() / 1000);
    for (const spec of specs) {
      const f: Folder = {
        id: nextFolderId++,
        accountId: acc.id,
        path: spec.path,
        displayName: spec.name ?? spec.path[spec.path.length - 1] ?? '',
        role: spec.role,
        selectable: spec.selectable ?? true,
        total: 0,
        unread: 0,
        syncedCount: 0,
      };
      folders.set(f.id, f);
      const list: MessageSummary[] = [];
      let t = now - Math.floor(rnd() * 600);
      // Spread dates so we get today / yesterday / this week / older / previous years.
      const avgGap = spec.count > 5000 ? 5400 : spec.count > 500 ? 14_000 : 60_000;
      for (let i = 0; i < spec.count; i++) {
        list.push(makeMessage(f, me, rnd, t, i));
        t -= Math.floor(60 + rnd() * rnd() * avgGap * 3);
      }
      folderMsgs.set(f.id, list);
      recount(f);
    }
    threadify(acc.id, me, rnd);
    return acc;
  }

  const baseSubject = (s: string) => s.replace(/^((re|fwd?|aw):\s*)+/i, '');
  const threadOf = (m: MessageSummary) => meta.get(m.id)?.threadId ?? m.id;
  function addToThread(tid: number, m: MessageSummary) {
    const md = meta.get(m.id);
    if (md) md.threadId = tid;
    const list = threads.get(tid);
    if (list) list.push(m);
    else threads.set(tid, [m]);
  }

  /** Turn some inbox messages into conversations of 2–8 messages, partly living in Sent. */
  function threadify(accountId: number, me: Person, rnd: () => number) {
    const accFolders = [...folders.values()].filter((f) => f.accountId === accountId);
    const inbox = accFolders.find((f) => f.role === 'inbox');
    const sent = accFolders.find((f) => f.role === 'sent');
    const trash = accFolders.find((f) => f.role === 'trash');
    const allMail = accFolders.find((f) => f.role === 'all' || f.role === 'archive');
    const allList = allMail ? folderMsgs.get(allMail.id) ?? null : null;
    const extraAll: MessageSummary[] = [];
    if (!inbox) return;
    const trashList = trash ? folderMsgs.get(trash.id) ?? null : null;
    const extraTrash: MessageSummary[] = [];
    const inboxList = folderMsgs.get(inbox.id) ?? [];
    const sentList = sent ? folderMsgs.get(sent.id) ?? null : null;
    const extraInbox: MessageSummary[] = [];
    const extraSent: MessageSummary[] = [];
    // Make sure the very top of the inbox has a showcase thread that starts with a forwarded newsletter.
    let showcase = inboxList.slice(0, 8).findIndex((m) => meta.get(m.id)?.kind === 'person');
    inboxList.forEach((m, idx) => {
      const md = meta.get(m.id);
      if (!md || (md.kind !== 'person' && md.kind !== 'personHtml')) return;
      const isShowcase = idx === showcase;
      if (!isShowcase && rnd() > 0.33) return;
      const k = isShowcase ? 4 : 1 + Math.floor(rnd() * rnd() * 7);
      const base = isShowcase ? 'Fwd: The Morning Brief: markets, Mars, and a very good dog' : baseSubject(m.subject);
      const other = rnd() < 0.5 ? pick(rnd, PEOPLE) : { name: m.fromName, addr: m.fromAddr };
      const cast: Person[] = [{ name: m.fromName, addr: m.fromAddr }, other];
      let t = m.date;
      for (let j = 0; j < k; j++) {
        t -= Math.floor(900 + rnd() * rnd() * 86400 * 1.5);
        const oldest = j === k - 1;
        const fromMe = !!sentList && !(isShowcase && oldest) && (isShowcase ? j === 0 : rnd() < 0.45);
        // Some replies were deleted: they live in Trash (the showcase always has one).
        const trashed =
          !fromMe && !oldest && !!trashList && (isShowcase ? j === 1 : rnd() < 0.1);
        // Others were archived: only in All Mail / Archive (the showcase always has one).
        const archived =
          !fromMe && !trashed && !(isShowcase && oldest) && !!allList && (isShowcase ? j === 2 : rnd() < 0.25);
        const folder = fromMe ? sent! : trashed ? trash! : archived ? allMail! : inbox;
        const r = makeMessage(folder, me, rnd, t, 1000);
        const rm = meta.get(r.id)!;
        const p = fromMe ? me : pick(rnd, cast);
        r.fromName = p.name;
        r.fromAddr = p.addr;
        if (rm.kind !== 'person' && rm.kind !== 'personHtml') rm.kind = 'person';
        r.snippet = pick(rnd, PERSON_SNIPPETS);
        r.subject = oldest ? base : `Re: ${base}`;
        // Recent conversations: the reply just before the newest is sometimes unread too.
        r.unread = !fromMe && j === 0 && m.unread && rnd() < 0.5;
        if (isShowcase && oldest) {
          rm.kind = 'brief';
          r.snippet = '---------- Forwarded message ---------- From: The Morning Brief. Good morning. Here’s what you need to know today.';
          r.hasAttachments = false;
          rm.attachments = [];
        }
        addToThread(m.id, r);
        (fromMe ? extraSent : trashed ? extraTrash : archived ? extraAll : extraInbox).push(r);
      }
      m.subject = `Re: ${base}`;
      addToThread(m.id, m);
    });
    showcase = -1;
    const byDateDesc = (a: MessageSummary, b: MessageSummary) => b.date - a.date;
    if (extraInbox.length) {
      inboxList.push(...extraInbox);
      inboxList.sort(byDateDesc);
      recount(inbox);
    }
    if (allMail && allList && extraAll.length) {
      allList.push(...extraAll);
      allList.sort(byDateDesc);
      recount(allMail);
    }
    if (trash && trashList && extraTrash.length) {
      trashList.push(...extraTrash);
      trashList.sort(byDateDesc);
      recount(trash);
    }
    if (sent && sentList && extraSent.length) {
      sentList.push(...extraSent);
      sentList.sort(byDateDesc);
      recount(sent);
    }
  }

  function threadMembers(tid: number): MessageSummary[] {
    const list = threads.get(tid);
    if (list) return [...list].sort((a, b) => a.date - b.date);
    const m = byId.get(tid);
    return m ? [m] : [];
  }

  /** api-contract ConversationScope: current folder always; 'all' adds non-Trash/Spam; includeTrash adds Trash/Spam. */
  function selectMembers(tid: number, folderId: number, scope: ConversationScope, includeTrash: boolean) {
    return threadMembers(tid).filter((m) => {
      if (m.folderId === folderId) return true;
      const role = folders.get(m.folderId)?.role;
      const isTrash = role === 'trash' || role === 'junk';
      return isTrash ? includeTrash : scope === 'all';
    });
  }

  function threadSummary(
    tid: number,
    folderId: number,
    newestInFolder: MessageSummary,
    scope: ConversationScope,
    includeTrash: boolean,
  ): ThreadSummary {
    const first = threadMembers(tid)[0];
    const all = selectMembers(tid, folderId, scope, includeTrash);
    const participants: string[] = [];
    for (let i = all.length - 1; i >= 0 && participants.length < 4; i--) {
      const m = all[i]!;
      const n = m.fromName || m.fromAddr;
      if (!participants.includes(n)) participants.push(n);
    }
    return {
      id: tid,
      folderId,
      subject: first?.subject ?? newestInFolder.subject,
      participants,
      messageCount: all.length,
      unread: all.some((m) => m.unread),
      flagged: all.some((m) => m.flagged),
      hasAttachments: all.some((m) => m.hasAttachments),
      date: newestInFolder.date,
      snippet: newestInFolder.snippet,
      latestMessageId: newestInFolder.id,
    };
  }

  function recount(f: Folder) {
    const list = folderMsgs.get(f.id) ?? [];
    f.total = list.length;
    f.syncedCount = list.length;
    let u = 0;
    for (const m of list) if (m.unread) u++;
    f.unread = u;
  }

  function setStatus(accountId: number, phase: SyncPhase, folderId: number | null, done: number, total: number, message: string | null) {
    const s: SyncStatus = { accountId, phase, folderId, done, total, message };
    statuses.set(accountId, s);
    emit('sync-status', s);
  }

  async function simulateSync(acc: Account, serverTotal: number, host: string) {
    const inbox = [...folders.values()].find((f) => f.accountId === acc.id && f.role === 'inbox');
    setStatus(acc.id, 'connecting', null, 0, 0, `Connecting to ${host}…`);
    await sleep(900);
    const nf = [...folders.values()].filter((f) => f.accountId === acc.id).length;
    for (let i = 0; i <= nf; i += 3) {
      setStatus(acc.id, 'folders', null, Math.min(i, nf), nf, 'Listing folders');
      await sleep(120);
    }
    const step = Math.max(200, Math.round(serverTotal / 45));
    for (let d = 0; d < serverTotal; d += step) {
      setStatus(acc.id, 'headers', inbox?.id ?? null, d, serverTotal, 'Inbox');
      await sleep(140);
    }
    setStatus(acc.id, 'headers', inbox?.id ?? null, serverTotal, serverTotal, 'Inbox');
    const bodies = Math.round(serverTotal / 60);
    for (let d = 0; d < bodies; d += Math.max(10, Math.round(bodies / 20))) {
      setStatus(acc.id, 'bodies', inbox?.id ?? null, d, bodies, 'Downloading messages');
      await sleep(120);
    }
    setStatus(acc.id, 'idle', null, 0, 0, null);
  }

  function deliverNewMail(acc: Account) {
    const inbox = [...folders.values()].find((f) => f.accountId === acc.id && f.role === 'inbox');
    if (!inbox) return;
    const list = folderMsgs.get(inbox.id);
    if (!list) return;
    const rnd = mulberry32(Date.now() & 0xffffffff);
    const k = 1 + Math.floor(rnd() * 2);
    const now = Math.floor(Date.now() / 1000);
    for (let i = 0; i < k; i++) {
      const m = makeMessage(inbox, { name: acc.displayName, addr: acc.email }, rnd, now - i, 0, true);
      // Half of new mail continues a recent conversation.
      const target = rnd() < 0.5 ? list[Math.floor(rnd() * Math.min(15, list.length))] : undefined;
      if (target && meta.get(target.id)?.kind !== 'brief') {
        const tid = threadOf(target);
        const md = meta.get(m.id)!;
        md.kind = 'person';
        const p = pick(rnd, PEOPLE);
        m.fromName = p.name;
        m.fromAddr = p.addr;
        m.snippet = pick(rnd, PERSON_SNIPPETS);
        m.subject = `Re: ${baseSubject(target.subject)}`;
        if (!threads.has(tid)) addToThread(tid, byId.get(tid) ?? target);
        addToThread(tid, m);
      }
      list.unshift(m);
    }
    recount(inbox);
    setStatus(acc.id, 'headers', inbox.id, k, k, 'Inbox');
    setTimeout(() => setStatus(acc.id, 'idle', null, 0, 0, null), 600);
    emit('messages-changed', { folderIds: [inbox.id] });
    emit('folders-changed', { accountId: acc.id });
  }

  function buildDetail(m: MessageSummary, allowRemote: boolean): MessageDetail {
    const md = meta.get(m.id);
    const acc = accounts.find((a) => a.id === md?.accountId) ?? accounts[0];
    const me: Person = acc ? { name: acc.displayName, addr: acc.email } : { name: 'You', addr: 'you@example.com' };
    const seed = md?.seed ?? m.id;
    let html: string | null = null;
    let text: string | null = null;
    switch (md?.kind ?? 'person') {
      case 'person':
        text = personText({ name: m.fromName, addr: m.fromAddr }, me, m.snippet ?? PERSON_SNIPPETS[0]!, seed);
        break;
      case 'personHtml':
        html = personHtml({ name: m.fromName, addr: m.fromAddr }, me, m.snippet ?? PERSON_SNIPPETS[0]!);
        text = m.snippet;
        break;
      case 'brief':
        html = briefHtml(m.subject, seed);
        break;
      case 'promo':
        html = promoHtml(seed);
        break;
      case 'receipt':
        html = receiptHtml(m.subject, seed);
        break;
      case 'notify':
        html = notifyHtml(m.subject, seed);
        break;
    }
    let hasRemoteContent = false;
    if (html && !allowRemote) {
      const s = stripRemote(html);
      html = s.html;
      hasRemoteContent = s.had;
    }
    const rnd = mulberry32(seed);
    return {
      id: m.id,
      subject: m.subject,
      from: { name: m.fromName, addr: m.fromAddr },
      to: [{ name: me.name, addr: me.addr }],
      cc: md?.cc ? [pick(rnd, PEOPLE), pick(rnd, PEOPLE)].map((p) => ({ name: p.name, addr: p.addr })) : [],
      date: m.date,
      html,
      text,
      attachments: md?.attachments ?? [],
      hasRemoteContent,
      remoteAllowed: remoteAllowed.has(m.id),
    };
  }

  const startEmpty = typeof location !== 'undefined' && new URLSearchParams(location.search).has('empty');
  if (!startEmpty) {
    const g = createAccount('rick.hall@gmail.com', 'Rick Hall', 'gmail', GMAIL_FOLDERS);
    const w = createAccount('rick@hallworks.dev', 'Hallworks', 'imap', IMAP_FOLDERS);
    setTimeout(() => void simulateSync(g, 48_000, 'imap.gmail.com'), 300);
    const params = new URLSearchParams(location.search);
    if (params.has('error')) {
      // `?error`: the second account fails to connect (demonstrates the error footer row).
      setTimeout(() => setStatus(w.id, 'error', null, 0, 0, 'Connection to mail.hallworks.dev:993 timed out'), 900);
    } else if (params.has('auth')) {
      setTimeout(() => setStatus(w.id, 'auth', null, 0, 0, 'The server rejected the password: [AUTHENTICATIONFAILED] Invalid credentials'), 900);
    } else {
      setTimeout(() => void simulateSync(w, 6_000, 'mail.hallworks.dev'), 900);
    }
    setInterval(() => deliverNewMail(g), 25_000);
    if (params.has('many')) {
      // `?many`: four more accounts (six total) to exercise the status popover.
      const extra = ['ops@hallworks.dev', 'rick@side-project.io', 'family.hall@icloud.com', 'rhall@university.edu'];
      extra.forEach((email, i) => {
        const acc = createAccount(email, email.split('@')[0] ?? email, 'imap', SMALL_FOLDERS);
        setTimeout(() => void simulateSync(acc, 1_200 + i * 900, `imap.${email.split('@')[1]}`), 1_200 + i * 700);
      });
    }
    // Periodic incremental syncs so the status footer keeps cycling through phases.
    setInterval(() => {
      if (statuses.get(g.id)?.phase === 'idle') void simulateSync(g, 1_500, 'imap.gmail.com');
    }, 20_000);
  }

  // ---- quick search over sender / recipients / subject
  function runSearch(
    accountId: number,
    folderId: number | null,
    query: string,
    fields: SearchField[] | null,
  ): MessageSummary[] {
    const inField = (f: SearchField) => !fields || fields.length === 0 || fields.includes(f);
    const terms: Term[] = parseQuery(query);
    if (terms.length === 0) return [];
    const acc = accounts.find((a) => a.id === accountId);
    const toTokens = tokenize(`${acc?.displayName ?? ''} ${acc?.email ?? ''}`);
    const out: MessageSummary[] = [];
    for (const f of folders.values()) {
      if (f.accountId !== accountId || (folderId != null && f.id !== folderId)) continue;
      for (const m of folderMsgs.get(f.id) ?? []) {
        const fromT = tokenize(`${m.fromName} ${m.fromAddr}`);
        const subjT = tokenize(m.subject);
        const ok = terms.every((t) => {
          const hit =
            t.field === 'from'
              ? fieldMatches(fromT, t.words)
              : t.field === 'to'
                ? fieldMatches(toTokens, t.words)
                : t.field === 'subject'
                  ? fieldMatches(subjT, t.words)
                  : (inField('sender') && fieldMatches(fromT, t.words)) ||
                    (inField('subject') && fieldMatches(subjT, t.words)) ||
                    (inField('recipients') && fieldMatches(toTokens, t.words));
          return t.neg ? !hit : hit;
        });
        if (ok) out.push(m);
      }
    }
    return out.sort((a, b) => b.date - a.date);
  }

  const latency = () => sleep(8 + Math.random() * 25);

  const handlers: { [K in CommandName]: (args: CommandMap[K]['args']) => Promise<CommandMap[K]['ret']> } = {
    async list_accounts() {
      await latency();
      return accounts.map((a) => ({ ...a }));
    },
    async add_account(a: AddAccountArgs) {
      await sleep(1100);
      const email = a.email.trim();
      if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) throw 'Invalid email address';
      if (!a.password || a.password === 'wrong') {
        throw 'Authentication failed: [AUTHENTICATIONFAILED] Invalid credentials (Failure)';
      }
      if (accounts.some((x) => x.email === email)) throw `Account ${email} already exists`;
      const domain = email.split('@')[1]?.toLowerCase() ?? '';
      const provider: Provider =
        domain === 'gmail.com' || domain === 'googlemail.com'
          ? 'gmail'
          : /outlook|hotmail|live\.com/.test(domain)
            ? 'outlook'
            : 'imap';
      const acc = createAccount(email, a.displayName?.trim() || email.split('@')[0] || email, provider, SMALL_FOLDERS);
      void simulateSync(acc, 2_400, a.imapHost || `imap.${domain}`);
      emit('folders-changed', { accountId: acc.id });
      return { ...acc };
    },
    async list_folders({ accountId }) {
      await latency();
      return [...folders.values()].filter((f) => f.accountId === accountId).map((f) => ({ ...f, path: [...f.path] }));
    },
    async open_folder() {
      await latency();
    },
    async list_messages({ folderId, offset, limit }) {
      await latency();
      const list = folderMsgs.get(folderId) ?? [];
      return {
        total: list.length,
        messages: list.slice(offset, offset + limit).map((m) => ({ ...m })),
      };
    },
    async search_messages({ accountId, folderId, query, offset, limit, fields }) {
      await latency();
      const key = `${accountId}|${folderId}|${query}|${fields?.join(',') ?? ''}`;
      if (searchCache?.key !== key) searchCache = { key, results: runSearch(accountId, folderId, query, fields) };
      const results = searchCache.results;
      return { total: results.length, messages: results.slice(offset, offset + limit).map((m) => ({ ...m })) };
    },
    async list_threads({ folderId, offset, limit, scope, includeTrash }) {
      await latency();
      const list = folderMsgs.get(folderId) ?? [];
      const order: number[] = [];
      const newest = new Map<number, MessageSummary>();
      for (const m of list) {
        const tid = threadOf(m);
        if (!newest.has(tid)) {
          newest.set(tid, m);
          order.push(tid);
        }
      }
      return {
        total: order.length,
        threads: order.slice(offset, offset + limit).map((tid) => threadSummary(tid, folderId, newest.get(tid)!, scope, includeTrash)),
      };
    },
    async get_thread({ threadId, folderId, scope, includeTrash }) {
      await latency();
      const shown = selectMembers(threadId, folderId, scope, includeTrash);
      const ids = new Set(shown.map((m) => m.id));
      let hiddenOther = 0;
      let hiddenTrash = 0;
      for (const m of threadMembers(threadId)) {
        if (ids.has(m.id)) continue;
        const role = folders.get(m.folderId)?.role;
        if (role === 'trash' || role === 'junk') hiddenTrash++;
        else hiddenOther++;
      }
      return { messages: shown.map((m) => ({ ...m })), hiddenOther, hiddenTrash };
    },
    async get_message({ id, allowRemote }) {
      await sleep(6);
      const m = byId.get(id);
      if (!m) return null;
      if (!downloaded.has(id)) {
        if (!pendingDownloads.has(id)) {
          pendingDownloads.add(id);
          setTimeout(() => {
            pendingDownloads.delete(id);
            downloaded.add(id);
            m.hasBody = true;
            emit('message-ready', { id });
          }, 350 + Math.random() * 150);
        }
        return null;
      }
      return buildDetail(m, allowRemote);
    },
    async update_password({ accountId, password }) {
      await sleep(600);
      if (password === 'wrong') throw 'That password was rejected. Check it and try again.';
      const acc = accounts.find((a) => a.id === accountId);
      if (acc) {
        setStatus(acc.id, 'connecting', null, 0, 0, null);
        setTimeout(() => void simulateSync(acc, 6_000, 'mail.hallworks.dev'), 300);
      }
    },
    async set_allow_remote({ id, allow }) {
      await sleep(3);
      if (allow) remoteAllowed.add(id);
      else remoteAllowed.delete(id);
    },
    async set_read({ id, read }) {
      await sleep(4);
      const m = byId.get(id);
      if (!m || m.unread === !read) return;
      m.unread = !read;
      const f = folders.get(m.folderId);
      if (f) {
        recount(f);
        emit('messages-changed', { folderIds: [f.id] });
        emit('folders-changed', { accountId: f.accountId });
      }
    },
    async get_sync_status() {
      await latency();
      return [...statuses.values()].map((s) => ({ ...s }));
    },
  };

  return {
    invoke(cmd, args) {
      const h = handlers[cmd] as (a: typeof args) => Promise<CommandMap[typeof cmd]['ret']>;
      return h(args);
    },
    async listen(event, cb) {
      let set = listeners.get(event);
      if (!set) {
        set = new Set();
        listeners.set(event, set);
      }
      const fn = cb as (p: unknown) => void;
      set.add(fn);
      return () => {
        set.delete(fn);
      };
    },
  };
}
