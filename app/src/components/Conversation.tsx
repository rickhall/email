import { batch, createEffect, createMemo, createSignal, For, Match, on, onCleanup, Show, Switch } from 'solid-js';
import { createStore, reconcile } from 'solid-js/store';
import { api, errorMessage, events, type MessageSummary, type ThreadSummary } from '../api';
import { createMessageBody } from '../lib/body';
import { longDate, smartDate } from '../lib/format';
import {
  accounts,
  conversationScope,
  folderById,
  includeTrash,
  selectedThread,
  selectedThreadId,
  selectThread,
} from '../state';
import { ForwardIcon, ImageIcon, PaperclipIcon, ReplyIcon } from './Icons';
import { AddrList, AttachmentControl, Avatar, BodySkeleton, buildSrcdoc, PlainBody, ReaderEmpty } from './Reader';
import { DRAG } from '../lib/platform';
import { CardActions, HeaderActions, type ActionTarget } from './Actions';
import { searchTag, TagChips, TagDots } from './Tags';
import { runAction } from '../lib/actions';
import { forwardFrameKeys } from '../lib/frameKeys';
import { SendState } from './MessageList';
import { editDraft } from '../lib/compose';

// ------------------------------------------------------------- auto-height frame

/** Extra CSS for card bodies: compact padding, never scroll vertically inside the frame. */
const CARD_FRAME_CSS = 'html{overflow-y:hidden}body{padding:4px 20px 20px}';

/**
 * Sandboxed HTML body that grows to its content height.
 * `allow-same-origin` lets the parent read the inner document to measure it; with
 * `allow-scripts` the app's own listeners run in it under WebKit (keys, quote toggles). The
 * email can't run code: it's sanitised and the document's CSP has script-src 'none' (see
 * `buildSrcdoc`). Built imperatively so sandbox + srcdoc are set before attachment.
 */
export function AutoHeightFrame(props: { doc: string }) {
  let host!: HTMLDivElement;
  createEffect(() => {
    const f = document.createElement('iframe');
    f.className = 'card-frame';
    f.title = 'Message body';
    f.setAttribute('sandbox', 'allow-same-origin allow-scripts allow-popups allow-popups-to-escape-sandbox');
    f.setAttribute('referrerpolicy', 'no-referrer');
    f.setAttribute('scrolling', 'no');
    f.style.height = '160px';
    f.srcdoc = props.doc;

    let ro: ResizeObserver | undefined;
    let last = -1;
    const measure = () => {
      const doc = f.contentDocument;
      const de = doc?.documentElement;
      const body = doc?.body;
      if (!doc || !de || !body) return;
      const cs = doc.defaultView?.getComputedStyle(body);
      const margins = cs ? parseFloat(cs.marginTop) + parseFloat(cs.marginBottom) : 0;
      // The html box's own height shrinks with content (unlike scrollHeight, which is floored
      // at the viewport); body.scrollHeight catches overflowing children.
      const h = Math.ceil(Math.max(de.getBoundingClientRect().height, body.scrollHeight + margins, 24));
      if (Math.abs(h - last) > 1) {
        last = h;
        f.style.height = `${h}px`;
      }
    };
    f.addEventListener('load', () => {
      const doc = f.contentDocument;
      if (!doc) return;
      forwardFrameKeys(f);
      // First pass: the frame is still small, so scrollHeight is exact here.
      last = -1;
      f.style.height = `${Math.max(24, doc.documentElement.scrollHeight)}px`;
      measure();
      ro?.disconnect();
      ro = new ResizeObserver(() => measure());
      ro.observe(doc.documentElement);
      if (doc.body) ro.observe(doc.body);
      ro.observe(f); // width changes (pane resize) reflow the content
      // Expanding/collapsing quoted history (native <details>): 'toggle' doesn't bubble, so capture.
      doc.addEventListener('toggle', measure, true);
      doc.querySelectorAll('img').forEach((img) => {
        if (!img.complete) {
          img.addEventListener('load', measure);
          img.addEventListener('error', measure);
        }
      });
      void doc.fonts?.ready.then(measure);
    });
    host.replaceChildren(f);
    onCleanup(() => ro?.disconnect());
  });
  return <div class="card-frame-wrap" ref={host} />;
}

// ------------------------------------------------------------- cards

type Me = { name: string; email: string } | undefined;

function senderLabel(m: MessageSummary, me: Me) {
  if (me && m.fromAddr.toLowerCase() === me.email.toLowerCase()) return 'me';
  return m.fromName || m.fromAddr;
}

