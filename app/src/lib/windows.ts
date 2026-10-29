import { getCurrentWindow } from '@tauri-apps/api/window';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { emit as tauriEmit, listen as tauriListen } from '@tauri-apps/api/event';
import { api, isTauri, type FileInfo } from '../api';

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

// ------------------------------------------------------------- files to attach

/** The system file picker (several files). */
export async function pickFiles(): Promise<FileInfo[]> {
  if (!isTauri) return [];
  const chosen = await openDialog({ multiple: true, directory: false, title: 'Attach files' });
  const paths = chosen == null ? [] : Array.isArray(chosen) ? chosen : [chosen];
  return paths.length ? api.fileInfo(paths) : [];
}

/** Files dragged over / dropped on this window. `over`: show the drop target. */
export function onFileDrop(over: (on: boolean) => void, drop: (files: FileInfo[]) => void): void {
  if (isTauri) {
    void getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type === 'enter' || e.payload.type === 'over') over(true);
      else if (e.payload.type === 'leave') over(false);
      else if (e.payload.type === 'drop') {
        over(false);
        if (e.payload.paths.length) void api.fileInfo(e.payload.paths).then(drop);
      }
    });
    return;
  }
  // Preview: browsers don't expose paths; stand-ins with the files' names and sizes.
  let depth = 0;
  document.addEventListener('dragenter', (e) => {
    if (!e.dataTransfer?.types.includes('Files')) return;
    depth++;
    over(true);
  });
  document.addEventListener('dragleave', () => {
    depth = Math.max(0, depth - 1);
    if (!depth) over(false);
  });
  document.addEventListener('dragover', (e) => e.preventDefault());
  document.addEventListener('drop', (e) => {
    e.preventDefault();
    depth = 0;
    over(false);
    const files = [...(e.dataTransfer?.files ?? [])].map((f) => ({ path: f.name, name: f.name, size: f.size }));
    if (files.length) drop(files);
  });
}
