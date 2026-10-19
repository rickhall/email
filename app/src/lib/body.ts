import { createSignal, onCleanup } from 'solid-js';
import { api, errorMessage, events, type MessageDetail } from '../api';

// ------------------------------------------------------------- shared detail cache

const CACHE_MAX = 80;
const cache = new Map<string, MessageDetail>();
const cacheKey = (id: number, remote: boolean) => `${id}:${remote ? 1 : 0}`;

export function cacheGet(id: number, remote: boolean): MessageDetail | undefined {
  const k = cacheKey(id, remote);
  const v = cache.get(k);
  if (v) {
    cache.delete(k);
    cache.set(k, v);
  }
  return v;
}

export function cachePut(d: MessageDetail, remote: boolean): void {
  cache.set(cacheKey(d.id, remote), d);
  while (cache.size > CACHE_MAX) {
    const first = cache.keys().next().value;
    if (first === undefined) break;
    cache.delete(first);
  }
}

/** Mark cached copies as "Load images" remembered. */
export function cacheMarkRemoteAllowed(id: number): void {
  for (const r of [false, true]) {
    const hit = cache.get(cacheKey(id, r));
    if (hit) hit.remoteAllowed = true;
  }
}

// ------------------------------------------------------------- message-ready

/** id -> time a `message-ready` arrived; closes the race where the event beats the null reply. */
export const readyAt = new Map<number, number>();
const readyListeners = new Set<(id: number) => void>();
void events.onMessageReady(({ id }) => {
  readyAt.set(id, performance.now());
  if (readyAt.size > 500) readyAt.clear();
  readyListeners.forEach((fn) => fn(id));
});

/** Subscribe to `message-ready` for the lifetime of the current owner. */
export function onMessageReady(fn: (id: number) => void): void {
  readyListeners.add(fn);
  onCleanup(() => readyListeners.delete(fn));
}

// ------------------------------------------------------------- per-message body loader

export type BodyPhase = 'loading' | 'downloading' | 'ready' | 'error';

/**
 * Loads one message body via get_message with the full null → message-ready dance,
 * the shared cache, and the remembered "Load images" choice. Must run inside an owner.
 */
export function createMessageBody(id: number) {
  const [phase, setPhase] = createSignal<BodyPhase>('loading');
  const [shown, setShown] = createSignal<{ d: MessageDetail; remote: boolean } | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  let wantRemote = false;
  let token = 0;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;

  function show(d: MessageDetail, remote: boolean) {
    // Remembered "Load images" but we asked without: fetch the unstripped version.
    if (d.remoteAllowed && !remote && d.hasRemoteContent) {
      wantRemote = true;
      void load();
      return true;
    }
    setShown({ d, remote: remote || d.remoteAllowed });
    setPhase('ready');
    return false;
  }

  async function load() {
    if (disposed) return;
    const my = ++token;
    const remote = wantRemote;
    clearTimeout(retryTimer);
    setError(null);
    const hit = cacheGet(id, remote);
    if (hit) {
      show(hit, remote);
      return;
    }
    if (phase() !== 'downloading' && !shown()) setPhase('loading');
    const started = performance.now();
    try {
      const d = await api.getMessage(id, remote);
      if (my !== token || disposed) return;
      if (d) {
        cachePut(d, remote);
        show(d, remote);
        return;
      }
      setPhase('downloading');
      const r = readyAt.get(id);
      if (r !== undefined && r >= started) {
        void load();
        return;
      }
      retryTimer = setTimeout(() => {
        if (my === token) void load();
      }, 10_000);
    } catch (e) {
      if (my !== token || disposed) return;
      setError(errorMessage(e));
      setPhase('error');
    }
  }

  onMessageReady((rid) => {
    if (rid === id && phase() === 'downloading') void load();
  });
  onCleanup(() => {
    disposed = true;
    clearTimeout(retryTimer);
  });

  void load();

  return {
    phase,
    shown,
    error,
    retry: () => void load(),
    /** "Load images": show them now and remember the choice for this message. */
    loadImages() {
      cacheMarkRemoteAllowed(id);
      api.setAllowRemote(id, true).catch(() => {});
      wantRemote = true;
      void load();
    },
  };
}
