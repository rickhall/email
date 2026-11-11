// UI tests: the browser preview with its mock backend (src/mock.ts, `?demo`), driven in Chrome.
// For behaviour that spans the list, the search and the reader, which no unit test reaches.
//
//   cd app && npm run test:ui
//
// Uses the installed Google Chrome (or CHROME=/path/to/chromium).

import { after, before, describe, test } from 'node:test';
import assert from 'node:assert/strict';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServer } from 'vite';
import { chromium } from 'playwright-core';

const APP = join(dirname(fileURLToPath(import.meta.url)), '..');
const NOW = new Date('2026-10-13T09:41:00-04:00');

let server;
let browser;
let base;

before(async () => {
  server = await createServer({ root: APP, logLevel: 'error', server: { port: 1498, strictPort: false } });
  await server.listen();
  base = server.resolvedUrls.local[0];
  browser = await chromium.launch(process.env.CHROME ? { executablePath: process.env.CHROME } : { channel: 'chrome' });
});

after(async () => {
  await browser?.close();
  await server?.close();
});

/** The demo in a fresh window, as a list of messages or of conversations. */
async function open({ threaded }) {
  const context = await browser.newContext({ viewport: { width: 1400, height: 850 }, locale: 'en-US', timezoneId: 'America/New_York' });
  await context.clock.setFixedTime(NOW);
  await context.addInitScript((t) => localStorage.setItem('ui.threaded', JSON.stringify(t)), threaded);
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.goto(base + '?demo');
  await page.waitForSelector('.folder-layer .row');
  await page.waitForTimeout(300);
  return { page, errors, context, close: () => context.close() };
}

const selected = (page) => page.locator('.folder-layer .row.selected .row-subject').first();
const reader = (page) => page.locator('h1').first();

