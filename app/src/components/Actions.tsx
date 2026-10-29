import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from 'solid-js';
import { hint } from '../lib/shortcuts';
import { Portal } from 'solid-js/web';
import type { Folder, FolderRole } from '../api';
import { definedTags, TagMenu } from './Tags';
import { clearHeaderHandler, runAction, setHeaderHandler, type BarCommand } from '../lib/actions';
import { foldersByAccount, folderById } from '../state';
import { ArchiveIcon, DeleteForeverIcon, ForwardIcon, InboxIcon, JunkIcon, MoreIcon, MoveIcon, ReadIcon, ReplyAllIcon, ReplyIcon, RoleIcon, StarFilledIcon, StarIcon, TagIcon, TrashIcon, UnreadIcon } from './Icons';
import { startReply } from '../lib/compose';

// ------------------------------------------------------------- popover placement

/** Fixed position for a popover anchored under `anchor` (right edges aligned), flipped up near the bottom. */
export function placeNear(anchor: HTMLElement, w: number, h: number) {
  const r = anchor.getBoundingClientRect();
  const below = window.innerHeight - r.bottom - 8;
  const up = below < h && r.top - 8 > below;
  const left = Math.max(8, Math.min(window.innerWidth - w - 8, r.right - w));
  return { left, top: up ? Math.max(8, r.top - 6 - h) : r.bottom + 6, up };
}

/** Close on outside mousedown / scroll elsewhere / resize while mounted. */
export function useDismiss(el: () => HTMLElement | undefined, anchor: HTMLElement, close: () => void, reposition: () => void) {
  onMount(() => {
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (el()?.contains(t) || anchor.contains(t)) return;
      if ((t as Element).closest?.('.modal-backdrop')) return;
      close();
    };
    const onScroll = (e: Event) => {
      if (!el()?.contains(e.target as Node)) close();
    };
    // Clicks in an email body happen inside its iframe and never reach this document; the
    // window losing focus to the iframe (or another app) is the signal instead.
    const onBlur = () => close();
    // Esc anywhere. The popup's own keydown handler stops propagation when focus is inside it.
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !document.querySelector('.modal-backdrop')) close();
    };
    document.addEventListener('mousedown', onDown, true);
    document.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', reposition);
    window.addEventListener('blur', onBlur);
    window.addEventListener('keydown', onKey);
    onCleanup(() => {
      document.removeEventListener('mousedown', onDown, true);
      document.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', reposition);
      window.removeEventListener('blur', onBlur);
      window.removeEventListener('keydown', onKey);
    });
  });
}

// ------------------------------------------------------------- confirm dialog

const [confirmReq, setConfirmReq] = createSignal<{ text: string; ok: string; resolve: (v: boolean) => void } | null>(null);
export function confirmDialog(text: string, ok: string): Promise<boolean> {
  return new Promise((resolve) => setConfirmReq({ text, ok, resolve }));
}
export function ConfirmHost() {
  const done = (v: boolean) => {
    const r = confirmReq();
    setConfirmReq(null);
    r?.resolve(v);
  };
  return (
    <Show when={confirmReq()}>
      {(r) => (
        <div
          class="modal-backdrop"
          onMouseDown={(e) => e.target === e.currentTarget && done(false)}
          onKeyDown={(e) => {
            e.stopPropagation();
            if (e.key === 'Escape') done(false);
          }}
        >
          <div class="card modal confirm" role="alertdialog" aria-modal="true" aria-label={r().text}>
            <p class="confirm-text">{r().text}</p>
            <div class="form-actions">
              <button class="btn secondary" onClick={() => done(false)}>
                Cancel
              </button>
              <button class="btn danger" ref={(el) => queueMicrotask(() => el.focus())} onClick={() => done(true)}>
                {r().ok}
              </button>
            </div>
          </div>
        </div>
      )}
    </Show>
  );
}

// ------------------------------------------------------------- move picker

