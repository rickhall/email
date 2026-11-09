import { createSignal, For, onMount, Show } from 'solid-js';
import { Portal } from 'solid-js/web';
import { placeNear, useDismiss } from './Actions';

// Snooze is a reminder: the message stays where it is, and at the chosen time it comes back
// unread at the top of the Inbox (out of the archive, if it went there), with a notification
// (see `tags::SNOOZE_PREFIX` in the core).

const MENU_W = 260;

/** Unix seconds for a local date at h:00. */
const at = (d: Date, h: number) => Math.floor(new Date(d.getFullYear(), d.getMonth(), d.getDate(), h).getTime() / 1000);
const plusDays = (d: Date, n: number) => new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);

/** The preset times from `now`: later today, this evening, tomorrow, the weekend, next week. */
export function snoozeChoices(now: Date): { label: string; until: number }[] {
  const out: { label: string; until: number }[] = [];
  const h = now.getHours();
  // Three hours on, on the hour, while that's still daytime.
  const later = new Date(now.getTime() + 3 * 3600_000);
  later.setMinutes(0, 0, 0);
  if (later.getDate() === now.getDate() && later.getHours() < 20) {
    out.push({ label: 'Later today', until: Math.floor(later.getTime() / 1000) });
  }
  if (h < 17 && at(now, 18) !== out[0]?.until) out.push({ label: 'This evening', until: at(now, 18) });
  out.push({ label: 'Tomorrow', until: at(plusDays(now, 1), 8) });
  const dow = now.getDay(); // 0 Sunday … 6 Saturday
  if (dow >= 1 && dow <= 4) out.push({ label: 'This weekend', until: at(plusDays(now, 6 - dow), 8) });
  out.push({ label: 'Next week', until: at(plusDays(now, ((8 - dow) % 7) || 7), 8) });
  // On a Sunday next week is tomorrow: each time once.
  return out.filter((c, i) => out.findIndex((d) => d.until === c.until) === i);
}

/** The latest snooze still to go off among `ms`, or null. */
export function pendingSnooze(ms: { snoozedUntil?: number | null }[]): number | null {
  const t = Math.max(0, ...ms.map((m) => m.snoozedUntil ?? 0));
  return t > Date.now() / 1000 ? t : null;
}

/** When a snoozed message comes back: "14:00" today, "Tue 08:00" this week, else "21 Nov". */
export function fmtWake(until: number, now = new Date()): string {
  const d = new Date(until * 1000);
  const time = d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
  const days = Math.round((at(d, 0) - at(now, 0)) / 86400);
  if (days === 0) return time;
  if (days > 0 && days < 7) return `${d.toLocaleDateString(undefined, { weekday: 'short' })} ${time}`;
  return d.toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: d.getFullYear() === now.getFullYear() ? undefined : 'numeric' });
}

/** `datetime-local` value for Unix seconds (local time). */
function localInput(t: number): string {
  const d = new Date(t * 1000);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
}

export function SnoozeMenu(props: {
  anchor: HTMLElement;
  /** already snoozed until then: offered to cancel */
  snoozedUntil: number | null;
  onPick: (until: number) => void;
  onUnsnooze: () => void;
  onClose: (refocus: boolean) => void;
}) {
  let el: HTMLDivElement | undefined;
  const now = new Date();
  const choices = snoozeChoices(now);
  const [custom, setCustom] = createSignal(localInput(at(plusDays(now, 1), 8)));
  const customTime = () => {
    const t = new Date(custom()).getTime();
    return Number.isFinite(t) ? Math.floor(t / 1000) : null;
  };
  const customOk = () => (customTime() ?? 0) > Date.now() / 1000;
  const [pos, setPos] = createSignal(placeNear(props.anchor, MENU_W, 260));
  const reposition = () => setPos(placeNear(props.anchor, MENU_W, el?.offsetHeight || 260));
  useDismiss(() => el, props.anchor, () => props.onClose(false), reposition);
  onMount(() =>
    queueMicrotask(() => {
      reposition();
      el?.querySelector<HTMLElement>('.vm-item')?.focus();
    }),
  );
  const pickCustom = () => {
    const t = customTime();
    if (t != null && customOk()) props.onPick(t);
  };
  return (
    <Portal>
      <div
        ref={el}
        class="view-menu snooze-menu"
        role="menu"
        aria-label="Remind me"
        style={{ left: `${pos().left}px`, top: `${pos().top}px`, right: 'auto', width: `${MENU_W}px` }}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === 'Escape') {
            e.preventDefault();
            props.onClose(true);
            return;
          }
          if (e.target instanceof HTMLInputElement) {
            if (e.key === 'Enter') {
              e.preventDefault();
              pickCustom();
            }
            return;
          }
          const list = [...(el?.querySelectorAll<HTMLElement>('.vm-item, .snooze-custom input') ?? [])];
          const i = list.indexOf(document.activeElement as HTMLElement);
          if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
            e.preventDefault();
            list[(i + (e.key === 'ArrowDown' ? 1 : -1) + list.length) % list.length]?.focus();
          }
        }}
      >
        <div class="vm-section" title="At that time it comes back unread at the top of the Inbox">Remind me</div>
        <Show when={props.snoozedUntil}>
          {(t) => (
            <button role="menuitem" class="vm-item" onClick={() => props.onUnsnooze()}>
              <span class="vm-label">Unsnooze</span>
              <span class="snooze-when">set for {fmtWake(t(), now)}</span>
            </button>
          )}
        </Show>
        <For each={choices}>
          {(c) => (
            <button role="menuitem" class="vm-item" onClick={() => props.onPick(c.until)}>
              <span class="vm-label">{c.label}</span>
              <span class="snooze-when">{fmtWake(c.until, now)}</span>
            </button>
          )}
        </For>
        <div class="vm-sep" />
        <div class="snooze-custom">
          <input
            type="datetime-local"
            aria-label="Date and time"
            value={custom()}
            min={localInput(Math.ceil(Date.now() / 60_000) * 60)}
            onInput={(e) => setCustom(e.currentTarget.value)}
          />
          <button class="btn small" disabled={!customOk()} onClick={pickCustom}>
            Snooze
          </button>
        </div>
      </div>
    </Portal>
  );
}
