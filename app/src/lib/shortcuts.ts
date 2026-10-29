import { createMemo, createRoot, createSignal } from 'solid-js';
import { platform } from './platform';
import { loadPref, savePref } from './format';

// One source of truth for keyboard shortcuts: the handlers look keys up here (commandFor), the
// sheet and Settings render the sections, tooltips use hint(). Single keys are remappable in
// Settings; ⌘/Ctrl combos (macOS menu accelerators too), arrows, Esc and the like are fixed.

/** "Mod" = ⌘ on macOS, Ctrl elsewhere. */
export const MOD = platform === 'macos' ? '⌘' : 'Ctrl';
const SHIFT = platform === 'macos' ? '⇧' : 'Shift';

/**
 * A key as stored: `KeyboardEvent.key` for a key pressed without ⌘/Ctrl/Alt (so `U` is
 * Shift+U and `#` is whatever produces # on the layout), or a display-only combo such as
 * "Mod+G" for the fixed modifier shortcuts.
 */
export type Key = string;

export interface Command {
  id: string;
  desc: string;
  /** Default remappable keys. */
  keys: Key[];
  /** Always bound, not editable (arrows, Enter, ⌘ combos…). */
  fixed?: Key[];
}
export interface CommandSection {
  title: string;
  items: Command[];
}

const c = (id: string, desc: string, keys: Key[], fixed?: Key[]): Command => ({ id, desc, keys, fixed });

export const COMMANDS: CommandSection[] = [
  {
    title: 'Navigation',
    items: [
      c('next', 'Next message', ['j'], ['ArrowDown']),
      c('prev', 'Previous message', ['k'], ['ArrowUp']),
      c('next-unread', 'Next unread message', ['n']),
      c('open', 'Open in the reader', ['o'], ['Enter']),
      c('first', 'First message', [], ['Home']),
      c('last', 'Last message', [], ['End']),
      c('page-up', 'Page up', [], ['PageUp']),
      c('page-down', 'Page down', [], ['PageDown']),
      c('parent', 'Parent folder', ['Backspace']),
      c('goto', 'Go to folder…', [], ['Mod+G']),
      c('rail', 'Sidebar ↔ rail', [], ['Mod+1']),
      c('flyout', 'All folders (rail)', [], ['Mod+Shift+1']),
      c('escape', 'Close popup / back to the list', [], ['Escape']),
    ],
  },
  {
    title: 'Messages',
    items: [
      c('compose', 'New message', ['c'], ['Mod+N']),
      c('archive', 'Archive', ['e']),
      c('delete', 'Delete', ['#'], ['Delete']),
      c('move', 'Move to…', ['v']),
      c('spam', 'Spam / not spam', ['!']),
      c('unread', 'Mark read / unread', ['U']),
      c('star', 'Star / unstar', ['s']),
      c('undo', 'Undo', ['z']),
    ],
  },
  {
    title: 'Tags',
    items: [
      c('tag-n', 'Toggle the tag in that position', [], ['1 – 9']),
      c('tag-clear', 'Remove all tags', [], ['0']),
    ],
  },
  {
    title: 'View',
    items: [
      c('threaded', 'Group by conversation', ['t']),
      c('scope', 'Conversations: this folder / all mail', ['a']),
      c('shortcuts', 'Keyboard shortcuts', [], ['?']),
      c('settings', 'Settings', [], ['Mod+,']),
    ],
  },
  {
    title: 'Search',
    items: [
      c('search', 'Focus search', ['/'], ['Mod+F']),
      c('search-help', 'Search help (in an empty search field)', [], ['?']),
      c('search-clear', 'Clear search', [], ['Escape']),
    ],
  },
  {
    title: 'Attachments',
    items: [
      c('att-open', 'Open', [], ['Enter']),
      c('att-save', 'Save to Downloads', [], ['Shift+Enter', 'Mod+S']),
      c('att-save-all', 'Save all', [], ['Mod+Shift+S']),
    ],
  },
];

const byId = new Map(COMMANDS.flatMap((s) => s.items).map((i) => [i.id, i]));

// ------------------------------------------------------------- user bindings

