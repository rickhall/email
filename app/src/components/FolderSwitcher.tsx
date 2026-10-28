import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from 'solid-js';
import { Portal } from 'solid-js/web';
import type { Account, Folder } from '../api';
import { avatarColor, fmtNum } from '../lib/format';
import { DRAG } from '../lib/platform';
import { norm } from '../lib/query';
import { accounts, folderById, foldersByAccount, selectedFolderId, selectFolder } from '../state';
import { ChevronIcon, RoleIcon } from './Icons';
import { isInboxPath, ROLE_ORDER } from './Sidebar';

// ------------------------------------------------------------- helpers

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
const isRole = (f: Folder) => !!f.role || isInboxPath(f);
export const folderPathLabel = (f: Folder) => (isRole(f) ? f.displayName : f.path.join(' / '));

/** The account's marker: the same tinted letter as in the rail (colour from the address, so
 *  accounts sharing a display name still differ); the address is in the tooltip. */
export function AccountDot(props: { account: Account; size?: number }) {
  return (
    <span
      class="acct-dot"
      style={{ background: avatarColor(props.account.email), width: `${props.size ?? 18}px`, height: `${props.size ?? 18}px` }}
      title={props.account.email}
      aria-label={props.account.email}
      role="img"
    >
      {(props.account.email[0] ?? '?').toUpperCase()}
    </span>
  );
}

function sortedSelectable(accountId: number): Folder[] {
  const rank = (f: Folder) => (isInboxPath(f) ? -1 : f.role ? ROLE_ORDER.indexOf(f.role) : 99);
  return (foldersByAccount[accountId] ?? [])
    .filter((f) => f.selectable)
    .sort((a, b) => rank(a) - rank(b) || collator.compare(folderPathLabel(a), folderPathLabel(b)));
}

/** Nearest selectable ancestor (by path) in the same account, or undefined. */
export function parentFolder(f: Folder): Folder | undefined {
  if (isRole(f)) return undefined;
  const list = foldersByAccount[f.accountId] ?? [];
  for (let n = f.path.length - 1; n > 0; n--) {
    const p = list.find((x) => x.selectable && x.path.length === n && x.path.every((s, i) => s === f.path[i]));
    if (p) return p;
  }
  return undefined;
}

// ------------------------------------------------------------- switcher state

const [open, setOpen] = createSignal(false);
let returnFocus: HTMLElement | null = null;

export function openFolderSwitcher(): void {
  returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  setOpen(true);
}

// ------------------------------------------------------------- breadcrumb (list header title)

/**
 * "account / Work / Clients ▾": selectable ancestors navigate; the title / ▾ opens the
 * switcher. Middle segments shrink first (ellipsis), the last one keeps its room.
 */
export function FolderCrumbs(props: { folder: Folder }) {
  const segs = createMemo(() => {
    const f = props.folder;
    const out: { label: string; folder?: Folder; current?: boolean }[] = [];
    if (isRole(f)) out.push({ label: f.displayName, current: true });
    else {
      const list = foldersByAccount[f.accountId] ?? [];
      f.path.forEach((seg, i) => {
        const last = i === f.path.length - 1;
        if (last) out.push({ label: f.displayName || seg, current: true });
        else {
          const anc = list.find((x) => x.path.length === i + 1 && x.path.every((s, k) => s === f.path[k]));
          out.push({ label: seg, folder: anc?.selectable ? anc : undefined });
        }
      });
    }
    return out;
  });
  const account = createMemo(() =>
    (accounts()?.length ?? 0) > 1 ? (accounts() ?? []).find((a) => a.id === props.folder.accountId) : undefined,
  );
  return (
    <h2 class="crumbs" data-tauri-drag-region={DRAG}>
      <Show when={account()}>{(a) => <AccountDot account={a()} />}</Show>
      <For each={segs()}>
        {(s, i) => (
          <>
            <Show when={i() > 0}>
              <span class="crumb-sep" aria-hidden="true">
                /
              </span>
            </Show>
            <Show
              when={s.current}
              fallback={
                <Show when={s.folder} fallback={<span class="crumb muted" title={s.label}>{s.label}</span>}>
                  {(f) => (
                    <button class="crumb link" title={`Go to ${folderPathLabel(f())}`} onClick={() => selectFolder(f().id)}>
                      {s.label}
                    </button>
                  )}
                </Show>
              }
            >
              <button class="crumb current" title="Switch folder (Ctrl+G)" onClick={() => openFolderSwitcher()}>
                {s.label}
              </button>
            </Show>
          </>
        )}
      </For>
      <button class="crumb-switch" title="Switch folder (Ctrl+G)" aria-label="Switch folder" aria-haspopup="dialog" aria-expanded={open()} onClick={() => openFolderSwitcher()}>
        <ChevronIcon size={11} />
      </button>
    </h2>
  );
}

// ------------------------------------------------------------- the popover

type Entry = { kind: 'group'; account: Account } | { kind: 'folder'; f: Folder; label: string };
const W = 340;
const MAX_H = 440;

