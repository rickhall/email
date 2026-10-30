// Conservative detection of quoted reply history.
//
// The one hard rule: NEVER collapse when the author wrote inline (interleaved with the quote)
// or below it. When in doubt, leave the message alone. Pure functions; the HTML part needs a
// DOMParser (browser), the plain-text part runs anywhere (see scripts/quotes-selfcheck.js).

/** Minimum size of a quote worth collapsing. */
const MIN_QUOTE_CHARS = 200;
const MIN_QUOTE_LINES = 3;
/** An attribution ("On … wrote:") is short and ends with a colon. */
const MAX_ATTRIBUTION_CHARS = 200;

const ZW = /[​‌‍﻿]/g;
const isBlank = (s: string) => s.replace(ZW, '').trim() === '';
const looksLikeAttribution = (s: string) => {
  const t = s.replace(ZW, '').trim();
  return t.length > 0 && t.length < MAX_ATTRIBUTION_CHARS && /:\s*$/.test(t);
};

// ===================================================================== HTML

const QUOTE_SEL = 'blockquote, div.gmail_quote';
const SUMMARY_HTML =
  '<summary><span class="mq-show">⋯ Show quoted text</span><span class="mq-hide">Hide quoted text</span></summary>';

/** CSS for the collapsed-quote pill inside the message iframe. */
export const QUOTE_FRAME_CSS = `
details.mail-quote{margin:.8em 0}
details.mail-quote>summary{list-style:none;display:inline-flex;align-items:center;cursor:pointer;font:500 11.5px/18px -apple-system,"SF Pro Text","Inter",system-ui,sans-serif;padding:1px 10px;border-radius:9px;background:#eef0f3;color:#5c6170;user-select:none;-webkit-user-select:none}
details.mail-quote>summary:hover{background:#e3e6eb;color:#3b3f48}
details.mail-quote>summary::-webkit-details-marker{display:none}
details.mail-quote>summary::marker{content:''}
details.mail-quote .mq-hide{display:none}
details.mail-quote[open]>summary .mq-show{display:none}
details.mail-quote[open]>summary .mq-hide{display:inline}
details.mail-quote[open]>summary{margin-bottom:.6em}
`;

/** Text of a DOM range, ignoring <style>/<script>; images count as content. */
function rangeHasContent(doc: Document, setup: (r: Range) => void): boolean {
  const r = doc.createRange();
  setup(r);
  const frag = r.cloneContents();
  frag.querySelectorAll('style, script, template, title, meta, link').forEach((e) => e.remove());
  // Media counts as content even without text (e.g. an image signature after the quote).
  if (frag.querySelector('img, video, iframe, object, embed, svg, canvas')) return true;
  return !isBlank(frag.textContent ?? '');
}

/** Outermost quote regions (a gmail_quote div counts once, not its inner blockquote). */
export function findQuoteRegions(body: HTMLElement): Element[] {
  return [...body.querySelectorAll(QUOTE_SEL)].filter((el) => !el.parentElement?.closest(QUOTE_SEL));
}

function isSubstantial(region: Element): boolean {
  const text = (region.textContent ?? '').replace(ZW, '').trim();
  if (text.length >= MIN_QUOTE_CHARS) return true;
  const breaks = region.querySelectorAll('br, p, div, li, tr').length;
  const lines = text.split('\n').filter((l) => l.trim()).length;
  return Math.max(breaks, lines) >= MIN_QUOTE_LINES;
}

/**
 * Attribution right before the region: the immediately preceding element sibling if it's
 * .moz-cite-prefix / .gmail_attr / a short block ending with ":", or a trailing text line
 * (after the last <br>) ending with ":". Returns the nodes to move into the collapsed part.
 */
