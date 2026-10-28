import { createEffect, createSignal, For, onCleanup, Show } from 'solid-js';
import { createStore } from 'solid-js/store';
import { Portal } from 'solid-js/web';
import { api, errorMessage, type Attachment } from '../api';
import { fmtSize } from '../lib/format';
import { showToast } from './Toast';
import { ChevronIcon, CloudIcon, DownloadIcon, ExternalIcon, FileIcon, ImageIcon, PaperclipIcon } from './Icons';

export { ToastHost as AttachmentToast } from './Toast';

// ------------------------------------------------------------- actions (shared busy state)

const baseName = (p: string) => p.split(/[\\/]/).pop() || p;
type Action = 'open' | 'save';
/** "msgId:index" -> running action; shared so the chip and the popover agree. */
const [busy, setBusy] = createStore<Record<string, Action | undefined>>({});
const [savingAll, setSavingAll] = createStore<Record<number, boolean>>({});
const key = (messageId: number, a: Attachment) => `${messageId}:${a.index}`;

async function runAttachment(messageId: number, a: Attachment, action: Action) {
  const k = key(messageId, a);
  if (busy[k]) return;
  if (action === 'open' && !a.openable) action = 'save';
  setBusy(k, action);
  try {
    if (action === 'open') await api.openAttachment(messageId, a.index);
    else {
      const path = await api.saveAttachment(messageId, a.index);
      showToast({ kind: 'ok', text: `Saved ${baseName(path)} to Downloads`, path });
    }
  } catch (e) {
    showToast({ kind: 'error', text: errorMessage(e) });
  } finally {
    setBusy(k, undefined);
  }
}

async function saveAll(messageId: number, count: number) {
  if (savingAll[messageId]) return;
  setSavingAll(messageId, true);
  try {
    const paths = await api.saveAllAttachments(messageId);
    // "Show in folder" highlights the first file; they're all in Downloads.
    showToast({ kind: 'ok', text: `Saved ${paths.length || count} attachments to Downloads`, path: paths[0] });
  } catch (e) {
    showToast({ kind: 'error', text: errorMessage(e) });
  } finally {
    setSavingAll(messageId, false);
  }
}

const isImage = (a: Attachment) => a.mime.startsWith('image/');
const sizeLabel = (a: Attachment) => `${a.downloaded ? '' : '~'}${fmtSize(a.size)}`;
const saveKey = (e: KeyboardEvent) =>
  (e.key === 'Enter' && e.shiftKey) || ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === 's');

function TypeIcon(props: { a: Attachment; busy: boolean; size?: number }) {
  return (
    <span class="att-icon">
      <Show when={!props.busy} fallback={<span class="spinner small" />}>
        {isImage(props.a) ? <ImageIcon size={props.size ?? 13} /> : <FileIcon size={props.size ?? 13} />}
      </Show>
    </span>
  );
}

// ------------------------------------------------------------- single attachment: same pill, ⤓ segment

function SinglePill(props: { messageId: number; a: Attachment }) {
  const b = () => busy[key(props.messageId, props.a)];
  const size = () => sizeLabel(props.a);
  const tip = () =>
    [
      `${props.a.name} · ${props.a.openable ? 'Click to open' : "This file type can't be opened directly; it will be saved to Downloads"}`,
      !props.a.downloaded ? 'Will be downloaded from the server' : '',
    ]
      .filter(Boolean)
      .join('\n');
  const onKey = (e: KeyboardEvent, def: Action) => {
    if (saveKey(e) || e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      e.stopPropagation();
      void runAttachment(props.messageId, props.a, saveKey(e) ? 'save' : def);
    }
  };
  return (
    <span class="att-pill single" classList={{ busy: !!b() }}>
      <button
        class="att-pill-main"
        aria-disabled={!!b()}
        aria-label={`${props.a.openable ? 'Open' : 'Save'} ${props.a.name}, ${fmtSize(props.a.size)}`}
        title={tip()}
        onClick={() => void runAttachment(props.messageId, props.a, 'open')}
        onKeyDown={(e) => onKey(e, 'open')}
      >
        <span class="att-icon">
          <Show when={!b()} fallback={<span class="spinner small" />}>
            <PaperclipIcon size={12} />
          </Show>
        </span>
        <span class="att-pill-count">1</span>
        <span class="att-pill-size">· {size()}</span>
      </button>
      <button
        class="att-pill-save"
        aria-disabled={!!b()}
        aria-label={`Save ${props.a.name}`}
        title={`Save ${props.a.name} to Downloads`}
        onClick={() => void runAttachment(props.messageId, props.a, 'save')}
        onKeyDown={(e) => onKey(e, 'save')}
      >
        <DownloadIcon size={12} />
      </button>
    </span>
  );
}

