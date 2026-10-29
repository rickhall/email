import { createSignal, For, onCleanup, onMount, Show } from 'solid-js';
import { allKeys, COMMANDS, label } from '../lib/shortcuts';
import { openSettings } from './Settings';
import { CloseIcon } from './Icons';

const [open, setOpen] = createSignal(false);
let returnFocus: HTMLElement | null = null;

export function openShortcuts(): void {
  if (open()) return;
  returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  setOpen(true);
}
function close() {
  setOpen(false);
  queueMicrotask(() => returnFocus?.focus({ preventScroll: true }));
}
export function toggleShortcuts(): void {
  if (open()) close();
  else openShortcuts();
}
export const shortcutsOpen = open;

/** `?` anywhere outside text inputs (and not over another modal) toggles the sheet. */
export function isTextInput(t: EventTarget | null): boolean {
  if (!(t instanceof HTMLElement)) return false;
  return t.isContentEditable || t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT';
}

function Sheet() {
  let card!: HTMLDivElement;
  onMount(() => {
    card.focus();
    // Same dismissal rule as the popups: window blur (e.g. a click into an email iframe) closes.
    const onBlur = () => close();
    window.addEventListener('blur', onBlur);
    onCleanup(() => window.removeEventListener('blur', onBlur));
  });
  return (
    <div
      class="modal-backdrop"
      onMouseDown={(e) => e.target === e.currentTarget && close()}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === 'Escape' || e.key === '?') {
          e.preventDefault();
          close();
        }
      }}
    >
      <div class="card modal shortcuts-sheet" ref={card} tabIndex={-1} role="dialog" aria-modal="true" aria-label="Keyboard shortcuts">
        <div class="modal-head">
          <h2>Keyboard shortcuts</h2>
          <button class="icon-btn" onClick={close} aria-label="Close">
            <CloseIcon />
          </button>
        </div>
        <div class="sc-grid">
          <For each={COMMANDS}>
            {(sec) => (
              <section class="sc-section">
                <h3>{sec.title}</h3>
                <For each={sec.items}>
                  {(it) => (
                    <div class="sc-row">
                      <span class="sc-keys">
                        <For each={allKeys(it.id).map(label)}>
                          {(combo, i) => (
                            <>
                              <Show when={i() > 0}>
                                <span class="sc-or">/</span>
                              </Show>
                              <span class="sc-combo">
                                <For each={combo}>{(key) => <kbd>{key}</kbd>}</For>
                              </span>
                            </>
                          )}
                        </For>
                      </span>
                      <span class="sc-desc">{it.desc}</span>
                    </div>
                  )}
                </For>
              </section>
            )}
          </For>
        </div>
        <div class="sc-foot">
          <button
            class="btn secondary"
            onClick={() => {
              close();
              // after close() has handed focus back, so Settings returns it there too
              queueMicrotask(openSettings);
            }}
          >
            Customize…
          </button>
        </div>
      </div>
    </div>
  );
}

export function ShortcutsHost() {
  return (
    <Show when={open()}>
      <Sheet />
    </Show>
  );
}
