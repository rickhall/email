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
async function open({ threaded, query = '' }) {
  const context = await browser.newContext({ viewport: { width: 1400, height: 850 }, locale: 'en-US', timezoneId: 'America/New_York' });
  await context.clock.setFixedTime(NOW);
  await context.addInitScript((t) => localStorage.setItem('ui.threaded', JSON.stringify(t)), threaded);
  // The app's window refuses blob: pictures (its CSP allows data:); a browser tab wouldn't, so
  // make them fail here too.
  await context.addInitScript(() => {
    URL.createObjectURL = () => {
      throw new Error('blob: URLs are refused in the app window (CSP img-src)');
    };
  });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.goto(base + '?demo' + query);
  await page.waitForSelector('.folder-layer .row');
  await page.waitForTimeout(300);
  return { page, errors, context, close: () => context.close() };
}

const selected = (page) => page.locator('.folder-layer .row.selected .row-subject').first();
const reader = (page) => page.locator('h1').first();

/** Wait until `locator`'s text is `want` (or time out with what it was). */
/** Choose `label` in one of our drop-downs (components/Select.tsx): `box` is its button. */
async function choose(box, label) {
  await box.click();
  const menu = box.page().locator('.sel-menu');
  await menu.getByRole('option', { name: label, exact: true }).click();
  await menu.waitFor({ state: 'detached' });
}
/** The options of one of our drop-downs, opened and closed again. */
async function optionsOf(box) {
  await box.click();
  const menu = box.page().locator('.sel-menu');
  const all = await menu.getByRole('option').allTextContents();
  await box.page().keyboard.press('Escape');
  await menu.waitFor({ state: 'detached' });
  return all.map((t) => t.trim());
}

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

