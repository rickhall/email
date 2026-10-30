import { createResource, createSignal, For, Match, onCleanup, onMount, Show, Switch, type JSX } from 'solid-js';
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
import { api, errorMessage, type AccountDetails } from '../api';
import { accounts } from '../state';
import { loadPref, savePref } from '../lib/format';
import { AccountDot } from './FolderSwitcher';

// Settings: sections on the left, the chosen one on the right (each with its own heading,
// scrolling and buttons). Fixed size, so switching sections never moves the dialog.

export type SettingsSection = 'accounts' | 'keyboard' | 'troubleshooting';
const SECTIONS: { id: SettingsSection; title: string }[] = [
  { id: 'accounts', title: 'Accounts' },
  { id: 'keyboard', title: 'Keyboard' },
  { id: 'troubleshooting', title: 'Troubleshooting' },
];

const [open, setOpen] = createSignal(false);
const [section, setSectionSig] = createSignal<SettingsSection>(loadPref<SettingsSection>('ui.settingsSection', 'accounts'));
const setSection = (s: SettingsSection) => {
  setSectionSig(s);
  savePref('ui.settingsSection', s);
};
let returnFocus: HTMLElement | null = null;

/** Open Settings (at `at`, else where it was last). */
export function openSettings(at?: SettingsSection): void {
  if (at) setSection(at);
  if (open()) return;
  returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  setOpen(true);
}
function close() {
  setRecording(null);
  setStatus(null);
  setOpen(false);
  queueMicrotask(() => returnFocus?.focus({ preventScroll: true }));
}

// ------------------------------------------------------------- keyboard

/** Commands whose keys are part of something else (tag positions, popups, attachment list). */
const LOCKED = new Set(['tag-n', 'tag-clear', 'escape', 'search-help', 'search-clear', 'att-open', 'att-save', 'att-save-all']);

const descOf = (id: string) => COMMANDS.flatMap((s) => s.items).find((c) => c.id === id)?.desc ?? id;

/** Command waiting for a key press. */
const [recording, setRecording] = createSignal<string | null>(null);
/** One line under the shortcut list: what the last change did, or why a key was refused. */
const [status, setStatus] = createSignal<{ text: string; error?: boolean } | null>(null);

function Keycaps(props: { k: string }) {
  return (
    <span class="sc-combo">
      <For each={label(props.k)}>{(cap) => <kbd>{cap}</kbd>}</For>
    </span>
  );
}

/** While recording, the next key press is the new shortcut. Returns true if handled. */
function recordKeyPress(e: KeyboardEvent, refocus: () => void): boolean {
  const id = recording();
  if (!id) return false;
  e.preventDefault();
  if (e.key === 'Escape') {
    setRecording(null);
    setStatus(null);
    return true;
  }
  const r = recordKey(e);
  if (!r) return true; // a modifier on its own: keep waiting
  if (!r.ok) {
    setStatus({ text: r.reason, error: true });
    return true;
  }
  const from = bindKey(id, r.key);
  setRecording(null);
  refocus();
  const keys = label(r.key).join(' ');
  setStatus({ text: from ? `${keys} now does “${descOf(id)}” (removed from “${descOf(from)}”)` : `${keys} added to “${descOf(id)}”` });
  return true;
}

function ShortcutRow(p: { cmd: Command }) {
  const locked = LOCKED.has(p.cmd.id);
  return (
    <div class="st-row" classList={{ recording: recording() === p.cmd.id }}>
      <span class="st-desc">
        {p.cmd.desc}
        {/* beside the name (not in a column of its own): no empty space when unchanged, and
            the keys don't move when it appears */}
        <Show when={!isDefault(p.cmd.id)}>
          <button
            class="link-btn st-default"
            onClick={() => {
              resetKeys(p.cmd.id);
              setStatus({ text: `“${p.cmd.desc}” back to its default` });
            }}
          >
            Default
          </button>
        </Show>
      </span>
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
    </div>
  );
}

