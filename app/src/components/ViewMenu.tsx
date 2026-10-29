import { createEffect, createSignal, onCleanup, Show } from 'solid-js';
import { Portal } from 'solid-js/web';
import {
  conversationScope,
  includeTrash,
  setConversationScope,
  setIncludeTrash,
  setThreaded,
  threaded,
} from '../state';
import { CheckIcon, SlidersIcon } from './Icons';

// Module-level so the menu stays open while the list remounts on "Group by conversation".
const [open, setOpen] = createSignal(false);
const [pos, setPos] = createSignal<{ top: number; right: number }>({ top: 0, right: 0 });

function place() {
  const btn = document.querySelector<HTMLElement>('.view-menu-btn');
  if (!btn) return;
  const r = btn.getBoundingClientRect();
  setPos({ top: r.bottom + 6, right: Math.max(8, window.innerWidth - r.right) });
}

/** "View" menu: app-wide list/conversation display settings. Overlay only — no layout shift. */
export function ViewMenuButton() {
  return (
    <button
      class="icon-btn toggle view-menu-btn"
      classList={{ open: open() }}
      aria-haspopup="menu"
      aria-expanded={open()}
      title="View options"
      aria-label="View options"
      onClick={() => {
        place();
        setOpen((v) => !v);
      }}
    >
      <SlidersIcon />
    </button>
  );
}

function Check(props: { on: boolean; radio?: boolean }) {
  return (
    <span class="vm-check" classList={{ on: props.on, radio: !!props.radio }} aria-hidden="true">
      <Show when={props.on}>{props.radio ? <span class="vm-dot" /> : <CheckIcon size={11} />}</Show>
    </span>
  );
}

export function ViewMenuPopover() {
  let el: HTMLDivElement | undefined;
  createEffect(() => {
    if (!open()) return;
    place();
    const onDown = (e: MouseEvent) => {
      const t = e.target as Element;
      if (el?.contains(t) || t.closest?.('.view-menu-btn')) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
    };
    window.addEventListener('resize', place);
    // Clicks in the email body (an iframe) only show up as the window losing focus.
    const onBlur = () => setOpen(false);
    window.addEventListener('blur', onBlur);
    document.addEventListener('mousedown', onDown, true);
    window.addEventListener('keydown', onKey, true);
    onCleanup(() => {
      window.removeEventListener('resize', place);
      document.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('blur', onBlur);
      window.removeEventListener('keydown', onKey, true);
    });
  });

  return (
    <Show when={open()}>
      <Portal>
        <div
          ref={el}
          class="view-menu"
          role="menu"
          aria-label="View options"
          style={{ top: `${pos().top}px`, right: `${pos().right}px` }}
        >
          <button
            role="menuitemcheckbox"
            aria-checked={threaded()}
            class="vm-item"
            onClick={() => setThreaded(!threaded())}
          >
            <Check on={threaded()} />
            <span class="vm-label">Group by conversation</span>
            <kbd>T</kbd>
          </button>
          <div class="vm-sep" />
          <div class="vm-section">Conversations show</div>
          <button
            role="menuitemradio"
            aria-checked={conversationScope() === 'folder'}
            class="vm-item"
            onClick={() => setConversationScope('folder')}
          >
            <Check on={conversationScope() === 'folder'} radio />
            <span class="vm-label">This folder</span>
            <kbd>A</kbd>
          </button>
          <button
            role="menuitemradio"
            aria-checked={conversationScope() === 'all'}
            class="vm-item"
            onClick={() => setConversationScope('all')}
          >
            <Check on={conversationScope() === 'all'} radio />
            <span class="vm-label">All mail</span>
          </button>
          <button
            role="menuitemcheckbox"
            aria-checked={includeTrash()}
            class="vm-item"
            title="Also show messages from conversations that are in Trash or Spam"
            onClick={() => setIncludeTrash(!includeTrash())}
          >
            <Check on={includeTrash()} />
            <span class="vm-label">Include Trash</span>
          </button>
        </div>
      </Portal>
    </Show>
  );
}
