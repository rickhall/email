import { onCleanup, onMount, Show } from 'solid-js';
import { searchActive, searchQuery, setSearchQuery } from '../state';
import { CloseIcon, SearchIcon } from './Icons';

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
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'j' }));
  }
}

/** Focus the quick-search field (shortcut `/`, ⌘F, macOS menu "Find"). */
export function focusSearch() {
  const el = document.querySelector<HTMLInputElement>('.search-field input');
  el?.focus();
  el?.select();
}

export function SearchBar() {
  let input!: HTMLInputElement;

  const onGlobalKey = (e: KeyboardEvent) => {
    if (document.querySelector('.modal-backdrop')) return;
    const mod = e.metaKey || e.ctrlKey;
    if (mod && !e.altKey && !e.shiftKey && e.key.toLowerCase() === 'f') {
      e.preventDefault();
      input.focus();
      input.select();
    } else if (e.key === '/' && !mod && !e.altKey && !isTypingTarget(e.target)) {
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
      if (searchQuery() !== '') setSearchQuery('');
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
    <div class="search-bar" data-tauri-drag-region>
      <label class="search-field" classList={{ filled: searchQuery() !== '' }}>
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
          onInput={(e) => setSearchQuery(e.currentTarget.value)}
          onKeyDown={onKeyDown}
        />
        <Show when={searchQuery() !== ''}>
          <button
            class="search-clear"
            aria-label="Clear search"
            title="Clear (Esc)"
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => {
              setSearchQuery('');
              input.focus();
            }}
          >
            <CloseIcon size={10} />
          </button>
        </Show>
      </label>
    </div>
  );
}
