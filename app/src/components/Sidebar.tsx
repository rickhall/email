import { createEffect, createMemo, createSignal, For, Match, onCleanup, Show, Switch, untrack } from 'solid-js';
import { createStore } from 'solid-js/store';
import { api, type Account, type Folder, type FolderRole, type SyncStatus } from '../api';
import { foldersByAccount, selectedFolderId, selectFolder, syncByAccount, accounts } from '../state';
import { fmtNum, loadPref, savePref } from '../lib/format';
import { AlertIcon, CheckIcon, ChevronIcon, FolderIcon, LockIcon, PlusIcon, RoleIcon } from './Icons';
import { PasswordDialog } from './PasswordPrompt';
import { DRAG } from '../lib/platform';

const ROLE_ORDER: Exclude<FolderRole, null>[] = [
  'inbox',
  'flagged',
  'important',
  'sent',
  'drafts',
  'archive',
  'all',
  'junk',
  'trash',
];

/** Roles whose unread count isn't interesting. Drafts show their total instead. */
const NO_BADGE = new Set<FolderRole>(['sent', 'trash', 'junk', 'all', 'archive', 'important']);

interface TreeNode {
  key: string;
  name: string;
  depth: number;
  folder: Folder | null;
  children: TreeNode[];
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

function isInboxPath(f: Folder) {
  return f.path.length === 1 && f.path[0]?.toUpperCase() === 'INBOX';
}

function buildTree(accountId: number, folders: Folder[]): TreeNode[] {
  const root: TreeNode = { key: '', name: '', depth: -1, folder: null, children: [] };
  const index = new Map<string, TreeNode>();
  for (const f of folders) {
    if (f.role || isInboxPath(f)) continue;
    let parent = root;
    f.path.forEach((seg, i) => {
      const key = `${accountId}/${f.path.slice(0, i + 1).join('\u0001')}`;
      let node = index.get(key);
      if (!node) {
        node = { key, name: seg, depth: i, folder: null, children: [] };
        index.set(key, node);
        parent.children.push(node);
      }
      if (i === f.path.length - 1) {
        node.folder = f;
        if (f.displayName) node.name = f.displayName;
      }
      parent = node;
    });
  }
  // Containers with nothing selectable underneath (e.g. "[Gmail]" whose children are all role folders) vanish.
  const prune = (nodes: TreeNode[]): TreeNode[] =>
    nodes
      .map((n) => ({ ...n, children: prune(n.children) }))
      .filter((n) => n.children.length > 0 || (n.folder?.selectable ?? false))
      .sort((a, b) => collator.compare(a.name, b.name));
  return prune(root.children);
}

function roleFolders(folders: Folder[]): Folder[] {
  return folders
    .filter((f) => f.selectable && (f.role || isInboxPath(f)))
    .sort((a, b) => {
      const ra = ROLE_ORDER.indexOf(a.role ?? 'inbox');
      const rb = ROLE_ORDER.indexOf(b.role ?? 'inbox');
      return ra - rb || collator.compare(a.displayName, b.displayName);
    });
}

// Persisted expand/collapse state.
const [ui, setUi] = createStore<{ collapsedAccounts: Record<number, boolean>; expanded: Record<string, boolean> }>({
  collapsedAccounts: loadPref('ui.collapsedAccounts', {}),
  expanded: loadPref('ui.expanded', {}),
});

function toggleAccount(id: number) {
  setUi('collapsedAccounts', id, (v) => !v);
  savePref('ui.collapsedAccounts', ui.collapsedAccounts);
}
function isExpanded(n: TreeNode) {
  return ui.expanded[n.key] ?? n.depth === 0;
}
function toggleNode(n: TreeNode) {
  setUi('expanded', n.key, !isExpanded(n));
  savePref('ui.expanded', ui.expanded);
}

function badgeFor(f: Folder): number {
  if (f.role === 'drafts') return f.total;
  if (NO_BADGE.has(f.role)) return 0;
  return f.unread;
}

const ACTIVE_PHASES = new Set(['connecting', 'folders', 'headers', 'bodies']);

function syncLabel(s: SyncStatus): string {
  switch (s.phase) {
    case 'connecting':
      return s.message || 'Connecting…';
    case 'folders':
      return 'Updating folders…';
    case 'headers':
      return s.total > 0 ? `Syncing ${fmtNum(s.done)} / ${fmtNum(s.total)}` : 'Syncing…';
    case 'bodies':
      return s.total > 0 ? `Downloading ${fmtNum(s.done)} / ${fmtNum(s.total)}` : 'Downloading…';
    case 'error':
      return 'Sync error';
    case 'auth':
      return 'Password needed';
    case 'limited':
      return 'Connection limit · retrying';
    default:
      return 'Up to date';
  }
}

// ------------------------------------------------------------- app version (fetched once)

const [appVersion, setAppVersion] = createSignal<string | null>(null);
api
  .appVersion()
  .then(setAppVersion)
  .catch(() => {
    /* older core: no version line */
  });

// ------------------------------------------------------------- status footer

/**
 * Follow a changing value but emit at most once per `ms` (leading + trailing), so
 * progress digits don't flutter. Must run inside an owner.
 */
function throttled<T>(src: () => T, ms: number): () => T {
  const [v, setV] = createSignal<T>(untrack(src));
  let last = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let latest: T;
  createEffect(() => {
    latest = src();
    const wait = last + ms - performance.now();
    if (wait <= 0) {
      last = performance.now();
      setV(() => latest);
    } else if (!timer) {
      timer = setTimeout(() => {
        timer = undefined;
        last = performance.now();
        setV(() => latest);
      }, wait);
    }
  });
  onCleanup(() => clearTimeout(timer));
  return v;
}

const isAttention = (p: string | undefined) => p === 'error' || p === 'auth';
/** Temporary server refusal (connection limit): shown as a warning, resolves on its own. */
const isWarning = (p: string | undefined) => p === 'limited';

/** Fixed-size 14px status glyph shared by the summary and the popover rows. */
function StatusGlyph(props: { phase: string }) {
  return (
    <span class="sf-icon" aria-hidden="true">
      <Switch fallback={<CheckIcon size={12} />}>
        <Match when={ACTIVE_PHASES.has(props.phase)}>
          <span class="spinner small" />
        </Match>
        <Match when={props.phase === 'error' || props.phase === 'limited'}>
          <AlertIcon size={13} />
        </Match>
        <Match when={props.phase === 'auth'}>
          <LockIcon size={12} />
        </Match>
      </Switch>
    </span>
  );
}

function ProgressTrack(props: { active: boolean; pct: number | null }) {
  return (
    <span class="sf-track" classList={{ active: props.active }} aria-hidden="true">
      <span
        class="sf-fill"
        classList={{ shown: props.active, indeterminate: props.active && props.pct == null }}
        style={{ transform: props.pct == null ? undefined : `scaleX(${props.pct})` }}
      />
    </span>
  );
}

/** One account in the status popover: fixed-height row (+ the error text when failing). */
function SyncPopoverRow(props: { account: Account; status: SyncStatus | undefined; onFix: () => void }) {
  const phase = () => props.status?.phase ?? 'idle';
  const active = () => ACTIVE_PHASES.has(phase());
  const pct = () => {
    const s = props.status;
    if (!s || !active() || s.total <= 0) return null;
    return Math.max(0, Math.min(1, s.done / s.total));
  };
  return (
    <div class="sp-item">
      <div class="sf-row sp-row" classList={{ error: isAttention(phase()), warn: isWarning(phase()), active: active() }}>
        <StatusGlyph phase={phase()} />
        <span class="sf-email" title={props.account.email}>
          {props.account.email}
        </span>
        <span class="sf-status sp-status">
          <span class="sf-text">{props.status ? syncLabel(props.status) : 'Up to date'}</span>
          <Show when={phase() === 'auth'}>
            <button class="link-btn sf-fix" onClick={() => props.onFix()}>
              Fix…
            </button>
          </Show>
        </span>
        <ProgressTrack active={active()} pct={pct()} />
      </div>
      <Show when={(phase() === 'error' || phase() === 'limited') && props.status?.message}>
        <div class="sp-error" classList={{ warn: phase() === 'limited' }}>{props.status?.message}</div>
      </Show>
    </div>
  );
}

/**
 * One fixed-height summary row pinned to the sidebar bottom; clicking it opens a popover
 * (overlay, never pushes layout) with per-account details.
 */
function SyncFooter() {
  const [open, setOpen] = createSignal(false);
  const [fixFor, setFixFor] = createSignal<Account | null>(null);
  let rowEl!: HTMLButtonElement;
  let popEl: HTMLDivElement | undefined;

  // Snapshot of every account's status, updated at most twice a second.
  const snap = throttled(
    () => (accounts() ?? []).map((a) => ({ account: a, status: syncByAccount[a.id] ? { ...syncByAccount[a.id]! } : undefined })),
    500,
  );

  const summary = createMemo(() => {
    const list = snap();
    const attention = list.filter((x) => isAttention(x.status?.phase)).length;
    const activeList = list.filter((x) => x.status && ACTIVE_PHASES.has(x.status.phase));
    if (attention > 0) {
      return {
        phase: list.find((x) => x.status?.phase === 'auth') ? 'auth' : 'error',
        text: attention === 1 ? '1 account needs attention' : `${attention} accounts need attention`,
        active: false,
        pct: null as number | null,
      };
    }
    const limited = list.filter((x) => isWarning(x.status?.phase)).length;
    if (limited > 0) {
      return {
        phase: 'limited',
        text: limited === 1 ? 'Connection limit · retrying' : `${limited} accounts at connection limit`,
        active: false,
        pct: null as number | null,
      };
    }
    if (activeList.length === 0) return { phase: 'idle', text: 'All accounts up to date', active: false, pct: null };
    const counted = activeList.filter((x) => (x.status!.phase === 'headers' || x.status!.phase === 'bodies') && x.status!.total > 0);
    const done = counted.reduce((n, x) => n + x.status!.done, 0);
    const total = counted.reduce((n, x) => n + x.status!.total, 0);
    const pct = total > 0 ? Math.max(0, Math.min(1, done / total)) : null;
    let text: string;
    if (activeList.length === 1) {
      const st = activeList[0]!.status!;
      text = st.phase === 'connecting' || st.phase === 'folders' ? 'Updating…' : syncLabel(st);
    } else if (total > 0) {
      text = `Syncing ${activeList.length} accounts · ${fmtNum(done)} / ${fmtNum(total)}`;
    } else {
      text = 'Updating…';
    }
    return { phase: 'headers', text, active: true, pct };
  });

  // Close on Esc / outside click while open.
  createEffect(() => {
    if (!open()) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (popEl?.contains(t) || rowEl.contains(t)) return;
      if ((t as Element).closest?.('.modal-backdrop')) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || fixFor()) return;
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
      rowEl.focus();
    };
    document.addEventListener('mousedown', onDown, true);
    window.addEventListener('keydown', onKey, true);
    onCleanup(() => {
      document.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('keydown', onKey, true);
    });
  });

  return (
    <footer class="sync-footer">
      <Show when={open()}>
        <div class="sync-popover" ref={popEl} role="dialog" aria-label="Account sync status">
          <For each={snap()}>
            {(x) => <SyncPopoverRow account={x.account} status={x.status} onFix={() => setFixFor(x.account)} />}
          </For>
          <div class="sp-version">{appVersion() ? `Email ${appVersion()}` : '\u00a0'}</div>
        </div>
      </Show>
      <button
        ref={rowEl}
        class="sf-row sf-summary"
        classList={{ error: isAttention(summary().phase), warn: isWarning(summary().phase), active: summary().active, open: open() }}
        aria-expanded={open()}
        aria-label={`Sync status: ${summary().text}`}
        title={summary().text}
        onClick={() => setOpen((v) => !v)}
      >
        <StatusGlyph phase={summary().phase} />
        <span class="sf-text sf-summary-text">{summary().text}</span>
        <span class="sf-chevron" aria-hidden="true">
          <ChevronIcon size={12} />
        </span>
        <ProgressTrack active={summary().active} pct={summary().pct} />
      </button>
      <Show when={fixFor()}>{(a) => <PasswordDialog account={a()} onClose={() => setFixFor(null)} />}</Show>
    </footer>
  );
}

