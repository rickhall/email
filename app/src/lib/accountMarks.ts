// An account's marker (the tinted letter in the sidebar, rail and folder switcher), made to
// tell accounts apart: two addresses starting with the same letter, whose colours happen to be
// alike, would otherwise look the same. The user can set either (Settings); what's left is
// picked to differ from those. Import-free, for the unit tests.

/** FNV-1a: a stable number for a string (the colour of an address). */
export function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

/** The colours a marker can be set to (stored by name), each readable under white text in
 *  both themes, like the automatic ones (same lightness). */
export const MARK_COLORS = [
  { name: 'red', label: 'Red', hue: 2, sat: 66 },
  { name: 'orange', label: 'Orange', hue: 24, sat: 78 },
  { name: 'amber', label: 'Amber', hue: 40, sat: 72 },
  { name: 'green', label: 'Green', hue: 130, sat: 45 },
  { name: 'teal', label: 'Teal', hue: 172, sat: 55 },
  { name: 'cyan', label: 'Cyan', hue: 195, sat: 68 },
  { name: 'blue', label: 'Blue', hue: 215, sat: 68 },
  { name: 'indigo', label: 'Indigo', hue: 240, sat: 52 },
  { name: 'purple', label: 'Purple', hue: 272, sat: 50 },
  { name: 'pink', label: 'Pink', hue: 325, sat: 60 },
  { name: 'slate', label: 'Slate', hue: 215, sat: 14 },
] as const;

const css = (hue: number, sat: number) => `hsl(${Math.round(hue)} ${sat}% 52%)`;

export interface AccountMark {
  letter: string;
  color: string;
  /** what it would be without the user's setting (shown as "Automatic" in Settings) */
  autoLetter: string;
  autoColor: string;
}

export interface MarkedAccount {
  id: number;
  email: string;
  markLetter?: string | null;
  markColor?: string | null;
}

/** Closer hues than this (degrees) read as the same colour at marker size. */
const MIN_HUE_GAP = 40;
const GOLDEN_ANGLE = 137.5;

const hueGap = (a: number, b: number) => {
  const d = Math.abs(a - b) % 360;
  return Math.min(d, 360 - d);
};

const paletteColor = (name: string | null | undefined) => MARK_COLORS.find((c) => c.name === name);

/**
 * Each account's letter and colour. What the user set is kept as is, and the rest steer clear
 * of it. Automatic ones are given in id order (the order accounts were added), each keeping
 * what it would get alone unless an earlier one has it: the letter falls back to the domain's
 * initial, the colour turns round the wheel until it's clear of the others. So adding an
 * account never changes the existing ones.
 */
export function accountMarks(accounts: readonly MarkedAccount[]): Map<number, AccountMark> {
  const marks = new Map<number, AccountMark>();
  const letters = new Set<string>();
  const hues: number[] = [];
  const setLetter = (a: MarkedAccount) => a.markLetter?.trim().toUpperCase() || null;
  for (const a of accounts) {
    const l = setLetter(a);
    if (l) letters.add(l);
    const c = paletteColor(a.markColor);
    if (c && c.sat > 25) hues.push(c.hue);
  }
  for (const a of [...accounts].sort((x, y) => x.id - y.id)) {
    const email = a.email.toLowerCase();
    const [local = '', domain = ''] = email.split('@');
    const candidates = [local[0], domain[0]].filter(Boolean).map((c) => c!.toUpperCase());
    const autoLetter = candidates.find((c) => !letters.has(c)) ?? candidates[0] ?? '?';
    const letter = setLetter(a);
    letters.add(letter ?? autoLetter);

    const h = hash(email);
    const base = h % 360;
    let best = { hue: base, gap: -1 };
    for (let i = 0; i < 12; i++) {
      const hue = (base + i * GOLDEN_ANGLE) % 360;
      const gap = Math.min(360, ...hues.map((o) => hueGap(o, hue)));
      if (gap > best.gap) best = { hue, gap };
      if (gap >= MIN_HUE_GAP) break;
    }
    const autoColor = css(best.hue, 52 + ((h >> 9) % 18));
    const set = paletteColor(a.markColor);
    if (!set) hues.push(best.hue);
    marks.set(a.id, {
      letter: letter ?? autoLetter,
      color: set ? css(set.hue, set.sat) : autoColor,
      autoLetter,
      autoColor,
    });
  }
  return marks;
}
