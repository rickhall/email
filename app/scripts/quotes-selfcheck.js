// Self-check for the plain-text quote rules: `node scripts/quotes-selfcheck.js` (Node ≥ 23.6
// strips the TypeScript types natively). HTML rules need a DOMParser and are exercised in the
// browser (see the headless checks in the dev notes).
import { analyzePlain, parseLines } from '../src/lib/quotes.ts';

let failed = 0;
const check = (name, cond, extra = '') => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${name}${extra ? ` — ${extra}` : ''}`);
  if (!cond) failed++;
};

const topPost = `Hi John,\n\nSounds good.\n\nRavi\n\n-- \nRavi Patel\n\nOn Thu, Oct 15, 2026 John Doe <r@x.com> wrote:\n> line one\n>\n> line two\n> > nested\n> line three\n`;
let r = analyzePlain(topPost);
check('top-post collapses', r.collapsed !== null, r.reason);
check('attribution goes into collapsed part', r.collapsed?.[0]?.t === 'text' && /wrote:$/.test(r.collapsed[0].text));
check('nested level parsed', parseLines('> > x')[0].level === 2 && parseLines('>> x')[0].level === 2);

r = analyzePlain(`On Thu John wrote:\n> a?\n\nYes.\n\n> b?\n\nNo.\n`);
check('interleaved never collapses', r.collapsed === null, r.reason);
check('interleaved still renders quote blocks', r.head.filter((n) => n.t === 'quote').length === 2);

r = analyzePlain(`On Thu John wrote:\n> one\n> two\n> three\n\nMy answer below.\n`);
check('bottom-post never collapses', r.collapsed === null, r.reason);

r = analyzePlain(`> one\n> two\n> three\n`);
check('quote only (nothing before) never collapses', r.collapsed === null, r.reason);

r = analyzePlain(`Thanks!\n\n> short\n> quote\n`);
check('short quote (<3 lines) never collapses', r.collapsed === null, r.reason);

r = analyzePlain(`Thanks!\n\n> one\n> two\n> three\n\n   \n`);
check('trailing blank lines allowed', r.collapsed !== null, r.reason);

r = analyzePlain(`Thanks!\n\n> one\n> two\n> three\n--\nsig after quote\n`);
check('anything after quote (even a signature) blocks collapse', r.collapsed === null, r.reason);

r = analyzePlain(`No quotes here.\nJust text.`);
check('no quote: untouched', r.collapsed === null && r.head.length === 1);

console.log(failed ? `\n${failed} check(s) failed` : '\nall plain-text checks passed');
process.exit(failed ? 1 : 0);
