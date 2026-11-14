import { createMemo, createSignal, For, onMount, Show } from 'solid-js';
import { monthGrid, parseTime, stepTime } from '../lib/clock';
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

/** The clock shows hours 0–23 here (else 1–12 with am/pm). */
const H24 = (() => {
  try {
    const c = new Intl.DateTimeFormat(undefined, { hour: 'numeric' }).resolvedOptions().hourCycle;
    return c === 'h23' || c === 'h24';
  } catch {
    return false;
  }
})();
/** The week starts on this day here (0 Sunday, 1 Monday). */
const FIRST_DAY = (() => {
  try {
    const info = (new Intl.Locale(navigator.language) as Intl.Locale & { weekInfo?: { firstDay: number } }).weekInfo;
    return info ? info.firstDay % 7 : 0;
  } catch {
    return 0;
  }
})();
const fmtMinutes = (m: number) =>
  new Date(2000, 0, 1, Math.floor(m / 60), m % 60).toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
const sameDay = (a: Date, b: Date) => a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();

/** A date and time of the user's choosing: a month calendar (click a day, or arrow keys) and a
 *  time typed ("9", "2pm", "14:15") or stepped with ↑/↓ (15 minutes; Shift: an hour). The
 *  app's own controls: WebKitGTK's date-time field kept taking the focus back to its popup. */
function CustomTime(props: { now: Date; onPick: (until: number) => void }) {
  const today = new Date(props.now.getFullYear(), props.now.getMonth(), props.now.getDate());
  const [day, setDay] = createSignal(plusDays(today, 1));
  const [shown, setShown] = createSignal(new Date(today.getFullYear(), today.getMonth(), 1));
  const [minutes, setMinutes] = createSignal(8 * 60);
  const [text, setText] = createSignal(fmtMinutes(8 * 60));
  const [bad, setBad] = createSignal(false);
  const until = () => Math.floor(new Date(day().getFullYear(), day().getMonth(), day().getDate(), 0, minutes()).getTime() / 1000);
  const ok = () => !bad() && until() > Date.now() / 1000;
  const grid = createMemo(() => monthGrid(shown().getFullYear(), shown().getMonth(), FIRST_DAY));
  const weekdays = createMemo(() =>
    grid().slice(0, 7).map((d) => d.toLocaleDateString(undefined, { weekday: 'narrow' })),
  );
  const pickDay = (d: Date) => {
    if (d < today) return;
    setDay(d);
    if (d.getMonth() !== shown().getMonth() || d.getFullYear() !== shown().getFullYear()) setShown(new Date(d.getFullYear(), d.getMonth(), 1));
  };
  const moveMonth = (n: number) => setShown(new Date(shown().getFullYear(), shown().getMonth() + n, 1));
  const takeText = () => {
    const m = parseTime(text(), H24);
    if (m == null) return setBad(text().trim() !== '');
    setBad(false);
    setMinutes(m);
    setText(fmtMinutes(m));
  };
  const setTime = (m: number) => {
    setBad(false);
    setMinutes(m);
    setText(fmtMinutes(m));
  };
  const summary = () =>
    `${day().toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' })} at ${fmtMinutes(minutes())}`;
  let calEl: HTMLDivElement | undefined;
  return (
    <div class="snz-custom">
      <div class="snz-month">
        <button class="icon-btn snz-nav" aria-label="Previous month" disabled={shown() <= new Date(today.getFullYear(), today.getMonth(), 1)} onClick={() => moveMonth(-1)}>
          ‹
        </button>
        <span class="snz-month-name">{shown().toLocaleDateString(undefined, { month: 'long', year: 'numeric' })}</span>
        <button class="icon-btn snz-nav" aria-label="Next month" onClick={() => moveMonth(1)}>
          ›
        </button>
      </div>
      <div
        class="snz-cal"
        role="grid"
        aria-label="Day"
        ref={calEl}
        onKeyDown={(e) => {
          const by = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -7, ArrowDown: 7 }[e.key];
          if (by == null) return;
          e.preventDefault();
          e.stopPropagation();
          const d = plusDays(day(), by);
          if (d < today) return;
          pickDay(d);
          queueMicrotask(() => calEl?.querySelector<HTMLElement>('.snz-day.sel')?.focus());
        }}
      >
        <For each={weekdays()}>{(w) => <span class="snz-wd">{w}</span>}</For>
        <For each={grid()}>
          {(d) => (
            <button
              class="snz-day"
              classList={{
                sel: sameDay(d, day()),
                today: sameDay(d, today),
                out: d.getMonth() !== shown().getMonth(),
              }}
              disabled={d < today}
              tabIndex={sameDay(d, day()) ? 0 : -1}
              aria-label={d.toLocaleDateString(undefined, { weekday: 'long', month: 'long', day: 'numeric' })}
              aria-selected={sameDay(d, day())}
              onClick={() => pickDay(d)}
            >
              {d.getDate()}
            </button>
          )}
        </For>
      </div>
      <div class="snz-row">
        <input
          class="snz-time"
          classList={{ invalid: bad() }}
          type="text"
          spellcheck={false}
          aria-label="Time (↑/↓ to change)"
          title="Type a time (9, 2pm, 14:15) or use ↑/↓"
          value={text()}
          onInput={(e) => setText(e.currentTarget.value)}
          onBlur={takeText}
          onKeyDown={(e) => {
            if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
              e.preventDefault();
              e.stopPropagation();
              const m = parseTime(text(), H24) ?? minutes();
              setTime(stepTime(m, (e.key === 'ArrowUp' ? 1 : -1) * (e.shiftKey ? 60 : 15)));
            } else if (e.key === 'Enter') {
              e.preventDefault();
              e.stopPropagation();
              takeText();
              if (ok()) props.onPick(until());
            }
          }}
        />
        <button class="btn primary small" disabled={!ok()} onClick={() => ok() && props.onPick(until())}>
          Snooze
        </button>
      </div>
      <div class="snz-summary" aria-live="polite">
        {bad() ? 'Not a time: try 9, 2pm or 14:15' : ok() ? summary() : 'That time has passed'}
      </div>
    </div>
  );
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
  const [pos, setPos] = createSignal(placeNear(props.anchor, MENU_W, 260));
  const reposition = () => setPos(placeNear(props.anchor, MENU_W, el?.offsetHeight || 260));
  useDismiss(() => el, props.anchor, () => props.onClose(false), reposition);
  onMount(() =>
    queueMicrotask(() => {
      reposition();
      el?.querySelector<HTMLElement>('.vm-item')?.focus();
    }),
  );
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
          if (e.target instanceof HTMLInputElement) return;
          const list = [...(el?.querySelectorAll<HTMLElement>('.vm-item, .snz-time') ?? [])];
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
        <CustomTime now={now} onPick={props.onPick} />
      </div>
    </Portal>
  );
}
