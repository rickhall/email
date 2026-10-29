import { createEffect, on, onCleanup, onMount } from 'solid-js';
import { flyout, RAIL_W, setFlyout } from '../lib/layout';
import { folderById, selectedFolderId } from '../state';
import { Sidebar } from './Sidebar';

/**
 * The full sidebar tree slid out over the list (rail mode). Overlay only: never resizes
 * anything. Opened explicitly; closes on Esc, a click outside, or picking a folder.
 */
export function Flyout(props: { width: number; onAddAccount: () => void }) {
  let el!: HTMLDivElement;
  const close = () => setFlyout(false);

  // Picking a folder closes it.
  createEffect(on(selectedFolderId, close, { defer: true }));

  onMount(() => {
    // Open at the account being viewed: its section at the top.
    const acct = folderById(selectedFolderId() ?? -1)?.accountId;
    if (acct != null) {
      queueMicrotask(() => el.querySelector<HTMLElement>(`[data-account-id="${acct}"]`)?.scrollIntoView({ block: 'start' }));
    }
    if (flyout() === 'keyboard') {
      queueMicrotask(() => (el.querySelector<HTMLElement>('.folder.selected') ?? el.querySelector<HTMLElement>('.folder'))?.focus());
    }
    // The rail's own buttons toggle/switch the flyout themselves.
    const inside = (t: EventTarget | null) =>
      t instanceof Element && !!t.closest('.flyout, .rail, .modal-backdrop, .sync-popover, .move-picker, .view-menu');
    const onDown = (e: MouseEvent) => {
      if (!inside(e.target)) close();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || document.querySelector('.modal-backdrop')) return;
      e.preventDefault();
      e.stopPropagation();
      close();
    };
    // Clicks in the email body (an iframe) only show up as the window losing focus.
    const onBlur = () => close();
    window.addEventListener('blur', onBlur);
    document.addEventListener('mousedown', onDown, true);
    window.addEventListener('keydown', onKey, true);
    onCleanup(() => {
      document.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('blur', onBlur);
      window.removeEventListener('keydown', onKey, true);
    });
  });

  return (
    <div class="flyout" ref={el} style={{ left: `${RAIL_W}px`, width: `${props.width}px` }}>
      <Sidebar onAddAccount={props.onAddAccount} flyout />
    </div>
  );
}
