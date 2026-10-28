import { createSignal, Show } from 'solid-js';
import { api, errorMessage, type Account } from '../api';
import { AlertIcon, ChevronIcon, CloseIcon, MailIcon } from './Icons';
import { DRAG } from '../lib/platform';

function isGmail(email: string) {
  const d = email.split('@')[1]?.trim().toLowerCase() ?? '';
  return d === 'gmail.com' || d === 'googlemail.com';
}

export function AddAccountForm(props: { onAdded: (a: Account) => void; onCancel?: () => void; autofocus?: boolean }) {
  const [email, setEmail] = createSignal('');
  const [password, setPassword] = createSignal('');
  const [displayName, setDisplayName] = createSignal('');
  const [host, setHost] = createSignal('');
  const [port, setPort] = createSignal('');
  const [advanced, setAdvanced] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const portNum = () => {
    const p = port().trim();
    if (!p) return null;
    const n = Number(p);
    return Number.isInteger(n) && n > 0 && n < 65536 ? n : NaN;
  };
  const canSubmit = () => !busy() && email().includes('@') && password().length > 0 && !Number.isNaN(portNum());

  async function submit(e: Event) {
    e.preventDefault();
    if (!canSubmit()) return;
    setBusy(true);
    setError(null);
    try {
      const acc = await api.addAccount({
        email: email().trim(),
        password: password(),
        displayName: displayName().trim() || null,
        imapHost: host().trim() || null,
        imapPort: portNum(),
      });
      props.onAdded(acc);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form class="form" onSubmit={submit} novalidate>
      <label class="field">
        <span class="field-label">Email</span>
        <input
          type="email"
          autocomplete="username"
          placeholder="you@example.com"
          value={email()}
          onInput={(e) => setEmail(e.currentTarget.value)}
          ref={(el) => props.autofocus && queueMicrotask(() => el.focus())}
          required
        />
      </label>
      <label class="field">
        <span class="field-label">Password</span>
        <input
          type="password"
          autocomplete="current-password"
          placeholder={isGmail(email()) ? 'App password' : 'Password'}
          value={password()}
          onInput={(e) => setPassword(e.currentTarget.value)}
          required
        />
        <Show when={isGmail(email())}>
          <span class="field-help">
            Gmail needs an app password. Create one at{' '}
            <a href="https://myaccount.google.com/apppasswords" target="_blank" rel="noopener noreferrer">
              myaccount.google.com/apppasswords
            </a>
            .
          </span>
        </Show>
      </label>
      <label class="field">
        <span class="field-label">
          Display name <span class="optional">optional</span>
        </span>
        <input
          type="text"
          autocomplete="name"
          placeholder="Jane Appleseed"
          value={displayName()}
          onInput={(e) => setDisplayName(e.currentTarget.value)}
        />
      </label>

      <button type="button" class="advanced-toggle" onClick={() => setAdvanced((v) => !v)} aria-expanded={advanced()}>
        <span class="disclosure" classList={{ open: advanced() }}>
          <ChevronIcon size={12} />
        </span>
        Advanced
      </button>
      <Show when={advanced()}>
        <div class="field-row">
          <label class="field grow">
            <span class="field-label">IMAP host</span>
            <input
              type="text"
              placeholder="imap.example.com"
              autocapitalize="off"
              spellcheck={false}
              value={host()}
              onInput={(e) => setHost(e.currentTarget.value)}
            />
          </label>
          <label class="field port">
            <span class="field-label">Port</span>
            <input
              type="text"
              inputmode="numeric"
              placeholder="993"
              value={port()}
              classList={{ invalid: Number.isNaN(portNum()) }}
              onInput={(e) => setPort(e.currentTarget.value)}
            />
          </label>
        </div>
      </Show>

      <Show when={error()}>
        <div class="form-error" role="alert">
          <AlertIcon size={14} />
          <span>{error()}</span>
        </div>
      </Show>

      <div class="form-actions">
        <Show when={props.onCancel}>
          <button type="button" class="btn secondary" onClick={() => props.onCancel?.()} disabled={busy()}>
            Cancel
          </button>
        </Show>
        <button type="submit" class="btn primary" disabled={!canSubmit()}>
          <Show when={busy()} fallback="Add Account">
            <span class="spinner small on-accent" /> Connecting…
          </Show>
        </button>
      </div>
    </form>
  );
}

export function Onboarding(props: { onAdded: (a: Account) => void }) {
  return (
    <div class="onboarding" data-tauri-drag-region={DRAG}>
      <div class="card">
        <div class="card-hero">
          <div class="hero-icon">
            <MailIcon size={28} />
          </div>
          <h1>Welcome to Email</h1>
          <p>Add an IMAP account to get started. Your mail is synced and stored on this device.</p>
        </div>
        <AddAccountForm onAdded={props.onAdded} autofocus />
      </div>
    </div>
  );
}

export function AddAccountModal(props: { onAdded: (a: Account) => void; onClose: () => void }) {
  return (
    <div
      class="modal-backdrop"
      onMouseDown={(e) => e.target === e.currentTarget && props.onClose()}
      onKeyDown={(e) => e.key === 'Escape' && props.onClose()}
    >
      <div class="card modal" role="dialog" aria-modal="true" aria-label="Add account">
        <div class="modal-head">
          <h2>Add Account</h2>
          <button class="icon-btn" onClick={props.onClose} aria-label="Close">
            <CloseIcon />
          </button>
        </div>
        <AddAccountForm onAdded={props.onAdded} onCancel={props.onClose} autofocus />
      </div>
    </div>
  );
}
