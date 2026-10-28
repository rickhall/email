import { batch, onCleanup } from 'solid-js';
import { createStore, produce, reconcile } from 'solid-js/store';
import { api, events, type MessageSummary } from '../api';

export const PAGE_SIZE = 200;

interface ListState<T> {
  /** null until the first page has answered. */
  total: number | null;
  pages: Record<number, T[] | undefined>;
  error: string | null;
  /** A reset() is in flight: the rows on screen belong to the previous source. */
  pending: boolean;
}

export interface MessageListHooks {
  /** After any page lands in the store. */
  onPageLoaded?: () => void;
  /** Right before a `messages-changed` invalidation (capture scroll anchors here). */
  onInvalidate?: () => void;
  /** Right after a reset() result has been applied (same task, before paint). */
  onReset?: () => void;
}

export type PageFetcher<T> = (offset: number, limit: number) => Promise<{ total: number; items: T[] }>;

/**
 * Lazily paged, offset-keyed list (messages or threads) for one folder.
 * Must be created inside a reactive owner (component) — it subscribes to
 * `messages-changed` and cleans up with the owner.
 */
export function createPagedList<T extends { id: number }>(
  folderId: number | null,
  initialFetcher: PageFetcher<T>,
  hooks: MessageListHooks = {},
) {
  const [state, setState] = createStore<ListState<T>>({ total: null, pages: {}, error: null, pending: false });
  let fetcher = initialFetcher;
  /** true while a reset() is waiting for its first page; old rows stay on screen meanwhile */
  let resetting = false;
  /** generation bumps on invalidate so late responses from before are dropped */
  let gen = 0;
  const inflight = new Map<number, number>();
  let visStart = 0;
  let visEnd = 0;
  let disposed = false;

  const pageOf = (i: number) => Math.floor(i / PAGE_SIZE);

  async function fetchPage(p: number) {
    const g = gen;
    inflight.set(p, g);
    try {
      const res = await fetcher(p * PAGE_SIZE, PAGE_SIZE);
      if (disposed || g !== gen) return;
      batch(() => {
        setState('error', null);
        if (res.total !== state.total) {
          setState('total', res.total);
          // Drop pages that now lie past the end.
          const maxPage = pageOf(Math.max(0, res.total - 1));
          setState(
            'pages',
            produce((pages) => {
              for (const k of Object.keys(pages)) if (Number(k) > maxPage) delete pages[Number(k)];
            }),
          );
        }
        setState('pages', p, reconcile(res.items, { key: 'id' }) as never);
      });
      hooks.onPageLoaded?.();
    } catch (e) {
      if (!disposed && g === gen) {
        console.error('list_messages failed', e);
        setState('error', typeof e === 'string' ? e : 'Could not load messages');
      }
    } finally {
      if (inflight.get(p) === g) inflight.delete(p);
    }
  }

  function ensure(p: number) {
    if (p < 0 || resetting) return;
    if (state.total != null && p * PAGE_SIZE >= Math.max(state.total, 1)) return;
    if (state.pages[p] !== undefined || inflight.has(p)) return;
    void fetchPage(p);
  }

  function visiblePages(): number[] {
    const out: number[] = [];
    for (let p = pageOf(visStart); p <= pageOf(visEnd); p++) out.push(p);
    return out;
  }

  /** Called by the view whenever the virtualizer's rendered range changes. */
  function setVisibleRange(start: number, end: number) {
    visStart = Math.max(0, start);
    visEnd = Math.max(visStart, end);
    for (const p of visiblePages()) ensure(p);
  }

  /** Data changed underneath us: refetch what's on screen, forget the rest. Keeps stale rows visible meanwhile. */
  function invalidate() {
    if (resetting) {
      reset(fetcher); // the pending reset would be stale; restart it
      return;
    }
    hooks.onInvalidate?.();
    gen++;
    inflight.clear();
    const keep = new Set(visiblePages());
    setState(
      'pages',
      produce((pages) => {
        for (const k of Object.keys(pages)) if (!keep.has(Number(k))) delete pages[Number(k)];
      }),
    );
    for (const p of keep) void fetchPage(p);
  }

  /**
   * Switch to a new source (e.g. a new search query) without flicker: keep showing the
   * current rows and total until the new first page arrives, then swap everything in one
   * batch. Older responses are dropped.
   */
  function reset(next: PageFetcher<T>) {
    fetcher = next;
    gen++;
    inflight.clear();
    resetting = true;
    setState('pending', true);
    const g = gen;
    next(0, PAGE_SIZE).then(
      (res) => {
        if (disposed || g !== gen) return;
        resetting = false;
        batch(() => {
          setState('pages', reconcile({ 0: res.items } as Record<number, T[] | undefined>, { key: 'id' }));
          setState('total', res.total);
          setState('error', null);
          setState('pending', false);
          hooks.onReset?.();
        });
      },
      (e) => {
        if (disposed || g !== gen) return;
        resetting = false;
        console.error('list reset failed', e);
        batch(() => {
          setState('error', typeof e === 'string' ? e : 'Could not load messages');
          setState('pending', false);
        });
      },
    );
  }

  function row(i: number): T | undefined {
    return state.pages[pageOf(i)]?.[i % PAGE_SIZE];
  }

  /** Index of a loaded message, or -1. Untracked-ish linear scan over loaded pages only. */
  function indexOfId(id: number): number {
    for (const [k, page] of Object.entries(state.pages)) {
      if (!page) continue;
      const j = page.findIndex((m) => m.id === id);
      if (j >= 0) return Number(k) * PAGE_SIZE + j;
    }
    return -1;
  }

  /**
   * Index of `id`, loading pages in order (up to `maxPages`) until it's found; -1 if not.
   * Used to restore a folder's remembered selection deep in the list.
   */
  async function locate(id: number, maxPages: number): Promise<number> {
    for (let p = 0; p < maxPages; p++) {
      const k = indexOfId(id);
      if (k >= 0) return k;
      if (disposed) return -1;
      if (state.total != null && p * PAGE_SIZE >= state.total) return -1;
      if (state.pages[p] === undefined) {
        await fetchPage(p);
        if (state.pages[p] === undefined) return indexOfId(id); // superseded or failed
      }
    }
    return indexOfId(id);
  }

  /** The row at `i`, fetching just its page if needed (undefined if gone/superseded). */
  async function rowAt(i: number): Promise<T | undefined> {
    const p = pageOf(i);
    if (state.pages[p] === undefined && !disposed) await fetchPage(p);
    return row(i);
  }

  const unlistenP = events.onMessagesChanged(({ folderIds }) => {
    // folderId null = results span folders (search): any change may affect them.
    if (folderId == null || folderIds.includes(folderId)) invalidate();
  });
  onCleanup(() => {
    disposed = true;
    void unlistenP.then((un) => un());
  });

  // Kick off the first page immediately.
  ensure(0);

  return {
    state,
    row,
    indexOfId,
    setVisibleRange,
    invalidate,
    reset,
    locate,
    rowAt,
  };
}

export function createMessageList(folderId: number, hooks: MessageListHooks = {}) {
  return createPagedList<MessageSummary>(
    folderId,
    (offset, limit) => api.listMessages(folderId, offset, limit).then((r) => ({ total: r.total, items: r.messages })),
    hooks,
  );
}


export type MessageListModel = ReturnType<typeof createMessageList>;