const ROLE_ORDER: Exclude<FolderRole, null>[] = ['inbox', 'flagged', 'important', 'sent', 'drafts', 'archive', 'all', 'junk', 'trash'];
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
const isInbox = (f: Folder) => f.role === 'inbox' || (f.path.length === 1 && f.path[0]?.toUpperCase() === 'INBOX');
const folderLabel = (f: Folder) => (f.role || isInbox(f) ? f.displayName : f.path.join(' / '));

function moveTargets(accountId: number, excludeId: number): Folder[] {
  const list = (foldersByAccount[accountId] ?? []).filter((f) => f.selectable && f.id !== excludeId);
  const roleRank = (f: Folder) => (isInbox(f) ? 0 : f.role ? ROLE_ORDER.indexOf(f.role) : 99);
  return [...list].sort((a, b) => roleRank(a) - roleRank(b) || collator.compare(folderLabel(a), folderLabel(b)));
}

const PICK_W = 280;
const PICK_H = 340;

export function MovePicker(props: { accountId: number; excludeFolderId: number; anchor: HTMLElement; onPick: (f: Folder) => void; onClose: (refocus: boolean) => void }) {
  let el: HTMLDivElement | undefined;
  let input!: HTMLInputElement;
  const [q, setQ] = createSignal('');
  const [active, setActive] = createSignal(0);
  const [pos, setPos] = createSignal(placeNear(props.anchor, PICK_W, PICK_H));
  const reposition = () => setPos(placeNear(props.anchor, PICK_W, el?.offsetHeight || PICK_H));
  useDismiss(() => el, props.anchor, () => props.onClose(false), reposition);

  const all = createMemo(() => moveTargets(props.accountId, props.excludeFolderId));
  const shown = createMemo(() => {
    const needle = q().trim().toLowerCase();
    return needle ? all().filter((f) => folderLabel(f).toLowerCase().includes(needle)) : all();
  });
  createEffect(() => {
    shown();
    setActive(0);
  });
  onMount(() => {
    reposition();
    queueMicrotask(() => {
      reposition();
      input.focus();
    });
  });
  const scrollActive = () => queueMicrotask(() => el?.querySelector('.mp-row.active')?.scrollIntoView({ block: 'nearest' }));

  return (
    <Portal>
      <div
        ref={el}
        class="move-picker"
        classList={{ up: pos().up }}
        role="dialog"
        aria-label="Move to folder"
        style={{ left: `${pos().left}px`, top: `${pos().top}px`, width: `${PICK_W}px`, 'max-height': `${PICK_H}px` }}
        onKeyDown={(e) => {
          e.stopPropagation();
          const n = shown().length;
          if (e.key === 'Escape') {
            e.preventDefault();
            props.onClose(true);
          } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
            e.preventDefault();
            if (n) setActive((i) => (i + (e.key === 'ArrowDown' ? 1 : -1) + n) % n);
            scrollActive();
          } else if (e.key === 'Enter') {
            e.preventDefault();
            const f = shown()[active()];
            if (f) props.onPick(f);
          }
        }}
      >
        <input
          ref={input}
          class="mp-filter"
          type="text"
          placeholder="Move to…"
          aria-label="Filter folders"
          spellcheck={false}
          autocomplete="off"
          value={q()}
          onInput={(e) => setQ(e.currentTarget.value)}
        />
        <div class="mp-list" role="listbox" aria-label="Folders">
          <For each={shown()} fallback={<div class="mp-empty">No matching folder</div>}>
            {(f, i) => (
              <div
                class="mp-row"
                classList={{ active: active() === i() }}
                role="option"
                aria-selected={active() === i()}
                title={f.path.join(' / ')}
                onMouseMove={() => setActive(i())}
                onClick={() => props.onPick(f)}
              >
                <span class="mp-icon">
                  <RoleIcon role={f.role ?? (isInbox(f) ? 'inbox' : null)} size={14} />
                </span>
                <span class="mp-name">{folderLabel(f)}</span>
              </div>
            )}
          </For>
        </div>
      </div>
    </Portal>
  );
}

// ------------------------------------------------------------- action bars

