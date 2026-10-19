import { createSignal } from 'solid-js';

// ------------------------------------------------------------- smart dates

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' });
const weekdayFmt = new Intl.DateTimeFormat(undefined, { weekday: 'long' });
const monthDayFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });
const fullDateFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', year: 'numeric' });
const longFmt = new Intl.DateTimeFormat(undefined, {
  weekday: 'long',
  month: 'long',
  day: 'numeric',
  year: 'numeric',
  hour: 'numeric',
  minute: '2-digit',
});

function startOfToday(): number {
  const d = new Date();
  d.setHours(0, 0, 0, 0);
  return d.getTime() / 1000;
}

/** Bumps at local midnight so rows re-render their relative dates. */
const [dayStart, setDayStart] = createSignal(startOfToday());
let cache = new Map<number, string>();
setInterval(() => {
  const s = startOfToday();
  if (s !== dayStart()) {
    cache = new Map();
    setDayStart(s);
  }
}, 60_000);

export function smartDate(ts: number): string {
  const today = dayStart(); // tracked
  const hit = cache.get(ts);
  if (hit !== undefined) return hit;
  const d = new Date(ts * 1000);
  let out: string;
  if (ts >= today) out = timeFmt.format(d);
  else if (ts >= today - 86400) out = 'Yesterday';
  else if (ts >= today - 6 * 86400) out = weekdayFmt.format(d);
  else if (d.getFullYear() === new Date(today * 1000).getFullYear()) out = monthDayFmt.format(d);
  else out = fullDateFmt.format(d);
  if (cache.size > 20_000) cache = new Map();
  cache.set(ts, out);
  return out;
}

export function longDate(ts: number): string {
  return longFmt.format(new Date(ts * 1000));
}

// ------------------------------------------------------------- numbers / sizes

const numFmt = new Intl.NumberFormat();
export const fmtNum = (n: number) => numFmt.format(n);

export function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

// ------------------------------------------------------------- avatars

function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export function avatarColor(addr: string): string {
  const h = hash(addr.toLowerCase());
  const hue = h % 360;
  const sat = 52 + (h >> 9) % 18;
  return `hsl(${hue} ${sat}% 52%)`;
}

export function initials(name: string | null | undefined, addr: string): string {
  const src = (name && name.trim()) || addr.split('@')[0] || '?';
  const words = src
    .replace(/["'()[\]<>]/g, '')
    .split(/[\s._-]+/)
    .filter(Boolean);
  if (words.length === 0) return '?';
  const first = [...(words[0] ?? '')][0] ?? '';
  const last = words.length > 1 ? ([...(words[words.length - 1] ?? '')][0] ?? '') : '';
  return (first + last).toUpperCase();
}

// ------------------------------------------------------------- linkify

export type TextPart = { t: 'text'; v: string } | { t: 'link'; v: string; href: string };

const URL_RE = /\b(?:https?:\/\/|www\.)[^\s<>"']+|\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi;

export function linkify(text: string): TextPart[] {
  const parts: TextPart[] = [];
  let last = 0;
  for (const m of text.matchAll(URL_RE)) {
    let url = m[0];
    // Trim trailing punctuation that is almost never part of the URL.
    const trail = /[.,;:!?)\]}’]+$/.exec(url);
    if (trail) url = url.slice(0, -trail[0].length);
    const start = m.index ?? 0;
    if (start > last) parts.push({ t: 'text', v: text.slice(last, start) });
    const href = url.includes('@') && !url.includes('/') ? `mailto:${url}` : url.startsWith('www.') ? `https://${url}` : url;
    parts.push({ t: 'link', v: url, href });
    last = start + url.length;
  }
  if (last < text.length) parts.push({ t: 'text', v: text.slice(last) });
  return parts;
}

// ------------------------------------------------------------- storage

export function loadPref<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw == null ? fallback : (JSON.parse(raw) as T);
  } catch {
    return fallback;
  }
}

export function savePref(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    /* storage unavailable */
  }
}