function Badge(props: { n: number }) {
  return (
    <Show when={props.n > 0}>
      <span class="badge">{props.n > 99_999 ? '99k+' : fmtNum(props.n)}</span>
    </Show>
  );
}

function FolderRow(props: { folder: Folder; depth: number; role?: boolean }) {
  const selected = () => selectedFolderId() === props.folder.id;
  return (
    <button
      class="folder"
      classList={{ selected: selected(), role: !!props.role }}
      style={{ 'padding-left': `${12 + props.depth * 14 + 18}px` }}
      onClick={() => selectFolder(props.folder.id)}
      title={props.folder.path.join(' / ')}
    >
      <span class="folder-icon">
        <RoleIcon role={props.folder.role ?? (isInboxPath(props.folder) ? 'inbox' : null)} />
      </span>
      <span class="folder-name">{props.folder.displayName}</span>
      <Badge n={badgeFor(props.folder)} />
    </button>
  );
}

function TreeItem(props: { node: TreeNode }) {
  const n = () => props.node;
  const hasKids = () => n().children.length > 0;
  const selectable = () => n().folder?.selectable ?? false;
  const selected = () => selectable() && selectedFolderId() === n().folder?.id;
  const open = () => isExpanded(n());
  return (
    <>
      <div
        class="folder"
        role="treeitem"
        tabIndex={0}
        aria-expanded={hasKids() ? open() : undefined}
        aria-selected={selected()}
        classList={{ selected: selected(), container: !selectable() }}
        style={{ 'padding-left': `${12 + n().depth * 14}px` }}
        onClick={() => {
          const f = n().folder;
          if (f && f.selectable) selectFolder(f.id);
          else if (hasKids()) toggleNode(n());
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            e.currentTarget.click();
          } else if (e.key === 'ArrowRight' && hasKids() && !open()) toggleNode(n());
          else if (e.key === 'ArrowLeft' && hasKids() && open()) toggleNode(n());
        }}
        title={n().folder?.path.join(' / ') ?? n().name}
      >
        <span
          class="disclosure"
          classList={{ open: open(), hidden: !hasKids() }}
          onClick={(e) => {
            e.stopPropagation();
            if (hasKids()) toggleNode(n());
          }}
        >
          <ChevronIcon size={12} />
        </span>
        <span class="folder-icon">
          <FolderIcon />
        </span>
        <span class="folder-name">{n().name}</span>
        <Show when={n().folder}>{(f) => <Badge n={badgeFor(f())} />}</Show>
      </div>
      <Show when={hasKids() && open()}>
        <div role="group">
          <For each={n().children}>{(c) => <TreeItem node={c} />}</For>
        </div>
      </Show>
    </>
  );
}

