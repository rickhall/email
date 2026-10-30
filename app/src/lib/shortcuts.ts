import { createMemo, createRoot, createSignal } from 'solid-js';
import { platform } from './platform';
import { loadPref, savePref } from './format';

// One source of truth for keyboard shortcuts: the handlers look keys up here (commandFor), the
// sheet and Settings render the sections, tooltips use hint(). The remappable keys come from a
// preset (Gmail's or Thunderbird's keys), with the user's changes from Settings on top; arrows,
// Esc, the app's own ⌘/Ctrl combos and the like are fixed.

/** "Mod" = ⌘ on macOS, Ctrl elsewhere. */
export const MOD = platform === 'macos' ? '⌘' : 'Ctrl';
const SHIFT = platform === 'macos' ? '⇧' : 'Shift';

/**
 * A key as stored: `KeyboardEvent.key` for a key pressed without ⌘/Ctrl/Alt (so `U` is
 * Shift+U and `#` is whatever produces # on the layout), or a ⌘/Ctrl combo such as "Mod+R" or
 * "Mod+Shift+R" (letters upper case, Shift spelled out).
 */
export type Key = string;

export interface Command {
  id: string;
  desc: string;
  /** Always bound, not editable (arrows, Enter, the app's ⌘ combos…). */
  fixed?: Key[];
}
export interface CommandSection {
  title: string;
  items: Command[];
}

const c = (id: string, desc: string, fixed?: Key[]): Command => ({ id, desc, fixed });

export const COMMANDS: CommandSection[] = [
  {
    title: 'Navigation',
    items: [
      c('next', 'Next message', ['ArrowDown']),
      c('prev', 'Previous message', ['ArrowUp']),
      c('next-unread', 'Next unread message'),
      c('prev-unread', 'Previous unread message'),
      c('open', 'Move focus to message', ['Enter']),
      c('first', 'First message', ['Home']),
      c('last', 'Last message', ['End']),
      c('page-up', 'Page up', ['PageUp']),
      c('page-down', 'Page down', ['PageDown']),
      c('parent', 'Parent folder'),
      c('goto', 'Go to folder…', ['Mod+G']),
      c('rail', 'Sidebar ↔ rail', ['Mod+1']),
      c('flyout', 'All folders (rail)', ['Mod+Shift+1']),
      c('escape', 'Close popup / back to the list', ['Escape']),
    ],
  },
  {
    title: 'Messages',
    items: [
      c('compose', 'New message'),
      c('reply', 'Reply'),
      c('reply-all', 'Reply all'),
      c('forward', 'Forward'),
      c('archive', 'Archive'),
      c('delete', 'Delete'),
      c('move', 'Move to…'),
      c('spam', 'Spam / not spam'),
      c('unread', 'Mark read / unread'),
      c('star', 'Star / unstar'),
      c('undo', 'Undo'),
    ],
  },
  {
    title: 'Tags',
    items: [
      c('tag-menu', 'Tags…'),
      c('tag-n', 'Toggle the tag in that position', ['1 – 9']),
      c('tag-clear', 'Remove all tags', ['0']),
    ],
  },
  {
    title: 'View',
    items: [
      c('threaded', 'Group by conversation', ['Mod+T']),
      c('scope', 'Conversations: this folder / all mail', ['Mod+Shift+A']),
      c('shortcuts', 'Keyboard shortcuts', ['?']),
      c('settings', 'Settings', ['Mod+,']),
    ],
  },
  {
    title: 'Search',
    items: [
      c('search', 'Focus search'),
      c('search-help', 'Search help (in an empty search field)', ['?']),
      c('search-clear', 'Clear search', ['Escape']),
    ],
  },
  {
    title: 'Attachments',
    items: [
      c('att-open', 'Open', ['Enter']),
      c('att-save', 'Save to Downloads', ['Shift+Enter', 'Mod+S']),
      c('att-save-all', 'Save all', ['Mod+Shift+S']),
    ],
  },
];

const byId = new Map(COMMANDS.flatMap((s) => s.items).map((i) => [i.id, i]));

// ------------------------------------------------------------- presets

export type PresetId = 'gmail' | 'thunderbird';
export const PRESETS: { id: PresetId; name: string }[] = [
  { id: 'gmail', name: 'Gmail' },
  { id: 'thunderbird', name: 'Thunderbird' },
];

/**
 * Each client's own keys for the commands both clients have. A command only one of them (or
 * neither) has gets the same key in both: that client's key where there is one.
 */