// ------------------------------------------------------------- several: pill + dropdown

const POP_W = 320;
const POP_MAX_H = 340;

function AttachmentMenu(props: {
  messageId: number;
  list: Attachment[];
  anchor: HTMLElement;
  onClose: (refocus: boolean) => void;
}) {
  let el!: HTMLDivElement;
  const [pos, setPos] = createSignal({ left: 0, top: 0, up: false });
  const place = () => {
    const r = props.anchor.getBoundingClientRect();
    const h = Math.min(POP_MAX_H, el?.offsetHeight || POP_MAX_H);
    const below = window.innerHeight - r.bottom - 8;
    const up = below < h && r.top - 8 > below;
    const left = Math.max(8, Math.min(window.innerWidth - POP_W - 8, r.right - POP_W));
    setPos({ left, top: up ? r.top - 6 - h : r.bottom + 6, up });
  };
  const rows = () => [...el.querySelectorAll<HTMLElement>('[data-nav]')];
  const move = (d: number) => {
    const list = rows();
    const i = list.indexOf(document.activeElement as HTMLElement);
    list[(i + d + list.length) % list.length]?.focus();
  };

  createEffect(() => {
    place();
    queueMicrotask(() => {
      place(); // real height known now
      rows()[0]?.focus({ preventScroll: true });
    });
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (el.contains(t) || props.anchor.contains(t)) return;
      props.onClose(false);
    };
    const onScroll = (e: Event) => {
      if (!el.contains(e.target as Node)) props.onClose(false);
    };
    document.addEventListener('mousedown', onDown, true);
    document.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', place);
    onCleanup(() => {
      document.removeEventListener('mousedown', onDown, true);
      document.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', place);
    });
  });

  const total = () => props.list.length;
  return (
    <Portal>
      <div
        ref={el}
        class="att-menu"
        classList={{ up: pos().up }}
        role="menu"
        aria-label={`${total()} attachments`}
        style={{ left: `${pos().left}px`, top: `${pos().top}px`, width: `${POP_W}px`, 'max-height': `${POP_MAX_H}px` }}
        onKeyDown={(e) => {
          // Never let these reach the list's global shortcuts.
          e.stopPropagation();
          if (e.key === 'Escape') {
            e.preventDefault();
            props.onClose(true);
          } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
            e.preventDefault();
            move(e.key === 'ArrowDown' ? 1 : -1);
          } else if (e.key === 'Home' || e.key === 'End') {
            e.preventDefault();
            const l = rows();
            (e.key === 'Home' ? l[0] : l[l.length - 1])?.focus();
          } else if (e.key === 'Tab') {
            e.preventDefault();
            move(e.shiftKey ? -1 : 1);
          }
        }}
      >
        <div class="att-menu-rows">
          <For each={props.list}>
            {(a) => {
              const b = () => busy[key(props.messageId, a)];
              return (
                <div
                  class="att-row"
                  role="menuitem"
                  tabIndex={-1}
                  data-nav
                  aria-label={`${a.name}, ${fmtSize(a.size)}${a.openable ? '' : ', save only'}`}
                  title={a.name}
                  onClick={() => void runAttachment(props.messageId, a, 'open')}
                  onKeyDown={(e) => {
                    if (saveKey(e) || e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      void runAttachment(props.messageId, a, saveKey(e) ? 'save' : 'open');
                    }
                  }}
                >
                  <TypeIcon a={a} busy={!!b()} size={14} />
                  <span class="att-row-name">{a.name}</span>
                  <span class="att-row-size">
                    {sizeLabel(a)}
                    <Show when={!a.downloaded}>
                      <span title="Will be downloaded from the server">
                        <CloudIcon size={11} class="icon att-cloud" />
                      </span>
                    </Show>
                  </span>
                  <button
                    class="att-row-btn"
                    tabIndex={-1}
                    aria-label={`Open ${a.name}`}
                    aria-disabled={!a.openable || !!b()}
                    title={a.openable ? 'Open (Enter)' : "This file type can't be opened directly — save it instead"}
                    onClick={(e) => {
                      e.stopPropagation();
                      if (a.openable) void runAttachment(props.messageId, a, 'open');
                    }}
                  >
                    <ExternalIcon size={13} />
                  </button>
                  <button
                    class="att-row-btn"
                    tabIndex={-1}
                    aria-label={`Save ${a.name}`}
                    aria-disabled={!!b()}
                    title="Save to Downloads (Shift+Enter)"
                    onClick={(e) => {
                      e.stopPropagation();
                      void runAttachment(props.messageId, a, 'save');
                    }}
                  >
                    <DownloadIcon size={13} />
                  </button>
                </div>
              );
            }}
          </For>
        </div>
      </div>
    </Portal>
  );
}