function AccountSection(props: { account: Account }) {
  const folders = () => foldersByAccount[props.account.id] ?? [];
  // Rebuild structure only when the folder *shape* changes; counts update in place
  // because nodes hold the (reconciled, stable) store proxies.
  const shape = createMemo(() =>
    folders()
      .map((f) => `${f.id}|${f.path.join('\u0001')}|${f.selectable}|${f.role}|${f.displayName}`)
      .join('\n'),
  );
  const roles = createMemo(() => {
    shape();
    return untrack(() => roleFolders(folders()));
  });
  const tree = createMemo(() => {
    shape();
    return untrack(() => buildTree(props.account.id, folders()));
  });
  const collapsed = () => !!ui.collapsedAccounts[props.account.id];
  const status = () => syncByAccount[props.account.id];
  const totalUnread = createMemo(() => roles().find((f) => f.role === 'inbox' || isInboxPath(f))?.unread ?? 0);
  return (
    <section class="account">
      <button
        class="account-head"
        onClick={() => toggleAccount(props.account.id)}
        aria-expanded={!collapsed()}
        title={props.account.displayName && props.account.displayName !== props.account.email ? `${props.account.displayName} <${props.account.email}>` : props.account.email}
      >
        <span class="disclosure" classList={{ open: !collapsed() }}>
          <ChevronIcon size={12} />
        </span>
        <span class="account-name">{props.account.email}</span>
        {/* Always-reserved slot so the header never shifts when a problem appears. */}
        <span class="acct-flag" aria-hidden={!isAttention(status()?.phase)}>
          <Show when={status()?.phase === 'auth'}>
            <LockIcon size={12} />
          </Show>
          <Show when={status()?.phase === 'error'}>
            <AlertIcon size={12} />
          </Show>
          <Show when={status()?.phase === 'limited'}>
            <span class="acct-flag-warn" title="Connection limit reached · retrying">
              <AlertIcon size={12} />
            </span>
          </Show>
        </span>
        <Show when={collapsed() && totalUnread() > 0}>
          <span class="badge subtle">{fmtNum(totalUnread())}</span>
        </Show>
      </button>
      <Show when={!collapsed()}>
        <div class="folders" role="tree">
          <For each={roles()}>{(f) => <FolderRow folder={f} depth={0} role />}</For>
          <Show when={tree().length > 0}>
            <For each={tree()}>{(n) => <TreeItem node={n} />}</For>
          </Show>
        </div>
      </Show>
    </section>
  );
}

export function Sidebar(props: { onAddAccount: () => void }) {
  return (
    <nav class="sidebar" aria-label="Mailboxes">
      <div class="sidebar-top" data-tauri-drag-region={DRAG}>
        <span class="sidebar-title" data-tauri-drag-region={DRAG}>
          Mailboxes
        </span>
        <button class="icon-btn" onClick={props.onAddAccount} title="Add account" aria-label="Add account">
          <PlusIcon />
        </button>
      </div>
      <div class="sidebar-scroll">
        <For each={accounts() ?? []}>{(a) => <AccountSection account={a} />}</For>
      </div>
      <SyncFooter />
    </nav>
  );
}
