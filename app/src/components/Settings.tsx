import { createSignal, For, onCleanup, onMount, Show } from 'solid-js';
import {
  bindKey,
  COMMANDS,
  isDefault,
  keysOf,
  label,
  recordKey,
  resetKeys,
  unbindKey,
  type Command,
} from '../lib/shortcuts';
import { CloseIcon } from './Icons';

const [open, setOpen] = createSignal(false);
let returnFocus: HTMLElement | null = null;

export function openSettings(): void {
  if (open()) return;
  returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  setOpen(true);
}
function close() {
  setOpen(false);
  queueMicrotask(() => returnFocus?.focus({ preventScroll: true }));
}

/** Commands whose keys are part of something else (tag positions, popups, attachment list). */
const LOCKED = new Set(['tag-n', 'tag-clear', 'escape', 'search-help', 'search-clear', 'att-open', 'att-save', 'att-save-all']);

const descOf = (id: string) => COMMANDS.flatMap((s) => s.items).find((c) => c.id === id)?.desc ?? id;

function Keycaps(props: { k: string }) {
  return (
    <span class="sc-combo">
      <For each={label(props.k)}>{(cap) => <kbd>{cap}</kbd>}</For>
    </span>
  );
}

function Panel() {
  let card!: HTMLDivElement;
  /** Command waiting for a key press. */
  const [recording, setRecording] = createSignal<string | null>(null);
  /** One line under the list: what the last change did, or why a key was refused. */
  const [status, setStatus] = createSignal<{ text: string; error?: boolean } | null>(null);

  onMount(() => {
    card.focus();
    const onBlur = () => close();
    window.addEventListener('blur', onBlur);
    // Capture on the window: keys reach the panel even when focus has fallen out of it (the
    // + button that started recording is gone), and never reach the app's own shortcuts.
    window.addEventListener('keydown', onKey, true);
    onCleanup(() => {
      window.removeEventListener('blur', onBlur);
      window.removeEventListener('keydown', onKey, true);
    });
  });

  function onKey(e: KeyboardEvent) {
    e.stopPropagation();
    const id = recording();
    // Not recording: Tab/Enter/Space work the dialog's own buttons as usual.
    if (e.key === 'Escape') {
      e.preventDefault();
      if (id) {
        setRecording(null);
        setStatus(null);
      } else close();
      return;
    }
    if (!id) return;
    e.preventDefault();
    const r = recordKey(e);
    if (!r) return; // a modifier on its own: keep waiting
    if (!r.ok) {
      setStatus({ text: r.reason, error: true });
      return;
    }
    const from = bindKey(id, r.key);
    setRecording(null);
    card.focus({ preventScroll: true });
    const keys = label(r.key).join(' ');
    setStatus({ text: from ? `${keys} now does “${descOf(id)}” (removed from “${descOf(from)}”)` : `${keys} added to “${descOf(id)}”` });
  }

  function Row(p: { cmd: Command }) {
    const locked = LOCKED.has(p.cmd.id);
    return (
      <div class="st-row" classList={{ recording: recording() === p.cmd.id }}>
        <span class="st-desc">{p.cmd.desc}</span>
        <span class="st-keys">
          <For each={keysOf(p.cmd.id)}>
            {(k) => (
              <span class="st-key">
                <Keycaps k={k} />
                <Show when={!locked}>
                  <button
                    class="st-x"
                    aria-label={`Remove ${label(k).join(' ')}`}
                    title="Remove"
                    onClick={() => {
                      unbindKey(p.cmd.id, k);
                      setStatus({ text: `${label(k).join(' ')} removed from “${p.cmd.desc}”` });
                    }}
                  >
                    <CloseIcon size={8} />
                  </button>
                </Show>
              </span>
            )}
          </For>
          <For each={p.cmd.fixed ?? []}>
            {(k) => (
              <span class="st-key fixed" title="Fixed">
                <Keycaps k={k} />
              </span>
            )}
          </For>
          <Show when={!locked}>
            <Show
              when={recording() === p.cmd.id}
              fallback={
                <button
                  class="st-add"
                  title="Add a key"
                  aria-label={`Add a key for ${p.cmd.desc}`}
                  onClick={() => {
                    setRecording(p.cmd.id);
                    setStatus({ text: `Press a key for “${p.cmd.desc}” · Esc to cancel` });
                  }}
                >
                  +
                </button>
              }
            >
              <span class="st-listen">Press a key…</span>
            </Show>
          </Show>
        </span>
        <span class="st-reset-slot">
          <Show when={!isDefault(p.cmd.id)}>
            <button
              class="link-btn"
              onClick={() => {
                resetKeys(p.cmd.id);
                setStatus({ text: `“${p.cmd.desc}” back to its default` });
              }}
            >
              Default
            </button>
          </Show>
        </span>
      </div>
    );
  }

  return (
    <div class="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && close()}>
      <div class="card modal settings" ref={card} tabIndex={-1} role="dialog" aria-modal="true" aria-label="Settings">
        <div class="modal-head">
          <h2>Settings</h2>
          <button class="icon-btn" onClick={close} aria-label="Close">
            <CloseIcon />
          </button>
        </div>
        <h3 class="st-title">Keyboard shortcuts</h3>
        <p class="st-note">Click + to add a key and × to remove one. Dimmed keys are fixed.</p>
        <div class="st-list">
          <For each={COMMANDS}>
            {(sec) => (
              <section class="st-section">
                <h4>{sec.title}</h4>
                <For each={sec.items}>{(cmd) => <Row cmd={cmd} />}</For>
              </section>
            )}
          </For>
        </div>
        <div class="st-foot">
          <span class="st-status" classList={{ error: !!status()?.error }} aria-live="polite">
            {status()?.text ?? ''}
          </span>
          <button
            class="btn secondary"
            onClick={() => {
              resetKeys();
              setRecording(null);
              setStatus({ text: 'All shortcuts back to their defaults' });
            }}
          >
            Restore defaults
          </button>
        </div>
      </div>
    </div>
  );
}

export function SettingsHost() {
  return (
    <Show when={open()}>
      <Panel />
    </Show>
  );
}