export interface ActionTarget {
  ids: number[];
  /** folder the messages are viewed in */
  folderId: number;
  anyUnread: boolean;
  /** message whose star the Star button toggles */
  starId: number | null;
  /** message Reply / Reply all / Forward start from (a conversation's newest) */
  replyId: number | null;
  starred: boolean;
  /** open the next older item when this removes what's being viewed */
  advance: boolean;
  /** tags of each viewed message (Tag menu ✓ / mixed state, 1–9 / 0) */
  tagSets: string[][];
}

function useActions(t: () => ActionTarget | null) {
  const role = () => {
    const x = t();
    return x ? folderById(x.folderId)?.role ?? null : null;
  };
  const inTrash = () => role() === 'trash' || role() === 'junk';
  const inSpam = () => role() === 'junk';
  const [picker, setPicker] = createSignal<HTMLElement | null>(null);

  async function command(c: BarCommand, anchor?: HTMLElement) {
    const x = t();
    if (!x || x.ids.length === 0) return;
    const base = { ids: x.ids, folderId: x.folderId, advance: x.advance };
    switch (c) {
      case 'archive':
        return runAction({ kind: 'archive' }, base);
      case 'delete':
        if (inTrash()) {
          const n = x.ids.length;
          const ok = await confirmDialog(`Delete ${n === 1 ? '1 message' : `${n} messages`} forever? This can't be undone.`, 'Delete forever');
          if (ok) return runAction({ kind: 'deleteForever' }, base);
          return;
        }
        return runAction({ kind: 'trash' }, base);
      case 'spam':
        return runAction({ kind: inSpam() ? 'notSpam' : 'spam' }, base);
      case 'unread':
        return runAction({ kind: x.anyUnread ? 'read' : 'unread' }, { ...base, advance: false });
      case 'star':
        if (x.starId == null) return;
        return runAction({ kind: x.starred ? 'unflag' : 'flag' }, { ids: [x.starId], folderId: x.folderId, advance: false });
      case 'move':
        // The trigger toggles: a second press closes it.
        if (anchor) setPicker(picker() ? null : anchor);
        return;
      case 'tag-menu':
        if (anchor) setTagMenu(tagMenu() ? null : anchor);
        return;
      case 'tag-clear':
        return clearTags();
      case 'reply':
      case 'replyAll':
      case 'forward':
        if (x.replyId != null) return startReply(x.replyId, c);
        return;
      default: {
        // tag-N: toggle the Nth defined tag
        const t = definedTags()[Number(c.slice(4)) - 1];
        if (t) toggleTag(t.keyword, !x.tagSets.every((s) => s.includes(t.keyword)));
        return;
      }
    }
  }
  const [tagMenu, setTagMenu] = createSignal<HTMLElement | null>(null);
  /** Read/star-like: applied at once, no undo toast. Only sends ids that change. */
  function toggleTag(keyword: string, on: boolean) {
    const x = t();
    if (!x) return;
    const ids = x.ids.filter((_, i) => (x.tagSets[i]?.includes(keyword) ?? false) !== on);
    if (ids.length) void runAction({ kind: 'tag', keyword, on }, { ids, folderId: x.folderId, advance: false });
  }
  function clearTags() {
    const x = t();
    if (!x) return;
    const all = new Set(x.tagSets.flat());
    for (const k of all) toggleTag(k, false);
  }
  const tagMenuEl = () => {
    const a = tagMenu();
    const x = t();
    if (!a || !x) return null;
    return (
      <TagMenu
        anchor={a}
        tagSets={x.tagSets}
        onToggle={toggleTag}
        onClear={() => {
          clearTags();
          setTagMenu(null);
        }}
        onClose={(refocus) => {
          setTagMenu(null);
          if (refocus) a.focus();
        }}
      />
    );
  };
  const pickerEl = () => {
    const a = picker();
    const x = t();
    const f = x ? folderById(x.folderId) : undefined;
    if (!a || !x || !f) return null;
    return (
      <MovePicker
        accountId={f.accountId}
        excludeFolderId={x.folderId}
        anchor={a}
        onClose={(refocus) => {
          setPicker(null);
          if (refocus) a.focus();
        }}
        onPick={(dest) => {
          setPicker(null);
          void runAction({ kind: 'move', targetFolderId: dest.id }, { ids: x.ids, folderId: x.folderId, advance: x.advance });
        }}
      />
    );
  };
  return { command, inTrash, inSpam, pickerEl, tagMenuEl, toggleTag };
}

