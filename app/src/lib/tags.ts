import { createRoot, createSignal } from 'solid-js';
import { api, errorMessage, events, type TagDef } from '../api';

// Tag definitions (keyword → name/colour). Keywords not known yet render with their keyword
// and a neutral colour until the next refresh.

const [tagList, setTagList] = createSignal<TagDef[]>([]);
export { tagList };

const NEUTRAL = '#8E8E93';

export function tagInfo(keyword: string): TagDef {
  return tagList().find((t) => t.keyword === keyword) ?? { keyword, name: keyword, color: NEUTRAL, position: 9999, defined: false };
}

/** Keywords ordered by their definition position (Thunderbird: the first one tints the row). */
export function sortTags(keywords: readonly string[]): string[] {
  return [...keywords].sort((a, b) => tagInfo(a).position - tagInfo(b).position);
}

let inflight: Promise<void> | null = null;
export function refreshTags(): Promise<void> {
  if (inflight) return inflight;
  inflight = api
    .listTags()
    .then((l) => {
      setTagList([...l].sort((a, b) => Number(b.defined) - Number(a.defined) || a.position - b.position));
    })
    .catch((e) => console.warn('list_tags failed', errorMessage(e)))
    .finally(() => (inflight = null));
  return inflight;
}

createRoot(() => {
  void refreshTags();
  void events.onFoldersChanged(() => void refreshTags());
});

// ------------------------------------------------------------- readable tints

function rgb(hex: string): [number, number, number] | null {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return null;
  const n = parseInt(m[1]!, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
function lum([r, g, b]: [number, number, number]) {
  const f = (c: number) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b);
}
const contrast = (a: number, b: number) => (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
function mix(c: [number, number, number], w: [number, number, number], t: number): [number, number, number] {
  return [0, 1, 2].map((i) => Math.round(c[i]! + (w[i]! - c[i]!) * t)) as [number, number, number];
}
const hex = (c: [number, number, number]) => `#${c.map((x) => x.toString(16).padStart(2, '0')).join('')}`;

const cache = new Map<string, { light: string; dark: string }>();
/** The tag colour adjusted to ≥3.5:1 contrast on the light (#fff) and dark (#1c1c1f) list. */
export function tagTint(color: string): { light: string; dark: string } {
  const hit = cache.get(color);
  if (hit) return hit;
  const c = rgb(color);
  if (!c) return { light: color, dark: color };
  const fit = (bg: [number, number, number], toward: [number, number, number]) => {
    const lb = lum(bg);
    let x = c;
    for (let t = 0; t <= 1 && contrast(lum(x), lb) < 3.5; t += 0.1) x = mix(c, toward, t);
    return hex(x);
  };
  const out = { light: fit([255, 255, 255], [0, 0, 0]), dark: fit([28, 28, 31], [255, 255, 255]) };
  cache.set(color, out);
  return out;
}
