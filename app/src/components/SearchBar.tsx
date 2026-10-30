import { createSignal, For, onCleanup, onMount, Show } from 'solid-js';
import { Portal } from 'solid-js/web';
import { placeNear, useDismiss } from './Actions';
import { openShortcuts } from './ShortcutsSheet';
import { commandFor, hint } from '../lib/shortcuts';
import { newMessage } from '../lib/compose';
import { searchActive, searchQuery, setSearchQuery } from '../state';
import { CloseIcon, ComposeIcon, HelpIcon, SearchIcon } from './Icons';
import { DRAG } from '../lib/platform';

function isTypingTarget(t: EventTarget | null): boolean {
  if (!(t instanceof HTMLElement)) return false;
  return t.isContentEditable || t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT';
}

/** Move keyboard focus into the visible list (results while searching, else the folder). */
function focusList(selectFirst: boolean) {
  const el = document.querySelector<HTMLElement>(
    searchActive() ? '.search-layer .list-scroll' : '.folder-layer .list-scroll',
  );
  el?.focus({ preventScroll: true });
  if (selectFirst && el && !el.querySelector('.row.selected')) {
    // ↓ is a fixed key for "next message" (j may have been remapped)
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown' }));
  }
}

/** Focus the quick-search field (shortcut `/`, ⌘F, macOS menu "Find"). */
export function focusSearch() {
  const el = document.querySelector<HTMLInputElement>('.search-field input');
  el?.focus();
  el?.select();
}

// ------------------------------------------------------------- search help

const EXAMPLES: [string, string][] = [
  ['ada bob', 'both words, anywhere'],
  ['"weekly report"', 'exact phrase'],
  ['-draft', 'exclude a word'],
  ['from:ada', 'sender only'],
  ['to:bob@corp', 'recipients only'],
  ['subject:invoice', 'subject only'],
  ['tag:work', 'tagged Work'],
  ['tag:"to do"', 'tag with spaces'],
];
const HELP_W = 340;

function SearchHelp(props: { anchor: HTMLElement; onPick: (q: string) => void; onClose: (refocus: boolean) => void }) {
  let el: HTMLDivElement | undefined;
  const place = () => {
    const r = props.anchor.getBoundingClientRect();
    const p = placeNear(props.anchor, HELP_W, el?.offsetHeight || 380);
    return { ...p, left: Math.max(8, Math.min(window.innerWidth - HELP_W - 8, r.left)) };
  };
  const [pos, setPos] = createSignal(place());
  useDismiss(() => el, props.anchor, () => props.onClose(false), () => setPos(place()));
  onMount(() => queueMicrotask(() => setPos(place())));
  return (
    <Portal>
      <div
        ref={el}
        class="search-help"
        role="dialog"
        aria-label="Search help"
        style={{ left: `${pos().left}px`, top: `${pos().top}px`, width: `${HELP_W}px` }}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === 'Escape') {
            e.preventDefault();
            props.onClose(true);
          }
        }}
      >
        <div class="sh-title">Search syntax</div>
        <div class="sh-table">
          <For each={EXAMPLES}>
            {([q, d]) => (
              <>
                <button class="sh-ex" title="Insert into the search field" onClick={() => props.onPick(q)}>
                  <code>{q}</code>
                </button>
                <span class="sh-desc">{d}</span>
              </>
            )}
          </For>
        </div>
        <p class="sh-note">Words match prefixes as you type (inv → invoice).</p>
        <p class="sh-note">The Sender / Recipients / Subject chips limit plain words; All folders / the folder name sets the scope.</p>
        <div class="sh-foot">
          <button
            class="link-btn"
            onClick={() => {
              props.onClose(false);
              openShortcuts();
            }}
          >
            All keyboard shortcuts
          </button>
          <kbd>?</kbd>
        </div>
      </div>
    </Portal>
  );
}

export function SearchBar() {
  let input!: HTMLInputElement;
  let field!: HTMLLabelElement;
  const [help, setHelp] = createSignal(false);
  const insert = (q: string) => {
    setHelp(false);
    setSearchQuery(q);
    input.value = q;
    input.focus();
    input.setSelectionRange(q.length, q.length);
  };

  const onGlobalKey = (e: KeyboardEvent) => {
    if (document.querySelector('.modal-backdrop')) return;
    if (commandFor(e) === 'search' && !isTypingTarget(e.target)) {
      e.preventDefault();
      input.focus();
      input.select();
    }
  };
  onMount(() => window.addEventListener('keydown', onGlobalKey));
  onCleanup(() => window.removeEventListener('keydown', onGlobalKey));

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      if (help()) setHelp(false);
      else if (searchQuery() !== '') setSearchQuery('');
      else {
        input.blur();
        focusList(false);
      }
    } else if ((e.key === 'ArrowDown' || e.key === 'Enter') && searchActive()) {
      e.preventDefault();
      focusList(true);
    }
  };

  return (
    <div class="search-bar" data-tauri-drag-region={DRAG}>
      <label class="search-field" ref={field} classList={{ filled: searchQuery() !== '' }}>
        <SearchIcon size={14} class="icon search-icon" />
        <input
          ref={input}
          type="text"
          spellcheck={false}
          autocomplete="off"
          autocapitalize="off"
          placeholder="Search sender, recipients, subject"
          aria-label="Search messages"
          value={searchQuery()}
          onInput={(e) => {
            // "?" as the whole query opens search help instead of searching for it.
            if (e.currentTarget.value === '?' && searchQuery() === '') {
              e.currentTarget.value = '';
              setHelp(true);
              return;
            }
            setSearchQuery(e.currentTarget.value);
          }}
          onKeyDown={onKeyDown}
        />
        {/* One fixed slot: help "?" while empty, clear × while typing — no width change. */}
        <span class="search-slot">
          <Show
            when={searchQuery() !== ''}
            fallback={
              <button
                class="search-help-btn"
                aria-label="Search help"
                aria-expanded={help()}
                title={`Search help (${hint('search-help')})`}
                onMouseDown={(e) => e.preventDefault()}
                onClick={(e) => {
                  e.preventDefault();
                  setHelp((v) => !v);
                }}
              >
                <HelpIcon size={13} />
              </button>
            }
          >
            <button
              class="search-clear"
              aria-label="Clear search"
              title={`Clear (${hint('search-clear')})`}
              onMouseDown={(e) => e.preventDefault()}
              onClick={(e) => {
                e.preventDefault();
                setSearchQuery('');
                input.focus();
              }}
            >
              <CloseIcon size={10} />
            </button>
          </Show>
        </span>
      </label>
      <button class="icon-btn search-compose" title={`New message (${hint('compose')})`} aria-label="New message" onClick={() => newMessage()}>
        <ComposeIcon />
      </button>
      <Show when={help()}>
        <SearchHelp
          anchor={field}
          onPick={insert}
          onClose={(refocus) => {
            setHelp(false);
            if (refocus) input.focus();
          }}
        />
      </Show>
    </div>
  );
}