const PRESET_KEYS: Record<PresetId, Record<string, Key[]>> = {
  gmail: {
    next: ['j'],
    prev: ['k'],
    'next-unread': ['n'],
    'prev-unread': ['p'],
    open: ['o'],
    parent: ['Backspace'],
    compose: ['c'],
    reply: ['r'],
    'reply-all': ['a'],
    forward: ['f'],
    archive: ['e'],
    delete: ['#'],
    move: ['v'],
    spam: ['!'],
    unread: ['U'],
    star: ['s'],
    undo: ['z'],
    'tag-menu': ['l'],
    search: ['/'],
  },
  thunderbird: {
    next: ['f'],
    prev: ['b'],
    'next-unread': ['n'],
    'prev-unread': ['p'],
    parent: ['Backspace'],
    compose: ['Mod+N'],
    reply: ['Mod+R'],
    'reply-all': ['Mod+Shift+R'],
    forward: ['Mod+L'],
    archive: ['a'],
    delete: ['Delete'],
    move: ['v'],
    // J junk, Shift+J not junk: both toggle here
    spam: ['j', 'J'],
    unread: ['m'],
    star: ['s'],
    undo: ['Mod+Z'],
    'tag-menu': ['l'],
    search: ['Mod+K', 'Mod+Shift+K'],
  },
};

const PRESET_PREF = 'ui.keyPreset';
const [preset, setPresetSig] = createSignal<PresetId>(
  loadPref<PresetId>(PRESET_PREF, 'gmail') === 'thunderbird' ? 'thunderbird' : 'gmail',
);
export { preset };
/** The preset the keys start from (Settings resets the user's changes along with it). */
export function setPreset(id: PresetId): void {
  setPresetSig(id);
  savePref(PRESET_PREF, id);
}
export const presetName = (): string => PRESETS.find((p) => p.id === preset())?.name ?? '';

const presetKeys = (id: string): Key[] => PRESET_KEYS[preset()][id] ?? [];

// ------------------------------------------------------------- user bindings

const PREF = 'ui.keys';
/** Only commands the user changed: id → its remappable keys. */
const [overrides, setOverrides] = createSignal<Record<string, Key[]>>(loadPref<Record<string, Key[]>>(PREF, {}));

function save(next: Record<string, Key[]>) {
  setOverrides(next);
  savePref(PREF, next);
}

/** Keys the user gave to some command: the preset's binding of them gives way. */
const claimed = createRoot(() => createMemo(() => new Set(Object.values(overrides()).flat())));

/** The remappable keys currently bound to a command. */
export function keysOf(id: string): Key[] {
  const own = overrides()[id];
  if (own) return own;
  const taken = claimed();
  return presetKeys(id).filter((k) => !taken.has(k));
}
export function isDefault(id: string): boolean {
  const own = overrides()[id];
  return !own || sameKeys(own, presetKeys(id));
}
const sameKeys = (a: Key[], b: Key[]) => a.length === b.length && a.every((k, i) => k === b[i]);

/** Keys that can't be given to a command (fixed elsewhere, or with a meaning of their own). */
const RESERVED = new Set<Key>([
  'Escape', 'Enter', 'Tab', ' ', 'ArrowDown', 'ArrowUp', 'ArrowLeft', 'ArrowRight',
  'Home', 'End', 'PageUp', 'PageDown', '?', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
]);
const NAMED_OK = new Set<Key>(['Backspace', 'Delete', 'Insert', ...Array.from({ length: 12 }, (_, i) => `F${i + 1}`)]);
/** ⌘/Ctrl combos the system or the app needs (editing, windows, the fixed shortcuts above). */
const RESERVED_COMBOS = new Set<Key>([
  ...['A', 'C', 'V', 'X', 'Shift+Z', 'Q', 'W', 'H', 'M', 'Enter', '/'].map((k) => `Mod+${k}`),
  ...[...byId.values()].flatMap((cmd) => cmd.fixed ?? []).filter((k) => k.startsWith('Mod+')),
]);
/** Fixed combos the macOS menu bar handles itself (as accelerators of its items). */
const MENU_COMBOS = new Set<Key>(platform === 'macos' ? ['Mod+T', 'Mod+Shift+A'] : []);

/** A keydown as a stored key: a plain key, a "Mod+…" combo, or null (Alt, or Control on macOS). */
function keyOf(e: KeyboardEvent): Key | null {
  if (e.altKey) return null;
  const mod = platform === 'macos' ? e.metaKey : e.ctrlKey;
  if (platform === 'macos' ? e.ctrlKey : e.metaKey) return null;
  if (!mod) {
    // Caps Lock alone shouldn't turn `j` into Shift+J.
    return e.key.length === 1 && !e.shiftKey && e.getModifierState?.('CapsLock') ? e.key.toLowerCase() : e.key;
  }
  // The letter or digit itself, whatever Shift made of it (⌘⇧1 is "!" in `key`).
  const base = /^[a-z]$/i.test(e.key)
    ? e.key.toUpperCase()
    : /^(Key|Digit)./.test(e.code)
      ? e.code.slice(e.code.startsWith('Key') ? 3 : 5)
      : e.key;
  return `Mod+${e.shiftKey ? 'Shift+' : ''}${base}`;
}