function Switcher() {
  let el!: HTMLDivElement;
  let input!: HTMLInputElement;
  const [q, setQ] = createSignal('');
  const [active, setActive] = createSignal(0);
  const anchor = () => document.querySelector<HTMLElement>('.folder-layer .crumbs') ?? document.querySelector<HTMLElement>('.list-head');
  const [pos, setPos] = createSignal({ left: 0, top: 0 });
  const place = () => {
    const r = anchor()?.getBoundingClientRect();
    if (!r) return setPos({ left: 80, top: 80 });
    setPos({ left: Math.max(8, Math.min(window.innerWidth - W - 8, r.left - 6)), top: r.bottom + 6 });
  };

  const entries = createMemo<Entry[]>(() => {
    const needle = norm(q().trim());
    const out: Entry[] = [];
    for (const a of accounts() ?? []) {
      const fs = sortedSelectable(a.id)
        .map((f) => ({ f, label: folderPathLabel(f) }))
        .filter((x) => !needle || norm(x.label).includes(needle));
      if (fs.length === 0) continue;
      if ((accounts()?.length ?? 0) > 1) out.push({ kind: 'group', account: a });
      for (const x of fs) out.push({ kind: 'folder', ...x });
    }
    return out;
  });
  const folders = createMemo(() => entries().filter((e): e is Extract<Entry, { kind: 'folder' }> => e.kind === 'folder'));
  // Start on the current folder when unfiltered; on the first match when filtering.
  createEffect(() => {
    const list = folders();
    const cur = q().trim() ? -1 : list.findIndex((x) => x.f.id === selectedFolderId());
    setActive(Math.max(0, cur));
    queueMicrotask(() => el?.querySelector('.fs-row.active')?.scrollIntoView({ block: 'nearest' }));
  });

  const close = (refocus: boolean) => {
    setOpen(false);
    if (refocus) returnFocus?.focus();
  };
  const pick = (f: Folder) => {
    setOpen(false);
    selectFolder(f.id);
    queueMicrotask(() => document.querySelector<HTMLElement>('.folder-layer .list-scroll')?.focus({ preventScroll: true }));
  };

  onMount(() => {
    place();
    input.focus();
    const onDown = (e: MouseEvent) => {
      const t = e.target as Element;
      if (el.contains(t) || t.closest?.('.crumb-switch, .crumb.current')) return;
      close(false);
    };
    document.addEventListener('mousedown', onDown, true);
    window.addEventListener('resize', place);
    onCleanup(() => {
      document.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('resize', place);
    });
  });

  return (
    <div
      ref={el}
      class="move-picker folder-switcher"
      role="dialog"
      aria-label="Switch folder"
      style={{ left: `${pos().left}px`, top: `${pos().top}px`, width: `${W}px`, 'max-height': `${MAX_H}px` }}
      onKeyDown={(e) => {
        e.stopPropagation();
        const n = folders().length;
        if (e.key === 'Escape') {
          e.preventDefault();
          close(true);
        } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
          e.preventDefault();
          if (n) setActive((i) => (i + (e.key === 'ArrowDown' ? 1 : -1) + n) % n);
          queueMicrotask(() => el.querySelector('.fs-row.active')?.scrollIntoView({ block: 'nearest' }));
        } else if (e.key === 'Enter') {
          e.preventDefault();
          const x = folders()[active()];
          if (x) pick(x.f);
        }
      }}
    >
      <input
        ref={input}
        class="mp-filter"
        type="text"
        placeholder="Go to folder…"
        aria-label="Filter folders"
        spellcheck={false}
        autocomplete="off"
        value={q()}
        onInput={(e) => setQ(e.currentTarget.value)}
      />
      <div class="mp-list" role="listbox" aria-label="Folders">
        <For each={entries()} fallback={<div class="mp-empty">No matching folder</div>}>
          {(e) =>
            e.kind === 'group' ? (
              <div class="fs-group" title={e.account.email}>
                <AccountDot account={e.account} size={14} />
                <span>{e.account.email}</span>
              </div>
            ) : (
              (() => {
                const idx = () => folders().indexOf(e);
                return (
                  <div
                    class="mp-row fs-row"
                    classList={{ active: active() === idx(), current: e.f.id === selectedFolderId() }}
                    role="option"
                    aria-selected={active() === idx()}
                    title={e.f.path.join(' / ')}
                    onMouseMove={() => setActive(idx())}
                    onClick={() => pick(e.f)}
                  >
                    <span class="mp-icon">
                      <RoleIcon role={e.f.role ?? (isInboxPath(e.f) ? 'inbox' : null)} size={14} />
                    </span>
                    <span class="mp-name">{e.label}</span>
                    <Show when={e.f.unread > 0}>
                      <span class="fs-count">{fmtNum(e.f.unread)}</span>
                    </Show>
                  </div>
                );
              })()
            )
          }
        </For>
      </div>
    </div>
  );
}

export function FolderSwitcherHost() {
  return (
    <Show when={open()}>
      <Portal>
        <Switcher />
      </Portal>
    </Show>
  );
}

/** Backspace in the list: climb to the nearest selectable parent folder. */
export function climbToParent(): boolean {
  const f = folderById(selectedFolderId());
  const p = f ? parentFolder(f) : undefined;
  if (!p) return false;
  selectFolder(p.id);
  return true;
}
