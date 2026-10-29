import { createEffect, createMemo, createSignal, For, Match, on, onCleanup, Show, Switch } from 'solid-js';
import { api, errorMessage, type Addr, type Attachment, type MessageDetail } from '../api';
import { AttachmentControl } from './Attachments';
export { AttachmentControl };
import { analyzePlain, collapseHtmlQuotes, QUOTE_FRAME_CSS, type QNode } from '../lib/quotes';
import { cacheGet, cacheMarkRemoteAllowed, cachePut, onMessageReady, readyAt } from '../lib/body';
import { avatarColor, initials, linkify, longDate } from '../lib/format';
import { patchSelectedSummary, restorePending, searchActive, selectedFolderId, selectedMessageId, selectedSummary } from '../state';
import { HeaderActions, type ActionTarget } from './Actions';
import { searchTag, TagChips } from './Tags';
import { runAction } from '../lib/actions';
import { ImageIcon, MailIcon, PaperclipIcon } from './Icons';
import { DRAG } from '../lib/platform';
import { forwardFrameKeys } from '../lib/frameKeys';

// ------------------------------------------------------------- srcdoc

export function buildSrcdoc(rawHtml: string, allowRemote: boolean, extraCss = ''): string {
  // Collapse a trailing reply history behind a pill — only when it is unambiguous.
  const html = collapseHtmlQuotes(rawHtml);
  const csp =
    // script-src 'none' is the guard against code in an email (with the sanitiser): the frame
    // itself has allow-scripts, which WebKit needs for the app's key listener (lib/frameKeys.ts).
    `default-src 'none'; script-src 'none'; img-src data: cid:${allowRemote ? ' https: http:' : ''}; ` +
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
${extraCss}
</style></head><body>${html}</body></html>`;
}

/**
 * Sandboxed body frame. Built imperatively so `sandbox` + `srcdoc` are set before the
 * element is attached (setting srcdoc on an already-attached sandboxed frame can leave it
 * unpainted in Blink), and each document gets a fresh browsing context.
 * allow-same-origin + allow-scripts let the app listen for keys inside it (shortcuts keep
 * working while the message has focus; WebKit runs no listener at all in a frame without
 * allow-scripts). The email itself still can't run code: the sanitiser removes scripts, event
 * handlers and javascript: links, and the document's CSP has script-src 'none'.
 */
function HtmlFrame(props: { doc: string }) {
  let host!: HTMLDivElement;
  createEffect(() => {
    const f = document.createElement('iframe');
    f.className = 'html-frame';
    f.title = 'Message body';
    f.setAttribute('sandbox', 'allow-same-origin allow-scripts allow-popups allow-popups-to-escape-sandbox');
    f.setAttribute('referrerpolicy', 'no-referrer');
    f.srcdoc = props.doc;
    f.addEventListener('load', () => forwardFrameKeys(f));
    host.replaceChildren(f);
  });
  return <div class="html-wrap" ref={host} />;
}

// ------------------------------------------------------------- small pieces

export function Avatar(props: { name: string | null; addr: string; size?: number }) {
  const size = () => props.size ?? 40;
  return (
    <div
      class="avatar"
      style={{
        background: avatarColor(props.addr),
        width: `${size()}px`,
        height: `${size()}px`,
        'font-size': `${Math.round(size() * 0.38)}px`,
      }}
      aria-hidden="true"
    >
      {initials(props.name, props.addr)}
    </div>
  );
}

function addrLabel(a: Addr) {
  return a.name ? a.name : a.addr;
}

export function AddrList(props: { label: string; list: Addr[] }) {
  return (
    <Show when={props.list.length > 0}>
      <div class="recips-line">
        <span class="recips-label">{props.label}</span>
        <For each={props.list}>
          {(a, i) => (
            <>
              <span class="recip" title={a.addr}>
                {addrLabel(a)}
              </span>
              {i() < props.list.length - 1 ? ', ' : ''}
            </>
          )}
        </For>
      </div>
    </Show>
  );
}

function LinkifiedText(props: { text: string }) {
  const parts = createMemo(() => linkify(props.text));
  return (
    <For each={parts()}>
      {(p) =>
        p.t === 'link' ? (
          <a href={p.href} target="_blank" rel="noopener noreferrer">
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

export function PlainBody(props: { text: string }) {
  const res = createMemo(() => analyzePlain(props.text));
  return (
    <div class="plain" tabIndex={-1}>
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

export function ReaderEmpty() {
  return (
    <div class="reader-empty" classList={{ quiet: restorePending() }} data-tauri-drag-region={DRAG}>
      <MailIcon size={44} class="icon empty-icon" />
      <div class="reader-empty-title">No message selected</div>
      <div class="reader-empty-sub">
        Use <kbd>J</kbd> / <kbd>K</kbd> to move, <kbd>O</kbd> to open, <kbd>T</kbd> conversations, <kbd>A</kbd> all mail
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
  return { ids: [s.id], folderId, anyUnread: s.unread, starId: s.id, starred: s.flagged, advance: !searchActive(), tagSets: [s.tags ?? []] };
};

export function Reader(props: { ref?: (el: HTMLElement) => void }) {
  const [allowRemote, setAllowRemote] = createSignal(false);
  /** The body currently on screen, with the allowRemote it was fetched with. */
  const [shown, setShown] = createSignal<{ d: MessageDetail; remote: boolean } | null>(null);
  const detail = () => shown()?.d ?? null;
  const [phase, setPhase] = createSignal<Phase>('idle');
  const [error, setError] = createSignal<string | null>(null);
  let token = 0;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;

  // New message → images blocked again.
  createEffect(on(selectedMessageId, () => setAllowRemote(false)));

  /** "Load images": show them now and remember the choice for this message. */
  function allowRemoteFor(id: number | null) {
    if (id == null) return;
    cacheMarkRemoteAllowed(id);
    setAllowRemote(true);
    api.setAllowRemote(id, true).catch(() => {});
  }

  async function load(id: number, remote: boolean) {
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
    if (detail()?.id !== id) setShown(null);
    if (phase() !== 'downloading' || detail()?.id !== id) setPhase('loading');
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
        // Opening a message marks it read; the core no-ops if it already is.
        api
          .setRead(id, true)
          .then(() => patchSelectedSummary(id, { unread: false }))
          .catch(() => {});
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
    const id = selectedMessageId();
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
    if (id === selectedMessageId() && phase() === 'downloading') void load(id, allowRemote());
  });
  onCleanup(() => clearTimeout(retryTimer));

  // Header data: prefer full detail, fall back to the list summary for an instant paint.
  const head = createMemo(() => {
    const d = detail();
    const s = selectedSummary();
    if (d && d.id === selectedMessageId()) {
      return { subject: d.subject, from: d.from, to: d.to, cc: d.cc, date: d.date, attachments: d.attachments };
    }
    if (s && s.id === selectedMessageId()) {
      return {
        subject: s.subject,
        from: { name: s.fromName || null, addr: s.fromAddr },
        to: [] as Addr[],
        cc: [] as Addr[],
        date: s.date,
        attachments: [] as Attachment[],
      };
    }
    return null;
  });

  /** Body to render: the loaded one, or the previous body of the same message while re-fetching with images. */
  const ready = () => {
    const s = shown();
    if (!s || s.d.id !== selectedMessageId()) return null;
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
                <Show when={h().attachments.length > 0}>
                  <span class="dot-sep">·</span>
                  <span class="band-attach">
                    <PaperclipIcon size={12} />
                    {h().attachments.length} {h().attachments.length === 1 ? 'attachment' : 'attachments'}
                  </span>
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
            <div class="reader-head">
              <div class="reader-from">
                <Avatar name={h().from.name} addr={h().from.addr} size={40} />
                <div class="reader-from-text">
                  <div class="reader-from-line">
                    <span class="reader-from-name">{h().from.name || h().from.addr}</span>
                    <Show when={h().from.name}>
                      <span class="reader-from-addr">&lt;{h().from.addr}&gt;</span>
                    </Show>
                  </div>
                  <div class="recips">
                    <AddrList label="To" list={h().to} />
                    <AddrList label="Cc" list={h().cc} />
                  </div>
                </div>
                {/* Slot reserved whenever the message has attachments (known from the list row). */}
                <Show when={h().attachments.length > 0 || selectedSummary()?.hasAttachments}>
                  <div class="hdr-att">
                    <Show when={h().attachments.length > 0} fallback={<span class="att-pill-ph" />}>
                      <AttachmentControl messageId={selectedMessageId()!} list={h().attachments} />
                    </Show>
                  </div>
                </Show>
              </div>
            </div>
            <Show when={ready()?.d.hasRemoteContent && !ready()?.remote && !allowRemote()}>
              <div class="remote-banner">
                <ImageIcon size={14} />
                <span>Remote images blocked</span>
                <span class="dot-sep">·</span>
                <button class="link-btn" onClick={() => allowRemoteFor(selectedMessageId())}>
                  Load images
                </button>
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
                        const id = selectedMessageId();
                        if (id != null) void load(id, allowRemote());
                      }}
                    >
                      Try again
                    </button>
                  </div>
                </Match>
                <Match when={ready()}>
                  {(d) => (
                    <Show when={d().d.html} fallback={<PlainBody text={d().d.text ?? ''} />}>
                      {(html) => <HtmlFrame doc={buildSrcdoc(html(), d().remote)} />}
                    </Show>
                  )}
                </Match>
                <Match when={phase() === 'loading' || phase() === 'downloading'}>
                  <BodySkeleton downloading={phase() === 'downloading'} />
                </Match>
              </Switch>
            </div>
          </>
        )}
      </Show>
    </section>
  );
}