/** Replied / forwarded arrows for a message card. */
function Marks(props: { m: MessageSummary }) {
  return (
    <>
      <Show when={props.m.answered}>
        <span class="cm-mark" title="Replied">
          <ReplyIcon size={12} />
        </span>
      </Show>
      <Show when={props.m.forwarded}>
        <span class="cm-mark" title="Forwarded">
          <ForwardIcon size={12} />
        </span>
      </Show>
    </>
  );
}

function CollapsedCard(props: { m: MessageSummary; me: Me; folderTag?: string; onToggle: () => void }) {
  return (
    <button class="card-msg collapsed" classList={{ unread: props.m.unread }} onClick={() => props.onToggle()}>
      <span class="unread-dot" />
      <Avatar name={props.m.fromName} addr={props.m.fromAddr} size={28} />
      <span class="cm-from">{senderLabel(props.m, props.me)}</span>
      <span class="cm-snippet">{props.m.snippet ?? ''}</span>
      <Marks m={props.m} />
      <Show when={props.m.hasAttachments}>
        <PaperclipIcon size={13} class="icon cm-clip" />
      </Show>
      <TagDots tags={props.m.tags ?? []} />
      <Show when={props.folderTag}>
        <span class="folder-tag cm-folder">{props.folderTag}</span>
      </Show>
      <SendState m={props.m} class="cm-date" fallback={<span class="cm-date">{smartDate(props.m.date)}</span>} />
    </button>
  );
}

function ExpandedCard(props: { m: MessageSummary; me: Me; folderTag?: string; actions: ActionTarget; onToggle: () => void }) {
  const id = props.m.id;
  const body = createMessageBody(id);
  const detail = () => body.shown()?.d ?? null;

  // Mark read once the body is actually on screen.
  let marked = false;
  createEffect(() => {
    if (!marked && body.phase() === 'ready' && props.m.unread) {
      marked = true;
      api.setRead(id, true).catch(() => {});
    }
  });

  return (
    <article class="card-msg expanded" classList={{ unread: props.m.unread }}>
      <header class="cm-head" onClick={() => props.onToggle()} title="Collapse">
        <Avatar name={props.m.fromName} addr={props.m.fromAddr} size={36} />
        <div class="cm-head-text">
          <div class="reader-from-line">
            <span class="reader-from-name">{props.m.fromName || props.m.fromAddr}</span>
            <Show when={props.m.fromName}>
              <span class="reader-from-addr">&lt;{props.m.fromAddr}&gt;</span>
            </Show>
          </div>
          <div class="recips">
            <Show when={detail()} fallback={<span class="recips-label">&nbsp;</span>}>
              {(d) => (
                <>
                  <AddrList label="To" list={d().to} />
                  <AddrList label="Cc" list={d().cc} />
                </>
              )}
            </Show>
          </div>
        </div>
        <span class="cm-head-right">
          <span class="cm-head-date">
            <Show when={props.m.draftId}>
              {(d) => (
                <button
                  class="link-btn cm-edit-draft"
                  onClick={(e) => {
                    e.stopPropagation();
                    editDraft(d());
                  }}
                >
                  Edit draft
                </button>
              )}
            </Show>
            <Marks m={props.m} />
            <TagDots tags={props.m.tags ?? []} />
            <Show when={props.folderTag}>
              <span class="folder-tag cm-folder">{props.folderTag}</span>
            </Show>
            <SendState
              m={props.m}
              class="reader-date"
              fallback={
                <time class="reader-date" dateTime={new Date(props.m.date * 1000).toISOString()}>
                  {longDate(props.m.date)}
                </time>
              }
            />
            {/* Overlays the date on hover/focus: no layout change. */}
            <CardActions target={props.actions} />
          </span>
          {/* Second row, slot reserved from the summary so nothing shifts when the detail lands. */}
          <Show when={props.m.hasAttachments || (detail()?.attachments.length ?? 0) > 0}>
            <span class="hdr-att">
              <Show when={(detail()?.attachments.length ?? 0) > 0} fallback={<span class="att-pill-ph" />}>
                <AttachmentControl messageId={id} list={detail()!.attachments} />
              </Show>
            </span>
          </Show>
        </span>
      </header>
      <Show when={body.shown()?.d.hasRemoteContent && !body.shown()?.remote}>
        <div class="remote-banner cm-banner">
          <ImageIcon size={14} />
          <span>Remote images blocked</span>
          <span class="dot-sep">·</span>
          <button class="link-btn" onClick={() => body.loadImages()}>
            Load images
          </button>
        </div>
      </Show>
      <div class="cm-body">
        <Switch>
          <Match when={body.phase() === 'error'}>
            <div class="body-error">
              <div class="body-error-title">Couldn’t open this message</div>
              <div class="body-error-sub">{body.error()}</div>
              <button class="btn secondary" onClick={() => body.retry()}>
                Try again
              </button>
            </div>
          </Match>
          <Match when={body.shown()}>
            {(s) => (
              <Show when={s().d.html} fallback={<PlainBody text={s().d.text ?? ''} />}>
                {(html) => <AutoHeightFrame doc={buildSrcdoc(html(), s().remote, CARD_FRAME_CSS)} />}
              </Show>
            )}
          </Match>
          <Match when={true}>
            <BodySkeleton downloading={body.phase() === 'downloading'} />
          </Match>
        </Switch>
      </div>
    </article>
  );
}