function KeyboardSection() {
  return (
    <>
      <div class="st-body">
        <p class="st-note">Click + to add a key and × to remove one. Dimmed keys are fixed.</p>
        <For each={COMMANDS}>
          {(sec) => (
            <section class="st-section">
              <h4>{sec.title}</h4>
              <For each={sec.items}>{(cmd) => <ShortcutRow cmd={cmd} />}</For>
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
    </>
  );
}

// ------------------------------------------------------------- accounts

const PROVIDER: Record<string, string> = { gmail: 'Gmail', outlook: 'Outlook', imap: 'IMAP' };

function Field(props: { label: string; children: JSX.Element }) {
  return (
    <div class="st-field">
      <span class="st-field-label">{props.label}</span>
      <span class="st-field-value">{props.children}</span>
    </div>
  );
}

function AccountCard(props: { d: AccountDetails }) {
  const account = () => (accounts() ?? []).find((a) => a.id === props.d.id);
  const [editing, setEditing] = createSignal(false);
  const [pw, setPw] = createSignal('');
  const [busy, setBusy] = createSignal(false);
  const [msg, setMsg] = createSignal<{ text: string; error?: boolean } | null>(null);
  const save = async (e: Event) => {
    e.preventDefault();
    if (!pw() || busy()) return;
    setBusy(true);
    setMsg(null);
    try {
      await api.updatePassword(props.d.id, pw());
      setEditing(false);
      setPw('');
      setMsg({ text: 'Password updated; reconnecting.' });
    } catch (err) {
      setMsg({ text: errorMessage(err), error: true });
    } finally {
      setBusy(false);
    }
  };
  return (
    <section class="st-acct">
      <div class="st-acct-head">
        <Show when={account()}>{(a) => <AccountDot account={a()} size={20} />}</Show>
        <span class="st-acct-email">{props.d.email}</span>
        <Show when={props.d.displayName && props.d.displayName !== props.d.email}>
          <span class="st-acct-name">{props.d.displayName}</span>
        </Show>
      </div>
      <Field label="Type">{PROVIDER[props.d.provider] ?? props.d.provider}</Field>
      <Field label="Incoming (IMAP)">
        {props.d.imapHost}:{props.d.imapPort}
      </Field>
      <Field label="Outgoing (SMTP)">
        <Show when={props.d.smtpHost} fallback={<span class="st-muted">not set</span>}>
          {props.d.smtpHost}:{props.d.smtpPort}
          <Show when={props.d.smtpDefault}>
            <span class="st-muted"> · the standard server for {props.d.imapHost}</span>
          </Show>
        </Show>
      </Field>
      <Field label="Password">
        <Show
          when={editing()}
          fallback={
            <button class="link-btn" onClick={() => (setEditing(true), setMsg(null))}>
              Update password…
            </button>
          }
        >
          <form class="st-pw" onSubmit={save}>
            <input
              class="st-input"
              type="password"
              placeholder="App password"
              autocomplete="current-password"
              value={pw()}
              ref={(el) => queueMicrotask(() => el.focus())}
              onInput={(e) => setPw(e.currentTarget.value)}
            />
            <button class="btn primary st-pw-save" type="submit" disabled={!pw() || busy()}>
              Save
            </button>
            <button class="btn secondary" type="button" onClick={() => (setEditing(false), setPw(''))}>
              Cancel
            </button>
          </form>
        </Show>
      </Field>
      <Show when={msg()}>{(m) => <p class="st-acct-msg" classList={{ error: !!m().error }}>{m().text}</p>}</Show>
    </section>
  );
}

function AccountsSection() {
  const [details] = createResource(() => api.accountDetails());
  // in the user's order (as in the sidebar)
  const ordered = () => {
    const d = details() ?? [];
    return (accounts() ?? []).map((a) => d.find((x) => x.id === a.id)).filter((x): x is AccountDetails => !!x);
  };
  return (
    <div class="st-body">
      <For each={ordered()}>{(d) => <AccountCard d={d} />}</For>
      <Show when={details.error}>
        <p class="st-acct-msg error">{errorMessage(details.error)}</p>
      </Show>
    </div>
  );
}

// ------------------------------------------------------------- troubleshooting

function TroubleshootingSection() {
  const [version] = createResource(() => api.appVersion().catch(() => ''));
  const [msg, setMsg] = createSignal<string | null>(null);
  const act = (f: () => Promise<void>) => () => void f().catch((e) => setMsg(errorMessage(e)));
  return (
    <div class="st-body">
      <Field label="Version">{version() ?? ''}</Field>
      <Field label="App log">
        <span class="st-muted">Connections, sync, sending: what to look at when something misbehaves.</span>
        <button class="link-btn st-field-action" onClick={act(() => api.showLog())}>
          Show file
        </button>
      </Field>
      <Field label="Data folder">
        <span class="st-muted">Where mail is stored on this computer.</span>
        <button class="link-btn st-field-action" onClick={act(() => api.showDataDir())}>
          Show folder
        </button>
      </Field>
      <Field label="Developer tools">
        <span class="st-muted">The web inspector for this window.</span>
        <button class="link-btn st-field-action" onClick={act(() => api.openDevtools())}>
          Open
        </button>
      </Field>
      <Show when={msg()}>
        <p class="st-acct-msg error">{msg()}</p>
      </Show>
    </div>
  );
}

// ------------------------------------------------------------- the dialog

function Panel() {
  let card!: HTMLDivElement;
  onMount(() => {
    card.focus();
    const onBlur = () => close();
    window.addEventListener('blur', onBlur);
    // Capture on the window: while a shortcut is being recorded the key is the shortcut (and
    // reaches nothing else, even with focus fallen out of the dialog); Esc closes. Other keys
    // go where they're typed (the password field…).
    window.addEventListener('keydown', onKey, true);
    onCleanup(() => {
      window.removeEventListener('blur', onBlur);
      window.removeEventListener('keydown', onKey, true);
    });
  });

  function onKey(e: KeyboardEvent) {
    if (recordKeyPress(e, () => card.focus({ preventScroll: true }))) {
      e.stopPropagation();
      return;
    }
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      close();
    }
  }

  const title = () => SECTIONS.find((s) => s.id === section())?.title ?? '';
  return (
    <div class="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && close()}>
      <div class="card modal settings" ref={card} tabIndex={-1} role="dialog" aria-modal="true" aria-label="Settings">
        <nav class="st-nav" aria-label="Settings sections">
          <h2>Settings</h2>
          <For each={SECTIONS}>
            {(s) => (
              <button
                class="st-nav-item"
                classList={{ on: section() === s.id }}
                aria-current={section() === s.id ? 'page' : undefined}
                onClick={() => {
                  setRecording(null);
                  setSection(s.id);
                }}
              >
                {s.title}
              </button>
            )}
          </For>
        </nav>
        <div class="st-pane">
          <header class="st-pane-head">
            <h3>{title()}</h3>
            <button class="icon-btn" onClick={close} aria-label="Close">
              <CloseIcon />
            </button>
          </header>
          <Switch>
            <Match when={section() === 'accounts'}>
              <AccountsSection />
            </Match>
            <Match when={section() === 'keyboard'}>
              <KeyboardSection />
            </Match>
            <Match when={section() === 'troubleshooting'}>
              <TroubleshootingSection />
            </Match>
          </Switch>
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
