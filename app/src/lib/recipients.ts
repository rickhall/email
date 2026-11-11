// Kept free of other imports: scripts/unit-tests.js loads it straight into Node.
import type { Addr } from '../types';

/** Recipients a reply mode fills in (the core's `reply_context`). */
export interface ReplyRecipients {
  to: Addr[];
  cc: Addr[];
}

/**
 * Switching between Reply and Reply all: the recipients `from` put there and `target` doesn't
 * want go, the ones `target` adds (and that aren't there yet) come, in its field. Anyone the
 * user added or removed by hand stays that way.
 */
export function switchRecipients(
  cur: { to: Addr[]; cc: Addr[]; bcc: Addr[] },
  from: ReplyRecipients,
  target: ReplyRecipients,
): { to: Addr[]; cc: Addr[] } {
  const key = (a: Addr) => a.addr.trim().toLowerCase();
  const wanted = new Set([...target.to, ...target.cc].map(key));
  const drop = new Set([...from.to, ...from.cc].map(key).filter((k) => !wanted.has(k)));
  const to = cur.to.filter((a) => !drop.has(key(a)));
  const cc = cur.cc.filter((a) => !drop.has(key(a)));
  const have = new Set([...to, ...cc, ...cur.bcc].map(key));
  for (const [list, out] of [
    [target.to, to],
    [target.cc, cc],
  ] as const) {
    for (const a of list) {
      if (!have.has(key(a))) {
        have.add(key(a));
        out.push(a);
      }
    }
  }
  return { to, cc };
}
