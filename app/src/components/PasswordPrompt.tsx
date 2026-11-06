import { createSignal, Show } from 'solid-js';
import { api, errorMessage, type Account } from '../api';
import { AlertIcon, CloseIcon, LockIcon } from './Icons';

/**
 * Re-enter the password for an account whose saved password the server rejected
 * (sync phase `auth`). Opened from the sidebar status footer's "Fix…" link — never unprompted.
 * Saving verifies with the server, stores it in the keychain and restarts sync.
 */
export function PasswordDialog(props: { account: Account; onClose: () => void }) {
  return (
    <Show
      when={props.account.auth === 'microsoft'}
      fallback={<PasswordForm account={props.account} onClose={props.onClose} />}
    >
      <SignInAgain account={props.account} onClose={props.onClose} />
    </Show>
  );
}

/**
 * A Microsoft account whose sign-in expired or was revoked: sign in again in the browser (the
 * address is filled in there). Cancel stops waiting for the browser.
 */
function SignInAgain(props: { account: Account; onClose: () => void }) {
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  async function signIn() {
    setBusy(true);
    setError(null);
    try {
      await api.microsoftSignInAgain(props.account.id);
      props.onClose();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }
  const close = () => (busy() ? void api.cancelSignIn() : props.onClose());

  return (
    <div
      class="modal-backdrop"
      onMouseDown={(e) => e.target === e.currentTarget && !busy() && props.onClose()}
      onKeyDown={(e) => e.key === 'Escape' && close()}
    >
      <div class="card modal pw-dialog" role="dialog" aria-modal="true" aria-label="Sign-in needed">
        <div class="modal-head">
          <h2 class="pw-dialog-title">
            <LockIcon size={15} />
            Sign-in needed
          </h2>
          <button class="icon-btn" onClick={close} aria-label="Close">
            <CloseIcon />
          </button>
        </div>
        <p class="pw-dialog-text">
          Microsoft needs you to sign in again for <strong>{props.account.email}</strong>. Sync is paused until you do.
        </p>
        <Show when={error()}>
          <div class="form-error" role="alert">
            <AlertIcon size={14} />
            <span>{error()}</span>
          </div>
        </Show>
        <div class="form-actions">
          <button type="button" class="btn secondary" onClick={close}>
            Cancel
          </button>
          <button
            type="button"
            class="btn primary"
            onClick={() => void signIn()}
            disabled={busy()}
            ref={(el) => queueMicrotask(() => el.focus())}
          >
            <Show when={busy()} fallback="Sign in with Microsoft">
              <span class="spinner small on-accent" /> Finish in your browser…
            </Show>
          </button>
        </div>
      </div>
    </div>
  );
}

function PasswordForm(props: { account: Account; onClose: () => void }) {
  const [password, setPassword] = createSignal('');
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  async function submit(e: Event) {
    e.preventDefault();
    if (busy() || !password()) return;
    setBusy(true);
    setError(null);
    try {
      await api.updatePassword(props.account.id, password());
      props.onClose();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div
      class="modal-backdrop"
      onMouseDown={(e) => e.target === e.currentTarget && !busy() && props.onClose()}
      onKeyDown={(e) => e.key === 'Escape' && !busy() && props.onClose()}
    >
      <div class="card modal pw-dialog" role="dialog" aria-modal="true" aria-label="Password needed">
        <div class="modal-head">
          <h2 class="pw-dialog-title">
            <LockIcon size={15} />
            Password needed
          </h2>
          <button class="icon-btn" onClick={() => props.onClose()} aria-label="Close" disabled={busy()}>
            <CloseIcon />
          </button>
        </div>
        <p class="pw-dialog-text">
          The server rejected the saved password for <strong>{props.account.email}</strong>. Sync is paused until you
          enter a new one.
        </p>
        <form class="form" onSubmit={submit} novalidate>
          <label class="field">
            <span class="field-label">{props.account.provider === 'gmail' ? 'New app password' : 'New password'}</span>
            <input
              type="password"
              autocomplete="current-password"
              value={password()}
              onInput={(e) => setPassword(e.currentTarget.value)}
              disabled={busy()}
              ref={(el) => queueMicrotask(() => el.focus())}
            />
            <Show when={props.account.provider === 'gmail'}>
              <span class="field-help">
                Create one at{' '}
                <a href="https://myaccount.google.com/apppasswords" target="_blank" rel="noopener noreferrer">
                  myaccount.google.com/apppasswords
                </a>
                .
              </span>
            </Show>
          </label>
          <Show when={error()}>
            <div class="form-error" role="alert">
              <AlertIcon size={14} />
              <span>{error()}</span>
            </div>
          </Show>
          <div class="form-actions">
            <button type="button" class="btn secondary" onClick={() => props.onClose()} disabled={busy()}>
              Cancel
            </button>
            <button type="submit" class="btn primary" disabled={busy() || !password()}>
              <Show when={busy()} fallback="Save & Reconnect">
                <span class="spinner small on-accent" /> Checking…
              </Show>
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
