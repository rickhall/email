import { Select } from './Select';
import { WhoStrip } from './WhoStrip';
import { AutoHeightFrame } from './AutoHeightFrame';
import { addressMenu } from './AddressMenu';
import { createEffect, createMemo, createSignal, For, Match, on, onCleanup, Show, Switch } from 'solid-js';
import { api, errorMessage, type Addr, type Attachment, type MessageDetail } from '../api';
import { AttachmentChips } from './Attachments';
export { AttachmentChips };
import { analyzePlain, collapseHtmlQuotes, QUOTE_FRAME_CSS, type QNode } from '../lib/quotes';
import { cacheClear, cacheGet, cacheMarkRemoteAllowed, cachePut, onMessageReady, readyAt } from '../lib/body';
import { linkify, longDate } from '../lib/format';
import { domainRules } from '../lib/privacy';
import { patchSelectedSummary, readerMessageId, restorePending, searchActive, selectedFolderId, selectedMessageId, selectedSummary } from '../state';
import { HeaderActions, type ActionTarget } from './Actions';
import { pendingSnooze } from './Snooze';
import { searchTag, TagChips } from './Tags';
import { runAction } from '../lib/actions';
import { ForwardIcon, ImageIcon, MailIcon, PaperclipIcon, ReplyIcon } from './Icons';
import { DRAG } from '../lib/platform';
import { peekHandlers } from './LinkPeek';
import { bodyMenu } from './BodyMenu';
import { keepImageShapes, ZOOM_CSS } from '../lib/images';
import { isDraftMessage, openDraftSummary } from '../lib/compose';
import { allKeys, label, MOD } from '../lib/shortcuts';

// ------------------------------------------------------------- srcdoc

export function buildSrcdoc(rawHtml: string, allowRemote: boolean, extraCss = ''): string {
  // Collapse a trailing reply history behind a pill — only when it is unambiguous.
  const html = keepImageShapes(collapseHtmlQuotes(rawHtml));
  const csp =
    // script-src 'none' is the guard against code in an email (with the sanitiser): the frame
    // itself has allow-scripts, which WebKit needs for the app's key listener (lib/frameKeys.ts).
    `default-src 'none'; script-src 'none'; img-src data: cid:${allowRemote ? ' https: http: mailimg:' : ''}; ` +
    `style-src 'unsafe-inline'${allowRemote ? ' https:' : ''}; font-src data:${allowRemote ? ' https:' : ''}`;
  return `<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="color-scheme" content="light">
<base target="_blank">
<style>
html{background:#fff;color:#1d1d1f;-webkit-text-size-adjust:100%}
body{margin:0;padding:20px 24px 32px;font:14px/1.5 -apple-system,"SF Pro Text","Inter",system-ui,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;overflow-wrap:break-word}
img{max-width:100%}
pre{white-space:pre-wrap}
blockquote{margin:.6em 0;padding:0 0 0 12px;border-left:2px solid #c3c8d1;color:#4b5058}
blockquote blockquote{border-left-color:#d9bd84}
blockquote blockquote blockquote{border-left-color:#9fc5a8}
blockquote blockquote blockquote blockquote{border-left-color:#c3c8d1}
.moz-cite-prefix,.gmail_attr{color:#6b7078}
a{color:#0a64d6}
${QUOTE_FRAME_CSS}
${ZOOM_CSS}
${extraCss}
</style></head><body>${html}</body></html>`;
}


// ------------------------------------------------------------- small pieces

/** Over blocked images: always load them, from this sender or from everyone at its domain
 *  (when the user turned such rules on, the core offers a domain: not gmail.com and the like,
 *  nor where the server checks senders and this one failed). A menu, so the banner stays one
 *  short line. */
export function AlwaysLoad(props: { addr: string; domain: string | null; onPick: (who: 'sender' | 'domain') => void }) {
  return (
    <Select
      class="remote-always"
      variant="link"
      action
      label="Always load images"
      placeholder="Always load…"
      heading="Always load images"
      value={null}
      options={[
        { value: 'sender' as const, label: `From ${props.addr}` },
        ...(props.domain && domainRules() ? [{ value: 'domain' as const, label: `From everyone at ${props.domain}` }] : []),
      ]}
      onChange={(who) => props.onPick(who)}
    />
  );
}

export { Avatar } from './Avatar';
import { Avatar } from './Avatar';

function addrLabel(a: Addr) {
  return a.name ? a.name : a.addr;
}

/** To and Cc. With neither (mail sent to everyone as Bcc, "undisclosed-recipients:;"), once the
 *  message is loaded: its Bcc when it's one of ours, else a line saying they weren't disclosed,
 *  so the sender never stands alone. */