function findAttribution(region: Element): Node[] {
  const nodes: Node[] = [];
  let n = region.previousSibling;
  // Skip whitespace-only text between the attribution and the quote.
  while (n && n.nodeType === Node.TEXT_NODE && isBlank(n.textContent ?? '')) n = n.previousSibling;
  if (!n) return nodes;
  if (n.nodeType === Node.ELEMENT_NODE) {
    const el = n as Element;
    if (el.tagName === 'BR') {
      // "On … wrote:<br><blockquote>" — gather the text line before the <br>.
      const line: Node[] = [el];
      let p = el.previousSibling;
      while (p && !(p.nodeType === Node.ELEMENT_NODE && ((p as Element).tagName === 'BR' || isBlock(p as Element)))) {
        line.unshift(p);
        p = p.previousSibling;
      }
      const text = line.map((x) => x.textContent ?? '').join('');
      return looksLikeAttribution(text) ? line : nodes;
    }
    if (el.matches('.moz-cite-prefix, .gmail_attr') || (isBlock(el) && looksLikeAttribution(el.textContent ?? ''))) {
      return [el];
    }
    return nodes;
  }
  if (n.nodeType === Node.TEXT_NODE) {
    // Bare text directly before the quote, on a line of its own.
    const t = n.textContent ?? '';
    const prev = n.previousSibling;
    const startsLine =
      !prev || (prev.nodeType === Node.ELEMENT_NODE && ((prev as Element).tagName === 'BR' || isBlock(prev as Element)));
    if (startsLine && !t.trim().includes('\n') && looksLikeAttribution(t)) return [n];
  }
  return nodes;
}

const BLOCKS = new Set(['P', 'DIV', 'SECTION', 'ARTICLE', 'HEADER', 'FOOTER', 'TABLE', 'UL', 'OL', 'LI', 'H1', 'H2', 'H3', 'H4', 'H5', 'H6', 'PRE', 'HR', 'BLOCKQUOTE']);
function isBlock(el: Element) {
  return BLOCKS.has(el.tagName);
}

export interface HtmlQuoteDecision {
  collapse: boolean;
  reason: string;
}

/** Decide (without modifying) whether this document's quote history may be collapsed. */
export function decideHtml(doc: Document): HtmlQuoteDecision & { region?: Element; attribution?: Node[] } {
  const body = doc.body;
  if (!body) return { collapse: false, reason: 'no body' };
  const regions = findQuoteRegions(body);
  // Outlook history (no quote markup) vetoes collapsing only when it is this message's own
  // structure. Markers *inside* a quote are just an older Outlook message being quoted.
  const outlook = [...body.querySelectorAll('#divRplyFwdMsg, #appendonsend')].some(
    (el) => !regions.some((r) => r.contains(el)),
  );
  if (outlook) {
    return { collapse: false, reason: 'outlook-style history' };
  }
  if (regions.length === 0) return { collapse: false, reason: 'no quote' };
  if (regions.length > 1) return { collapse: false, reason: 'several quotes (inline reply)' };
  const region = regions[0]!;
  const attribution = findAttribution(region);
  const first = attribution[0] ?? region;
  if (rangeHasContent(doc, (r) => { r.setStartAfter(region); r.setEnd(body, body.childNodes.length); })) {
    return { collapse: false, reason: 'content after quote (bottom/inline reply)' };
  }
  if (!rangeHasContent(doc, (r) => { r.setStart(body, 0); r.setEndBefore(first); })) {
    return { collapse: false, reason: 'nothing written before quote' };
  }
  if (!isSubstantial(region)) return { collapse: false, reason: 'quote too short' };
  return { collapse: true, reason: 'top-post with trailing history', region, attribution };
}

/**
 * Wrap a trailing quote history in <details class="mail-quote"> when it is safe to do so.
 * Returns the input unchanged in every other case.
 */
export function collapseHtmlQuotes(html: string): string {
  if (typeof DOMParser === 'undefined' || !/blockquote|gmail_quote/i.test(html)) return html;
  let doc: Document;
  try {
    doc = new DOMParser().parseFromString(html, 'text/html');
  } catch {
    return html;
  }
  const d = decideHtml(doc);
  if (!d.collapse || !d.region) return html;
  const details = doc.createElement('details');
  details.className = 'mail-quote';
  details.innerHTML = SUMMARY_HTML;
  const first = d.attribution?.[0] ?? d.region;
  first.parentNode!.insertBefore(details, first);
  for (const n of d.attribution ?? []) details.appendChild(n);
  details.appendChild(d.region);
  // Keep head styles and <body> attributes (the outer srcdoc parser merges them).
  return doc.head.innerHTML + doc.body.outerHTML;
}

// ===================================================================== plain text