function MultiPill(props: { messageId: number; list: Attachment[] }) {
  const [open, setOpen] = createSignal(false);
  let pill!: HTMLButtonElement;
  const n = () => props.list.length;
  const total = () => props.list.reduce((t, a) => t + a.size, 0);
  const approx = () => props.list.some((a) => !a.downloaded);
  const rowBusy = () => props.list.some((a) => busy[key(props.messageId, a)]);
  const saving = () => !!savingAll[props.messageId];
  const size = () => `${approx() ? '~' : ''}${fmtSize(total())}`;
  const saveAllKey = (e: KeyboardEvent) =>
    (e.metaKey || e.ctrlKey) && e.shiftKey && !e.altKey && e.key.toLowerCase() === 's';
  return (
    <span class="att-pill multi" classList={{ open: open() }}>
      <button
        ref={pill}
        class="att-pill-main"
        aria-haspopup="menu"
        aria-expanded={open()}
        aria-label={`${n()} attachments, ${size()}`}
        title={props.list.map((a) => a.name).join('\n')}
        onClick={() => setOpen((v) => !v)}
        onKeyDown={(e) => {
          if (saveAllKey(e)) {
            e.preventDefault();
            e.stopPropagation();
            void saveAll(props.messageId, n());
          } else if (e.key === 'Enter' || e.key === ' ' || e.key === 'ArrowDown') {
            e.preventDefault();
            e.stopPropagation();
            setOpen(true);
          }
        }}
      >
        <span class="att-icon">
          {/* A per-file action running with the dropdown closed: show it here. */}
          <Show when={!rowBusy() || open()} fallback={<span class="spinner small" />}>
            <PaperclipIcon size={12} />
          </Show>
        </span>
        <span class="att-pill-count">{n()}</span>
        <span class="att-pill-size">· {size()}</span>
        <span class="att-pill-disc" aria-hidden="true">
          <ChevronIcon size={10} />
        </span>
      </button>
      <button
        class="att-pill-save"
        aria-disabled={saving()}
        aria-label="Save all attachments"
        title={`Save all ${n()} attachments to a folder in Downloads`}
        onClick={() => void saveAll(props.messageId, n())}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            e.stopPropagation();
            void saveAll(props.messageId, n());
          }
        }}
      >
        <Show when={!saving()} fallback={<span class="spinner small" />}>
          <DownloadIcon size={12} />
        </Show>
      </button>
      <Show when={open()}>
        <AttachmentMenu
          messageId={props.messageId}
          list={props.list}
          anchor={pill}
          onClose={(refocus) => {
            setOpen(false);
            if (refocus) pill.focus();
          }}
        />
      </Show>
    </span>
  );
}

/**
 * The attachment control in a message header: a pill (count · size) with a ⤓ segment.
 * One attachment: main = Open, ⤓ = Save. Several: main = dropdown, ⤓ = Save all.
 * Clicks/keys don't reach the header (which toggles conversation cards).
 */
export function AttachmentControl(props: { messageId: number; list: Attachment[] }) {
  return (
    <span class="att-ctl" onClick={(e) => e.stopPropagation()}>
      <Show when={props.list.length > 1} fallback={<Show when={props.list[0]}>{(a) => <SinglePill messageId={props.messageId} a={a()} />}</Show>}>
        <MultiPill messageId={props.messageId} list={props.list} />
      </Show>
    </span>
  );
}