/** Wait until `locator`'s text is `want` (or time out with what it was). */
async function hasText(locator, want, what) {
  const end = Date.now() + 3000;
  let got = null;
  while (Date.now() < end) {
    got = await locator.textContent({ timeout: 200 }).catch(() => null);
    if (got?.trim() === want) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  assert.fail(`${what}: expected "${want}", got "${got}"`);
}

/** Search, pick the first result whose folder tag matches `where` and isn't `not`; its subject. */
async function pickResult(page, query, where, not) {
  await page.fill('.search-field input', query);
  await page.waitForSelector('.search-layer .row');
  await page.waitForTimeout(300);
  const rows = await page.locator('.search-layer .row').evaluateAll((rs) =>
    rs.map((r) => [r.querySelector('.folder-tag')?.textContent ?? '', r.querySelector('.row-subject')?.textContent ?? '']),
  );
  const k = rows.findIndex(([f, s]) => where.test(f) && s !== not);
  assert.ok(k >= 0, `no result in ${where} among ${JSON.stringify(rows)}`);
  await page.locator('.search-layer .row').nth(k).click();
  await hasText(reader(page), rows[k][1], 'the picked result opens');
  return rows[k][1];
}

for (const threaded of [false, true]) {
  describe(threaded ? 'conversation view' : 'message view', () => {
    test('clearing a search keeps a picked result the folder has', async () => {
      const { page, errors, close } = await open({ threaded });
      try {
        await page.locator('.folder-layer .row').nth(2).click();
        const before = (await selected(page).textContent()).trim();
        const picked = await pickResult(page, 'Northwind', /^Inbox$/, before);
        await page.fill('.search-field input', '');
        await hasText(selected(page), picked, 'selected in the folder');
        await hasText(reader(page), picked, 'still open');
        assert.deepEqual(errors, []);
      } finally {
        await close();
      }
    });

    test('clearing a search after picking a result from elsewhere brings back the selection', async () => {
      const { page, errors, close } = await open({ threaded });
      try {
        await page.locator('.folder-layer .row').nth(2).click();
        const before = (await selected(page).textContent()).trim();
        await pickResult(page, 'Northwind', /^(All Mail|Newsletters)$/, before);
        await page.fill('.search-field input', '');
        await hasText(selected(page), before, 'the earlier selection');
        await hasText(reader(page), before, 'the earlier message open');
        assert.deepEqual(errors, []);
      } finally {
        await close();
      }
    });

    test('a notification click opens its message in the folder on screen', async () => {
      const { page, errors, close } = await open({ threaded });
      try {
        await page.locator('.folder-layer .row').first().click();
        // A message further down the Inbox than the one open.
        const subject = (await page.locator('.folder-layer .row .row-subject').nth(4).textContent()).trim();
        assert.ok(await page.evaluate((s) => window.__clickNotification(s), subject), `"${subject}" in the Inbox`);
        await hasText(selected(page), subject, 'selected');
        await hasText(reader(page), subject, 'open');
        assert.deepEqual(errors, []);
      } finally {
        await close();
      }
    });
  });
}

/** Wait until the message's card in the open conversation is expanded and on screen. */
async function cardOpen(page, id, what) {
  const card = page.locator(`.card-slot[data-mid="${id}"] .card-msg.expanded`);
  await card.waitFor({ timeout: 3000 }).catch(() => assert.fail(`${what}: message ${id} isn't expanded`));
  await page.waitForTimeout(200);
  const box = await card.boundingBox();
  const view = page.viewportSize();
  assert.ok(box && box.y >= 0 && box.y < view.height - 40, `${what}: message ${id} isn't on screen (${JSON.stringify(box)})`);
}

describe('conversation view: the message asked for', () => {
  test('a notification click opens that message, not the newest of its conversation', async () => {
    const { page, errors, close } = await open({ threaded: true });
    try {
      const older = await page.evaluate(() => window.__olderInConversation('inbox'));
      assert.ok(older, 'an older message in an Inbox conversation');
      await page.evaluate((id) => window.__clickNotification(id), older.id);
      await cardOpen(page, older.id, 'notification');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('clearing a search in All Mail opens the picked message in its conversation', async () => {
    const { page, errors, close } = await open({ threaded: true });
    try {
      await page.getByText('All Mail', { exact: true }).first().click();
      await page.waitForSelector('.folder-layer .row');
      const older = await page.evaluate(() => window.__olderInConversation('all'));
      assert.ok(older, 'an older message in an All Mail conversation');
      await page.fill('.search-field input', `subject:"${older.subject}"`);
      const row = page.locator(`.search-layer .row[data-id="${older.id}"]`);
      await row.waitFor({ timeout: 3000 });
      await row.click();
      await page.waitForTimeout(300);
      await page.fill('.search-field input', '');
      await cardOpen(page, older.id, 'after clearing');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('compose', () => {
  test('Reply and Reply all switch over, changing only their own recipients', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      // The first demo message has people in Cc.
      await page.locator('.folder-layer .row').first().click();
      await page.waitForTimeout(300);
      const popup = context.waitForEvent('page');
      await page.locator('.reader [aria-label="Reply all"]').first().click();
      const w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.waitForSelector('.cmp-mode');
      const on = async () => (await w.locator('.cmp-mode button.on').textContent()).trim();
      const people = async () =>
        (await w.locator('.cmp-fields .addr-chip').allTextContents()).map((t) => t.replace('×', '').trim());
      await w.waitForFunction(() => document.querySelectorAll('.cmp-fields .addr-chip').length >= 3);
      assert.equal(await on(), 'Reply all');
      const all = await people();

      await w.locator('.cmp-mode button', { hasText: /^Reply$/ }).click();
      assert.equal(await on(), 'Reply');
      assert.equal((await people()).length, 1, 'only the sender');

      await w.locator('.cmp-mode button', { hasText: 'Reply all' }).click();
      assert.equal(await on(), 'Reply all');
      assert.deepEqual(await people(), all);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('accounts', () => {
  test('the order and markers set by the user come back from the core', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const names = () => page.locator('.sidebar .account-name').allTextContents();
      const before = await names();
      assert.ok(before.length >= 2, 'the demo has two accounts');
      // Alt+↓ on the first account's header: it moves down one
      await page.locator('.sidebar .account-head').first().focus();
      await page.keyboard.press('Alt+ArrowDown');
      const moved = [before[1], before[0], ...before.slice(2)];
      assert.deepEqual(await names(), moved);
      // Setting a marker reloads the accounts from the core: the order holds
      await page.keyboard.press('Control+,');
      const acct = page.locator('.st-acct', { hasText: before[0] });
      await acct.locator('.st-mark-letter').fill('w');
      await acct.locator('.st-swatch[title="Teal"]').click();
      await page.waitForTimeout(300);
      await page.keyboard.press('Escape');
      const dot = page.locator('.sidebar .account', { hasText: before[0] }).locator('.acct-dot');
      await hasText(dot, 'W', 'the marker letter');
      assert.equal(await dot.evaluate((e) => getComputedStyle(e).backgroundColor), 'rgb(65, 200, 182)');
      assert.deepEqual(await names(), moved);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});