export type QNode = { t: 'text'; text: string } | { t: 'quote'; children: QNode[] };

export interface PlainQuoteResult {
  /** rendered as-is */
  head: QNode[];
  /** collapsed behind the pill (attribution + quote), or null */
  collapsed: QNode[] | null;
  reason: string;
}

interface Line {
  level: number;
  text: string;
}

const QUOTE_PREFIX = /^(?:[ \t]*>)+[ \t]?/;

export function parseLines(text: string): Line[] {
  const lines = text.replace(/\r\n?/g, '\n').split('\n').map((raw) => {
    const m = QUOTE_PREFIX.exec(raw);
    if (!m) return { level: 0, text: raw };
    return { level: (m[0].match(/>/g) ?? []).length, text: raw.slice(m[0].length) };
  });
  // Blank lines between two quoted lines belong to the quote block.
  for (let i = 0; i < lines.length; i++) {
    const l = lines[i]!;
    if (l.level !== 0 || !isBlank(l.text)) continue;
    let a = i - 1;
    while (a >= 0 && lines[a]!.level === 0 && isBlank(lines[a]!.text)) a--;
    let b = i + 1;
    while (b < lines.length && lines[b]!.level === 0 && isBlank(lines[b]!.text)) b++;
    const la = a >= 0 ? lines[a]!.level : 0;
    const lb = b < lines.length ? lines[b]!.level : 0;
    if (la > 0 && lb > 0) l.level = Math.min(la, lb);
  }
  return lines;
}

/** Lines → nested nodes (consecutive lines of a level become one text node). */
export function buildTree(lines: Line[], depth = 0): QNode[] {
  const out: QNode[] = [];
  let i = 0;
  while (i < lines.length) {
    const l = lines[i]!;
    if (l.level <= depth) {
      const buf: string[] = [];
      while (i < lines.length && lines[i]!.level <= depth) buf.push(lines[i++]!.text);
      out.push({ t: 'text', text: buf.join('\n') });
    } else {
      const start = i;
      while (i < lines.length && lines[i]!.level > depth) i++;
      out.push({ t: 'quote', children: buildTree(lines.slice(start, i), depth + 1) });
    }
  }
  return out;
}

export function analyzePlain(text: string): PlainQuoteResult {
  const lines = parseLines(text);
  const all = (reason: string): PlainQuoteResult => ({ head: buildTree(trimTrailingBlank(lines)), collapsed: null, reason });
  // Top-level quote blocks = maximal runs of quoted lines.
  const blocks: [number, number][] = [];
  for (let i = 0; i < lines.length; ) {
    if (lines[i]!.level > 0) {
      const s = i;
      while (i < lines.length && lines[i]!.level > 0) i++;
      blocks.push([s, i]);
    } else i++;
  }
  if (blocks.length === 0) return all('no quote');
  if (blocks.length > 1) return all('several quotes (inline reply)');
  const [qs, qe] = blocks[0]!;
  if (lines.slice(qe).some((l) => !isBlank(l.text))) return all('content after quote (bottom/inline reply)');
  const quoted = lines.slice(qs, qe).filter((l) => !isBlank(l.text)).length;
  // Attribution: the last non-blank line before the quote, if it ends with ":".
  let a = qs - 1;
  while (a >= 0 && isBlank(lines[a]!.text)) a--;
  const hasAttr = a >= 0 && looksLikeAttribution(lines[a]!.text);
  const headEnd = hasAttr ? a : qs;
  if (!lines.slice(0, headEnd).some((l) => !isBlank(l.text))) return all('nothing written before quote');
  if (quoted < MIN_QUOTE_LINES) return all('quote too short');
  const headLines = trimTrailingBlank(lines.slice(0, headEnd));
  const collapsed: QNode[] = [];
  if (hasAttr) collapsed.push({ t: 'text', text: lines[a]!.text });
  collapsed.push(...buildTree(lines.slice(qs, qe)));
  return { head: buildTree(headLines), collapsed, reason: 'top-post with trailing history' };
}

function trimTrailingBlank(lines: Line[]): Line[] {
  let e = lines.length;
  while (e > 0 && lines[e - 1]!.level === 0 && isBlank(lines[e - 1]!.text)) e--;
  return lines.slice(0, e);
}