export type Recorded = { ok: true; key: Key } | { ok: false; reason: string } | null;

/** The key a keydown would bind, null for a lone modifier, or why it can't be used. */
export function recordKey(e: KeyboardEvent): Recorded {
  if (['Shift', 'Control', 'Alt', 'Meta', 'CapsLock', 'Dead', 'Unidentified'].includes(e.key)) return null;
  const k = keyOf(e);
  if (!k) return { ok: false, reason: `Use a single key, or one with ${MOD}` };
  if (k.startsWith('Mod+')) {
    if (RESERVED_COMBOS.has(k)) return { ok: false, reason: `${label(k).join(' ')} is reserved` };
    return { ok: true, key: k };
  }
  if (RESERVED.has(k)) return { ok: false, reason: `${label(k).join(' ')} is reserved` };
  if (k.length === 1 || NAMED_OK.has(k)) return { ok: true, key: k };
  return { ok: false, reason: `${k} can't be used` };
}

/** The command a key is bound to (remappable keys only). */
export function ownerOf(key: Key): string | null {
  for (const cmd of byId.values()) if (keysOf(cmd.id).includes(key)) return cmd.id;
  return null;
}

/** Bind `key` to `id` (taking it from any other command); returns the command it was taken from. */
export function bindKey(id: string, key: Key): string | null {
  const from = ownerOf(key);
  const next = { ...overrides() };
  // A key the user gave `from` is taken back from it; one from the preset just gives way.
  if (from && from !== id && overrides()[from]) next[from] = keysOf(from).filter((k) => k !== key);
  if (!keysOf(id).includes(key)) next[id] = [...keysOf(id), key];
  save(normalize(next));
  return from && from !== id ? from : null;
}
export function unbindKey(id: string, key: Key): void {
  save(normalize({ ...overrides(), [id]: keysOf(id).filter((k) => k !== key) }));
}
/** Back to the preset's keys: one command, or all of them. */
export function resetKeys(id?: string): void {
  if (id == null) return save({});
  const next = { ...overrides() };
  delete next[id];
  save(next);
}
/** Drop overrides that equal the preset's keys. */
function normalize(o: Record<string, Key[]>): Record<string, Key[]> {
  const out: Record<string, Key[]> = {};
  for (const [id, keys] of Object.entries(o)) if (!sameKeys(keys, presetKeys(id))) out[id] = keys;
  return out;
}

const keyToCommand = createRoot(() =>
  createMemo(() => {
    const m = new Map<Key, string>();
    for (const cmd of byId.values()) {
      for (const k of cmd.fixed ?? []) if (!MENU_COMBOS.has(k) && !m.has(k)) m.set(k, cmd.id);
      for (const k of keysOf(cmd.id)) m.set(k, cmd.id);
    }
    return m;
  }),
);

/** The command for a keydown (fixed or remappable), else null. */
export function commandFor(e: KeyboardEvent): string | null {
  const k = keyOf(e);
  return k ? (keyToCommand().get(k) ?? null) : null;
}

// ------------------------------------------------------------- display

const NAMES: Record<string, string> = {
  ArrowDown: '↓', ArrowUp: '↑', ArrowLeft: '←', ArrowRight: '→', Escape: 'Esc', ' ': 'Space',
  Mod: MOD, Shift: SHIFT,
};

/** A key as keycaps: "j" → ["J"], "U" → [⇧, "U"], "Mod+Shift+S" → [⌘, ⇧, "S"]. */
export function label(key: Key): string[] {
  if (key.length > 1 && key.includes('+') && key !== '+') {
    return key.split('+').map((p) => NAMES[p] ?? (p.length === 1 ? p.toUpperCase() : p));
  }
  if (key.length === 1 && key !== key.toLowerCase()) return [SHIFT, key];
  return [NAMES[key] ?? (key.length === 1 ? key.toUpperCase() : key)];
}

/** All keys of a command for display: remappable first, then fixed. */
export function allKeys(id: string): Key[] {
  return [...keysOf(id), ...(byId.get(id)?.fixed ?? [])];
}

/** Short text for tooltips, e.g. hint('archive') → "E", hint('delete') → "# / Delete". */
export function hint(id: string): string {
  return allKeys(id)
    .map((k) => label(k).join(platform === 'macos' ? '' : '+'))
    .join(' / ');
}