export function Recipients(props: { to: Addr[]; cc: Addr[]; bcc?: Addr[]; known: boolean }) {
  const none = () => props.known && props.to.length === 0 && props.cc.length === 0;
  return (
    <>
      <AddrList label="To" list={props.to} />
      <AddrList label="Cc" list={props.cc} />
      <Show when={none()}>
        <Show
          when={(props.bcc ?? []).length > 0}
          fallback={
            <div class="recips-line">
              <span class="recips-label">To</span>
              <span class="recips-none">Undisclosed recipients</span>
            </div>
          }
        >
          <AddrList label="Bcc" list={props.bcc!} />
        </Show>
      </Show>
    </>
  );
}

/** More than this many: the first few, then "and N more" (a click shows them all). */
const ADDR_FOLD = 5;
const ADDR_SHOWN = 3;

export function AddrList(props: { label: string; list: Addr[] }) {
  const [open, setOpen] = createSignal(false);
  const folds = () => props.list.length > ADDR_FOLD;
  const shown = () => (folds() && !open() ? props.list.slice(0, ADDR_SHOWN) : props.list);
  // In a conversation card the header expands/collapses the message: not these clicks.
  const toggle = (e: MouseEvent) => {
    e.stopPropagation();
    setOpen(!open());
  };
  return (
    <Show when={props.list.length > 0}>
      <div class="recips-line">
        <span class="recips-label">{props.label}</span>
        <For each={shown()}>
          {(a, i) => (
            <>
              <span class="recip" title={a.addr} onContextMenu={(e) => addressMenu(e, a)}>
                {addrLabel(a)}
              </span>
              {i() < shown().length - 1 ? ', ' : ''}
            </>
          )}
        </For>
        <Show when={folds()}>
          {' '}
          <button
            class="link-btn recips-more"
            onClick={toggle}
            title={open() ? undefined : props.list.slice(ADDR_SHOWN).map((a) => a.addr).join(', ')}
          >
            {open() ? 'show less' : `and ${props.list.length - ADDR_SHOWN} more`}
          </button>
        </Show>
      </div>
    </Show>
  );
}

function LinkifiedText(props: { text: string }) {
  const parts = createMemo(() => linkify(props.text));
  // The address shows while the pointer is over a link, at the bottom of the message's pane.
  const peek = peekHandlers('.reader, .conv-scroll, .plain');
  return (
    <For each={parts()}>
      {(p) =>
        p.t === 'link' ? (
          <a href={p.href} target="_blank" rel="noopener noreferrer" {...peek}>
            {p.v}
          </a>
        ) : (
          p.v
        )
      }
    </For>
  );
}

/** Plain-text quote tree: text runs and nested quote blocks (">" markers stripped). */
function QuoteNodes(props: { nodes: QNode[] }) {
  return (
    <For each={props.nodes}>
      {(n) =>
        n.t === 'text' ? (
          <span class="pt">
            <LinkifiedText text={n.text} />
          </span>
        ) : (
          <div class="pq">
            <QuoteNodes nodes={n.children} />
          </div>
        )
      }
    </For>
  );
}

export function PlainBody(props: { text: string; sender?: string; messageId?: number }) {
  const res = createMemo(() => analyzePlain(props.text));
  return (
    <div
      class="plain"
      tabIndex={-1}
      data-sender={props.sender}
      onContextMenu={(e) => {
        if (props.messageId != null) bodyMenu(e, props.messageId);
      }}
    >
      <QuoteNodes nodes={res().head} />
      <Show when={res().collapsed}>
        {(c) => (
          <details class="mail-quote">
            <summary>
              <span class="mq-show">⋯ Show quoted text</span>
              <span class="mq-hide">Hide quoted text</span>
            </summary>
            <QuoteNodes nodes={c()} />
          </details>
        )}
      </Show>
    </div>
  );
}

export function BodySkeleton(props: { downloading: boolean }) {
  return (
    <div class="body-loading" aria-live="polite">
      <div class="body-skel">
        <div class="sk-line w90" />
        <div class="sk-line w75" />
        <div class="sk-line w85" />
        <div class="sk-line w40" />
      </div>
      <Show when={props.downloading}>
        <div class="downloading">
          <span class="spinner small" />
          Downloading message…
        </div>
      </Show>
    </div>
  );
}

// ------------------------------------------------------------- reader

type Phase = 'idle' | 'loading' | 'downloading' | 'ready' | 'error';

/** A command's first key as one keycap's text ("J", "⇧U", "Ctrl+R"). */
const firstKey = (id: string) => label(allKeys(id)[0] ?? '').join(MOD === '⌘' ? '' : '+');

