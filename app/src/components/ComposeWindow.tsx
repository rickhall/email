import { createSignal, onMount, Show } from 'solid-js';
import { errorMessage } from '../api';
import { loadAccountsOnly } from '../state';
import { compose, loadDraft } from '../lib/compose';
import { Composer } from './Composer';
import { ToastHost } from './Toast';
import { ConfirmHost } from './Actions';

/** The whole page of a compose window: one draft, written and sent from here. */
export function ComposeWindow() {
  const [error, setError] = createSignal<string | null>(null);
  onMount(async () => {
    try {
      await Promise.all([loadAccountsOnly(), loadDraft()]);
    } catch (e) {
      setError(errorMessage(e));
    }
  });
  return (
    <div class="compose-window">
      <Show when={compose()?.key} keyed fallback={<p class="compose-error">{error() ?? ''}</p>}>
        {(_key) => <Composer />}
      </Show>
      <ToastHost />
      <ConfirmHost />
    </div>
  );
}
