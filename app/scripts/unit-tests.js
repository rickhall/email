// Unit tests for the frontend's plain logic (modules without imports beyond types: Node runs
// their TypeScript as is).
//
//   cd app && npm run test:unit

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { switchRecipients } from '../src/lib/recipients.ts';

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