export function ReaderEmpty() {
  return (
    <div class="reader-empty" classList={{ quiet: restorePending() }} data-tauri-drag-region={DRAG}>
      <MailIcon size={44} class="icon empty-icon" />
      <div class="reader-empty-title">No message selected</div>
      <div class="reader-empty-sub">
        {/* from the keymap: the chosen preset's keys, or the user's own */}
        Use <kbd>{firstKey('next')}</kbd> / <kbd>{firstKey('prev')}</kbd> to move, <kbd>?</kbd> for all shortcuts
      </div>
    </div>
  );
}

/** What the header actions act on in single-message view: the open message. */
const actionTarget = (): ActionTarget | null => {
  const s = selectedSummary();
  const id = selectedMessageId();
  if (!s || s.id !== id) return null;
  // Search results span folders: act in the message's own folder there.
  const folderId = searchActive() ? s.folderId : selectedFolderId() ?? s.folderId;
  return { ids: [s.id], folderId, anyUnread: s.unread, starId: s.id, replyId: s.id, starred: s.flagged, advance: true, tagSets: [s.tags ?? []], snoozedUntil: pendingSnooze([s]) };
};

/** The reader's page scrolls, not the message's own frame: it grows to its content. */
// The message's sides on the reader's 28px edge, as the subject, sender, chips and plain text.
const PAGE_FRAME_CSS = 'html{overflow:hidden}body{padding-left:28px;padding-right:28px}';

