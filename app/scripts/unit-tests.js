// Unit tests for the frontend's plain logic (modules without imports beyond types: Node runs
// their TypeScript as is).
//
//   cd app && npm run test:unit

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { switchRecipients } from '../src/lib/recipients.ts';
import { accountMarks, hash } from '../src/lib/accountMarks.ts';

const a = (addr, name = null) => ({ name, addr });
const addrs = (list) => list.map((x) => x.addr);

const reply = { to: [a('ann@example.com', 'Ann')], cc: [] };
const replyAll = { to: [a('ann@example.com', 'Ann'), a('bob@example.com')], cc: [a('cy@example.com'), a('di@example.com')] };

test('reply all adds the others, each in its field', () => {
  const next = switchRecipients({ to: reply.to, cc: [], bcc: [] }, reply, replyAll);
  assert.deepEqual(addrs(next.to), ['ann@example.com', 'bob@example.com']);
  assert.deepEqual(addrs(next.cc), ['cy@example.com', 'di@example.com']);
});

test('back to reply takes out only what reply all put there', () => {
  const cur = { to: [...replyAll.to, a('zoe@example.com')], cc: [...replyAll.cc], bcc: [] };
  const next = switchRecipients(cur, replyAll, reply);
  assert.deepEqual(addrs(next.to), ['ann@example.com', 'zoe@example.com']);
  assert.deepEqual(addrs(next.cc), []);
});

test('someone already there by hand (any field, any case) isn’t added twice', () => {
  // Di moved to Bcc by hand, Cy removed; case differs.
  const cur = { to: [a('Ann@Example.com', 'Ann')], cc: [], bcc: [a('di@example.com')] };
  const next = switchRecipients(cur, reply, replyAll);
  assert.deepEqual(addrs(next.to), ['Ann@Example.com', 'bob@example.com']);
  assert.deepEqual(addrs(next.cc), ['cy@example.com']);
});

test('the same recipients either way: nothing changes', () => {
  const cur = { to: [a('ann@example.com')], cc: [], bcc: [] };
  assert.deepEqual(switchRecipients(cur, reply, reply), { to: cur.to, cc: [] });
});

// ------------------------------------------------------------- account markers

const hueOf = (color) => Number(/hsl\((\d+)/.exec(color)[1]);
const gap = (a, b) => Math.min(Math.abs(a - b) % 360, 360 - (Math.abs(a - b) % 360));

test('accounts with the same initial get different letters', () => {
  const m = accountMarks([
    { id: 1, email: 'john@example.com' },
    { id: 2, email: 'jdoe@example.org' },
    { id: 3, email: 'j.doe@work.example.net' },
  ]);
  assert.deepEqual([1, 2, 3].map((id) => m.get(id).letter), ['J', 'E', 'W']);
});

test('accounts whose colours would match get different ones', () => {
  // Find two addresses that hash to the same hue (what happened with a real pair).
  const seen = new Map();
  let pair;
  for (let i = 0; !pair; i++) {
    const e = `user${i}@example.com`;
    const h = hash(e) % 360;
    if (seen.has(h)) pair = [seen.get(h), e];
    else seen.set(h, e);
  }
  const m = accountMarks([{ id: 1, email: pair[0] }, { id: 2, email: pair[1] }]);
  assert.ok(gap(hueOf(m.get(1).color), hueOf(m.get(2).color)) >= 40);
});

test('adding an account leaves the existing markers alone', () => {
  const before = accountMarks([{ id: 1, email: 'john@example.com' }, { id: 2, email: 'jane@example.org' }]);
  const after = accountMarks([{ id: 2, email: 'jane@example.org' }, { id: 1, email: 'john@example.com' }, { id: 3, email: 'jo@mail.example.net' }]);
  assert.deepEqual(after.get(1), before.get(1));
  assert.deepEqual(after.get(2), before.get(2));
  assert.equal(after.get(3).letter, 'M');
});

test('a marker the user set is kept, and the automatic ones steer clear of it', () => {
  const m = accountMarks([
    { id: 1, email: 'john@example.com' },
    { id: 2, email: 'kate@example.org', markLetter: 'j', markColor: 'teal' },
  ]);
  assert.equal(m.get(2).letter, 'J');
  assert.equal(m.get(2).color, 'hsl(172 55% 52%)');
  assert.equal(m.get(2).autoLetter, 'K', 'what automatic would be');
  // John's J is taken by Kate's choice: he falls back to his domain's E, his colour clear of teal
  assert.equal(m.get(1).letter, 'E');
  assert.ok(gap(hueOf(m.get(1).color), 172) >= 40);
});