const PREF = 'ui.keys';
/** Only commands the user changed: id → its remappable keys. */
const [overrides, setOverrides] = createSignal<Record<string, Key[]>>(loadPref<Record<string, Key[]>>(PREF, {}));

function save(next: Record<string, Key[]>) {
  setOverrides(next);
  savePref(PREF, next);
}

/** The remappable keys currently bound to a command. */
export function keysOf(id: string): Key[] {
  return overrides()[id] ?? byId.get(id)?.keys ?? [];
}
export function isDefault(id: string): boolean {
  return !(id in overrides());
}

/** Keys that can't be given to a command (fixed elsewhere, or with a meaning of their own). */
const RESERVED = new Set<Key>([
  'Escape', 'Enter', 'Tab', ' ', 'ArrowDown', 'ArrowUp', 'ArrowLeft', 'ArrowRight',
  'Home', 'End', 'PageUp', 'PageDown', 'Delete', '?', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
]);
const NAMED_OK = new Set<Key>(['Backspace', 'Insert', ...Array.from({ length: 12 }, (_, i) => `F${i + 1}`)]);

export type Recorded = { ok: true; key: Key } | { ok: false; reason: string } | null;

/** The key a keydown would bind, null for a lone modifier, or why it can't be used. */
export function recordKey(e: KeyboardEvent): Recorded {
  if (['Shift', 'Control', 'Alt', 'Meta', 'CapsLock', 'Dead', 'Unidentified'].includes(e.key)) return null;
  if (e.metaKey || e.ctrlKey || e.altKey) return { ok: false, reason: `Use a single key, without ${MOD} or ${platform === 'macos' ? '⌥' : 'Alt'}` };
  if (RESERVED.has(e.key)) return { ok: false, reason: `${label(e.key).join(' ')} is reserved` };
  if (e.key.length === 1 || NAMED_OK.has(e.key)) return { ok: true, key: e.key };
  return { ok: false, reason: `${e.key} can't be used` };
}

/** The command a key is bound to (remappable keys only). */
export function ownerOf(key: Key): string | null {
  return keyToCommand().get(key) ?? null;
}

/** Bind `key` to `id` (taking it from any other command); returns the command it was taken from. */
export function bindKey(id: string, key: Key): string | null {
  const from = ownerOf(key);
  const next = { ...overrides() };
  if (from && from !== id) next[from] = keysOf(from).filter((k) => k !== key);
  if (!keysOf(id).includes(key)) next[id] = [...keysOf(id), key];
  save(normalize(next));
  return from && from !== id ? from : null;
}
export function unbindKey(id: string, key: Key): void {
  save(normalize({ ...overrides(), [id]: keysOf(id).filter((k) => k !== key) }));
}
export function resetKeys(id?: string): void {
  if (id == null) return save({});
  const next = { ...overrides() };
  delete next[id];
  save(next);
}
/** Drop overrides that equal the defaults. */
function normalize(o: Record<string, Key[]>): Record<string, Key[]> {
  const out: Record<string, Key[]> = {};
  for (const [id, keys] of Object.entries(o)) {
    const def = byId.get(id)?.keys ?? [];
    if (keys.length !== def.length || keys.some((k, i) => k !== def[i])) out[id] = keys;
  }
  return out;
}

const keyToCommand = createRoot(() =>
  createMemo(() => {
    const m = new Map<Key, string>();
    for (const cmd of byId.values()) {
      for (const k of cmd.fixed ?? []) if (!k.includes('+') && !m.has(k)) m.set(k, cmd.id);
      for (const k of keysOf(cmd.id)) m.set(k, cmd.id);
    }
    return m;
  }),
);

/** The command for a keydown without ⌘/Ctrl/Alt (fixed or remappable), else null. */
export function commandFor(e: KeyboardEvent): string | null {
  if (e.metaKey || e.ctrlKey || e.altKey) return null;
  // Caps Lock alone shouldn't turn `j` into Shift+J.
  const k = e.key.length === 1 && !e.shiftKey && e.getModifierState?.('CapsLock') ? e.key.toLowerCase() : e.key;
  return keyToCommand().get(k) ?? null;
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
