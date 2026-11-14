// Times of day typed by hand ("9", "9:30", "2pm", "14:15", "noon") for the snooze picker, and
// a 6-week month grid for its calendar. Import-free, for the unit tests.

/** Minutes after midnight for a typed time, or null. Without am/pm, 1–6 mean the afternoon
 *  (nobody snoozes to 3 at night by accident); `h24` takes them as written. */
export function parseTime(text: string, h24 = false): number | null {
  const t = text.trim().toLowerCase().replace(/\./g, '');
  if (t === 'noon') return 12 * 60;
  if (t === 'midnight') return 0;
  const m = /^(\d{1,2})(?:[:h ]?(\d{2}))?\s*(am|pm|a|p)?$/.exec(t);
  if (!m) return null;
  let h = Number(m[1]);
  const min = m[2] ? Number(m[2]) : 0;
  const ap = m[3]?.[0];
  if (min > 59 || h > 23) return null;
  if (ap) {
    if (h < 1 || h > 12) return null;
    h = (h % 12) + (ap === 'p' ? 12 : 0);
  } else if (!h24 && h >= 1 && h <= 6) {
    h += 12;
  }
  return h * 60 + min;
}

/** `minutes` stepped by `step` (wrapping round the day), kept on the step's grid. */
export function stepTime(minutes: number, step: number): number {
  const day = 24 * 60;
  const size = Math.abs(step);
  // Off the grid: to the next mark that way; on it, one step.
  const next =
    minutes % size === 0 ? minutes + step : step > 0 ? Math.ceil(minutes / size) * size : Math.floor(minutes / size) * size;
  return ((next % day) + day) % day;
}

/** The days shown for a month: from the week holding its 1st, 6 weeks (`firstDay`: 0 Sunday,
 *  1 Monday). */
export function monthGrid(year: number, month: number, firstDay = 0): Date[] {
  const first = new Date(year, month, 1);
  const back = (first.getDay() - firstDay + 7) % 7;
  return Array.from({ length: 42 }, (_, i) => new Date(year, month, 1 - back + i));
}
