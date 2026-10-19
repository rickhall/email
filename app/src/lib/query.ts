// Tiny parser for the quick-search syntax (see api-contract.md). Used by the mock backend
// and for highlighting matched words in result rows. The real matching happens in core.

export type Field = 'any' | 'from' | 'to' | 'subject';

export interface Term {
  field: Field;
  /** normalized words; more than one = phrase */
  words: string[];
  neg: boolean;
}

/** Lowercase and strip accents. */
export function norm(s: string): string {
  return s.normalize('NFD').replace(/[̀-ͯ]/g, '').toLowerCase();
}

export function tokenize(s: string): string[] {
  return norm(s).split(/[^\p{L}\p{N}]+/u).filter(Boolean);
}

const TERM_RE = /(-)?(?:(from|to|subject):)?(?:"([^"]*)"?|(\S+))/gi;

export function parseQuery(q: string): Term[] {
  const out: Term[] = [];
  for (const m of q.matchAll(TERM_RE)) {
    const raw = m[3] ?? m[4] ?? '';
    const words = tokenize(raw);
    if (words.length === 0) continue;
    out.push({ field: (m[2]?.toLowerCase() as Field | undefined) ?? 'any', words, neg: !!m[1] });
  }
  return out;
}

/** Does a tokenized field contain the term (every word exact, last word as prefix, consecutive)? */
export function fieldMatches(tokens: string[], words: string[]): boolean {
  const last = words.length - 1;
  outer: for (let i = 0; i + last < tokens.length; i++) {
    for (let k = 0; k < last; k++) if (tokens[i + k] !== words[k]) continue outer;
    if (tokens[i + last]!.startsWith(words[last]!)) return true;
  }
  return false;
}

function escapeRe(s: string) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * Regex for highlighting: the positive words of the query matched as word prefixes,
 * case-insensitive. Group 1 is the preceding boundary char, group 2 the match.
 * Accent-insensitivity is approximate (plain ASCII queries only highlight ASCII prefixes).
 */
export function highlightRegex(q: string, field?: 'from' | 'subject', plainActive = true): RegExp | null {
  const words = new Set<string>();
  for (const t of parseQuery(q)) {
    if (t.neg) continue;
    // Plain terms only where the user searches; prefixed terms only in their own field.
    const applies = field == null || (t.field === 'any' ? plainActive : t.field === field);
    if (applies) for (const w of t.words) words.add(w);
  }
  if (words.size === 0) return null;
  const alt = [...words].sort((a, b) => b.length - a.length).map(escapeRe).join('|');
  return new RegExp(`(^|[^\\p{L}\\p{N}])(${alt})`, 'giu');
}

export type HlPart = { t: string; hit: boolean };

export function highlightParts(text: string, re: RegExp | null): HlPart[] {
  if (!re || !text) return [{ t: text, hit: false }];
  const parts: HlPart[] = [];
  let last = 0;
  re.lastIndex = 0;
  for (const m of text.matchAll(re)) {
    const start = (m.index ?? 0) + (m[1]?.length ?? 0);
    const len = m[2]?.length ?? 0;
    if (len === 0) continue;
    if (start > last) parts.push({ t: text.slice(last, start), hit: false });
    parts.push({ t: text.slice(start, start + len), hit: true });
    last = start + len;
  }
  if (last < text.length) parts.push({ t: text.slice(last), hit: false });
  return parts;
}