/** `hasText` with a longer wait. */
async function hasTextWithin(locator, want, ms, what) {
  const end = Date.now() + ms;
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

    test('Sent names who it went to; the Inbox who it came from', async () => {
      const { page, errors, close } = await open({ threaded });
      try {
        const from = page.locator('.folder-layer .row .row-from').first();
        const matches = async (re, what) => {
          const end = Date.now() + 3000;
          let got = null;
          while (Date.now() < end) {
            got = (await from.textContent({ timeout: 200 }).catch(() => null))?.trim();
            if (got && re.test(got)) return got;
            await new Promise((r) => setTimeout(r, 50));
          }
          assert.fail(`${what}: got "${got}"`);
        };
        await matches(/^(?!To:)\S/, 'the Inbox shows senders');
        await page.locator('.sidebar button.folder', { hasText: /^Sent$/ }).first().click();
        await matches(/^To: \S/, 'Sent shows the recipient');
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
  test('the outline a compose window shows while loading is laid out like the composer', async () => {
    const context = await browser.newContext({ viewport: { width: 760, height: 400 } });
    try {
      const page = await context.newPage();
      // only the page itself: the app's code never comes, as if it were still loading
      await page.route(/\.(tsx?|jsx?)(\?|$)|@vite|@id|node_modules/, (r) => r.abort());
      await page.goto(base + '?compose=1');
      const tops = await page.locator('#boot .boot-row').evaluateAll((rs) => rs.map((r) => [r.textContent, Math.round(r.getBoundingClientRect().top)]));
      assert.deepEqual(tops, [['From', 52], ['To', 92], ['Subject', 132]]);
    } finally {
      await context.close();
    }
  });

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

describe('reader', () => {
  test('pointing at a link shows where it goes, and text that pretends to be another site is called out', async () => {
    const { page, errors, close } = await open({ threaded: false, query: '&picture' });
    try {
      await page.locator('.folder-layer .row', { hasText: 'picture as the body' }).first().click();
      const frame = page.frameLocator('.reader iframe').first();
      await frame.locator('img').waitFor();
      await frame.locator('body').evaluate((b) => {
        b.insertAdjacentHTML(
          'afterbegin',
          '<p><a id="fake" href="https://fb-login.example/verify?u=1">https://www.facebook.com/security</a></p>' +
            '<p><a id="fine" href="https://www.facebook.com/help/">Help Centre</a></p>',
        );
      });
      const peek = page.locator('.link-peek');
      await frame.locator('#fine').hover();
      await peek.waitFor();
      assert.equal(await peek.locator('.lp-host').textContent(), 'www.facebook.com');
      assert.equal(await peek.locator('.lp-warn').count(), 0, 'a link in words has nothing to compare');

      await frame.locator('#fake').hover();
      await hasText(peek.locator('.lp-host'), 'fb-login.example', 'the real site, in bold');
      assert.match(await peek.locator('.lp-url').textContent(), /^https:\/\/fb-login\.example\/verify\?u=1$/);
      assert.match(await peek.locator('.lp-warn').textContent(), /says facebook\.com, but the link goes to fb-login\.example/);
      // It stays inside the reader.
      const [box, pane] = await Promise.all([peek.boundingBox(), page.locator('.reader iframe').first().boundingBox()]);
      assert.ok(box.x >= pane.x && box.x + box.width <= pane.x + pane.width + 1, `inside the message (${JSON.stringify(box)} in ${JSON.stringify(pane)})`);

      await page.locator('.reader-subject').hover();
      await peek.waitFor({ state: 'detached' });
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('the reader\u2019s head stays at the top while the message scrolls, unless turned off in Settings', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').first().click();
      await page.locator('.reader .reader-body .card-frame-wrap').waitFor();
      // A long message, whatever the demo's is.
      await page.evaluate(() => (document.querySelector('.reader .reader-body').style.minHeight = '4000px'));
      const head = page.locator('.reader .reader-head');
      const top = async () => (await head.boundingBox()).y - (await page.locator('.reader-scroll').boundingBox()).y;
      const scrollDown = async () => {
        await page.evaluate(() => {
          const s = document.querySelector('.reader-scroll');
          s.scrollTop = 0;
          s.scrollTop = 1500;
        });
        await page.waitForTimeout(50);
        assert.ok((await page.evaluate(() => document.querySelector('.reader-scroll').scrollTop)) > 1000, 'the message scrolled');
      };
      await scrollDown();
      assert.ok(Math.abs(await top()) < 2, `pinned: still at the top (${await top()})`);
      // Opaque, so the message doesn't show through it.
      assert.notEqual(await head.evaluate((e) => getComputedStyle(e).backgroundColor), 'rgba(0, 0, 0, 0)');
      // Too tall to pin (a long recipient list): it scrolls away rather than cover the message.
      await head.evaluate((e) => (e.style.minHeight = '900px'));
      await page.waitForFunction(() => !document.querySelector('.reader .reader-head').classList.contains('pinned'));
      await scrollDown();
      assert.ok((await top()) < -500, 'a tall head scrolls away');
      assert.equal(await head.evaluate((e) => getComputedStyle(e).overflowY), 'visible', 'and never scrolls within itself');
      await head.evaluate((e) => (e.style.minHeight = ''));
      await page.waitForFunction(() => document.querySelector('.reader .reader-head').classList.contains('pinned'));
      // Settings \u203a General \u203a Appearance \u203a Message header: off, and it scrolls away with the message.
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'General' }).click();
      const pin = page.locator('.st-field', { hasText: 'Message header' }).locator('input[type=checkbox]');
      assert.equal(await pin.isChecked(), true, 'on to begin with');
      await pin.uncheck();
      assert.equal(await page.evaluate(() => localStorage.getItem('ui.pinReaderHead')), 'false');
      await page.keyboard.press('Escape');
      await scrollDown();
      assert.ok((await top()) < -1000, 'not pinned: scrolled out of view');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('a picture wider than the reader shrinks and keeps its shape', async () => {
    const { page, errors, close } = await open({ threaded: false, query: '&picture' });
    try {
      await page.locator('.folder-layer .row', { hasText: 'picture as the body' }).first().click();
      const img = page.frameLocator('.reader iframe').first().locator('img');
      await img.waitFor();
      await page.waitForFunction(() => {
        const i = document.querySelector('.reader iframe')?.contentDocument?.querySelector('img');
        return i && i.getBoundingClientRect().width > 0;
      });
      const { w, h, room } = await img.evaluate((el) => ({ w: el.getBoundingClientRect().width, h: el.getBoundingClientRect().height, room: el.ownerDocument.body.clientWidth }));
      assert.ok(w < 1400 && w <= room, `shrunk to the reader (${w} of ${room})`);
      assert.ok(Math.abs(w / h - 2) < 0.02, `still 2:1, not squished (${w}×${h})`);
      // a click shows it at its actual size, another fits it again
      assert.equal(await img.evaluate((el) => getComputedStyle(el).cursor), 'zoom-in');
      await img.click();
      const full = await img.evaluate((el) => [el.getBoundingClientRect().width, el.getBoundingClientRect().height]);
      assert.ok(Math.abs(full[0] - 1400) < 1 && Math.abs(full[1] - 700) < 1, `actual size: ${full}`);
      assert.equal(await img.evaluate((el) => getComputedStyle(el).cursor), 'zoom-out');
      // and the reader's page scrolls sideways to the rest of it (the message itself never does)
      await page.waitForFunction(() => {
        const s = document.querySelector('.reader-scroll');
        return s.scrollWidth > s.clientWidth + 500;
      });
      const across = await page.evaluate(() => {
        const s = document.querySelector('.reader-scroll');
        s.scrollLeft = 10_000;
        const f = document.querySelector('.reader .card-frame');
        const de = f.contentDocument.documentElement;
        const i = f.contentDocument.querySelector('img');
        return {
          left: s.scrollLeft,
          frameScrolls: de.scrollWidth > de.clientWidth + 1,
          imgRight: f.getBoundingClientRect().left + i.getBoundingClientRect().right,
          paneRight: s.getBoundingClientRect().right,
        };
      });
      assert.ok(!across.frameScrolls, 'the message itself never scrolls sideways');
      assert.ok(across.left > 0 && Math.abs(across.imgRight - across.paneRight) < 40, `scrolled to its right edge: ${JSON.stringify(across)}`);
      await page.evaluate(() => (document.querySelector('.reader-scroll').scrollLeft = 0));
      await img.click({ position: { x: 20, y: 20 } });
      assert.ok(Math.abs((await img.evaluate((el) => el.getBoundingClientRect().width)) - w) < 1, 'fits again');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
  test('in a conversation, a picture at its actual size widens its card; the pane scrolls sideways', async () => {
    const { page, errors, close } = await open({ threaded: true, query: '&picture' });
    try {
      await page.locator('.folder-layer .row', { hasText: 'picture as the body' }).first().click();
      const img = page.frameLocator('.card-frame').first().locator('img');
      await img.waitFor();
      await page.waitForFunction(() => document.querySelector('.card-frame')?.contentDocument?.querySelector('img')?.classList.contains('mail-zoom'));
      const pane = () =>
        page.evaluate(() => {
          const s = document.querySelector('.conv-scroll');
          const f = document.querySelector('.card-frame');
          const de = f.contentDocument.documentElement;
          return { scroll: s.scrollWidth, room: s.clientWidth, frameScrolls: de.scrollWidth > de.clientWidth + 1, frameH: f.getBoundingClientRect().height };
        });
      const before = await pane();
      assert.ok(before.scroll <= before.room + 1, 'nothing to scroll sideways yet');
      await img.click();
      await page.waitForFunction(() => {
        const s = document.querySelector('.conv-scroll');
        return s.scrollWidth > s.clientWidth + 500;
      });
      const zoomed = await pane();
      assert.ok(!zoomed.frameScrolls, 'the message itself never scrolls sideways');
      assert.ok(zoomed.frameH > 700, `the frame holds the whole picture (${zoomed.frameH})`);
      // the pane's scrollbar reaches its right edge
      const edge = await page.evaluate(() => {
        const s = document.querySelector('.conv-scroll');
        s.scrollLeft = 10_000;
        const i = document.querySelector('.card-frame').contentDocument.querySelector('img');
        const f = document.querySelector('.card-frame').getBoundingClientRect();
        return { left: s.scrollLeft, imgRight: f.left + i.getBoundingClientRect().right, paneRight: s.getBoundingClientRect().right };
      });
      assert.ok(edge.left > 0 && edge.imgRight <= edge.paneRight, `its right edge in view: ${JSON.stringify(edge)}`);
      // fitted again, the card narrows back
      await page.evaluate(() => (document.querySelector('.conv-scroll').scrollLeft = 0));
      await img.click({ position: { x: 20, y: 20 } });
      await page.waitForFunction(() => {
        const s = document.querySelector('.conv-scroll');
        return s.scrollWidth <= s.clientWidth + 1;
      });
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('a long To line shows a few, then "and N more"', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row', { hasText: 'Notes from today' }).first().click();
      const line = page.locator('.reader .recips-line', { hasText: 'To' }).first();
      const more = line.locator('.recips-more');
      await more.waitFor();
      assert.equal(await line.locator('.recip').count(), 3);
      assert.match(await more.textContent(), /^and \d+ more$/);
      const n = Number((await more.textContent()).match(/\d+/)[0]);
      await more.click();
      assert.equal(await line.locator('.recip').count(), 3 + n);
      assert.equal((await more.textContent()).trim(), 'show less');
      await more.click();
      assert.equal(await line.locator('.recip').count(), 3);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('a busy sync', () => {
  test('new mail shows while the Inbox keeps changing and reads are slow', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      // reads take 400 ms, the Inbox is reported changed every 100 ms
      await page.evaluate(() => window.__busySync(400, 100));
      const subject = await page.evaluate(() => window.__newInboxMail());
      try {
        await hasTextWithin(page.locator('.folder-layer .row .row-subject').first(), subject, 4000, 'the new mail in the list');
      } finally {
        await page.evaluate(() => window.__stopBusySync());
      }
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('selected row colours', () => {
  test('a message body (its own frame) counts as the window: clicking in it stays blue, and brings the window back', async () => {
    const { page, errors, close } = await open({ threaded: false, query: '&picture' });
    try {
      await page.locator('.folder-layer .row', { hasText: 'picture as the body' }).first().click();
      const frame = page.frameLocator('.reader iframe').first();
      await frame.locator('img').waitFor();
      await page.evaluate(() => window.dispatchEvent(new Event('focus')));
      const icon = () => page.evaluate(() => getComputedStyle(document.querySelector('.folder .folder-icon')).color);
      await page.waitForTimeout(150);
      const blue = await icon();
      // into the body: the page blurs, the window doesn't
      await frame.locator('body').click({ position: { x: 5, y: 5 } });
      await page.waitForTimeout(150);
      assert.equal(await page.evaluate(() => document.activeElement?.tagName), 'IFRAME');
      assert.equal(await icon(), blue, 'still blue with the focus in the message');
      // another window in front, then back by a click into the body: only the frame is focused
      // the focus back on the page (the list: the same message stays open)
      await page.locator('.folder-layer .row.selected').first().click();
      await page.waitForFunction(() => document.activeElement?.tagName !== 'IFRAME');
      // Leaving the frame checks, a moment later, whether the window still has the focus (it
      // has: it's back on the page). That check runs first; then the other window takes it.
      await page.evaluate(() => new Promise((r) => setTimeout(r, 50)));
      await page.evaluate(() => window.dispatchEvent(new Event('blur')));
      await page.waitForTimeout(150);
      assert.notEqual(await icon(), blue, 'grey in the background');
      await page.evaluate(() => document.querySelector('.reader iframe').contentWindow.dispatchEvent(new Event('focus')));
      await page.waitForTimeout(150);
      assert.equal(await icon(), blue, 'blue again');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('in the background the window greys its selection and folder icons', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').nth(1).click();
      await page.mouse.move(5, 5);
      await page.evaluate(() => window.dispatchEvent(new Event('focus')));
      const look = () =>
        page.evaluate(() => ({
          row: getComputedStyle(document.querySelector('.folder-layer .row.selected .row-inner')).backgroundColor,
          icon: getComputedStyle(document.querySelector('.folder .folder-icon')).color,
          folder: getComputedStyle(document.querySelector('.folder.selected')).backgroundColor,
        }));
      await page.waitForTimeout(150);
      const front = await look();
      assert.equal(front.row, 'rgb(10, 108, 255)', 'solid blue in front');
      await page.evaluate(() => window.dispatchEvent(new Event('blur')));
      await page.waitForTimeout(150);
      const back = await look();
      for (const k of ['row', 'icon', 'folder']) assert.notEqual(back[k], front[k], `${k} changes`);
      const grey = (c) => {
        const [r, g, b] = c.match(/[\d.]+/g).map(Number);
        return Math.max(r, g, b) - Math.min(r, g, b) < 16;
      };
      assert.ok(grey(back.row) && grey(back.icon), `grey: ${JSON.stringify(back)}`);
      await page.evaluate(() => window.dispatchEvent(new Event('focus')));
      await page.waitForTimeout(150);
      assert.deepEqual(await look(), front, 'back to blue');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('only the selected row has them, and only in full while the list has the focus', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const rows = page.locator('.folder-layer .row');
      const look = (i) =>
        rows.nth(i).evaluate((r) => ({
          vars: r.style.getPropertyValue('--rs-text') !== '',
          subject: getComputedStyle(r.querySelector('.row-subject')).color,
          bg: getComputedStyle(r.querySelector('.row-inner')).backgroundColor,
        }));
      await rows.nth(1).click();
      await page.mouse.move(5, 5); // no hover colour
      await page.waitForTimeout(150);
      const selected = await look(1);
      assert.ok(selected.vars, 'the selected row carries its colours');
      assert.equal(selected.subject, 'rgb(255, 255, 255)');
      const normal = await look(3);
      await page.keyboard.press('ArrowDown');
      await page.waitForTimeout(150);
      assert.deepEqual(await look(1), normal, 'the row left behind looks like any other');
      assert.ok((await look(2)).vars);
      // focus elsewhere: still marked, in the quiet colour, normal text
      await page.locator('.search-field input').focus();
      await page.waitForTimeout(150);
      const quiet = await look(2);
      assert.equal(quiet.vars, false);
      assert.equal(quiet.subject, normal.subject);
      assert.notEqual(quiet.bg, normal.bg);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('conversation keyboard', () => {
  test('the Show / Hide rows are on the arrow path too', async () => {
    const { page, errors, close } = await open({ threaded: true });
    try {
      // a conversation with messages in other folders
      const rows = page.locator('.folder-layer .row');
      let found = false;
      for (let i = 0; i < 6 && !found; i++) {
        await rows.nth(i).click();
        await page.waitForSelector('.conv-scroll .card-slot');
        await page.waitForTimeout(200);
        found = (await page.locator('[data-reveal="other"]').count()) > 0;
      }
      assert.ok(found, 'a conversation with hidden messages');
      const cards = await page.locator('.conv-scroll .card-slot').count();
      await page.keyboard.press('Enter');
      await page.keyboard.press('Enter');
      // up from the first message to the Show buttons
      await page.locator('.conv-scroll .cm-toggle').first().focus();
      const focusedText = () => page.evaluate(() => document.activeElement?.closest('.reveal-row')?.getAttribute('data-reveal') ?? null);
      for (let i = 0; i < 3 && (await focusedText()) !== 'other'; i++) await page.keyboard.press('ArrowUp');
      assert.equal(await focusedText(), 'other');
      await page.keyboard.press('Enter');
      await page.waitForFunction((c) => document.querySelectorAll('.conv-scroll .card-slot').length > c, cards);
      assert.equal(await focusedText(), 'other', 'the focus stays on that row');
      assert.match(await page.evaluate(() => document.activeElement?.textContent ?? ''), /^Hide/);
      await page.keyboard.press('Enter');
      await page.waitForFunction((c) => document.querySelectorAll('.conv-scroll .card-slot').length === c, cards);
      assert.equal(await focusedText(), 'other');
      await page.keyboard.press('Escape');
      assert.ok(await page.evaluate(() => document.activeElement?.classList.contains('conv-scroll')), 'Esc: back to scrolling');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('Enter onto the messages, arrows between them, Enter to expand, ; for all', async () => {
    const { page, errors, close } = await open({ threaded: true });
    try {
      // a conversation of several messages, some collapsed
      const rows = page.locator('.folder-layer .row');
      const n = await rows.count();
      let k = -1;
      for (let i = 0; i < n; i++) {
        if (Number((await rows.nth(i).locator('.thread-count').textContent().catch(() => '')) || 0) >= 2) {
          k = i;
          break;
        }
      }
      assert.ok(k >= 0, 'a conversation with several messages');
      await rows.nth(k).click();
      await page.waitForSelector('.conv-scroll .card-slot');
      const expandedCount = () => page.locator('.conv-scroll .card-msg.expanded').count();
      const cards = await page.locator('.conv-scroll .card-slot').count();
      await page.keyboard.press('Enter'); // into the reader
      await page.keyboard.press('Enter'); // onto the message in view
      const onHeader = () => page.evaluate(() => !!document.activeElement?.classList.contains('cm-toggle'));
      assert.ok(await onHeader(), 'a message header has the focus');
      // ; toggles all: some are collapsed, so all open; again, all close
      await page.keyboard.press(';');
      await page.waitForFunction((c) => document.querySelectorAll('.conv-scroll .card-msg.expanded').length === c, cards);
      await page.keyboard.press(';');
      await page.waitForFunction(() => document.querySelectorAll('.conv-scroll .card-msg.expanded').length === 0);
      // first header, then down to the second, and expand it
      await page.locator('.conv-scroll .cm-toggle').first().focus();
      await page.keyboard.press('ArrowDown');
      const at = () => page.evaluate(() => document.activeElement?.closest('.card-slot')?.getAttribute('data-mid'));
      const second = await page.locator('.conv-scroll .card-slot').nth(1).getAttribute('data-mid');
      assert.equal(await at(), second);
      await page.keyboard.press('Enter');
      await page.waitForFunction(() => document.querySelectorAll('.conv-scroll .card-msg.expanded').length === 1);
      assert.equal(await at(), second, 'the focus stays on that message');
      assert.ok(await onHeader());
      await page.keyboard.press(' '); // and collapse it again
      await page.waitForFunction(() => document.querySelectorAll('.conv-scroll .card-msg.expanded').length === 0);
      // Esc: back to scrolling the reader; Esc again: back to the list
      await page.keyboard.press('Escape');
      assert.ok(await page.evaluate(() => document.activeElement?.classList.contains('conv-scroll')), 'scrolling the reader');
      await page.keyboard.press('Escape');
      assert.ok(await page.evaluate(() => !!document.activeElement?.closest('.folder-layer')), 'back in the list');
      await page.keyboard.press(';'); // one collapsed: all open
      await page.waitForFunction((c) => document.querySelectorAll('.conv-scroll .card-msg.expanded').length === c, cards);
      assert.equal(await expandedCount(), cards);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('snooze', () => {
  test('a day from the calendar and a typed time', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').nth(3).click();
      await page.locator('.reader [aria-label^="Snooze"]').first().click();
      await page.waitForSelector('.snz-cal');
      // the next month's 10th: forward a month, click it
      await page.locator('[aria-label="Next month"]').click();
      await page.locator('.snz-day:not(.out)', { hasText: /^10$/ }).click();
      const time = page.locator('.snz-time');
      await time.fill('2pm');
      await time.press('ArrowUp'); // 2:15 PM
      assert.match(await page.locator('.snz-summary').textContent(), /10 at 2:15/);
      await time.press('Enter');
      await page.waitForSelector('.snooze-menu', { state: 'detached' });
      await page.waitForSelector('.folder-layer .row.selected .snooze-mark');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('search', () => {
  test('the open message stays when the search finds it beyond the first page of results', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').first().click();
      const first = (await page.locator('.folder-layer .row.selected .row-subject').textContent()).trim();
      await page.keyboard.press('End');
      // The last page loads first: wait until the selection has moved there and opened.
      await page.waitForFunction((f) => {
        const s = document.querySelector('.folder-layer .row.selected .row-subject')?.textContent?.trim();
        const h = [...document.querySelectorAll('h1')].map((x) => x.textContent?.trim());
        return s && s !== f && h.includes(s);
      }, first);
      const row = page.locator('.folder-layer .row.selected');
      const subject = (await row.locator('.row-subject').textContent()).trim();
      const sender = (await row.locator('.row-from, .row-sender').first().textContent()).trim();
      await hasText(reader(page), subject, 'the oldest message open');
      await page.fill('.search-field input', sender);
      await page.waitForSelector('.search-layer .row');
      const count = page.locator('.search-count span').first();
      await page.waitForFunction(() => /\d+ results?$/.test(document.querySelector('.search-count span')?.textContent?.trim() ?? ''));
      const total = Number((await count.textContent()).replace(/[^\d]/g, ''));
      assert.ok(total > 200, `beyond the first page of 200 (${total} results)`);
      await page.waitForTimeout(800);
      assert.equal((await reader(page).textContent()).trim(), subject, 'still open: the search found it');
      // and one the search doesn't find goes
      await page.fill('.search-field input', 'zzz-nothing-like-this');
      await page.waitForFunction((s) => ![...document.querySelectorAll('h1')].some((h) => h.textContent?.trim() === s), subject);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('deleting a result moves on to the next one', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.fill('.search-field input', 'Northwind');
      await page.waitForSelector('.search-layer .row');
      await page.waitForTimeout(300);
      const subjects = await page.locator('.search-layer .row .row-subject').allTextContents();
      assert.ok(subjects.length >= 2, 'two results at least');
      await page.locator('.search-layer .row').first().click();
      await hasText(reader(page), subjects[0].trim(), 'the first result opens');
      await page.locator('.reader [aria-label="Delete"]').first().click();
      const sel = page.locator('.search-layer .row.selected .row-subject').first();
      await hasText(sel, subjects[1].trim(), 'the next result is selected');
      await hasText(reader(page), subjects[1].trim(), 'and open');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('words in the text find mail; the Text chip and body: limit it', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const count = async (q) => {
        await page.fill('.search-field input', q);
        await page.waitForTimeout(400);
        return page.locator('.search-layer .row').count();
      };
      // "package" is only in the text of the order mails ("your package is on its way").
      assert.ok((await count('package')) > 0, 'found by its text');
      const subjects = await page.locator('.search-layer .row .row-subject').allTextContents();
      assert.ok(subjects.every((s) => !/package/i.test(s)), `none by its subject: ${subjects.join(' | ')}`);
      assert.ok((await count('body:package')) > 0, 'body: finds it');
      assert.equal(await count('subject:package'), 0, 'not in a subject');
      // Text off: plain words skip it (the choice is kept, so it's put back).
      await count('package');
      const chip = page.locator('.field-chip', { hasText: 'Text' });
      assert.equal(await chip.getAttribute('aria-pressed'), 'true', 'on by default');
      await chip.click();
      await page.waitForTimeout(400);
      assert.equal(await page.locator('.search-layer .row').count(), 0, 'Text off: not found');
      await chip.click();
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('a saved choice of every field from before includes the text', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.evaluate(() => localStorage.setItem('ui.searchFields', JSON.stringify(['sender', 'recipients', 'subject'])));
      await page.reload();
      await page.fill('.search-field input', 'package');
      await page.locator('.field-chip', { hasText: 'Text' }).waitFor();
      assert.equal(await page.locator('.field-chip', { hasText: 'Text' }).getAttribute('aria-pressed'), 'true');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('settings layout', () => {
  test('accounts open one at a time; Sender pictures in General is the View option', async () => {
    const { page, errors, close } = await open({ threaded: false, query: '&many' });
    try {
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'Accounts' }).click();
      const cards = page.locator('.st-acct');
      await cards.first().waitFor();
      const open = () => page.locator('.st-acct.open').count();
      assert.equal(await open(), 1, 'the account being viewed, open');
      assert.ok((await cards.count()) > 2);
      // closed ones say what they hold
      await page.waitForFunction(() => /messages? ·/.test(document.querySelectorAll('.st-acct:not(.open) .st-acct-sum')[0]?.textContent ?? ''));
      await cards.nth(2).locator('.st-acct-head').click();
      assert.equal(await open(), 1, 'one at a time');
      assert.equal(await cards.nth(2).evaluate((e) => e.classList.contains('open')), true);
      for (const g of ['You', 'Server', 'On this computer']) await cards.nth(2).locator('.st-group-title', { hasText: g }).waitFor();
      await cards.nth(2).locator('.st-acct-head').click();
      assert.equal(await open(), 0, 'and closed again');
      // General › Sender pictures: a level, Off to begin with; View options' switch goes back to it
      await page.locator('.st-nav-item', { hasText: 'General' }).click();
      const level = (name) => page.locator('.st-level', { hasText: name }).locator('input[type=radio]');
      assert.equal(await level('Off').isChecked(), true);
      await level('Your contacts’ photos').check();
      assert.equal(await page.evaluate(() => localStorage.getItem('ui.senderPictureLevel')), '"contacts"');
      await level('Off').check();
      await page.keyboard.press('Escape');
      await page.locator('button[aria-label="View options"]').first().click();
      await page.locator('.vm-item', { hasText: 'Sender pictures' }).click();
      assert.equal(await page.evaluate(() => localStorage.getItem('ui.senderPictureLevel')), '"contacts"', 'back to the level chosen');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('themes', () => {
  test('a theme picked in General colours the app, and is kept', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const shot = process.env.THEME_SHOTS;
      const bg = () => page.evaluate(() => getComputedStyle(document.querySelector('.pane-sidebar')).backgroundColor);
      const attrs = () => page.evaluate(() => [document.documentElement.dataset.theme ?? null, document.documentElement.dataset.scheme]);
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'General' }).click();
      const pick = (name) => page.locator('.st-theme', { has: page.getByText(name, { exact: true }) });
      assert.equal(await pick('Auto').getAttribute('aria-checked'), 'true', 'Auto to begin with');
      const auto = await bg();
      await pick('Paper').click();
      assert.deepEqual(await attrs(), ['paper', 'light']);
      assert.equal(await bg(), 'rgb(240, 238, 230)');
      await pick('Solarized Dark').click();
      assert.deepEqual(await attrs(), ['solarized-dark', 'dark']);
      assert.equal(await bg(), 'rgb(0, 33, 43)');
      // The dark rules follow the theme, not only the system: the dark colour of the plain Dark.
      await pick('Dark').click();
      assert.deepEqual(await attrs(), [null, 'dark']);
      assert.equal(await bg(), 'rgb(22, 22, 24)');
      await pick('Auto').click();
      assert.equal(await bg(), auto);
      // Kept: it's there from the first paint after a reload.
      await pick('Solarized Light').click();
      await page.reload();
      await page.waitForSelector('.folder-layer .row');
      assert.deepEqual(await attrs(), ['solarized-light', 'light']);
      assert.equal(await bg(), 'rgb(238, 232, 213)');
      if (shot) {
        for (const name of ['Light', 'Dark', 'Paper', 'Solarized Light', 'Solarized Dark']) {
          await page.keyboard.press('ControlOrMeta+,');
          await page.locator('.st-nav-item', { hasText: 'General' }).click();
          await pick(name).click();
          await page.screenshot({ path: `${shot}/settings-${name.replace(' ', '-')}.png` });
          await page.keyboard.press('Escape');
          await page.waitForTimeout(200);
          await page.screenshot({ path: `${shot}/app-${name.replace(' ', '-')}.png` });
        }
      }
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('mac title band', () => {
  test('with the sidebar folded, the rail top is the colour of the search row beside it', async () => {
    const { page, errors, close } = await open({ threaded: false, query: '&mac' });
    try {
      await page.keyboard.press('Meta+1');
      await page.waitForSelector('.app.rail-mode');
      await page.waitForTimeout(300);
      const rail = await page.locator('.pane-sidebar').boundingBox();
      const band = await page.locator('.search-bar').first().boundingBox();
      // one pixel each, in the band above the search field (where the traffic lights sit)
      const pixel = (x, y) => page.screenshot({ clip: { x, y, width: 1, height: 1 } });
      const left = await pixel(rail.x + 4, 4);
      const right = await pixel(band.x + band.width / 2, band.y + 4);
      assert.ok(left.equals(right), 'one colour across the band');
      // and below the band the rail keeps its own colour
      assert.ok(!(await pixel(rail.x + 4, band.y + band.height + 40)).equals(right), 'the rail below is the sidebar colour');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('compose', () => {
  test('a pasted picture goes into the text', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      const popup = context.waitForEvent('page');
      await page.keyboard.press('c');
      const w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      const editor = w.locator('.cmp-editor .ProseMirror');
      await editor.waitFor();
      await editor.click();
      await w.keyboard.type('Look: ');
      await editor.evaluate((el) => {
        // a 1×1 PNG, as a screenshot or "Copy image" puts it on the clipboard
        const b64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';
        const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
        const dt = new DataTransfer();
        dt.items.add(new File([bytes], 'image.png', { type: 'image/png' }));
        el.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
      });
      const img = editor.locator('img[src^="data:image/png;base64,"]');
      await img.waitFor();
      assert.match(await editor.innerHTML(), /Look:[\s\S]*?<img[^>]*src="data:image\/png/);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('a suggested address can be removed for good', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      const popup = context.waitForEvent('page');
      await page.keyboard.press('c');
      const w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      const to = w.locator('input[aria-label="To"]');
      await to.waitFor();
      assert.equal(await w.locator('#boot').count(), 0, 'the outline gives way to the composer');
      await to.fill('a');
      await w.waitForSelector('.addr-sugg-row');
      const first = async () => (await w.locator('.addr-sugg-row .sugg-addr, .addr-sugg-row .sugg-name').first().textContent()).trim();
      const gone = await first();
      const all = await w.locator('.addr-sugg-row').allTextContents();
      await to.press('Shift+Delete');
      await w.waitForFunction((n) => document.querySelectorAll('.addr-sugg-row').length === n - 1, all.length);
      // typed again: still not suggested
      await to.fill('');
      await to.fill('a');
      await w.waitForSelector('.addr-sugg-row');
      await w.waitForTimeout(200);
      const again = await w.locator('.addr-sugg-row').allTextContents();
      assert.ok(!again.some((t) => t.includes(gone)), `${gone} not suggested again: ${again}`);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('accounts', () => {
  test('closing during an export asks: keep exporting, or stop it', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const who = (await page.locator('.sidebar .account-name').allTextContents())[1];
      const exportIt = async () => {
        await page.keyboard.press('Control+,');
        const card = page.locator('.st-acct', { hasText: who });
        if (!(await card.evaluate((e) => e.classList.contains('open')))) await card.locator('.st-acct-head').click();
        await card.getByRole('button', { name: 'Export…' }).click();
        await page.getByRole('button', { name: 'Choose folder and export…' }).click();
        await page.waitForSelector('.acct-dialog', { state: 'detached' });
        await page.keyboard.press('Escape');
      };
      await exportIt();
      assert.ok(await page.evaluate(() => window.__askClose(false)));
      const ask = page.locator('.modal.confirm');
      await ask.getByText(/An export of .* is still running/).waitFor();
      assert.match(await ask.textContent(), /Closing Email’s window stops it/);
      await ask.getByRole('button', { name: 'Keep exporting' }).click();
      await page.locator('.toast', { hasText: /Exported 40 messages/ }).waitFor({ timeout: 8000 });

      await exportIt();
      await page.evaluate(() => window.__askClose(true));
      await ask.getByRole('button', { name: 'Stop export and quit' }).click();
      await page.locator('.toast', { hasText: /Export stopped/ }).waitFor({ timeout: 8000 });
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('export an account, then remove it', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const names = () => page.locator('.sidebar .account-name').allTextContents();
      const before = await names();
      const who = before[1];
      await page.keyboard.press('Control+,');
      const card = page.locator('.st-acct', { hasText: who });
      await card.locator('.st-acct-head').click(); // closed: only the account being viewed starts open
      // Esc closes a dialog over Settings, not Settings
      await card.getByRole('button', { name: 'Remove account…' }).click();
      await page.waitForSelector('.acct-dialog');
      await page.keyboard.press('Escape');
      await page.waitForSelector('.acct-dialog', { state: 'detached' });
      assert.equal(await page.locator('.settings').count(), 1, 'Settings still open');
      await card.getByRole('button', { name: 'Export…' }).click();
      const dialog = page.locator('.acct-dialog');
      await dialog.getByText(/messages, as one mbox file per folder/).waitFor();
      await dialog.getByText(/downloaded for the export first/).waitFor();
      await dialog.getByRole('button', { name: 'Choose folder and export…' }).click();
      await page.waitForSelector('.acct-dialog', { state: 'detached' });
      // progress: in the card and in the sync status row
      await card.getByText(/Exporting · /).waitFor();
      await page.waitForFunction(() => /Exporting .* \d+ \/ 40/.test(document.querySelector('.sf-summary')?.getAttribute('title') ?? ''));
      await page.locator('.toast', { hasText: /Exported 40 messages to/ }).waitFor({ timeout: 8000 });
      assert.match(await page.locator('.toast').textContent(), /Show in folder/);
      await card.getByRole('button', { name: 'Export…' }).waitFor();

      // remove: says what goes and what stays; then it's gone
      await card.getByRole('button', { name: 'Remove account…' }).click();
      await dialog.getByText('Nothing changes on the server.').waitFor();
      await dialog.getByRole('button', { name: 'Remove account' }).click();
      await page.waitForSelector('.acct-dialog', { state: 'detached' });
      await page.keyboard.press('Escape');
      await page.waitForFunction((w) => ![...document.querySelectorAll('.sidebar .account-name')].some((e) => e.textContent === w), who);
      assert.deepEqual(await names(), before.filter((n) => n !== who));
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('a Microsoft account can turn on tags and snooze', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      // From Settings › Accounts (the sidebar has no + any more).
      assert.equal(await page.locator('.sidebar [aria-label="Add account"]').count(), 0);
      await page.keyboard.press('Control+,');
      await page.locator('.settings .st-nav-item', { hasText: 'Accounts' }).click().catch(() => {});
      await page.getByRole('button', { name: '+ Add account' }).click();
      assert.equal(await page.locator('.settings').count(), 0, 'Settings makes way for the form');
      await page.fill('input[placeholder="you@example.com"]', 'jane@outlook.com');
      await page.getByRole('button', { name: 'Continue with Microsoft' }).click();
      await page.waitForSelector('.sidebar .account-name:has-text("jane@outlook.com")', { timeout: 15000 });
      await page.waitForSelector('.folder-layer .row', { timeout: 15000 });
      await page.locator('.folder-layer .row').first().click();
      const tag = page.locator('.reader .act-btn[aria-label^="Tag"]').first();
      assert.match(await tag.getAttribute('aria-label'), /doesn't keep tags/);
      await page.keyboard.press('Control+,');
      const acct = page.locator('.st-acct', { hasText: 'jane@outlook.com' });
      await acct.getByRole('button', { name: 'Turn on…' }).click();
      await acct.getByText('On, as Outlook categories').waitFor();
      await page.keyboard.press('Escape');
      await page.waitForFunction(() => document.querySelector('.reader .act-btn[aria-label^="Tag"]')?.getAttribute('aria-label') === 'Tag');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

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
      // Saved, and the accounts reloaded (which redraws the card): only then open the panel, or
      // the reload closes it under the click (on a slow machine, CI).
      await acct.locator('.acct-dot', { hasText: /^W$/ }).first().waitFor();
      // One swatch, the chosen color; it opens the panel of them all, and a pick closes it.
      assert.equal(await acct.locator('.st-swatch').count(), 1, 'only the chosen color shows');
      await acct.locator('.st-color-btn').click();
      await page.locator('.st-color-panel .st-swatch[title="Teal"]').click();
      await page.locator('.st-color-panel').waitFor({ state: 'detached' });
      await acct.locator('.st-color-btn[title="Marker color: Teal"]').waitFor({ timeout: 3000 });
      // Esc closes the panel only, not Settings.
      await acct.locator('.st-color-btn').click();
      await page.locator('.st-color-panel').waitFor();
      await page.keyboard.press('Escape');
      await page.locator('.st-color-panel').waitFor({ state: 'detached' });
      assert.equal(await acct.count(), 1, 'Settings still open');
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

describe('sorting', () => {
  /** The list's canvas in DOM order: headers ({ head, count }) and rows ({ from, unread }). */
  const listed = (page) =>
    page.locator('.folder-layer .list-canvas').evaluate((c) =>
      [...c.children].map((el) =>
        el.classList.contains('group-head')
          ? { head: el.querySelector('.group-label')?.textContent ?? '', count: el.querySelector('.group-count')?.textContent ?? null }
          : { from: el.querySelector('.row-from')?.textContent ?? '', unread: el.classList.contains('unread') },
      ),
    );
  async function sortBy(page, label) {
    await page.click('.view-menu-btn');
    await page.locator('.view-menu .vm-item', { hasText: label }).click();
    await page.keyboard.press('Escape');
    await page.waitForTimeout(400);
  }

  test('by date, the list is grouped into periods, newest first', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const items = await listed(page);
      const heads = items.filter((x) => x.head != null).map((x) => x.head);
      assert.ok(items[0].head != null, 'a header comes first');
      const order = ['Today', 'Yesterday', 'Sunday', 'Saturday', 'Friday', 'Thursday', 'Wednesday', 'Tuesday', 'Monday', 'Last week', 'Two weeks ago', 'Three weeks ago', 'September', 'August'];
      const ranks = heads.map((h) => order.indexOf(h)).filter((r) => r >= 0);
      assert.ok(ranks.length >= 2, `known periods among ${heads}`);
      assert.deepEqual(ranks, [...ranks].sort((a, b) => a - b), `periods in order: ${heads}`);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('by sender: one header per sender over their messages; conversations are off; N follows the order', async () => {
    const { page, errors, close } = await open({ threaded: true });
    try {
      await sortBy(page, 'From');
      // Down to where the first sender's messages end and the next one's begin.
      const firstCount = Number(await page.locator('.folder-layer .group-head .group-count').first().textContent());
      await page.locator('.folder-layer .list-scroll').evaluate((el, n) => (el.scrollTop = (n - 4) * 72), firstCount);
      await page.waitForTimeout(300);
      await page.click('.view-menu-btn');
      assert.equal(await page.locator('.view-menu .vm-item', { hasText: 'Group by conversation' }).getAttribute('aria-disabled'), 'true');
      await page.keyboard.press('Escape');
      const items = await listed(page);
      let head = null;
      const heads = [];
      for (const x of items) {
        if (x.head != null) {
          head = x.head;
          heads.push(head.toLowerCase());
        } else if (x.from && head != null) assert.equal(x.from.toLowerCase(), head.toLowerCase(), 'each row under its sender');
      }
      // A boundary: the rows above the header are someone else's.
      const k = items.findIndex((x) => x.head != null);
      assert.ok(k > 0, 'a header with rows above it');
      assert.notEqual(items[k - 1].from.toLowerCase(), items[k].head.toLowerCase(), 'the sender changes at the header');
      assert.ok(Number(items[k].count) > 0, 'with a count');
      assert.deepEqual(heads, [...heads].sort(), 'senders A to Z');
      // N goes to the first unread one in this order.
      await page.locator('.folder-layer .list-scroll').evaluate((el) => (el.scrollTop = 0));
      await page.waitForTimeout(300);
      const top = (await listed(page)).filter((x) => x.head == null);
      const firstUnread = top.findIndex((x) => x.unread);
      assert.ok(firstUnread >= 0, 'an unread one near the top');
      await page.locator('.folder-layer .list-scroll').focus();
      await page.keyboard.press('n');
      await page.waitForTimeout(400);
      assert.equal(await page.locator('.folder-layer .row.selected').getAttribute('id'), `row-${firstUnread}`);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('archiving in a group takes one off its count; undo puts it back', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await sortBy(page, 'From');
      const first = page.locator('.folder-layer .group-head').first();
      const before = Number(await first.locator('.group-count').textContent());
      await page.locator('.folder-layer .row').first().click();
      await page.waitForTimeout(300);
      await page.locator('.reader [aria-label="Archive"]').first().click();
      await hasText(first.locator('.group-count'), String(before - 1), 'one fewer in the group');
      await page.locator('.toast button', { hasText: 'Undo' }).click();
      await page.waitForTimeout(400);
      await hasText(page.locator('.folder-layer .group-head').first().locator('.group-count'), String(before), 'back again');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('group headers', () => {
  test("a header's click picks its group; its trash deletes them all after asking", async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.click('.view-menu-btn');
      await page.locator('.view-menu .vm-item', { hasText: 'From' }).click();
      await page.keyboard.press('Escape');
      await page.waitForTimeout(400);
      // A sender with several messages.
      const heads = page.locator('.folder-layer .group-head');
      const counts = await heads.locator('.group-count').allTextContents();
      const k = counts.findIndex((c) => Number(c) >= 2);
      assert.ok(k >= 0, `a group of two or more: ${counts}`);
      const head = heads.nth(k);
      const n = Number(counts[k]);
      const label = (await head.locator('.group-label').textContent()).trim();
      await head.click();
      await hasText(page.locator('.multi-pane .reader-subject'), `${n} messages selected`, 'the group picked');
      await page.keyboard.press('Escape');
      await head.hover();
      await head.locator('.group-del').click();
      assert.match(await page.locator('.confirm-backdrop .confirm-text').textContent(), new RegExp(`Delete ${n} messages in`));
      await page.locator('.confirm-backdrop .btn.danger').click();
      await page.locator('.toast', { hasText: `Deleted ${n} messages` }).waitFor();
      await page.waitForTimeout(300);
      const left = await page.locator('.folder-layer .group-head .group-label').allTextContents();
      assert.ok(!left.map((x) => x.trim()).includes(label), 'the group is gone');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('size and filters', () => {
  const sizeOf = (t) => {
    const [n, u] = t.trim().split(' ');
    return Number(n) * ({ B: 1, KB: 1024, MB: 2 ** 20, GB: 2 ** 30 }[u] ?? 1);
  };
  async function menu(page, label) {
    await page.click('.view-menu-btn');
    await page.locator('.view-menu .vm-item', { hasText: label }).click();
    await page.keyboard.press('Escape');
    await page.waitForTimeout(400);
  }

  test('by size: largest first, under size ranges, sizes shown', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await menu(page, 'Size');
      const sizes = await page.locator('.folder-layer .row .row-size').allTextContents();
      assert.ok(sizes.length > 5, 'sizes on the rows');
      const bytes = sizes.map(sizeOf);
      assert.deepEqual(bytes, [...bytes].sort((a, b) => b - a), `largest first: ${sizes.slice(0, 8)}`);
      const head = await page.locator('.folder-layer .group-head .group-label').first().textContent();
      assert.match(head, /^(Huge|Very large|Large|Medium|Small|Tiny) \(/);
      assert.ok(Number(await page.locator('.folder-layer .group-head .group-count').first().textContent()) > 0);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('unread only: the list shows just those, says so, and the one opened stays', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const all = await page.locator('.folder-layer .list-sub').textContent();
      await menu(page, 'Unread only');
      await page.waitForSelector('.folder-layer .filter-bar');
      assert.match(await page.locator('.folder-layer .filter-what').textContent(), /Unread/);
      const rows = page.locator('.folder-layer .row');
      const n = await rows.count();
      assert.ok(n > 0);
      assert.equal(await page.locator('.folder-layer .row:not(.unread)').count(), 0, 'only unread rows');
      assert.notEqual(await page.locator('.folder-layer .list-sub').textContent(), all, 'fewer messages');
      // Opening one reads it; it stays (with the others) until something else is opened.
      await rows.first().click();
      await page.waitForTimeout(1200);
      const subject = await selected(page).textContent();
      assert.equal(await page.locator('.folder-layer .row', { hasText: subject }).count(), 1, 'the opened one stays');
      // Show all again.
      await page.locator('.folder-layer .filter-clear').click();
      await page.waitForTimeout(400);
      assert.equal(await page.locator('.folder-layer .filter-bar').count(), 0);
      assert.ok((await page.locator('.folder-layer .row:not(.unread)').count()) > 0, 'read ones back');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('the header counts: unread shows only unread (again: all), the message count shows all', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const unread = page.locator('.folder-layer .list-sub .count-unread');
      const bar = page.locator('.folder-layer .filter-bar');
      await unread.click();
      await bar.waitFor();
      assert.match(await page.locator('.folder-layer .filter-what').textContent(), /Unread/);
      assert.equal(await unread.getAttribute('aria-pressed'), 'true');
      // The list reloads after the bar shows: wait for it.
      const readRows = page.locator('.folder-layer .row:not(.unread)');
      const settles = async (ok, what) => {
        const end = Date.now() + 3000;
        while (!ok(await readRows.count())) {
          if (Date.now() > end) assert.fail(`${what}: ${await readRows.count()} read rows`);
          await page.waitForTimeout(50);
        }
      };
      await settles((n) => n === 0, 'only unread rows');
      await unread.click();
      await bar.waitFor({ state: 'detached' });
      assert.equal(await unread.getAttribute('aria-pressed'), 'false');
      await settles((n) => n > 0, 'read ones back');
      // The message count shows all, from any filter.
      await unread.click();
      await bar.waitFor();
      await page.locator('.folder-layer .list-sub .count-btn', { hasText: 'messages' }).click();
      await bar.waitFor({ state: 'detached' });
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('with attachments, in conversations too', async () => {
    const { page, errors, close } = await open({ threaded: true });
    try {
      await menu(page, 'With attachments only');
      await page.waitForSelector('.folder-layer .filter-bar');
      const rows = await page.locator('.folder-layer .row').count();
      assert.ok(rows > 0, 'some conversations with attachments');
      assert.equal(await page.locator('.folder-layer .row').filter({ hasNot: page.locator('.clip') }).count(), 0, 'each shows a paperclip');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('folder menu', () => {
  test('right-click a folder: Mark all as read', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const inbox = page.locator('.sidebar .folder', { hasText: 'Inbox' }).first();
      const badge = inbox.locator('.badge');
      assert.ok(Number((await badge.textContent()).replace(/\D/g, '')) > 0, 'unread to begin with');
      await inbox.click({ button: 'right' });
      await page.locator('.ctx-menu .vm-item', { hasText: 'Mark all as read' }).click();
      await page.waitForSelector('.toast:has-text("as read")');
      await page.waitForTimeout(300);
      assert.equal(await badge.count(), 0, 'no unread badge');
      assert.equal(await page.locator('.folder-layer .row.unread').count(), 0, 'no unread rows');
      // Nothing left: the item is greyed out.
      await inbox.click({ button: 'right' });
      assert.equal(await page.locator('.ctx-menu .vm-item', { hasText: 'Mark all as read' }).isDisabled(), true);
      await page.keyboard.press('Escape');
      assert.equal(await page.locator('.ctx-menu').count(), 0);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('signatures', () => {
  test('made in Settings, a new message gets the account default; it can be switched or taken off', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      await page.keyboard.press(process.platform === 'darwin' ? 'Meta+,' : 'Control+,');
      await page.locator('.st-nav-item', { hasText: 'Signatures' }).click();
      await page.locator('.sig-add').click();
      await page.waitForFunction(() => document.querySelector('.sig-name')?.value === 'Signature');
      await page.locator('.sig-name').fill('Work');
      await page.locator('.sig-editor .ProseMirror').click();
      await page.keyboard.type('Lars Pehrsson, Example AB');
      await page.waitForTimeout(700);
      await page.locator('.sig-add').click();
      await page.waitForFunction(() => document.querySelector('.sig-name')?.value === 'Signature 2');
      await page.locator('.sig-name').fill('Short');
      await page.locator('.sig-editor .ProseMirror').click();
      await page.keyboard.type('/L');
      await page.waitForTimeout(700);
      assert.deepEqual(await page.locator('.sig-item').allTextContents(), ['Work', 'Short']);
      // Work: the first account's new messages.
      await page.locator('.sig-item', { hasText: 'Work' }).click();
      await page.locator('.sig-use input[type="checkbox"]').first().check();
      await page.waitForTimeout(200);
      await page.keyboard.press('Escape');
      const popup = context.waitForEvent('page');
      await page.keyboard.press('c');
      const w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.locator('.cmp-sig-body').waitFor();
      assert.equal((await w.locator('.cmp-sig-body').textContent()).trim(), 'Lars Pehrsson, Example AB');
      // The picker shows the one in use (not "No signature").
      await w.waitForTimeout(300);
      const sigBox = w.locator('.sel.cmp-sig-select');
      assert.equal(await sigBox.locator('.sel-text').textContent(), 'Work');
      await choose(sigBox, 'Short');
      await hasText(w.locator('.cmp-sig-body'), '/L', 'switched');
      await choose(sigBox, 'No signature');
      await w.waitForTimeout(100);
      assert.equal(await w.locator('.cmp-sig-body').count(), 0, 'taken off');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('signature defaults', () => {
  test("choosing one account's default leaves the others' as they were", async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.keyboard.press(process.platform === 'darwin' ? 'Meta+,' : 'Control+,');
      await page.locator('.st-nav-item', { hasText: 'Signatures' }).click();
      for (const name of ['Work', 'Home', 'Short']) {
        await page.locator('.sig-add').click();
        await page.waitForTimeout(150);
        await page.locator('.sig-name').fill(name);
        await page.waitForTimeout(650);
      }
      // Each signature's boxes: [account 1 new, account 1 replies, account 2 new, account 2 replies].
      const boxes = page.locator('.sig-use input[type="checkbox"]');
      const checked = () => boxes.evaluateAll((els) => els.slice(0, 4).map((e) => e.checked));
      const show = async (name) => {
        await page.locator('.sig-item', { hasText: name }).click();
        await page.waitForTimeout(150);
      };
      await show('Home');
      assert.ok((await boxes.count()) >= 4, 'two accounts, two choices each');
      await boxes.nth(0).check();
      await page.waitForTimeout(250);
      await boxes.nth(3).check();
      await page.waitForTimeout(250);
      await show('Short');
      await boxes.nth(1).check();
      await page.waitForTimeout(250);
      await show('Work');
      // Where another signature is the default, it says which.
      assert.match(await page.locator('.sig-use-cell').nth(0).textContent(), /now Home/);
      await boxes.nth(2).check();
      await page.waitForTimeout(400);
      assert.deepEqual(await checked(), [false, false, true, false]);
      await show('Home');
      assert.deepEqual(await checked(), [true, false, false, true]);
      await show('Short');
      assert.deepEqual(await checked(), [false, true, false, false]);
      // The list shows where each is used.
      assert.equal(await page.locator('.sig-item', { hasText: 'Home' }).locator('.acct-dot').count(), 2);
      // Taking an account's default for one signature takes it from the other.
      await boxes.nth(0).check();
      await page.waitForTimeout(400);
      await show('Home');
      assert.deepEqual(await checked(), [false, false, false, true]);
      // Renaming a signature (the list reloads) changes no one's choice either.
      await show('Work');
      await page.locator('.sig-name').fill('Work 2');
      await page.waitForTimeout(800);
      assert.deepEqual(await checked(), [false, false, true, false]);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('signature html', () => {
  test('a signature pasted as HTML keeps its colours, opens as HTML again, and goes into the message', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      await page.keyboard.press(process.platform === 'darwin' ? 'Meta+,' : 'Control+,');
      await page.locator('.st-nav-item', { hasText: 'Signatures' }).click();
      await page.locator('.sig-add').click();
      await page.waitForFunction(() => document.querySelector('.sig-name')?.value === 'Signature');
      await page.locator('.sig-html-btn').click();
      assert.equal(await page.locator('.sig-editor').isVisible(), false);
      await page.locator('textarea.sig-html').fill('<p style="color:#c00">Lars</p><table><tr><td style="color:navy">PaperCut</td></tr></table>');
      await page.waitForTimeout(800);
      await page.locator('.sig-preview p[style*="color"]').waitFor();
      assert.equal(await page.locator('.sig-html-btn').getAttribute('aria-pressed'), 'true');
      // Going back to the simple editor would lose the colours: asked first.
      await page.locator('.sig-html-btn').click();
      await page.locator('.confirm-backdrop').waitFor();
      await page.locator('.confirm-backdrop .btn', { hasText: 'Cancel' }).click();
      assert.equal(await page.locator('textarea.sig-html').count(), 1, 'still HTML');
      await page.locator('.sig-use input[type="checkbox"]').first().check();
      await page.waitForTimeout(200);
      await page.keyboard.press('Escape');
      const popup = context.waitForEvent('page');
      await page.keyboard.press('c');
      const w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.locator('.cmp-sig-body p[style*="color"]').waitFor();
      assert.match(await w.locator('.cmp-sig-body').textContent(), /PaperCut/);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('pictures in a message', () => {
  test('a picture pasted into the message goes in where the cursor is, not attached', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      const popup = context.waitForEvent('page');
      await page.keyboard.press('c');
      const w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      const body = w.locator('.cmp-editor .ProseMirror');
      await body.waitFor();
      await body.click();
      await w.keyboard.type('Here it is: ');
      await body.evaluate(async (el) => {
        const c = document.createElement('canvas');
        c.width = 3000;
        c.height = 1500;
        const g = c.getContext('2d');
        g.fillStyle = '#3a7';
        g.fillRect(0, 0, 3000, 1500);
        const blob = await new Promise((r) => c.toBlob(r, 'image/png'));
        const dt = new DataTransfer();
        dt.items.add(new File([blob], 'shot.png', { type: 'image/png' }));
        el.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
      });
      const img = w.locator('.cmp-editor img[src^="data:image/"]');
      await img.waitFor();
      assert.equal(await img.evaluate((i) => i.decode().then(() => i.naturalWidth)), 1600, 'scaled down');
      assert.ok(Number(await img.getAttribute('width')) <= 640);
      assert.equal(await w.locator('.cmp-att').count(), 0, 'not an attachment');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('signature pictures', () => {
  test('a picture pasted or chosen goes in, scaled, and shows in the composer', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      await page.keyboard.press(process.platform === 'darwin' ? 'Meta+,' : 'Control+,');
      await page.locator('.st-nav-item', { hasText: 'Signatures' }).click();
      await page.locator('.sig-add').click();
      await page.waitForFunction(() => document.querySelector('.sig-name')?.value === 'Signature');
      await page.locator('.sig-editor .ProseMirror').click();
      await page.keyboard.type('Lars ');
      // Paste a 2000×1000 picture (a big screenshot): it comes in scaled down to 800 wide.
      await page.locator('.sig-editor .ProseMirror').evaluate(async (el) => {
        const c = document.createElement('canvas');
        c.width = 2000;
        c.height = 1000;
        const g = c.getContext('2d');
        g.fillStyle = '#e33';
        g.fillRect(0, 0, 2000, 1000);
        const blob = await new Promise((r) => c.toBlob(r, 'image/png'));
        const dt = new DataTransfer();
        dt.items.add(new File([blob], 'logo.png', { type: 'image/png' }));
        el.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
      });
      const img = page.locator('.sig-editor img[src^="data:image/png"]');
      await img.waitFor();
      const natural = await img.evaluate((i) => (i.decode ? i.decode().then(() => i.naturalWidth) : i.naturalWidth));
      assert.equal(natural, 800, 'scaled down');
      assert.ok(Number(await img.getAttribute('width')) <= 300, 'shown at most 300 wide');
      // And one chosen with the button.
      const png = await page.evaluate(() => {
        const c = document.createElement('canvas');
        c.width = c.height = 40;
        c.getContext('2d').fillRect(0, 0, 40, 40);
        return c.toDataURL('image/png').split(',')[1];
      });
      await page.locator('.sig-edit input[type="file"]').setInputFiles({ name: 'dot.png', mimeType: 'image/png', buffer: Buffer.from(png, 'base64') });
      // (ProseMirror puts spacer <img>s of its own around inline nodes: count pictures.)
      await page.waitForFunction(() => document.querySelectorAll('.sig-editor img[src^="data:"]').length === 2);
      await page.waitForTimeout(800);
      await page.locator('.sig-use input[type="checkbox"]').first().check();
      await page.waitForTimeout(200);
      await page.keyboard.press('Escape');
      const popup = context.waitForEvent('page');
      await page.keyboard.press('c');
      const w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.locator('.cmp-sig-body img').first().waitFor();
      assert.equal(await w.locator('.cmp-sig-body img[src^="data:image/png"]').count(), 2, 'both in the preview');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('sender pane', () => {
  test('shows everything from the sender; select all, delete, undo', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').first().click();
      const openId = await page.locator('.folder-layer .row.selected').getAttribute('data-id');
      const from = (await page.locator('.folder-layer .row.selected .row-from').textContent()).trim();
      await page.keyboard.press('ControlOrMeta+2');
      await hasText(page.locator('.sp-name'), from, 'the sender of the message on screen');
      /** The sender's total ("12 messages", "3 of 12 selected"). */
      const count = async () => {
        const t = await page.locator('.sp-all').textContent();
        return Number((t.includes(' of ') ? t.split(' of ')[1] : t).replace(/[^\d]/g, ''));
      };
      const before = await count();
      assert.ok(before >= 1);
      assert.equal(await page.locator('.sp-row.current').count(), 1, 'the open one is marked');
      assert.equal(await page.locator('.sp-row input[type="checkbox"]').count(), 0, 'no tick boxes');
      // A click picks one; ⌘/Ctrl+A in the panel all of them.
      await page.locator('.sp-row').first().click();
      assert.equal(await page.locator('.sp-row.on').count(), 1);
      await page.keyboard.press('ControlOrMeta+a');
      // (Everything from the sender comes in first, then it's all picked.)
      await page.waitForFunction(() => document.querySelectorAll('.sp-row.on').length > 1);
      const picked = await page.locator('.sp-row.on').count();
      assert.equal(picked, await page.locator('.sp-row').count(), 'all shown are picked');
      assert.equal(await page.locator('.folder-layer .multi-pane').count(), 0, "the list's selection is its own");
      await page.keyboard.press('Delete');
      if (picked > 1) await page.locator('.confirm-backdrop .btn.danger').click();
      await page.locator('.toast', { hasText: 'Deleted' }).waitFor();
      // The overview moves on to the next email instead of going blank.
      assert.equal(await page.locator('.reader-empty').count(), 0, 'a message is still open');
      assert.equal(await page.locator('.folder-layer .row.selected').count(), 1, 'the next row is selected');
      assert.notEqual(await page.locator('.folder-layer .row.selected').getAttribute('data-id'), openId);
      // The pane follows the open message: it now shows the next email's sender.
      await page.waitForFunction(() => {
        const name = document.querySelector('.sp-name')?.textContent?.trim();
        const from = document.querySelector('.folder-layer .row.selected .row-from')?.textContent?.trim();
        return !!name && !!from && name === from;
      });
      await page.locator('.toast button', { hasText: 'Undo' }).click();
      await page.waitForFunction((n) => {
        const t = document.querySelector('.sp-all')?.textContent ?? '';
        return Number((t.includes(' of ') ? t.split(' of ')[1] : t).replace(/[^\d]/g, '')) === n;
      }, before);
      // Undo opens the deleted message again, where it was.
      await page.waitForFunction((id) => document.querySelector('.folder-layer .row.selected')?.dataset.id === id, openId);
      // Off again, remembered.
      await page.keyboard.press('ControlOrMeta+2');
      assert.equal(await page.locator('.sender-pane').count(), 0);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('move everything from a sender to a folder, with one Undo', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.fill('.search-field input', 'from:lumengoods');
      await page.waitForSelector('.search-layer .row');
      await page.waitForTimeout(400);
      await page.locator('.search-layer .row').first().click();
      await page.keyboard.press('ControlOrMeta+2');
      await page.locator('.sp-row').first().waitFor();
      const folders = () => page.locator('.sp-row .sp-folder').allTextContents();
      const before = await folders();
      // Each row's folder by message, to check one by one after Undo (the rows shown can differ).
      const wasIn = await page.locator('.sp-row').evaluateAll((rows) => Object.fromEntries(rows.map((r) => [r.dataset.id, r.querySelector('.sp-folder')?.textContent])));
      assert.ok(before.length >= 2, 'more than one from the sender');
      assert.ok(before.some((f) => f !== 'Receipts'), 'not all in Receipts yet');
      // Select all, Move to… › type to find the folder › Enter.
      await page.locator('.sp-select-all').click();
      await page.waitForFunction(() => document.querySelectorAll('.sp-row.on').length > 1);
      await page.locator('.sender-pane [aria-label="Move to folder"]').click();
      await page.locator('.move-picker .mp-filter').fill('Receipts');
      await page.keyboard.press('Enter');
      // All of the sender's mail is picked (not only the rows shown); those already in Receipts
      // stay as they are and aren't counted.
      const total = Number((await page.locator('.sp-all').textContent()).split(' of ').pop().replace(/[^\d]/g, ''));
      const toast = page.locator('.toast', { hasText: /^Moved [\d,]+ messages to Receipts/ });
      await toast.waitFor();
      const moved = Number((await toast.textContent()).match(/Moved ([\d,]+)/)[1].replace(/,/g, ''));
      assert.ok(moved > 0 && moved < total, `moved ${moved} of ${total}: the ones in Receipts already left out`);
      assert.match(await toast.textContent(), new RegExp(`\\(${(total - moved).toLocaleString('en-US')} already there\\)`), 'and it says how many');
      await page.waitForFunction(() => [...document.querySelectorAll('.sp-row .sp-folder')].every((e) => e.textContent === 'Receipts'));
      // One Undo puts them all back where they were.
      await page.locator('.toast button', { hasText: 'Undo' }).click();
      await page.waitForFunction((was) => {
        const rows = [...document.querySelectorAll('.sp-row')].filter((r) => r.dataset.id in was);
        return rows.length > 0 && rows.every((r) => r.querySelector('.sp-folder')?.textContent === was[r.dataset.id]);
      }, wasIn);
      // One that's in Receipts already: nothing to move, and it says so.
      await page.locator('.sp-row', { has: page.locator('.sp-folder', { hasText: /^Receipts$/ }) }).first().click();
      await page.locator('.sender-pane [aria-label="Move to folder"]').click();
      await page.locator('.move-picker .mp-filter').fill('Receipts');
      await page.keyboard.press('Enter');
      await page.locator('.toast', { hasText: 'Already in Receipts' }).waitFor();
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('deleting just the open one opens the next email; Undo goes back', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').first().click();
      const openId = await page.locator('.folder-layer .row.selected').getAttribute('data-id');
      await page.keyboard.press('ControlOrMeta+2');
      await page.locator('.sp-row.current').waitFor();
      await page.locator('.sp-row.current').click();
      assert.equal(await page.locator('.sp-row.on').count(), 1, 'only the open one picked');
      await page.keyboard.press('Delete');
      await page.locator('.toast', { hasText: 'Deleted 1 message' }).waitFor();
      await page.waitForFunction((gone) => {
        const sel = document.querySelector('.folder-layer .row.selected');
        return !!sel && sel.dataset.id !== gone;
      }, openId);
      assert.equal(await page.locator('.reader-empty').count(), 0, 'the next email is open, not a blank reader');
      await page.locator('.toast button', { hasText: 'Undo' }).click();
      await page.waitForFunction((id) => document.querySelector('.folder-layer .row.selected')?.dataset.id === id, openId);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('conversations: deleting the open one from the panel never blanks the overview', async () => {
    const { page, errors, close } = await open({ threaded: true });
    try {
      await page.locator('.folder-layer .row').first().click();
      const openId = await page.locator('.folder-layer .row.selected').getAttribute('data-id');
      await page.keyboard.press('ControlOrMeta+2');
      await page.locator('.sp-row.current').waitFor();
      await page.locator('.sp-row.current').click();
      await page.keyboard.press('ControlOrMeta+a');
      await page.waitForFunction(() => document.querySelectorAll('.sp-row.on').length > 0);
      const picked = await page.locator('.sp-row.on').count();
      await page.keyboard.press('Delete');
      if (picked > 1) await page.locator('.confirm-backdrop .btn.danger').click();
      await page.locator('.toast', { hasText: 'Deleted' }).waitFor();
      await page.waitForTimeout(400);
      assert.ok((await page.locator('.folder-layer .row').count()) > 0, 'other conversations remain');
      assert.equal(await page.locator('.reader-empty').count(), 0, 'still reading, not blank');
      const sel = page.locator('.folder-layer .row.selected');
      assert.equal(await sel.count(), 1);
      // The conversation went entirely: the next one is open. Some of it left: it stays open.
      const stillThere = await page.locator(`.folder-layer .row[data-id="${openId}"]`).count();
      if (!stillThere) assert.notEqual(await sel.getAttribute('data-id'), openId);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('images', () => {
  test('always from this sender: its other mail loads images too; Settings lists it', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.fill('.search-field input', 'from:lumengoods');
      await page.waitForSelector('.search-layer .row');
      await page.waitForTimeout(400);
      const fromSender = page.locator('.search-layer .row');
      assert.ok((await fromSender.count()) >= 2, 'two from the sender');
      await fromSender.nth(0).click();
      const banner = page.locator('.reader .remote-banner');
      await banner.waitFor();
      const always = banner.locator('.sel.remote-always');
      assert.equal((await always.textContent()).trim(), 'Always load…');
      assert.deepEqual(await optionsOf(always), ['From news@lumengoods.com', 'From everyone at lumengoods.com']);
      await choose(always, 'From news@lumengoods.com');
      await page.waitForTimeout(500);
      assert.equal(await banner.count(), 0, 'loaded');
      await fromSender.nth(1).click();
      await page.waitForTimeout(800);
      assert.equal(await page.locator('.reader .remote-banner').count(), 0, 'the next one too');
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'General' }).click();
      await page.locator('.st-remote-senders li').first().waitFor();
      assert.match(await page.locator('.st-remote-senders').textContent(), /lumen/i);
      await page.locator('.st-remote-senders .link-btn').first().click();
      await page.waitForTimeout(200);
      assert.equal(await page.locator('.st-remote-senders li').count(), 0, 'removed');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('images from a domain', () => {
  test('everyone at the sender\u2019s domain: listed as @domain in Settings', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.fill('.search-field input', 'from:lumengoods');
      await page.waitForSelector('.search-layer .row');
      await page.waitForTimeout(400);
      const rows = page.locator('.search-layer .row');
      await rows.nth(0).click();
      const banner = page.locator('.reader .remote-banner');
      await banner.waitFor();
      await choose(banner.locator('.sel.remote-always'), 'From everyone at lumengoods.com');
      await page.waitForTimeout(500);
      assert.equal(await banner.count(), 0, 'loaded');
      await rows.nth(1).click();
      await page.waitForTimeout(800);
      assert.equal(await page.locator('.reader .remote-banner').count(), 0, 'the next one too');
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'General' }).click();
      await page.locator('.st-remote-senders li').first().waitFor();
      assert.match(await page.locator('.st-remote-senders').textContent(), /@lumengoods\.com/);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('attachments', () => {
  test('each attachment is a chip with its name and size; right-click to open, save, save as, or all', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('button[aria-label="View options"]').first().click();
      await page.locator('.vm-item', { hasText: 'With attachments only' }).click();
      await page.keyboard.press('Escape');
      const rows = page.locator('.folder-layer .row');
      await rows.first().waitFor();
      // One with several attachments.
      for (let k = 0; k < 15; k++) {
        await rows.nth(k).click();
        await page.waitForTimeout(300);
        if ((await page.locator('.reader .att-chip').count()) > 1) break;
      }
      const chips = page.locator('.reader .att-chip:not(.att-more)');
      const n = parseInt(await page.locator('.reader .att-chips').getAttribute('aria-label'));
      assert.ok(n > 1, 'several attachments');
      // The name and size are on the chip, not hidden in a menu.
      const first = chips.first();
      const name = (await first.locator('.att-chip-name').textContent()).trim();
      assert.match(name, /\.[a-z0-9]+$/i, 'a file name');
      assert.match(await first.locator('.att-chip-size').textContent(), /\d/);
      // Right-click: one file's actions, then all of them.
      await first.click({ button: 'right' });
      const items = (await page.locator('.ctx-menu .vm-item').allTextContents()).map((t) => t.trim());
      assert.deepEqual(items, ['Open', 'Save to Downloads', 'Save As…', `Open All ${n}`, `Save All ${n} to Downloads`]);
      assert.equal(await page.locator('.ctx-menu .vm-sep').count(), 1, 'a line between the two groups');
      await page.locator('.ctx-menu .vm-item', { hasText: 'Save As…' }).click();
      await page.locator('.toast', { hasText: `Saved ${name}` }).waitFor();
      await first.click({ button: 'right' });
      await page.locator('.ctx-menu .vm-item', { hasText: 'Save to Downloads' }).click();
      await page.locator('.toast', { hasText: /Saved .* to Downloads/ }).waitFor();
      await page.locator('.reader .att-save-all').click();
      await page.locator('.toast', { hasText: `Saved ${n} attachments to Downloads` }).waitFor({ timeout: 5000 });
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('attachments, many', () => {
  test('up to three rows of chips, then "+N more" shows the rest in place; the page scrolls as one', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('button[aria-label="View options"]').first().click();
      await page.locator('.vm-item', { hasText: 'With attachments only' }).click();
      await page.keyboard.press('Escape');
      const rows = page.locator('.folder-layer .row');
      await rows.first().waitFor();
      const total = async () => parseInt((await page.locator('.reader .att-chips').getAttribute('aria-label').catch(() => '0')) ?? '0');
      for (let k = 0; k < 40; k++) {
        await rows.nth(k).click();
        await page.waitForTimeout(300);
        if ((await page.locator('.reader .att-more').count()) > 0) break;
      }
      const more = page.locator('.reader .att-more');
      assert.equal(await more.count(), 1, 'a message with more than three rows of attachments');
      const n = await total();
      const chips = page.locator('.reader .att-chip:not(.att-more)');
      const shown = await chips.count();
      assert.equal((await more.textContent()).trim(), `+${n - shown} more`);
      // Three rows at most, the "+N more" chip included.
      const tops = await page.locator('.reader .att-chip').evaluateAll((els) => [...new Set(els.map((e) => Math.round(e.getBoundingClientRect().top)))]);
      assert.ok(tops.length <= 3, `rows: ${tops.length}`);
      // Every one, in place; and back.
      await more.click();
      assert.equal(await chips.count(), n, 'all shown');
      await page.locator('.reader .att-fewer').click();
      await page.locator('.reader .att-more').waitFor();
      const again = await page.locator('.reader .att-chip').evaluateAll((els) => [...new Set(els.map((e) => Math.round(e.getBoundingClientRect().top)))].length);
      assert.ok(again <= 3 && (await chips.count()) < n, `folded again (${again} rows)`);
      // No scrolling area of its own over the message: the page under the band scrolls.
      const head = await page.locator('.reader .reader-head').evaluate((el) => getComputedStyle(el).overflowY);
      assert.equal(head, 'visible');
      const frameScrolls = await page.evaluate(() => {
        const de = document.querySelector('.reader .card-frame')?.contentDocument?.documentElement;
        return de ? de.scrollHeight > de.clientHeight + 1 : false;
      });
      assert.equal(frameScrolls, false, 'the message is as tall as it is');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('people in the header', () => {
  test('right-click a name: write to them, or copy their address', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      await page.evaluate(() => {
        window.__copied = [];
        navigator.clipboard.writeText = async (t) => void window.__copied.push(t);
      });
      // A message with a named recipient.
      const rows = page.locator('.folder-layer .row');
      await rows.first().click();
      const recip = page.locator('.reader .recip').first();
      await recip.waitFor();
      const addr = await recip.getAttribute('title');
      const name = (await recip.textContent()).trim();
      await recip.click({ button: 'right' });
      const labels = (await page.locator('.ctx-menu .vm-item').allTextContents()).map((t) => t.trim());
      assert.equal(labels[0], `New message to ${name}`);
      assert.ok(labels.includes('Copy address'));
      await page.locator('.ctx-menu .vm-item', { hasText: 'Copy address' }).click();
      assert.deepEqual(await page.evaluate(() => window.__copied), [addr]);
      // The sender too: a new message to them.
      const from = page.locator('.reader .reader-from-name');
      const fromName = (await from.textContent()).trim();
      await from.click({ button: 'right' });
      const popup = context.waitForEvent('page');
      await page.locator('.ctx-menu .vm-item', { hasText: `New message to ${fromName}` }).click();
      const w = await popup;
      await w.locator('.cmp-fields .addr-chip').first().waitFor();
      const to = (await w.locator('.cmp-fields .addr-chip').allTextContents()).map((t) => t.replace('×', '').trim());
      assert.deepEqual(to, [fromName]);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('account picture', () => {
  test('a picture of your own for an account shows in the sidebar and the sync status', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'Accounts' }).click();
      const png = await page.evaluate(() => {
        const c = document.createElement('canvas');
        c.width = c.height = 300;
        const g = c.getContext('2d');
        g.fillStyle = '#e33';
        g.fillRect(0, 0, 300, 300);
        return c.toDataURL('image/png').split(',')[1];
      });
      // The file dialog takes the window's focus: Settings stays.
      await page.evaluate(() => window.dispatchEvent(new Event('blur')));
      assert.equal(await page.locator('.settings').count(), 1, 'still open');
      await page.locator('.st-mark-pic input[type="file"]').first().setInputFiles({ name: 'me.png', mimeType: 'image/png', buffer: Buffer.from(png, 'base64') });
      await page.locator('.st-mark-pic .acct-dot img').first().waitFor();
      const natural = await page.locator('.st-mark-pic .acct-dot img').first().evaluate((i) => i.naturalWidth);
      assert.equal(natural, 128, 'scaled down');
      await page.keyboard.press('Escape');
      await page.locator('.sidebar .account-head .acct-dot img').first().waitFor();
      await page.click('.sf-summary');
      assert.equal(await page.locator('.sync-popover .acct-dot img').count(), 1, 'in the sync status too');
      // Taken away again: the letter.
      await page.keyboard.press('Escape');
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-mark-pic .link-btn', { hasText: 'Remove' }).first().click();
      await page.keyboard.press('Escape');
      await page.waitForTimeout(200);
      assert.equal(await page.locator('.sidebar .account-head .acct-dot img').count(), 0);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('google contacts', () => {
  test('connect (asking for the OAuth client first), contacts arrive marked, new ones can go there, disconnect', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'Accounts' }).click();
      const field = page.locator('.st-acct').first().locator('.gc');
      await field.getByRole('button', { name: 'Connect…' }).click();
      // No client yet: asked for it.
      await field.locator('input[aria-label="Google client ID"]').fill('not-a-client');
      await field.getByRole('button', { name: 'Save' }).click();
      await field.locator('.st-status.error').waitFor();
      await field.locator('input[aria-label="Google client ID"]').fill('123-abc.apps.googleusercontent.com');
      await field.locator('input[aria-label="Google client secret"]').fill('GOCSPX-x');
      await field.getByRole('button', { name: 'Save' }).click();
      await field.locator('.gc-linked').waitFor();
      assert.match(await field.locator('.gc-linked').textContent(), /3 contacts/);
      assert.match(await page.locator('.gc-client-note').textContent(), /123…/);
      await page.keyboard.press('Escape');
      // In the address book: theirs marked; a new one can be saved in Google.
      await page.keyboard.press('ControlOrMeta+Shift+b').catch(() => {});
      if ((await page.locator('.contacts').count()) === 0) await page.locator('.sidebar [aria-label="Address book"]').click();
      await page.locator('.ct-row', { hasText: 'Olivia Hart' }).waitFor();
      await page.locator('.ct-row', { hasText: 'Olivia Hart' }).click();
      assert.match(await page.locator('.ct-where-text').textContent(), /Google Contacts/);
      // What Google has beyond the fields here: photo, title, kinds, address, birthday…
      await page.locator('.ct-photo').waitFor();
      assert.match(await page.locator('.ct-photo').getAttribute('src'), /^data:image\//);
      assert.equal(await page.locator('.ct-role').textContent(), 'Head of Design · Studio · “Liv”');
      assert.equal(await page.locator('.ct-line .ct-kind').first().textContent(), 'Work', 'the address\'s kind');
      const more = page.locator('.ct-google-more');
      assert.match(await more.textContent(), /12 Harbor St/);
      assert.match(await more.locator('.ct-field', { hasText: 'Birthday' }).textContent(), /14/);
      assert.match(await more.locator('.ct-field', { hasText: 'Spouse' }).textContent(), /Sam Hart/);
      assert.match(await page.locator('.ct-google-link').getAttribute('href'), /^https:\/\/contacts\.google\.com\/person\/[^?]+\?authuser=.+%40/, 'opened in the account it belongs to');
      // One with nothing more: no photo, no block, still the link to edit there.
      await page.locator('.ct-row', { hasText: 'Noah Becker' }).click();
      await page.locator('.ct-photo').waitFor({ state: 'detached' });
      assert.equal(await page.locator('.ct-google-more').count(), 0);
      await page.locator('.ct-google-link').waitFor();
      await page.locator('[aria-label="New contact"]').click();
      const saveIn = page.locator('.sel.ct-save-in');
      await saveIn.waitFor();
      assert.equal((await optionsOf(saveIn)).length, 2, 'this computer, or the Google account');
      await page.keyboard.press('Escape');
      // Disconnect: its contacts leave the address book.
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'Accounts' }).click();
      await page.locator('.st-acct').first().locator('.gc').getByRole('button', { name: 'Disconnect' }).click();
      // (The question comes over Settings.)
      await page.locator('.confirm-backdrop .btn', { hasText: 'Disconnect' }).click();
      await page.locator('.st-acct').first().getByRole('button', { name: 'Connect…' }).waitFor();
      const cards = await page.evaluate(() => JSON.parse(localStorage.getItem('mock.cards') ?? '[]'));
      assert.ok(!cards.some((c) => c.google != null), 'their cards went');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('contact groups', () => {
  test('the address book by Google account and group, with each contact\'s groups on its row', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const g = (r) => `contactGroups/${r}`;
      await page.evaluate((groups) => {
        localStorage.setItem(
          'mock.google',
          JSON.stringify({ clientId: 'x.apps.googleusercontent.com', links: [{ accountId: 1, googleEmail: 'me@gmail.com', synced: 1, error: null, count: 3, groups }] }),
        );
        const card = (id, name, google, groups) => ({ id, name, emails: [`${name.split(' ')[0].toLowerCase()}@x.com`], phones: [], company: '', notes: '', google, groups });
        localStorage.setItem(
          'mock.cards',
          JSON.stringify([
            card(1, 'Olivia Hart', 1, [groups[0].resource, groups[1].resource, groups[3].resource]),
            card(2, 'Noah Becker', 1, [groups[0].resource, groups[2].resource]),
            card(3, 'Emma Lindqvist', 1, [groups[0].resource]),
            card(4, 'Local Person', null, []),
          ]),
        );
        localStorage.removeItem('contacts.source');
      }, [
        { resource: g('myContacts'), name: 'My Contacts' },
        { resource: g('starred'), name: 'Starred' },
        { resource: g('family1'), name: 'Familie' },
        { resource: g('work1'), name: 'Kolleger' },
      ]);
      // (One way in: the shortcut and the button both toggle it.)
      const openContacts = async () => {
        await page.locator('.sidebar [aria-label="Address book"]').click();
        await page.locator('.contacts').waitFor();
      };
      await page.reload();
      await openContacts();
      // The sidebar lists whose contacts, in place of the folders.
      const sources = page.locator('.sidebar .ct-sources');
      await sources.waitFor();
      assert.equal(await page.locator('.sidebar .acct-section, .sidebar [role="tree"]').count(), 0, 'no folders');
      assert.equal(await page.locator('.sidebar-title').textContent(), 'Contacts');
      assert.equal(await page.locator('.contacts .ct-sources').count(), 0, 'not twice');
      const rows = async () => (await page.locator('.ct-row .ct-name').allTextContents()).map((t) => t.replace(/G$/, '').trim());
      const source = (name) => sources.locator('.ct-source', { hasText: name });
      // Everyone, each with their groups (Starred first; My Contacts not shown).
      await page.locator('.ct-row', { hasText: 'Olivia Hart' }).waitFor();
      assert.deepEqual(await rows(), ['Emma Lindqvist', 'Local Person', 'Noah Becker', 'Olivia Hart']);
      assert.match(await source('All contacts').textContent(), /4$/);
      assert.deepEqual(await page.locator('.ct-row', { hasText: 'Olivia Hart' }).locator('.ct-chip').allTextContents(), ['Starred', 'Kolleger']);
      assert.equal(await page.locator('.ct-row', { hasText: 'Emma Lindqvist' }).locator('.ct-chip').count(), 0);
      // One group; the account; this computer's.
      await source('Familie').click();
      assert.deepEqual(await rows(), ['Noah Becker']);
      assert.equal(await page.locator('.ct-row.on .ct-name').count(), 1, 'one of them chosen');
      await source('Starred').click();
      assert.deepEqual(await rows(), ['Olivia Hart']);
      await source('me@gmail.com').click();
      assert.deepEqual(await rows(), ['Emma Lindqvist', 'Noah Becker', 'Olivia Hart']);
      await source('This computer').click();
      assert.deepEqual(await rows(), ['Local Person']);
      // A new contact goes where you're looking.
      await source('Kolleger').click();
      await page.locator('[aria-label="New contact"]').click();
      assert.match(await page.locator('.sel.ct-save-in .sel-text').textContent(), /^Google Contacts/);
      await page.keyboard.press('Escape');
      // Kept for next time.
      await page.reload();
      await openContacts();
      await page.locator('.ct-source.on', { hasText: 'Kolleger' }).waitFor();
      await page.locator('.ct-row', { hasText: 'Olivia Hart' }).waitFor();
      assert.deepEqual(await rows(), ['Olivia Hart']);
      // Back to mail with the Mail button: the folders again.
      await page.locator('.sidebar [aria-label="Mail"]').click();
      await page.locator('.contacts').waitFor({ state: 'detached' });
      assert.equal(await page.locator('.sidebar .ct-sources').count(), 0);
      assert.equal(await page.locator('.sidebar-title').textContent(), 'Mailboxes');
      // With the sidebar as a rail, the address book has the column itself.
      await page.locator('.sidebar [aria-label="Collapse sidebar"]').click();
      await page.locator('[aria-label="Address book"]').first().click();
      await page.locator('.contacts .ct-sources').waitFor();
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('Thunderbird keys for new commands', () => {
  test('Ctrl/⌘+U shows the source, Shift+C marks the folder read', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').first().click();
      await page.keyboard.press('ControlOrMeta+U');
      const source = page.locator('[aria-label="Message source"]');
      await source.waitFor();
      await page.keyboard.press('Escape');
      await source.waitFor({ state: 'detached' });
      await page.keyboard.press('Shift+C');
      await page.locator('.toast', { hasText: /Marked [\d,]+ messages? as read|Nothing unread/ }).waitFor();
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('sender pictures', () => {
  test('a Google contact\'s own photo, in place of their company\'s logo', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      // The colour inside a row's picture (the demo's pictures are SVGs of one colour each).
      const pictureOf = async (who) => {
        await page.fill('.search-field input', who);
        const img = page.locator('.search-layer .row', { hasText: who }).first().locator('.row-avatar img');
        await img.waitFor({ timeout: 3000 });
        const src = await img.getAttribute('src');
        return /fill="(#[0-9a-f]{6})"/.exec(Buffer.from(src.split(',')[1], 'base64').toString())?.[1];
      };
      // Off until turned on (looking a domain up contacts it).
      await page.fill('.search-field input', 'GitHub');
      await page.locator('.search-layer .row', { hasText: 'GitHub' }).first().waitFor();
      assert.equal(await page.locator('.search-layer .row-avatar').count(), 0, 'no pictures by default');
      await page.evaluate(() => localStorage.setItem('ui.senderPictures', 'true'));
      await page.reload();
      assert.equal(await pictureOf('GitHub'), '#24292f', 'the company\'s logo');
      // Contacts' photos only: no company logos.
      await page.evaluate(() => localStorage.setItem('ui.senderPictureLevel', '"contacts"'));
      await page.reload();
      await page.fill('.search-field input', 'GitHub');
      await page.locator('.search-layer .row', { hasText: 'GitHub' }).first().waitFor();
      await page.waitForTimeout(400);
      assert.equal(await page.locator('.search-layer .row', { hasText: 'GitHub' }).first().locator('.row-avatar img').count(), 0, 'initials');
      await page.evaluate(() => localStorage.setItem('ui.senderPictureLevel', '"known"'));
      await page.reload();
      // Their address on a Google contact with a photo (pictures are asked for once a run).
      await page.evaluate(() =>
        localStorage.setItem(
          'mock.cards',
          JSON.stringify([{ id: 1, name: 'Olivia Hart', emails: ['Notifications@GitHub.com'], phones: [], company: '', notes: '', google: 1 }]),
        ),
      );
      await page.reload();
      assert.equal(await pictureOf('GitHub'), '#c2410c', 'their own photo');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('notifications', () => {
  test('Settings: also while in use (on), and a test notification', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'Notifications' }).click();
      const inUse = page.locator('.st-field', { hasText: 'Also while Email is the window' }).locator('input[type=checkbox]');
      assert.equal(await inUse.isChecked(), true, 'also while in use, by default');
      await page.getByRole('button', { name: 'Send a test notification' }).click();
      await page.waitForFunction(() => window.__testNotifications === 1);
      await page.getByText(/^Sent\. Nothing showed\?/).waitFor();
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('updates', () => {
  test('a copy built here says it doesn\'t update itself', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'Troubleshooting' }).click();
      await page.locator('.st-field', { hasText: 'Updates' }).getByText(/doesn’t update itself/).waitFor();
      assert.equal(await page.locator('.sf-update').count(), 0);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('one from the releases: Check now downloads it, Restart to update installs it', async () => {
    const { page, errors, close } = await open({ threaded: false, query: '&updates' });
    try {
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'Troubleshooting' }).click();
      const box = page.locator('.st-field', { hasText: 'Look for new versions' }).locator('input[type=checkbox]');
      assert.equal(await box.isChecked(), true, 'on by default');
      await page.getByRole('button', { name: 'Check now' }).click();
      await page.locator('.st-update-state', { hasText: '0.1.999 is downloaded' }).waitFor();
      await page.keyboard.press('Escape');
      // The sidebar offers the restart too.
      await page.locator('.sf-update').click();
      await page.waitForFunction(() => window.__updateInstalled === '0.1.999');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('message source', () => {
  test('right-click in a message body: Copy, Copy link address and Show source', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const rows = page.locator('.folder-layer .row');
      const showSourceHere = async () => {
        await page.locator('.ctx-menu .vm-item', { hasText: 'Show source…' }).click();
        await page.waitForFunction(() => document.querySelector('.source-view .src-text')?.textContent?.includes('Subject:'));
        await page.keyboard.press('Escape');
        await page.locator('.source-view').waitFor({ state: 'detached' });
      };
      // An HTML message: its body is a frame of its own.
      let k = 0;
      for (; k < 30; k++) {
        await rows.nth(k).click();
        await page.waitForTimeout(250);
        if ((await page.locator('.reader .card-frame').count()) > 0) break;
      }
      const frame = page.frameLocator('.reader .card-frame');
      await frame.locator('body').waitFor();
      await frame.locator('body').click({ button: 'right', position: { x: 40, y: 20 } });
      const labels = async () => (await page.locator('.ctx-menu .vm-item').allTextContents()).map((t) => t.trim());
      assert.deepEqual(await labels(), ['Show source…'], 'nothing selected, not on a link');
      // At the pointer, in the window (not at the frame's own coordinates).
      const box = await page.locator('.ctx-menu').boundingBox();
      const fbox = await page.locator('.reader .card-frame').boundingBox();
      assert.ok(box.x >= fbox.x && box.y >= fbox.y, 'over the message');
      await showSourceHere();
      // With text selected: Copy first.
      await frame.locator('body').evaluate((b) => {
        const r = b.ownerDocument.createRange();
        r.selectNodeContents(b.querySelector('p, td, div') ?? b);
        b.ownerDocument.getSelection().removeAllRanges();
        b.ownerDocument.getSelection().addRange(r);
      });
      await frame.locator('body').click({ button: 'right', position: { x: 40, y: 20 } });
      assert.equal((await labels())[0], 'Copy');
      await page.keyboard.press('Escape');
      // A plain-text message: the same menu.
      for (k = 0; k < 40; k++) {
        await rows.nth(k).click();
        await page.waitForTimeout(250);
        if ((await page.locator('.reader .plain').count()) > 0) break;
      }
      await page.locator('.reader .plain').click({ button: 'right' });
      assert.ok((await labels()).includes('Show source…'));
      await showSourceHere();
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('right-click › Show source; a question about it is answered; ⌥⌘U opens it too', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.evaluate(() => localStorage.setItem('mock.aiKey', 'sk-ant-demo'));
      const row = page.locator('.folder-layer .row').first();
      const subject = (await row.locator('.row-subject').textContent()).trim();
      await row.click({ button: 'right' });
      await page.locator('.ctx-menu .vm-item', { hasText: 'Show source' }).click();
      const src = page.locator('.source-view .src-text');
      await page.waitForFunction(() => document.querySelector('.source-view .src-text')?.textContent?.includes('Subject:'));
      assert.ok((await src.textContent()).includes(`Subject: ${subject}`));
      await page.locator('.source-view .chip-toggle', { hasText: 'spam' }).click();
      await page.waitForFunction(() => document.querySelector('.source-view .src-answer')?.textContent?.includes('X-Spam-Status'));
      // (Esc while it's answering stops the answer; once done, it closes.)
      await page.locator('.source-view .src-form button', { hasText: 'Ask' }).waitFor();
      await page.keyboard.press('Escape');
      assert.equal(await page.locator('.source-view').count(), 0);
      await row.click();
      await page.keyboard.press('ControlOrMeta+Alt+u');
      await page.locator('.source-view').waitFor();
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('ai apps', () => {
  test('off by default; turned on with chosen permissions; shows how to connect; asks before sending', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'AI apps' }).click();
      const master = page.locator('.mcp-master input');
      assert.equal(await master.isChecked(), false, 'off by default');
      assert.equal(await page.locator('.mcp-perms input').first().isDisabled(), true);
      await master.check();
      await page.locator('.mcp-row', { hasText: 'Write drafts' }).locator('input').check();
      await page.waitForTimeout(100);
      const saved = await page.evaluate(() => JSON.parse(localStorage.getItem('mock.mcp')));
      assert.equal(saved.enabled, true);
      assert.equal(saved.drafts, true);
      assert.equal(saved.send, false);
      // Leaving out an account lists the rest.
      await page.locator('.mcp-accounts input').first().uncheck();
      await page.waitForTimeout(100);
      assert.equal((await page.evaluate(() => JSON.parse(localStorage.getItem('mock.mcp')))).accounts.length, 1);
      assert.match(await page.locator('.mcp-code pre').first().textContent(), /"--mcp"/);
      await page.keyboard.press('Escape');
      // An AI app asks to send: the user sees it and declines.
      const answer = page.evaluate(() => window.__mcpAsk({ id: 7, client: 'Claude', title: 'Send this message?', lines: ['To: bob@x.com', 'Subject: Hi'], body: 'Hello Bob', ok: 'Send' }));
      await page.locator('.mcp-ask').waitFor();
      assert.match(await page.locator('.mcp-ask').textContent(), /Claude asks:.*Send this message\?.*To: bob@x.com.*Hello Bob/s);
      await page.locator('.mcp-ask .btn', { hasText: "Don't" }).click();
      assert.equal(await answer, false);
      assert.equal(await page.locator('.mcp-ask').count(), 0);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('move list', () => {
  test("the folders with most of the sender's mail come first", async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row', { hasText: 'Invoicely' }).first().click();
      await page.locator('.reader [aria-label^="Move to"]').first().click();
      await page.locator('.move-picker .mp-row').first().waitFor();
      await page.waitForTimeout(200);
      const rows = await page.locator('.move-picker .mp-row').evaluateAll((els) =>
        els.map((e) => ({ name: e.querySelector('.mp-name')?.textContent, n: Number(e.querySelector('.mp-count')?.textContent?.replace(/\D/g, '') || 0) })),
      );
      const counted = rows.filter((r) => r.n > 0);
      assert.ok(counted.length >= 1, `some folders hold the sender's mail: ${JSON.stringify(rows.slice(0, 5))}`);
      assert.deepEqual(rows.slice(0, counted.length), counted, 'those first');
      assert.deepEqual(counted.map((r) => r.n), [...counted.map((r) => r.n)].sort((a, b) => b - a), 'most first');
      assert.ok(!counted.some((r) => /All Mail|Inbox|Sent|Trash|Spam/.test(r.name)), 'not the folders everything is in');
      await page.keyboard.press('Escape');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('several messages', () => {
  const rows = (page) => page.locator('.folder-layer .row');
  const title = (page) => page.locator('.multi-pane .reader-subject');

  test('⌘/Ctrl-click and ⇧-click pick rows; one Delete for all; Undo picks them again', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const ids = async () => page.locator('.folder-layer .row').evaluateAll((els) => els.map((e) => e.dataset.id));
      const before = await ids();
      const gone = before.slice(2, 6);
      await rows(page).nth(0).click();
      await rows(page).nth(2).click({ modifiers: ['ControlOrMeta'] });
      await hasText(title(page), '2 messages selected', 'two picked');
      assert.equal(await page.locator('.folder-layer .row.selected').count(), 2);
      await rows(page).nth(5).click({ modifiers: ['Shift'] });
      await hasText(title(page), '4 messages selected', 'rows 2 to 5');
      await page.locator('.multi-pane [aria-label="Delete"]').click();
      // More than one: asked first.
      assert.match(await page.locator('.confirm-backdrop .confirm-text').textContent(), /Delete 4 messages\?/);
      await page.locator('.confirm-backdrop .btn.danger').click();
      await page.locator('.toast', { hasText: 'Deleted 4 messages' }).waitFor();
      await page.waitForTimeout(300);
      assert.equal(await page.locator('.multi-pane').count(), 0, 'back to one');
      const after = await ids();
      assert.ok(gone.every((id) => !after.includes(id)), 'the four are gone');
      assert.ok(after.includes(before[6]), 'the rows below moved up');
      await page.locator('.toast button', { hasText: 'Undo' }).click();
      await hasText(title(page), '4 messages selected', 'picked again');
      await page.waitForFunction((g) => {
        const shown = [...document.querySelectorAll('.folder-layer .row')].map((e) => e.dataset.id);
        return g.every((id) => shown.includes(id));
      }, gone);
      // Esc: just the open one again.
      await page.keyboard.press('Escape');
      assert.equal(await page.locator('.multi-pane').count(), 0);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('⌘/Ctrl+A picks the whole folder, conversations too; ⇧-arrows extend', async () => {
    const { page, errors, close } = await open({ threaded: true });
    try {
      const total = Number((await page.locator('.folder-layer .list-sub .count-btn').nth(1).textContent()).replace(/[^\d]/g, ''));
      await rows(page).nth(1).click();
      await page.keyboard.press('Shift+ArrowDown');
      await page.keyboard.press('Shift+ArrowDown');
      await hasText(title(page), '3 conversations selected', 'extended down');
      await page.keyboard.press('ControlOrMeta+a');
      await hasText(title(page), `${total.toLocaleString('en-US')} conversations selected`, 'all of them');
      assert.match(await page.locator('.multi-pane .band-meta').textContent(), /messages/);
      await page.keyboard.press('ArrowDown');
      assert.equal(await page.locator('.multi-pane').count(), 0, 'moving on picks one again');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('forward several as attachments', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      await rows(page).nth(0).click();
      await rows(page).nth(1).click({ modifiers: ['ControlOrMeta'] });
      await hasText(title(page), '2 messages selected', 'two picked');
      const popup = context.waitForEvent('page');
      await page.locator('.multi-pane [aria-label="Forward as attachments"]').click();
      const w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.locator('.cmp-att-msg').first().waitFor();
      assert.equal(await w.locator('.cmp-att-msg').count(), 2);
      assert.match(await w.locator('.cmp-att-msg .cmp-att-name').first().textContent(), /\.eml$/);
      assert.equal(await w.locator('#cmp-subject').inputValue(), 'Fwd: 2 messages');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('drag to folder', () => {
  const rows = (page) => page.locator('.folder-layer .row');
  const drag = async (page, row, target) => {
    const rb = await row.boundingBox();
    const tb = await target.boundingBox();
    await page.mouse.move(rb.x + rb.width / 2, rb.y + rb.height / 2);
    await page.mouse.down();
    await page.mouse.move(tb.x + tb.width / 2, tb.y + tb.height / 2, { steps: 12 });
  };

  test('released over a message body, the drag ends there: a later click on a folder moves nothing', async () => {
    const { page, errors, close } = await open({ threaded: false, query: '&picture' });
    try {
      await page.locator('.folder-layer .row', { hasText: 'picture as the body' }).first().click();
      const body = page.locator('.reader iframe').first();
      await page.frameLocator('.reader iframe').first().locator('img').waitFor();
      const id = await rows(page).first().getAttribute('data-id');
      await drag(page, rows(page).first(), body);
      await page.locator('.drag-ghost').waitFor();
      await page.mouse.up();
      await page.locator('.drag-ghost').waitFor({ state: 'detached' });
      await page.locator('.sidebar .folder', { hasText: 'Receipts' }).first().click();
      await page.waitForTimeout(500);
      assert.equal(await page.locator('.toast', { hasText: /Moved/ }).count(), 0, 'nothing moved');
      assert.equal(await page.locator(`.folder-layer .row[data-id="${id}"]`).count() + 1 > 0, true);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('Esc cancels a drag into the folder panel, which stays open', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.keyboard.press('ControlOrMeta+1');
      await page.waitForSelector('.app.rail-mode');
      await page.keyboard.press('ControlOrMeta+Shift+1');
      const flyout = page.locator('div.flyout');
      await flyout.waitFor();
      // The sidebar folds into the rail and the panel slides open (animated): measure once the
      // panel and the list have both stopped moving, for a few frames running.
      await page.waitForFunction(() => {
        const place = () =>
          ['div.flyout', '.folder-layer .row'].map((q) => {
            const r = document.querySelector(q)?.getBoundingClientRect();
            return r ? `${r.left},${r.right},${r.top}` : '';
          }).join('|');
        const first = place();
        return new Promise((done) => setTimeout(() => done(first !== '|' && !first.startsWith('|') && first === place()), 250));
      });
      // A point on the first row that the panel doesn't cover (it covers the list's left part).
      const at = await page.evaluate(() => {
        const row = document.querySelector('.folder-layer .row');
        const r = row.getBoundingClientRect();
        const y = r.top + r.height / 2;
        for (let x = r.right - 12; x > r.left; x -= 10) if (document.elementFromPoint(x, y)?.closest('.row') === row) return { x, y };
        return null;
      });
      assert.ok(at, 'a part of the row is free to press');
      const tb = await flyout.locator('.folder', { hasText: 'Receipts' }).first().boundingBox();
      await page.mouse.move(at.x, at.y);
      await page.mouse.down();
      await page.mouse.move(tb.x + tb.width / 2, tb.y + tb.height / 2, { steps: 12 });
      await page.locator('.drag-ghost').waitFor({ timeout: 5000 }).catch(async () => {
        const hit = await page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.outerHTML.slice(0, 160), at);
        assert.fail(`the drag didn't start; the press at ${JSON.stringify(at)} hit ${hit}`);
      });
      await page.keyboard.press('Escape');
      await page.locator('.drag-ghost').waitFor({ state: 'detached' });
      await page.mouse.up();
      await page.waitForTimeout(300);
      assert.equal(await flyout.count(), 1, 'the panel stays');
      assert.equal(await page.locator('.toast', { hasText: /Moved/ }).count(), 0, 'nothing moved');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('a message dragged onto a folder moves there, with Undo', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const id = await rows(page).first().getAttribute('data-id');
      const target = page.locator('.sidebar .folder', { hasText: 'Receipts' }).first();
      await drag(page, rows(page).first(), target);
      await page.locator('.drag-ghost.ok').waitFor();
      assert.match(await page.locator('.drag-ghost').textContent(), /1\s*message/);
      assert.equal(await target.evaluate((e) => e.classList.contains('drop-target')), true, 'the folder lights up');
      await page.mouse.up();
      await page.locator('.toast', { hasText: 'Moved to Receipts' }).waitFor();
      await page.waitForFunction((gone) => ![...document.querySelectorAll('.folder-layer .row')].some((e) => e.dataset.id === gone), id);
      assert.equal(await page.locator('.drag-ghost').count(), 0);
      await page.locator('.toast button', { hasText: 'Undo' }).click();
      await page.waitForFunction((back) => [...document.querySelectorAll('.folder-layer .row')].some((e) => e.dataset.id === back), id);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('rows picked together drag together; a conversation drags its messages', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const ids = await rows(page).evaluateAll((els) => els.slice(0, 2).map((e) => e.dataset.id));
      await rows(page).nth(0).click();
      await rows(page).nth(1).click({ modifiers: ['ControlOrMeta'] });
      await page.locator('.multi-pane').waitFor();
      const target = page.locator('.sidebar .folder', { hasText: 'Work' }).first();
      await drag(page, rows(page).first(), target);
      assert.match(await page.locator('.drag-ghost').textContent(), /2\s*messages/);
      await page.mouse.up();
      await page.locator('.toast', { hasText: 'Moved 2 messages to Work' }).waitFor();
      await page.waitForFunction((gone) => gone.every((id) => ![...document.querySelectorAll('.folder-layer .row')].some((e) => e.dataset.id === id)), ids);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('a conversation dragged to a folder moves its messages', async () => {
    const { page, errors, close } = await open({ threaded: true });
    try {
      const target = page.locator('.sidebar .folder', { hasText: 'Personal' }).first();
      await drag(page, rows(page).first(), target);
      await page.locator('.drag-ghost.ok').waitFor();
      await page.mouse.up();
      await page.locator('.toast').filter({ hasText: /Moved (\d+ messages )?to Personal/ }).waitFor();
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test("Escape cancels; another account's folder refuses the drop", async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      const id = await rows(page).first().getAttribute('data-id');
      const target = page.locator('.sidebar .folder', { hasText: 'Receipts' }).first();
      await drag(page, rows(page).first(), target);
      await page.locator('.drag-ghost').waitFor();
      await page.keyboard.press('Escape');
      assert.equal(await page.locator('.drag-ghost').count(), 0, 'cancelled');
      await page.mouse.up();
      // The second account's Inbox: not a target for this account's mail.
      const other = page.locator('.sidebar .account').nth(1).locator('.folder', { hasText: 'Inbox' }).first();
      await drag(page, rows(page).first(), other);
      assert.equal(await page.locator('.drag-ghost.ok').count(), 0, 'no drop offered');
      assert.equal(await other.evaluate((e) => e.classList.contains('drop-target')), false);
      await page.mouse.up();
      await page.waitForTimeout(300);
      assert.equal(await page.locator('.toast', { hasText: 'Moved' }).count(), 0, 'nothing moved');
      assert.equal(await rows(page).first().getAttribute('data-id'), id, 'the row stays');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('favorite folders', () => {
  test('right-click pins a folder above the accounts; it stays after a reload; unpinning removes it', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      assert.equal(await page.locator('.sidebar .favorites').count(), 0, 'none to begin with');
      await page.locator('.sidebar .account .folder', { hasText: 'Receipts' }).first().click({ button: 'right' });
      await page.locator('.ctx-menu .vm-item', { hasText: 'Add to favorites' }).click();
      const fav = page.locator('.sidebar .favorites .folder', { hasText: 'Receipts' });
      await fav.waitFor();
      const favBox = await page.locator('.sidebar .favorites').boundingBox();
      const acctBox = await page.locator('.sidebar .account').first().boundingBox();
      assert.ok(favBox.y < acctBox.y, 'favorites sit above the accounts');
      assert.ok((await fav.locator('.folder-sub').textContent()).length > 0, 'with more than one account, the row says whose it is');
      await fav.click();
      await page.waitForSelector('.folder-layer .row');
      assert.equal(await fav.evaluate((e) => e.classList.contains('selected')), true, 'the pinned row opens the folder');
      await page.reload();
      await page.waitForSelector('.sidebar .favorites .folder');
      assert.match(await page.locator('.sidebar .favorites .folder').first().textContent(), /Receipts/, 'remembered');
      await page.locator('.sidebar .favorites .folder').first().click({ button: 'right' });
      await page.locator('.ctx-menu .vm-item', { hasText: 'Remove from favorites' }).click();
      await page.waitForSelector('.sidebar .favorites', { state: 'detached' });
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('favorites stay at the top while the folders scroll, unless turned off in Settings', async () => {
    const { page, errors, close } = await open({ threaded: false, query: '&many' });
    try {
      await page.locator('.sidebar .account .folder', { hasText: 'Receipts' }).first().click({ button: 'right' });
      await page.locator('.ctx-menu .vm-item', { hasText: 'Add to favorites' }).click();
      await page.locator('.sidebar .favorites .folder').waitFor();
      const scroller = page.locator('.sidebar .sidebar-scroll');
      const top = async () => (await page.locator('.sidebar .favorites').boundingBox()).y - (await scroller.boundingBox()).y;
      const scrollDown = async () => {
        await scroller.evaluate((e) => (e.scrollTop = 0));
        await scroller.evaluate((e) => (e.scrollTop = 400));
        await page.waitForTimeout(100);
        assert.ok((await scroller.evaluate((e) => e.scrollTop)) > 100, 'the sidebar scrolled');
      };
      await scrollDown();
      assert.ok(Math.abs(await top()) < 2, 'pinned: still at the top');
      // Never wider than the sidebar: a long name ends in …, no sideways scrolling.
      await page.locator('.sidebar .favorites .folder-name').first().evaluate((e) => (e.textContent = 'A very long folder name that will not fit at all'));
      assert.equal(await page.locator('.sidebar .favorites').evaluate((e) => e.scrollWidth <= e.clientWidth), true, 'no sideways overflow');
      // Settings › General › Appearance › Favorites: off, and they scroll away with the rest.
      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: 'General' }).click();
      const pin = page.locator('.st-field', { hasText: 'Favorites' }).locator('input[type=checkbox]');
      assert.equal(await pin.isChecked(), true, 'on to begin with');
      await pin.uncheck();
      assert.equal(await page.evaluate(() => localStorage.getItem('ui.pinFavorites')), 'false');
      await page.keyboard.press('Escape');
      await scrollDown();
      assert.ok((await top()) < -100, 'not pinned: scrolled out of view');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('favorites reorder: Alt+arrows, Move up in the menu, and dragging; the order is kept', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      for (const name of ['Receipts', 'Newsletters', 'Travel']) {
        await page.locator('.sidebar .account .folder', { hasText: name }).first().click({ button: 'right' });
        await page.locator('.ctx-menu .vm-item', { hasText: 'Add to favorites' }).click();
      }
      const favs = page.locator('.sidebar .favorites .folder');
      const order = async () => (await favs.locator('.folder-name').allTextContents()).map((t) => t.trim());
      await page.waitForFunction(() => document.querySelectorAll('.sidebar .favorites .folder').length === 3);
      assert.deepEqual(await order(), ['Receipts', 'Newsletters', 'Travel'], 'in the order added');
      // Alt+↓ on the first: one down, and it keeps the focus for another press.
      await favs.first().focus();
      await page.keyboard.press('Alt+ArrowDown');
      assert.deepEqual(await order(), ['Newsletters', 'Receipts', 'Travel']);
      await page.keyboard.press('Alt+ArrowDown');
      assert.deepEqual(await order(), ['Newsletters', 'Travel', 'Receipts'], 'again, still focused');
      // Right-click › Move up in favorites (the first can't go up).
      await favs.nth(2).click({ button: 'right' });
      await page.locator('.ctx-menu .vm-item', { hasText: 'Move up in favorites' }).click();
      assert.deepEqual(await order(), ['Newsletters', 'Receipts', 'Travel']);
      await favs.first().click({ button: 'right' });
      assert.equal(await page.locator('.ctx-menu .vm-item', { hasText: 'Move up in favorites' }).isDisabled(), true);
      await page.keyboard.press('Escape');
      // Drag the last one to the top; letting go doesn't open it.
      const selectedBefore = await page.locator('.sidebar .folder.selected').first().textContent();
      const last = await favs.nth(2).boundingBox();
      const first = await favs.first().boundingBox();
      await page.mouse.move(last.x + 40, last.y + last.height / 2);
      await page.mouse.down();
      await page.mouse.move(last.x + 40, last.y + last.height / 2 - 10, { steps: 3 });
      await page.mouse.move(first.x + 40, first.y + 2, { steps: 8 });
      await page.mouse.up();
      await page.waitForTimeout(100);
      assert.deepEqual(await order(), ['Travel', 'Newsletters', 'Receipts'], 'dragged to the top');
      assert.equal(await page.locator('.sidebar .folder.selected').first().textContent(), selectedBefore, 'the drop opened nothing');
      // Kept.
      await page.reload();
      await page.waitForSelector('.sidebar .favorites .folder');
      assert.deepEqual(await order(), ['Travel', 'Newsletters', 'Receipts'], 'after a reload');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('the rail shows pinned folders above the account markers', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.sidebar .account .folder', { hasText: 'Receipts' }).first().click({ button: 'right' });
      await page.locator('.ctx-menu .vm-item', { hasText: 'Add to favorites' }).click();
      await page.locator('.sidebar .favorites .folder').waitFor();
      await page.locator('.sidebar [aria-label="Collapse sidebar"]').click();
      const railFav = page.locator('.rail-favs .rail-btn');
      await railFav.first().waitFor();
      const favBox = await page.locator('.rail-favs').boundingBox();
      const acctBox = await page.locator('.rail-acct').first().boundingBox();
      assert.ok(favBox.y < acctBox.y, 'above the accounts in the rail too');
      await railFav.first().click();
      await page.waitForSelector('.folder-layer .row');
      assert.equal(await railFav.first().evaluate((e) => e.classList.contains('selected')), true);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('drafts', () => {
  test('a draft from the server opens in the composer; changed, the server copy goes to Trash', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      await page.locator('.sidebar .account .folder', { hasText: 'Drafts' }).first().click();
      const row = page.locator('.folder-layer .row').first();
      await row.waitFor();
      const subject = (await row.locator('.row-subject').textContent()).trim();
      await row.click();
      // The reader offers it; double-clicking the row opens it as well.
      await page.locator('.reader .band-edit-draft').waitFor();
      let popup = context.waitForEvent('page');
      await row.dblclick();
      let w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.locator('.cmp-editor .ProseMirror').waitFor();
      assert.equal(await w.locator('input.cmp-subject').inputValue(), subject, 'its subject');
      assert.ok((await w.locator('.cmp-editor .ProseMirror').innerText()).trim().length > 0, 'its text');
      // Closed unchanged: the server's draft stays where it is.
      await w.close();
      await page.bringToFront();
      await page.waitForTimeout(300);
      assert.equal(await page.locator('.folder-layer .row', { hasText: subject }).count(), 1, 'still in Drafts');
      // Opened again and changed: this one replaces it, and the server's goes to Trash.
      popup = context.waitForEvent('page');
      await page.locator('.reader .band-edit-draft').click();
      w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.locator('.cmp-editor .ProseMirror').click();
      await w.keyboard.type(' More to say.');
      await page.bringToFront();
      await page.locator('.sidebar .account .folder', { hasText: 'Trash' }).first().click();
      await page.locator('.folder-layer .row', { hasText: subject }).first().waitFor({ timeout: 5000 });
      await page.locator('.sidebar .account .folder', { hasText: 'Drafts' }).first().click();
      await page.waitForTimeout(400);
      assert.equal(await page.locator('.folder-layer .row', { hasText: subject }).count(), 1, 'one copy in Drafts: the one written here');
      await w.close();
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('writing help', () => {
  test('without a key it says where to add one; with one, a reply is written and used', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').first().click();
      await page.waitForTimeout(300);
      let popup = context.waitForEvent('page');
      await page.locator('.reader [aria-label="Reply"]').first().click();
      let w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.locator('.fmt-ai').click();
      await w.locator('.ai-note').waitFor();
      await w.close();
      await page.bringToFront();

      await page.keyboard.press('ControlOrMeta+,');
      await page.locator('.st-nav-item', { hasText: /^AI$/ }).click();
      await page.locator('input.ai-key').fill('sk-ant-test');
      await page.locator('.st-pane button', { hasText: 'Save' }).click();
      await page.locator('.st-pane button', { hasText: 'Try it' }).click();
      await hasText(page.locator('.st-status'), 'Works! — Claude Opus 5.5', 'the key works');
      await page.keyboard.press('Escape');

      popup = context.waitForEvent('page');
      await page.locator('.reader [aria-label="Reply"]').first().click();
      w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.locator('.cmp-editor .ProseMirror').waitFor();
      await w.keyboard.press('ControlOrMeta+j');
      await w.locator('.ai-input').fill('yes, Thursday works');
      await w.keyboard.press('Enter');
      await w.locator('.ai-panel button', { hasText: 'Use this' }).click();
      const body = await w.locator('.cmp-editor .ProseMirror').innerText();
      assert.ok(body.includes('Yes, Thursday works.'), body);
      assert.equal(await w.locator('.ai-panel').count(), 0, 'the panel closes');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('address book', () => {
  test('a contact with no mail: the search finds none and the reader empties; a folder goes back to mail', async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').first().click();
      await page.locator('.pane-reader .reader-subject').waitFor();
      await page.locator('button[aria-label="Address book"]').click();
      await page.locator('.contacts').waitFor();
      await page.locator('button[aria-label="New contact"]').click();
      await page.locator('.ct-big').fill('Nobody Atall');
      await page.locator('input[aria-label="Email 1"]').fill('nobody@nowhere.example');
      await page.locator('.contacts button', { hasText: 'Show mail' }).click();
      await page.locator('.contacts').waitFor({ state: 'detached' });
      await hasText(page.locator('.search-layer .list-empty-title'), 'No results', 'no results');
      await page.locator('.reader-empty').waitFor();
      assert.equal(await page.locator('.pane-reader .reader-subject, .pane-reader .conv-view').count(), 0, 'nothing from before the search');

      // From the address book, Mail brings back the folders, and a folder its mail.
      await page.keyboard.press('Escape');
      await page.locator('button[aria-label="Address book"]').click();
      await page.locator('.contacts').waitFor();
      assert.equal(await page.locator('.sidebar button.folder').count(), 0, 'the sidebar lists contacts');
      await page.locator('.sidebar button[aria-label="Mail"]').click();
      await page.locator('.contacts').waitFor({ state: 'detached' });
      await page.locator('.sidebar button.folder').nth(1).click();
      assert.equal(await page.locator('button[aria-label="Address book"]').getAttribute('aria-pressed'), 'false');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  test('a sender added from the panel is kept, suggested first, and can be written to', async () => {
    const { page, errors, context, close } = await open({ threaded: false });
    try {
      await page.locator('.folder-layer .row').first().click();
      const from = (await page.locator('.folder-layer .row.selected .row-from').textContent()).trim();
      await page.keyboard.press('ControlOrMeta+2');
      await page.locator('.sp-card', { hasText: 'Add to contacts' }).click();
      await page.locator('.contacts').waitFor();
      assert.equal(await page.locator('.ct-big').inputValue(), from, 'prefilled with the sender');
      await page.locator('.ct-big').fill(`${from} (work)`);
      await page.locator('.ct-field', { hasText: 'Company' }).locator('input').fill('Acme');
      await page.locator('.ct-actions button', { hasText: 'Save' }).click();
      await hasText(page.locator('.ct-row .ct-name').first(), `${from} (work)`, 'listed');
      await page.keyboard.press('Escape');
      await page.locator('.contacts').waitFor({ state: 'detached' });
      await hasText(page.locator('.sp-card'), 'In contacts', 'the panel knows them now');

      const popup = context.waitForEvent('page');
      await page.keyboard.press('c');
      const w = await popup;
      w.on('pageerror', (e) => errors.push(e.message));
      await w.locator('input[aria-label="To"]').fill('acm');
      await hasText(w.locator('.addr-sugg-row .sugg-name').first(), `${from} (work)`, 'found by company, first, under its own name');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});

describe('contact pictures', () => {
  test("the overview shows each contact's picture, as the mail list does", async () => {
    const { page, errors, close } = await open({ threaded: false });
    try {
      await page.evaluate(() => {
        localStorage.setItem('ui.senderPictures', 'true');
        localStorage.setItem(
          'mock.cards',
          JSON.stringify([
            { id: 1, name: 'Olivia Hart', emails: ['olivia.hart@example.com'], phones: [], company: 'Fabrikam', notes: '', google: 1, groups: [] },
            { id: 2, name: 'Noah Becker', emails: ['noah@beckers.dev'], phones: [], company: '', notes: '', google: null, groups: [] },
          ]),
        );
      });
      await page.reload();
      await page.waitForSelector('.folder-layer .row');
      await page.locator('button[aria-label="Address book"]').click();
      const olivia = page.locator('.ct-row', { hasText: 'Olivia Hart' });
      await olivia.waitFor();
      await olivia.locator('.avatar.pic img').waitFor();
      assert.match(await olivia.locator('.avatar img').getAttribute('src'), /^data:image\//, 'her Google photo');
      const noah = page.locator('.ct-row', { hasText: 'Noah Becker' });
      assert.equal(await noah.locator('.avatar').textContent(), 'NB', 'initials when there is no photo');
      assert.equal(await noah.locator('.avatar img').count(), 0);
      // Pictures off: the rows go back to text only, like the mail list.
      await page.evaluate(() => localStorage.setItem('ui.senderPictures', 'false'));
      await page.reload();
      await page.waitForSelector('.folder-layer .row');
      await page.locator('button[aria-label="Address book"]').click();
      await page.locator('.ct-row', { hasText: 'Olivia Hart' }).waitFor();
      assert.equal(await page.locator('.ct-row .avatar').count(), 0, 'no avatars with pictures off');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });
});