export function Reader(props: { ref?: (el: HTMLElement) => void }) {
  let scrollEl: HTMLDivElement | undefined;
  // Another message: back to its top (and its own width, after a wide one).
  createEffect(
    on(readerMessageId, () => {
      if (!scrollEl) return;
      scrollEl.scrollTop = 0;
      scrollEl.scrollLeft = 0;
      scrollEl.querySelector<HTMLElement>('.reader-page')?.style.removeProperty('width');
    }),
  );
  /** The message shown is a draft (written here, or in a Drafts folder): "Edit draft". */
  const shownDraft = () => {
    const s = selectedSummary();
    return s && s.id === readerMessageId() && isDraftMessage(s) ? s : null;
  };
  const [allowRemote, setAllowRemote] = createSignal(false);
  /** The body currently on screen, with the allowRemote it was fetched with. */
  const [shown, setShown] = createSignal<{ d: MessageDetail; remote: boolean } | null>(null);
  const detail = () => shown()?.d ?? null;
  const [phase, setPhase] = createSignal<Phase>('idle');
  const [error, setError] = createSignal<string | null>(null);
  let token = 0;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;

  // New message → images blocked again.
  createEffect(on(readerMessageId, () => setAllowRemote(false)));

  /** "Load images": show them now and remember the choice for this message. */
  function allowRemoteFor(id: number | null) {
    if (id == null) return;
    cacheMarkRemoteAllowed(id);
    setAllowRemote(true);
    api.setAllowRemote(id, true).catch(() => {});
  }

  /** `quiet`: the message shown changed (a draft saved again): read it again, keeping what's
   *  shown until the new one is in. */
  async function load(id: number, remote: boolean, quiet = false) {
    const my = ++token;
    clearTimeout(retryTimer);
    setError(null);
    const hit = cacheGet(id, remote);
    if (hit) {
      if (hit.remoteAllowed && !remote && hit.hasRemoteContent) {
        setAllowRemote(true); // remembered choice: refetch unstripped
        return;
      }
      setShown({ d: hit, remote: remote || hit.remoteAllowed });
      setPhase('ready');
      return;
    }
    // Keep showing the previous body while toggling remote images for the same message.
    if (!quiet) {
      if (detail()?.id !== id) setShown(null);
      if (phase() !== 'downloading' || detail()?.id !== id) setPhase('loading');
    }
    const started = performance.now();
    try {
      const d = await api.getMessage(id, remote);
      if (my !== token) return;
      if (d) {
        cachePut(d, remote);
        if (d.remoteAllowed && !remote && d.hasRemoteContent) {
          // Remembered "Load images" but the body came back stripped: ask again with remote allowed.
          setAllowRemote(true);
          return;
        }
        // A remembered "Load images" choice applies without asking again.
        setShown({ d, remote: remote || d.remoteAllowed });
        setPhase('ready');
        // Opening a message marks it read; the core no-ops if it already is. Shown read first, so
        // a "Mark unread" made meanwhile isn't overwritten when the answer comes back.
        if (selectedSummary()?.id === id && selectedSummary()?.unread) patchSelectedSummary(id, { unread: false });
        api.setRead(id, true).catch(() => {});
        return;
      }
      setPhase('downloading');
      const r = readyAt.get(id);
      if (r !== undefined && r >= started) {
        void load(id, remote);
        return;
      }
      // Belt and braces: ask again if the ready event never shows up.
      retryTimer = setTimeout(() => {
        if (my === token) void load(id, remote);
      }, 10_000);
    } catch (e) {
      if (my !== token) return;
      setError(errorMessage(e));
      setPhase('error');
    }
  }

  createEffect(() => {
    const id = readerMessageId();
    const remote = allowRemote();
    if (id == null) {
      token++;
      clearTimeout(retryTimer);
      setShown(null);
      setPhase('idle');
      return;
    }
    void load(id, remote);
  });

  onMessageReady((id) => {
    if (id !== readerMessageId()) return;
    // Downloaded at last; or changed while shown (a draft saved again: its cached copy is gone).
    if (phase() === 'downloading') void load(id, allowRemote());
    else if (phase() === 'ready') void load(id, allowRemote(), true);
  });
  onCleanup(() => clearTimeout(retryTimer));

  /** The sender's block, for the strip that stands in for it once scrolled away. */
  let fromEl: HTMLDivElement | undefined;

  // Header data: prefer full detail, fall back to the list summary for an instant paint.
  const head = createMemo(() => {
    const d = detail();
    const s = selectedSummary();
    if (d && d.id === readerMessageId()) {
      return { subject: d.subject, from: d.from, to: d.to, cc: d.cc, bcc: d.bcc ?? [], full: true, date: d.date, attachments: d.attachments };
    }
    if (s && s.id === readerMessageId()) {
      return {
        subject: s.subject,
        from: { name: s.fromName || null, addr: s.fromAddr },
        to: [] as Addr[],
        cc: [] as Addr[],
        bcc: [] as Addr[],
        full: false,
        date: s.date,
        attachments: [] as Attachment[],
      };
    }
    return null;
  });

  /** Body to render: the loaded one, or the previous body of the same message while re-fetching with images. */
  const ready = () => {
    const s = shown();
    if (!s || s.d.id !== readerMessageId()) return null;
    return phase() === 'ready' || phase() === 'loading' ? s : null;
  };

  return (
    <section class="reader" aria-label="Message" ref={(el) => props.ref?.(el)} tabIndex={-1}>
      <Show
        when={head()}
        fallback={<ReaderEmpty />}
      >
        {(h) => (
          <>
            {/* Fixed-height band: lines up with the message list's header border. */}
            <header class="reader-band" data-tauri-drag-region={DRAG}>
              <h1 class="reader-subject" title={h().subject || '(no subject)'}>
                {h().subject || '(no subject)'}
              </h1>
              <div class="band-meta" data-tauri-drag-region={DRAG}>
                <time dateTime={new Date(h().date * 1000).toISOString()}>{longDate(h().date)}</time>
                {/* Icon only (title says which): the band's date mustn't be squeezed. */}
                <Show when={selectedSummary()?.id === readerMessageId() && selectedSummary()?.answered}>
                  <span class="dot-sep">·</span>
                  <span class="band-mark" title="Replied" aria-label="Replied">
                    <ReplyIcon size={13} />
                  </span>
                </Show>
                <Show when={selectedSummary()?.id === readerMessageId() && selectedSummary()?.forwarded}>
                  <span class="dot-sep">·</span>
                  <span class="band-mark" title="Forwarded" aria-label="Forwarded">
                    <ForwardIcon size={13} />
                  </span>
                </Show>
                <Show when={shownDraft()}>
                  {(s) => (
                    <>
                      <span class="dot-sep">·</span>
                      <button
                        class="link-btn band-edit-draft"
                        title="Open it in the composer (or double-click it in the list)"
                        onClick={() => openDraftSummary(s())}
                      >
                        Edit draft
                      </button>
                    </>
                  )}
                </Show>
                <Show when={h().attachments.length > 0}>
                  <span class="dot-sep">·</span>
                  {/* Brings the chips into sight, when they're scrolled away. */}
                  <button
                    class="band-attach"
                    title={h().attachments.length === 1 ? 'Show the attachment' : 'Show the attachments'}
                    onClick={(e) => {
                      e.currentTarget.blur();
                      const chips = scrollEl?.querySelector('.reader-head .att-chips');
                      if (!chips || !scrollEl) return;
                      const r = chips.getBoundingClientRect();
                      const v = scrollEl.getBoundingClientRect();
                      // in sight already (and not under the strip): stay put
                      if (r.top >= v.top + 34 && r.bottom <= v.bottom) return;
                      chips.scrollIntoView({ block: 'center', behavior: 'smooth' });
                    }}
                  >
                    <PaperclipIcon size={12} />
                    {h().attachments.length} {h().attachments.length === 1 ? 'attachment' : 'attachments'}
                  </button>
                </Show>
                <Show when={actionTarget()}>
                  {(x) => (
                    <TagChips
                      tags={x().tagSets[0] ?? []}
                      onRemove={(k) => void runAction({ kind: 'tag', keyword: k, on: false }, { ids: x().ids, folderId: x().folderId, advance: false })}
                      onOpen={searchTag}
                    />
                  )}
                </Show>
                <HeaderActions target={actionTarget()} />
              </div>
            </header>
            {/* Under the band, one page that scrolls: sender, recipients, attachments and the
                message together, each as tall as it is (an expanded list just makes it longer). */}
            {/* Just under the band and outside the scroller, so it spans the pane, scrollbar too. */}
            <div class="who-dock">
              <WhoStrip from={h().from} to={h().to} cc={h().cc} bcc={h().bcc} known={h().full} head={() => fromEl} scroller={() => scrollEl} />
            </div>
            <div class="reader-scroll" tabIndex={-1} ref={scrollEl}>
            <div class="reader-page">
            <div class="reader-head">
              <div class="reader-from" ref={fromEl}>
                <Avatar name={h().from.name} addr={h().from.addr} size={40} />
                <div class="reader-from-text">
                  <div class="reader-from-line">
                    <span class="reader-from-name" onContextMenu={(e) => addressMenu(e, h().from)}>
                      {h().from.name || h().from.addr}
                    </span>
                    <Show when={h().from.name}>
                      <span class="reader-from-addr" onContextMenu={(e) => addressMenu(e, h().from)}>
                        &lt;{h().from.addr}&gt;
                      </span>
                    </Show>
                  </div>
                  <div class="recips">
                    <Recipients to={h().to} cc={h().cc} bcc={h().bcc} known={h().full} />
                  </div>
                </div>
              </div>
              {/* Room kept whenever the message has attachments (known from the list row), so
                  nothing moves when the names come in. */}
              <Show when={h().attachments.length > 0 || selectedSummary()?.hasAttachments}>
                <Show when={h().attachments.length > 0} fallback={<div class="att-chips att-chips-ph" />}>
                  <AttachmentChips messageId={readerMessageId()!} list={h().attachments} />
                </Show>
              </Show>
            </div>
            <Show when={ready()?.d.hasRemoteContent && !ready()?.remote && !allowRemote()}>
              <div class="remote-banner">
                <ImageIcon size={14} />
                <span>Remote images blocked</span>
                <span class="dot-sep">·</span>
                <button class="link-btn" onClick={() => allowRemoteFor(readerMessageId())}>
                  Load images
                </button>
                <span class="dot-sep">·</span>
                <AlwaysLoad
                  addr={h().from.addr}
                  domain={ready()?.d.senderDomain ?? null}
                  onPick={(who) => {
                    const id = readerMessageId();
                    if (id == null) return;
                    void (who === 'domain' ? api.allowRemoteDomain(id) : api.allowRemoteSender(id)).then(() => {
                      cacheClear();
                      allowRemoteFor(id);
                    }, () => {});
                  }}
                />
              </div>
            </Show>
            <div class="reader-body">
              <Switch>
                <Match when={phase() === 'error'}>
                  <div class="body-error">
                    <div class="body-error-title">Couldn’t open this message</div>
                    <div class="body-error-sub">{error()}</div>
                    <button
                      class="btn secondary"
                      onClick={() => {
                        const id = readerMessageId();
                        if (id != null) void load(id, allowRemote());
                      }}
                    >
                      Try again
                    </button>
                  </div>
                </Match>
                <Match when={ready()}>
                  {(d) => (
                    <Show when={d().d.html} fallback={<PlainBody text={d().d.text ?? ''} sender={h().from.addr} messageId={d().d.id} />}>
                      {(html) => (
                        <AutoHeightFrame doc={buildSrcdoc(html(), d().remote, PAGE_FRAME_CSS)} sender={h().from.addr} messageId={d().d.id} remote={d().remote} />
                      )}
                    </Show>
                  )}
                </Match>
                <Match when={phase() === 'loading' || phase() === 'downloading'}>
                  <BodySkeleton downloading={phase() === 'downloading'} />
                </Match>
              </Switch>
            </div>
            </div>
            </div>
          </>
        )}
      </Show>
    </section>
  );
}