function Btn(props: { label: string; key?: string; onClick: (el: HTMLButtonElement) => void; children: any; ref?: (el: HTMLButtonElement) => void; danger?: boolean }) {
  let el!: HTMLButtonElement;
  return (
    <button
      ref={(e) => {
        el = e;
        props.ref?.(e);
      }}
      class="act-btn"
      classList={{ danger: !!props.danger }}
      aria-label={props.label}
      title={props.key ? `${props.label} (${props.key})` : props.label}
      onClick={(e) => {
        e.stopPropagation();
        props.onClick(el);
      }}
    >
      {props.children}
    </button>
  );
}

/** Reader header actions (fixed slot on the band's info row). Drives the keyboard shortcuts. */
export function HeaderActions(props: { target: ActionTarget | null }) {
  const a = useActions(() => props.target);
  let moveBtn!: HTMLButtonElement;
  let tagBtn!: HTMLButtonElement;
  const handler = (c: BarCommand) => void a.command(c, c === 'tag-menu' ? tagBtn : moveBtn);
  setHeaderHandler(handler);
  onCleanup(() => clearHeaderHandler(handler));
  const x = () => props.target;
  return (
    <div class="act-bar" role="toolbar" aria-label="Message actions" classList={{ off: !x() }}>
      <Btn label="Reply" key={hint('reply')} onClick={() => void a.command('reply')}>
        <ReplyIcon />
      </Btn>
      <Btn label="Reply all" key={hint('reply-all')} onClick={() => void a.command('replyAll')}>
        <ReplyAllIcon />
      </Btn>
      <Btn label="Forward" key={hint('forward')} onClick={() => void a.command('forward')}>
        <ForwardIcon />
      </Btn>
      <span class="act-sep" />
      <Btn label="Archive" key={hint('archive')} onClick={() => void a.command('archive')}>
        <ArchiveIcon />
      </Btn>
      <Show
        when={a.inTrash()}
        fallback={
          <Btn label="Delete" key={hint('delete')} onClick={() => void a.command('delete')}>
            <TrashIcon />
          </Btn>
        }
      >
        <Btn label="Delete forever" key={hint('delete')} danger onClick={() => void a.command('delete')}>
          <DeleteForeverIcon />
        </Btn>
      </Show>
      <Btn label="Move to…" key={hint('move')} ref={(e) => (moveBtn = e)} onClick={(el) => void a.command('move', el)}>
        <MoveIcon />
      </Btn>
      <Show
        when={a.inSpam()}
        fallback={
          <Btn label="Spam" key={hint('spam')} onClick={() => void a.command('spam')}>
            <JunkIcon />
          </Btn>
        }
      >
        <Btn label="Not spam" key={hint('spam')} onClick={() => void a.command('spam')}>
          <InboxIcon />
        </Btn>
      </Show>
      <Btn label={x()?.anyUnread ? 'Mark as read' : 'Mark as unread'} key={hint('unread')} onClick={() => void a.command('unread')}>
        <Show when={x()?.anyUnread} fallback={<UnreadIcon />}>
          <ReadIcon />
        </Show>
      </Btn>
      <Btn label={x()?.starred ? 'Unstar' : 'Star'} key={hint('star')} onClick={() => void a.command('star')}>
        <Show when={x()?.starred} fallback={<StarIcon />}>
          <StarFilledIcon class="icon starred" />
        </Show>
      </Btn>
      <Btn label="Tag" key={`${hint('tag-n')}, ${hint('tag-clear')}`} ref={(e) => (tagBtn = e)} onClick={(el) => void a.command('tag-menu', el)}>
        <TagIcon />
      </Btn>
      {a.pickerEl()}
      {a.tagMenuEl()}
    </div>
  );
}

