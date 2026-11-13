// mailto: links (RFC 6068) from other programs, e.g. a browser's "Email link": what a new
// message starts with. Import-free, for the unit tests.

export interface MailtoAddr {
  name: string | null;
  addr: string;
}

export interface Mailto {
  to: MailtoAddr[];
  cc: MailtoAddr[];
  bcc: MailtoAddr[];
  subject: string;
  /** The body as the composer's HTML: paragraphs at blank lines, line breaks within them. */
  html: string;
}

const decode = (s: string) => {
  try {
    return decodeURIComponent(s.replace(/\+/g, '%2B'));
  } catch {
    return s;
  }
};

/** "Ann <ann@example.com>, bob@example.com" → addresses (names when given). */
function addresses(list: string): MailtoAddr[] {
  return list
    .split(/[,;]/)
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) => {
      const m = /^(.*?)\s*<([^>]+)>$/.exec(part);
      if (m) return { name: m[1]!.replace(/^"|"$/g, '').trim() || null, addr: m[2]!.trim() };
      return { name: null, addr: part };
    })
    .filter((a) => a.addr.includes('@'));
}

const escapeHtml = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

/** Plain text → `<p>` paragraphs (blank lines between), `<br>` for the other line breaks. */
export function textToHtml(text: string): string {
  const paras = text
    .replace(/\r\n?/g, '\n')
    .split(/\n\s*\n/)
    .map((p) => p.replace(/^\n+|\n+$/g, ''))
    .filter((p) => p.trim() !== '');
  return paras.map((p) => `<p>${escapeHtml(p).replace(/\n/g, '<br>')}</p>`).join('');
}

/** Parse a mailto: link; null if it isn't one. */
export function parseMailto(url: string): Mailto | null {
  if (!/^mailto:/i.test(url)) return null;
  const rest = url.slice(7);
  const q = rest.indexOf('?');
  const out: Mailto = { to: addresses(decode(q < 0 ? rest : rest.slice(0, q))), cc: [], bcc: [], subject: '', html: '' };
  let body = '';
  if (q >= 0) {
    for (const pair of rest.slice(q + 1).split('&')) {
      const eq = pair.indexOf('=');
      const key = decode(eq < 0 ? pair : pair.slice(0, eq)).toLowerCase();
      const value = eq < 0 ? '' : decode(pair.slice(eq + 1));
      if (key === 'to') out.to.push(...addresses(value));
      else if (key === 'cc') out.cc.push(...addresses(value));
      else if (key === 'bcc') out.bcc.push(...addresses(value));
      else if (key === 'subject') out.subject = value;
      else if (key === 'body') body = value;
    }
  }
  out.html = textToHtml(body);
  return out;
}
