import { createMemo, For, Show } from 'solid-js';
import type { Account, Folder } from '../api';
import { avatarColor, fmtNum } from '../lib/format';
import { flyout, setFlyout, toggleSidebar } from '../lib/layout';
import { DRAG } from '../lib/platform';
import { accounts, foldersByAccount, selectedFolderId, selectFolder } from '../state';
import { FoldersIcon, RoleIcon, SidebarIcon } from './Icons';
import { badgeFor, isInboxPath, roleFolders, SyncFooter } from './Sidebar';

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

function RailAccount(props: { account: Account }) {
  const roles = createMemo(() => roleFolders(foldersByAccount[props.account.id] ?? []));
  const letter = () => (props.account.email[0] ?? '?').toUpperCase();
  return (
    <div class="rail-acct">
      <div class="rail-marker" title={props.account.email} aria-label={props.account.email}>
        <span style={{ background: avatarColor(props.account.email) }}>{letter()}</span>
      </div>
      <For each={roles()}>{(f) => <RailFolder f={f} />}</For>
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
        <button class="rail-btn" title="Expand sidebar (Ctrl+1)" aria-label="Expand sidebar" onClick={() => toggleSidebar()}>
          <SidebarIcon size={17} />
        </button>
        <button
          class="rail-btn"
          title="All folders (Ctrl+Shift+1)"
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
