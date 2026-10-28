import { createMemo, createRoot, createSignal } from 'solid-js';
import { loadPref, savePref } from './format';

// Sidebar: "full" tree or a 48px "rail". Below RAIL_BELOW px the rail is automatic; a user
// toggle overrides that until the window crosses the threshold again. In wide windows the
// toggle sets the persisted preference.

export type SidebarMode = 'full' | 'rail';
export const RAIL_W = 48;
const RAIL_BELOW = 1100;

const [pref, setPref] = createSignal<SidebarMode>(loadPref<SidebarMode>('ui.sidebarMode', 'full') === 'rail' ? 'rail' : 'full');
const [narrow, setNarrow] = createSignal(typeof window !== 'undefined' && window.innerWidth < RAIL_BELOW);
const [override, setOverride] = createSignal<SidebarMode | null>(null);

if (typeof window !== 'undefined') {
  let t: ReturnType<typeof setTimeout> | undefined;
  window.addEventListener('resize', () => {
    clearTimeout(t);
    // Debounced so dragging the window edge across the threshold doesn't flicker.
    t = setTimeout(() => {
      const n = window.innerWidth < RAIL_BELOW;
      if (n !== narrow()) {
        setOverride(null);
        setNarrow(n);
      }
    }, 150);
  });
}

export const sidebarMode = createRoot(() => createMemo<SidebarMode>(() => override() ?? (narrow() ? 'rail' : pref())));

export function toggleSidebar(): void {
  const next: SidebarMode = sidebarMode() === 'rail' ? 'full' : 'rail';
  setFlyout(false);
  if (narrow()) setOverride(next);
  else {
    setOverride(null);
    setPref(next);
    savePref('ui.sidebarMode', next);
  }
}

/**
 * The full tree slid out over the list while in rail mode. Only ever opened explicitly
 * ('click': the folders button; 'keyboard': Ctrl/⌘+Shift+1, which also moves focus into it),
 * and closed by picking a folder, Esc or a click elsewhere.
 */
const [flyout, setFlyoutSig] = createSignal<false | 'click' | 'keyboard'>(false);
export { flyout };
export function setFlyout(v: false | 'click' | 'keyboard'): void {
  setFlyoutSig(sidebarMode() === 'rail' ? v : false);
}

