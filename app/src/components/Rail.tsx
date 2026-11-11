import { createMemo, For, Show } from 'solid-js';
import { hint } from '../lib/shortcuts';
import type { Account, Folder } from '../api';
import { fmtNum } from '../lib/format';
import { flyout, setFlyout, toggleSidebar } from '../lib/layout';
import { DRAG } from '../lib/platform';
import { accountMark, accounts, folderById, foldersByAccount, selectedFolderId, selectFolder } from '../state';
import { FoldersIcon, RoleIcon, SidebarIcon } from './Icons';
import { badgeFor, isAccountCollapsed, isInboxPath, roleFolders, SyncFooter, toggleAccount } from './Sidebar';

const compact = (n: number) => (n > 99 ? '99+' : String(n));

function RailFolder(props: { f: Folder }) {
  const n = () => badgeFor(props.f);
  const tip = () => {
    const f = props.f;
    const parts = [f.displayName];
    if (f.unread > 0) parts.push(`${fmtNum(f.unread)} unread`);
    parts.push(`${fmtNum(f.total)} total`);
    return parts.join(' · ');
  };
  return (
    <button
      class="rail-btn"
      classList={{ selected: selectedFolderId() === props.f.id }}
      title={tip()}
      aria-label={tip()}
      aria-current={selectedFolderId() === props.f.id ? 'page' : undefined}
      onClick={() => selectFolder(props.f.id)}
    >
      <RoleIcon role={props.f.role ?? (isInboxPath(props.f) ? 'inbox' : null)} size={17} />
      <Show when={n() > 0}>
        <span class="rail-badge">{compact(n())}</span>
      </Show>
    </button>
  );
}

/** An account: its marker (click to fold it away, as in the sidebar), then its main folders. */
function RailAccount(props: { account: Account }) {
  const roles = createMemo(() => roleFolders(foldersByAccount[props.account.id] ?? []));
  const collapsed = () => isAccountCollapsed(props.account.id);
  // Folded: the Inbox's unread badge moves onto the marker (placed as on the Inbox icon), and
  // the marker is selected if the open folder is in this account.
  const inboxUnread = () => roles().find((f) => f.role === 'inbox' || isInboxPath(f))?.unread ?? 0;
  const holdsSelection = () => folderById(selectedFolderId())?.accountId === props.account.id;
  const tip = () => `${props.account.email}\n${collapsed() ? 'Show' : 'Hide'} its folders`;
  return (
    <div class="rail-acct">
      <button
        class="rail-marker"
        classList={{ selected: collapsed() && holdsSelection() }}
        title={tip()}
        aria-label={`${props.account.email}: ${collapsed() ? 'show' : 'hide'} folders`}
        aria-expanded={!collapsed()}
        onClick={() => toggleAccount(props.account.id)}
      >
        <span class="rail-letter" style={{ background: accountMark(props.account).color }}>
          {accountMark(props.account).letter}
        </span>
        <Show when={collapsed() && inboxUnread() > 0}>
          <span class="rail-badge">{compact(inboxUnread())}</span>
        </Show>
      </button>
      <Show when={!collapsed()}>
        <For each={roles()}>{(f) => <RailFolder f={f} />}</For>
      </Show>
    </div>
  );
}

/**
 * 48px sidebar: per-account role folders with badges, sync glyph at the bottom. The full tree
 * opens only on request: the folders button or Ctrl/⌘+Shift+1.
 */
export function Rail() {
  return (
    <nav class="rail" aria-label="Mailboxes">
      <div class="rail-top" data-tauri-drag-region={DRAG}>
        <button class="rail-btn" title={`Expand sidebar (${hint('rail')})`} aria-label="Expand sidebar" onClick={() => toggleSidebar()}>
          <SidebarIcon size={17} />
        </button>
        <button
          class="rail-btn"
          title={`All folders (${hint('flyout')})`}
          aria-label="Show all folders"
          onClick={(e) => {
            if (flyout()) {
              setFlyout(false);
              return;
            }
            setFlyout(e.detail === 0 ? 'keyboard' : 'click');
          }}
        >
          <FoldersIcon size={17} />
        </button>
      </div>
      <div class="rail-scroll">
        <For each={accounts() ?? []}>{(a) => <RailAccount account={a} />}</For>
      </div>
      <SyncFooter compact />
    </nav>
  );
}