// ------------------------------------------------------------- conversation

function ConversationView(props: { thread: ThreadSummary; ref?: (el: HTMLElement) => void }) {
  const t = props.thread;
  const [state, setState] = createStore<{
    list: MessageSummary[];
    loaded: boolean;
    error: string | null;
    hiddenOther: number;
    hiddenTrash: number;
  }>({ list: [], loaded: false, error: null, hiddenOther: 0, hiddenTrash: 0 });
  /** Local, temporary reveal of this conversation's hidden messages (resets per conversation). */
  const [revealOther, setRevealOther] = createSignal(false);
  const [revealTrash, setRevealTrash] = createSignal(false);
  /** How many each reveal brought in (for the "Hide N …" label). */
  const [revealedOther, setRevealedOther] = createSignal(0);
  const [revealedTrash, setRevealedTrash] = createSignal(0);
  /** Extra bottom space that lets a reveal keep the reader's card exactly in place. */
  const [spacer, setSpacer] = createSignal(0);
  const [expanded, setExpanded] = createStore<Record<number, boolean>>({});
  const [scrollTarget, setScrollTarget] = createSignal<number | null>(null);
  let scrollEl!: HTMLDivElement;
  let token = 0;
  let initialized = false;
  const seen = new Set<number>();

  const me = createMemo<Me>(() => {
    const f = folderById(t.folderId);
    const a = (accounts() ?? []).find((x) => x.id === f?.accountId);
    return a ? { name: a.displayName, email: a.email } : undefined;
  });

  /**
   * The card the user is looking at (the one crossing the upper third of the viewport,
   * else the topmost visible one) and its offset in the viewport.
   */
  function captureAnchor(): { id: number; offset: number } | null {
    const top = scrollEl.scrollTop;
    const eye = top + scrollEl.clientHeight / 3;
    const cards = [...scrollEl.querySelectorAll<HTMLElement>('.card-slot')];
    const el =
      cards.find((c) => c.offsetTop <= eye && c.offsetTop + c.offsetHeight > eye) ??
      cards.find((c) => c.offsetTop + c.offsetHeight > top);
    return el ? { id: Number(el.dataset.mid), offset: el.offsetTop - top } : null;
  }
  function restoreAnchor(a: { id: number; offset: number } | null) {
    if (!a) return;
    let el = scrollEl.querySelector<HTMLElement>(`[data-mid="${a.id}"]`);
    if (!el) {
      // The anchor card itself was hidden: keep the next card that's still there in place.
      el = [...scrollEl.querySelectorAll<HTMLElement>('.card-slot')].find((x) => x.offsetTop >= a.offset + scrollEl.scrollTop) ?? null;
    }
    if (!el) return;
    const want = el.offsetTop - a.offset;
    const max = scrollEl.scrollHeight - scrollEl.clientHeight;
    // Not enough content below to scroll that far: grow a bottom spacer instead of letting
    // the card jump (stays until another conversation is opened).
    if (want > max) setSpacer((h) => h + Math.ceil(want - max));
    scrollEl.scrollTop = want;
  }

  async function load(opts: { reveal?: boolean } = {}) {
    const my = ++token;
    try {
      const scope = conversationScope() === 'all' || revealOther() ? 'all' : 'folder';
      const trash = includeTrash() || revealTrash();
      const view = await api.getThread(t.id, t.folderId, scope, trash);
      if (my !== token) return;
      const list = view.messages;
      // The core reports folderId = this folder for every message still in it. None left
      // (archived/deleted elsewhere) → the conversation left this folder: blank the reader,
      // regardless of reveals or All mail / Include Trash (those only add other folders).
      if (!list.some((m) => m.folderId === t.folderId)) {
        if (selectedThreadId() === t.id) selectThread(null);
        return;
      }
      // Reveal/hide inserts or removes cards above the reader's eye line: keep it still.
      const anchor = opts.reveal && initialized ? captureAnchor() : null;
      batch(() => {
        if (opts.reveal) {
          // Newly revealed (archived/trashed) mail stays collapsed.
          for (const m of list) if (!seen.has(m.id)) seen.add(m.id);
        }
        setState('list', reconcile(list, { key: 'id' }));
        setState('hiddenOther', view.hiddenOther);
        setState('hiddenTrash', view.hiddenTrash);
        setState('loaded', true);
        setState('error', null);
      });
      restoreAnchor(anchor);
      if (!initialized) {
        initialized = true;
        const newest = list[list.length - 1];
        const open: Record<number, boolean> = {};
        for (const m of list) {
          seen.add(m.id);
          if (m.unread) open[m.id] = true;
        }
        if (newest) open[newest.id] = true;
        setExpanded(reconcile(open));
        const target = list.find((m) => m.unread) ?? newest;
        setScrollTarget(target?.id ?? null);
      } else {
        // New arrivals while open: expand them if unread.
        for (const m of list) {
          if (!seen.has(m.id)) {
            seen.add(m.id);
            if (m.unread) setExpanded(m.id, true);
          }
        }
      }
    } catch (e) {
      if (my !== token) return;
      setState('error', errorMessage(e));
    }
  }
  void load();

  function toggleRevealOther() {
    if (revealOther()) setRevealOther(false);
    else {
      setRevealedOther(state.hiddenOther);
      setRevealOther(true);
    }
    void load({ reveal: true });
  }
  function toggleRevealTrash() {
    if (revealTrash()) setRevealTrash(false);
    else {
      setRevealedTrash(state.hiddenTrash);
      setRevealTrash(true);
    }
    void load({ reveal: true });
  }
  const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;
  const folderTag = (m: MessageSummary) => (m.folderId !== t.folderId ? folderById(m.folderId)?.displayName : undefined);
  const metaText = () => {
    let s = plural(state.list.length, 'message', 'messages');
    if (conversationScope() === 'all') s += ' · all mail';
    if (includeTrash()) s += ' · incl. Trash';
    return s;
  };

  // Options changed: refetch the conversation and re-apply the default expansion to the new set.
  createEffect(
    on(
      [conversationScope, includeTrash],
      () => {
        initialized = false;
        seen.clear();
        void load();
      },
      { defer: true },
    ),
  );

  // Scroll the first relevant card into view once, after it's rendered.
  createEffect(() => {
    const id = scrollTarget();
    if (id == null) return;
    requestAnimationFrame(() => {
      const el = scrollEl.querySelector<HTMLElement>(`[data-mid="${id}"]`);
      const first = state.list[0];
      if (!el || first?.id === id) scrollEl.scrollTop = 0;
      else scrollEl.scrollTop = Math.max(0, el.offsetTop - 16);
      setScrollTarget(null);
    });
  });

  // Live updates: our folder or any folder the conversation touches.
  let refetchTimer: ReturnType<typeof setTimeout> | undefined;
  const unlistenP = events.onMessagesChanged(({ folderIds }) => {
    const touched = folderIds.includes(t.folderId) || state.list.some((m) => folderIds.includes(m.folderId));
    if (!touched) return;
    clearTimeout(refetchTimer);
    refetchTimer = setTimeout(() => void load(), 80);
  });
  onCleanup(() => {
    token++;
    clearTimeout(refetchTimer);
    void unlistenP.then((un) => un());
  });

  // Header actions act on every message being viewed (incl. revealed ones); star = newest.
  const headerTarget = (): ActionTarget | null => {
    if (!state.loaded || state.list.length === 0) return null;
    const newest = state.list[state.list.length - 1]!;
    return {
      ids: state.list.map((m) => m.id),
      folderId: t.folderId,
      anyUnread: state.list.some((m) => m.unread),
      starId: newest.id,
      replyId: newest.id,
      starred: newest.flagged,
      advance: true,
      tagSets: state.list.map((m) => m.tags ?? []),
    };
  };
  /** One card's own actions; it advances only when that message is the last one here. */
  const cardTarget = (m: MessageSummary): ActionTarget => ({
    ids: [m.id],
    folderId: t.folderId,
    anyUnread: m.unread,
    starId: m.id,
    replyId: m.id,
    starred: m.flagged,
    advance: m.folderId === t.folderId && !state.list.some((x) => x.id !== m.id && x.folderId === t.folderId),
    tagSets: [m.tags ?? []],
  });

  const subject = () => state.list[0]?.subject || t.subject || '(no subject)';
  const toggle = (id: number) => setExpanded(id, (v) => !v);

  return (
    <div class="conv-view">
      {/* Subject stays put; only the cards below scroll. */}
      <header class="conv-head reader-band" data-tauri-drag-region={DRAG}>
        <h1 class="reader-subject" title={subject()}>
          {subject()}
        </h1>
        <div class="conv-meta" data-tauri-drag-region={DRAG}>
          <span class="conv-count" data-tauri-drag-region={DRAG}>
            <Show when={state.loaded} fallback={<>&nbsp;</>}>
              {metaText()}
            </Show>
          </span>
          <Show when={headerTarget()}>
            {(x) => (
              <TagChips
                tags={[...new Set(x().tagSets.flat())]}
                onRemove={(k) =>
                  void runAction({ kind: 'tag', keyword: k, on: false }, { ids: x().ids.filter((_, i) => x().tagSets[i]?.includes(k)), folderId: x().folderId, advance: false })
                }
                onOpen={searchTag}
              />
            )}
          </Show>
          <HeaderActions target={headerTarget()} />
        </div>
      </header>
      <div class="conv-scroll" ref={(el) => { scrollEl = el; props.ref?.(el); }} tabIndex={-1}>
      <div class="conv-cards">
        <Show when={state.error && !state.loaded}>
          <div class="body-error">
            <div class="body-error-title">Couldn’t load this conversation</div>
            <div class="body-error-sub">{state.error}</div>
            <button class="btn secondary" onClick={() => void load()}>
              Try again
            </button>
          </div>
        </Show>
        <Show when={!state.loaded && !state.error}>
          <div class="card-msg expanded">
            <BodySkeleton downloading={false} />
          </div>
        </Show>
        {/* Rendered in the same pass as the cards (one fetch), fixed height. */}
        <Show when={state.loaded && (revealOther() || state.hiddenOther > 0)}>
          <div class="reveal-row">
            <Show
              when={revealOther()}
              fallback={
                <>
                  <span class="reveal-dots" aria-hidden="true">⋯</span>
                  <span class="reveal-text">{plural(state.hiddenOther, 'more message', 'more messages')} in other folders</span>
                  <button class="link-btn" onClick={toggleRevealOther}>
                    Show
                  </button>
                </>
              }
            >
              <button class="link-btn" onClick={toggleRevealOther}>
                Hide {plural(revealedOther(), 'message', 'messages')} from other folders
              </button>
            </Show>
          </div>
        </Show>
        <Show when={state.loaded && (revealTrash() || state.hiddenTrash > 0)}>
          <div class="reveal-row">
            <Show
              when={revealTrash()}
              fallback={
                <>
                  <span class="reveal-dots" aria-hidden="true">⋯</span>
                  <span class="reveal-text">{plural(state.hiddenTrash, 'message', 'messages')} in Trash</span>
                  <button class="link-btn" onClick={toggleRevealTrash}>
                    Show
                  </button>
                </>
              }
            >
              <button class="link-btn" onClick={toggleRevealTrash}>
                Hide {plural(revealedTrash(), 'message', 'messages')} from Trash
              </button>
            </Show>
          </div>
        </Show>
        <For each={state.list}>
          {(m) => (
            <div class="card-slot" data-mid={m.id}>
              <Show
                when={expanded[m.id]}
                fallback={<CollapsedCard m={m} me={me()} folderTag={folderTag(m)} onToggle={() => toggle(m.id)} />}
              >
                <ExpandedCard m={m} me={me()} folderTag={folderTag(m)} actions={cardTarget(m)} onToggle={() => toggle(m.id)} />
              </Show>
            </div>
          )}
        </For>
        <div class="conv-spacer" style={{ height: `${spacer()}px` }} aria-hidden="true" />
      </div>
      </div>
    </div>
  );
}

export function ConversationReader(props: { ref?: (el: HTMLElement) => void }) {
  return (
    <section class="reader conv" aria-label="Conversation">
      <Show when={selectedThread()} keyed fallback={<ReaderEmpty />}>
        {(t) => <ConversationView thread={t} ref={props.ref} />}
      </Show>
    </section>
  );
}