/** Per-message actions on an expanded conversation card (shown on header hover/focus). */
export function CardActions(props: { target: ActionTarget }) {
  const a = useActions(() => props.target);
  const [menu, setMenu] = createSignal<HTMLElement | null>(null);
  let moreBtn!: HTMLButtonElement;
  return (
    <span class="card-acts" onClick={(e) => e.stopPropagation()}>
      <Btn label="Reply" onClick={() => void a.command('reply')}>
        <ReplyIcon size={14} />
      </Btn>
      <Btn label="Archive" onClick={() => void a.command('archive')}>
        <ArchiveIcon size={14} />
      </Btn>
      <Btn label={a.inTrash() ? 'Delete forever' : 'Delete'} danger={a.inTrash()} onClick={() => void a.command('delete')}>
        <Show when={a.inTrash()} fallback={<TrashIcon size={14} />}>
          <DeleteForeverIcon size={14} />
        </Show>
      </Btn>
      <Btn label="More actions" ref={(e) => (moreBtn = e)} onClick={(el) => setMenu(menu() ? null : el)}>
        <MoreIcon size={14} />
      </Btn>
      <Show when={menu()}>
        {(anchor) => (
          <CardMenu
            anchor={anchor()}
            target={props.target}
            inSpam={a.inSpam()}
            onClose={(refocus) => {
              setMenu(null);
              if (refocus) moreBtn.focus();
            }}
            onCommand={(c) => {
              setMenu(null);
              void a.command(c, moreBtn);
            }}
          />
        )}
      </Show>
      {a.pickerEl()}
      {a.tagMenuEl()}
    </span>
  );
}

const MENU_W = 190;
function CardMenu(props: { anchor: HTMLElement; target: ActionTarget; inSpam: boolean; onClose: (refocus: boolean) => void; onCommand: (c: BarCommand) => void }) {
  let el: HTMLDivElement | undefined;
  const [pos, setPos] = createSignal(placeNear(props.anchor, MENU_W, 140));
  const reposition = () => setPos(placeNear(props.anchor, MENU_W, el?.offsetHeight || 140));
  useDismiss(() => el, props.anchor, () => props.onClose(false), reposition);
  onMount(() =>
    queueMicrotask(() => {
      reposition();
      el?.querySelector<HTMLElement>('.vm-item')?.focus();
    }),
  );
  const items: [BarCommand, () => string][] = [
    ['replyAll', () => 'Reply all'],
    ['forward', () => 'Forward'],
    ['move', () => 'Move to…'],
    ['tag-menu', () => 'Tag…'],
    ['spam', () => (props.inSpam ? 'Not spam' : 'Spam')],
    ['unread', () => (props.target.anyUnread ? 'Mark as read' : 'Mark as unread')],
    ['star', () => (props.target.starred ? 'Unstar' : 'Star')],
  ];
  return (
    <Portal>
      <div
        ref={el}
        class="view-menu card-menu"
        role="menu"
        style={{ left: `${pos().left}px`, top: `${pos().top}px`, right: 'auto', width: `${MENU_W}px` }}
        onKeyDown={(e) => {
          e.stopPropagation();
          const list = [...(el?.querySelectorAll<HTMLElement>('.vm-item') ?? [])];
          const i = list.indexOf(document.activeElement as HTMLElement);
          if (e.key === 'Escape') {
            e.preventDefault();
            props.onClose(true);
          } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
            e.preventDefault();
            list[(i + (e.key === 'ArrowDown' ? 1 : -1) + list.length) % list.length]?.focus();
          }
        }}
      >
        <For each={items}>
          {([c, label]) => (
            <button role="menuitem" class="vm-item" onClick={() => props.onCommand(c)}>
              <span class="vm-label">{label()}</span>
            </button>
          )}
        </For>
      </div>
    </Portal>
  );
}
