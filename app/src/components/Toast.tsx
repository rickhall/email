import { createSignal, Show } from 'solid-js';
import { api, errorMessage } from '../api';
import { CloseIcon } from './Icons';

// One toast at a time, overlaid at the bottom of the reader pane (never moves content).

export interface ToastSpec {
  kind: 'ok' | 'error';
  text: string;
  /** "Show in folder" for a saved file/folder */
  path?: string;
  /** an inline action, e.g. Undo */
  action?: { label: string; run: () => void };
  /** auto-hide after (default 5 s, errors 8 s) */
  ms?: number;
}
interface Toast extends ToastSpec {
  id: number;
}

const [toast, setToast] = createSignal<Toast | null>(null);
let toastSeq = 0;
let toastTimer: ReturnType<typeof setTimeout> | undefined;

/** Show a toast; returns its id (for hideToast). */
export function showToast(t: ToastSpec): number {
  clearTimeout(toastTimer);
  const id = ++toastSeq;
  setToast({ ...t, id });
  toastTimer = setTimeout(
    () => {
      if (toast()?.id === id) setToast(null);
    },
    t.ms ?? (t.kind === 'error' ? 8000 : 5000),
  );
  return id;
}

export function hideToast(id?: number): void {
  if (id == null || toast()?.id === id) setToast(null);
}

export function ToastHost() {
  return (
    <div class="toast-host" aria-live="polite">
      <Show when={toast()} keyed>
        {(t) => (
          <div class="toast" classList={{ error: t.kind === 'error' }} role={t.kind === 'error' ? 'alert' : 'status'}>
            <span class="toast-text" title={t.text}>
              {t.text}
            </span>
            <Show when={t.path}>
              {(p) => (
                <>
                  <span class="dot-sep">·</span>
                  <button
                    class="link-btn toast-action"
                    onClick={() => api.revealPath(p()).catch((e) => showToast({ kind: 'error', text: errorMessage(e) }))}
                  >
                    Show in folder
                  </button>
                </>
              )}
            </Show>
            <Show when={t.action}>
              {(a) => (
                <>
                  <span class="dot-sep">·</span>
                  <button
                    class="link-btn toast-action"
                    onClick={() => {
                      hideToast(t.id);
                      a().run();
                    }}
                  >
                    {a().label}
                  </button>
                </>
              )}
            </Show>
            <button class="toast-close" aria-label="Dismiss" onClick={() => setToast(null)}>
              <CloseIcon size={10} />
            </button>
          </div>
        )}
      </Show>
    </div>
  );
}
