import { getCurrentWindow } from '@tauri-apps/api/window';
import { emit as tauriEmit, listen as tauriListen } from '@tauri-apps/api/event';
import { api, isTauri } from '../api';

// Windows: the main window, and one compose window per draft ("compose-<draft id>"). In the
// browser preview a compose window is a popup page (?compose=<id>), and windows talk over a
// BroadcastChannel instead of Tauri events.

/** This page's window label: "main" or "compose-<draft id>". */
export const windowLabel: string = isTauri
  ? getCurrentWindow().label
  : (() => {
      const id = new URLSearchParams(location.search).get('compose');
      return id ? `compose-${id}` : 'main';
    })();

/** The draft this window edits (compose windows only). */
export const composeDraftId: number | null = windowLabel.startsWith('compose-') ? Number(windowLabel.slice(8)) : null;
export const isComposeWindow = composeDraftId != null;

/** Open draft `id` in its compose window (or bring that window forward). */
export async function openComposeWindow(id: number): Promise<void> {
  if (isTauri) return api.openCompose(id);
  window.open(`${location.pathname}?compose=${id}`, `compose-${id}`, 'width=760,height=720');
}

export async function closeThisWindow(): Promise<void> {
  // destroy: skips our own close-request handler (the decision has been made).
  if (isTauri) return getCurrentWindow().destroy();
  window.close();
}

export function setWindowTitle(title: string): void {
  if (isTauri) void getCurrentWindow().setTitle(title);
  else document.title = title;
}

/** The window's close button / ⌘W: `ask` decides (true = close now). */
export function onCloseRequested(ask: () => Promise<boolean>): void {
  if (!isTauri) return;
  void getCurrentWindow().onCloseRequested(async (e) => {
    e.preventDefault();
    if (await ask()) await getCurrentWindow().destroy();
  });
}

// ------------------------------------------------------------- between windows

/** Window-to-window events (not from the core). */
export interface WindowEvents {
  /** a compose window queued a message: the main window shows "Sending… Undo" */
  'compose-sent': { id: number; sendAt: number };
}

const channel = !isTauri && typeof BroadcastChannel !== 'undefined' ? new BroadcastChannel('email-windows') : null;

export function broadcast<K extends keyof WindowEvents>(event: K, payload: WindowEvents[K]): void {
  if (isTauri) void tauriEmit(event, payload);
  else channel?.postMessage({ event, payload });
}

export function onBroadcast<K extends keyof WindowEvents>(event: K, cb: (p: WindowEvents[K]) => void): void {
  if (isTauri) {
    void tauriListen<WindowEvents[K]>(event, (e) => cb(e.payload));
    return;
  }
  channel?.addEventListener('message', (m: MessageEvent<{ event: string; payload: WindowEvents[K] }>) => {
    if (m.data?.event === event) cb(m.data.payload);
  });
}
