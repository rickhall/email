import { platform } from './platform';

// One source of truth for keyboard shortcuts: the sheet renders this list, tooltips use hint().

/** "Mod" = ⌘ on macOS, Ctrl elsewhere. */
export const MOD = platform === 'macos' ? '⌘' : 'Ctrl';
const SHIFT = platform === 'macos' ? '⇧' : 'Shift';

/** A combo is a list of keys pressed together; an entry may offer alternatives. */
type Combo = string[];
export interface Shortcut {
  id: string;
  keys: Combo[];
  desc: string;
}
export interface ShortcutSection {
  title: string;
  items: Shortcut[];
}

const k = (...alts: (string | string[])[]): Combo[] => alts.map((a) => (Array.isArray(a) ? a : [a]));

export const SHORTCUTS: ShortcutSection[] = [
  {
    title: 'Navigation',
    items: [
      { id: 'next', keys: k('J', '↓'), desc: 'Next message' },
      { id: 'prev', keys: k('K', '↑'), desc: 'Previous message' },
      { id: 'open', keys: k('Enter', 'O'), desc: 'Open in the reader' },
      { id: 'ends', keys: k('Home', 'End'), desc: 'First / last message' },
      { id: 'page', keys: k('PageUp', 'PageDown'), desc: 'Page up / down' },
      { id: 'parent', keys: k('Backspace'), desc: 'Parent folder' },
      { id: 'goto', keys: k([MOD, 'G']), desc: 'Go to folder…' },
      { id: 'rail', keys: k([MOD, '1']), desc: 'Sidebar ↔ rail' },
      { id: 'flyout', keys: k([MOD, SHIFT, '1']), desc: 'All folders (rail)' },
      { id: 'escape', keys: k('Esc'), desc: 'Close popup / back to the list' },
    ],
  },
  {
    title: 'Messages',
    items: [
      { id: 'archive', keys: k('E'), desc: 'Archive' },
      { id: 'delete', keys: k('#', 'Delete'), desc: 'Delete' },
      { id: 'move', keys: k('V'), desc: 'Move to…' },
      { id: 'spam', keys: k('!'), desc: 'Spam / not spam' },
      { id: 'unread', keys: k([SHIFT, 'U']), desc: 'Mark read / unread' },
      { id: 'star', keys: k('S'), desc: 'Star / unstar' },
      { id: 'undo', keys: k('Z'), desc: 'Undo' },
    ],
  },
  {
    title: 'Tags',
    items: [
      { id: 'tag-n', keys: k('1 – 9'), desc: 'Toggle the tag in that position' },
      { id: 'tag-clear', keys: k('0'), desc: 'Remove all tags' },
    ],
  },
  {
    title: 'View',
    items: [
      { id: 'threaded', keys: k('T'), desc: 'Group by conversation' },
      { id: 'scope', keys: k('A'), desc: 'Conversations: this folder / all mail' },
      { id: 'shortcuts', keys: k('?'), desc: 'This sheet' },
    ],
  },
  {
    title: 'Search',
    items: [
      { id: 'search', keys: k('/', [MOD, 'F']), desc: 'Focus search' },
      { id: 'search-help', keys: k('?'), desc: 'Search help (in an empty search field)' },
      { id: 'search-clear', keys: k('Esc'), desc: 'Clear search' },
    ],
  },
  {
    title: 'Attachments',
    items: [
      { id: 'att-open', keys: k('Enter'), desc: 'Open' },
      { id: 'att-save', keys: k([SHIFT, 'Enter'], [MOD, 'S']), desc: 'Save to Downloads' },
      { id: 'att-save-all', keys: k([MOD, SHIFT, 'S']), desc: 'Save all' },
    ],
  },
];

const byId = new Map(SHORTCUTS.flatMap((s) => s.items).map((i) => [i.id, i]));

/** Short text for tooltips, e.g. hint('archive') → "E", hint('delete') → "# / Delete". */
export function hint(id: string): string {
  const s = byId.get(id);
  return s ? s.keys.map((c) => c.join(platform === 'macos' ? '' : '+')).join(' / ') : '';
}
