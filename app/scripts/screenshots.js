// The README's screenshots (docs/screenshots/*.png), taken from the browser preview and its mock
// backend (src/mock.ts: John Doe, invented mail). Run after UI changes:
//
//   cd app && npm run screenshots              # every scene
//   cd app && npm run screenshots -- inbox     # only scenes whose name contains "inbox"
//
// Uses the installed Google Chrome (or CHROME=/path/to/chromium). The clock, time zone, window
// size and remote images are fixed, so an unchanged UI gives unchanged files.

import { mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServer } from 'vite';
import { chromium } from 'playwright-core';

const APP = join(dirname(fileURLToPath(import.meta.url)), '..');
const OUT = join(APP, '..', 'docs', 'screenshots');
/** "Now" in every screenshot: a Tuesday morning. */
const NOW = new Date('2026-10-13T09:41:00-04:00');
const SIZE = { width: 1280, height: 800 };

// ------------------------------------------------------------- scenes

/** name → what to do before the shot; `shot` takes the page (or a popup) and an optional clip. */
const SCENES = [
  {
    // The main window: accounts and folders, the list, a conversation open.
    name: 'inbox',
    themes: ['light', 'dark'],
    async run({ page, shot }) {
      await openRow(page, '.row:has-text("Morning Brief")');
      await shot(page);
    },
  },
  {
    // Writing: a compose window, with address suggestions open.
    name: 'compose',
    async run({ page, context, shot }) {
      const popup = context.waitForEvent('page');
      await page.focus('.list-scroll');
      await page.keyboard.press('c');
      const w = await popup;
      await w.setViewportSize({ width: 760, height: 460 });
      await w.waitForSelector('.addr-input');
      await w.waitForTimeout(400);
      await w.fill('.cmp-subject input, input.cmp-subject', 'Saturday plans').catch(() => {});
      await w.click('.ProseMirror');
      await w.keyboard.type('Hi Hannah and Sarah,\nAre we still on for the hike on Saturday? I can drive — leaving around 8.\nJohn');
      await w.click('.addr-input');
      // Hannah from the suggestions, then the start of Sarah's name: her suggestions open.
      await w.keyboard.type('hann');
      await w.waitForSelector('.addr-sugg-row');
      await w.keyboard.press('Enter');
      await w.keyboard.type('sar');
      await w.waitForSelector('.addr-sugg-row');
      await shot(w);
    },
  },
  {
    // Quick search with a query and its results.
    name: 'search',
    async run({ page, shot }) {
      await page.click('.search-field input');
      await page.keyboard.type('from:sarah roadmap');
      await page.waitForTimeout(600);
      await openRow(page, '.search-layer .row >> nth=0');
      await shot(page);
    },
  },
  {
    // Tags: the Tag menu on a tagged message.
    name: 'tags',
    async run({ page, shot }) {
      await openRow(page, '.row:has(.tag-dot) >> nth=0');
      await page.keyboard.press('l');
      await page.waitForSelector('.tag-menu');
      await shot(page);
    },
  },
  {
    // The sidebar as a rail, with every folder sliding out over the list.
    name: 'rail',
    async run({ page, shot }) {
      await openRow(page, '.row:has-text("Morning Brief")');
      await page.keyboard.press('Control+1');
      await page.waitForTimeout(300);
      await page.keyboard.press('Control+Shift+1');
      await page.waitForTimeout(300);
      await shot(page);
    },
  },
  {
    // Ctrl+G: every folder of every account, filtered as you type.
    name: 'goto',
    async run({ page, shot }) {
      await openRow(page, '.row:has-text("Morning Brief")');
      await page.keyboard.press('Control+G');
      await page.keyboard.type('in');
      await page.waitForTimeout(200);
      await shot(page);
    },
  },
  {
    // Settings › Keyboard: Gmail's or Thunderbird's keys, each one changeable.
    name: 'keyboard',
    async run({ page, shot }) {
      await page.keyboard.press('Control+Comma');
      await page.click('.st-nav-item:has-text("Keyboard")');
      await page.waitForSelector('.st-row');
      await shot(page);
    },
  },
  {
    // `?`: the shortcuts sheet.
    name: 'shortcuts',
    async run({ page, shot }) {
      await page.focus('.list-scroll');
      await page.keyboard.press('?');
      await page.waitForSelector('.sc-grid');
      await shot(page);
    },
  },
];

// ------------------------------------------------------------- harness

/** Open a list row and wait until its message body has rendered (bodies load on demand). */
async function openRow(page, selector) {
  await page.click(selector);
  await page.waitForFunction(() => {
    const frames = [...document.querySelectorAll('.reader iframe, .conv-scroll iframe')];
    const plain = document.querySelector('.reader .plain, .conv-scroll .plain');
    return (plain?.textContent ?? '').trim().length > 0 || frames.some((f) => (f.contentDocument?.body?.textContent ?? '').trim().length > 0);
  });
  await page.waitForTimeout(300);
}

/** Stand-in for the newsletters' picsum.photos images: a soft gradient, the same every run. */
function placeholder(url) {
  let h = 0;
  for (const ch of url) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  const [a, b] = [h % 360, (h >> 9) % 360];
  const m = /\/(\d+)\/(\d+)(?:[/?]|$)/.exec(url);
  const [w, ht] = m ? [m[1], m[2]] : ['600', '400'];
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${ht}" viewBox="0 0 ${w} ${ht}">
    <defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="hsl(${a} 35% 72%)"/><stop offset="1" stop-color="hsl(${b} 30% 52%)"/>
    </linearGradient></defs><rect width="100%" height="100%" fill="url(#g)"/></svg>`;
}

const STILL = `*, *::before, *::after { transition: none !important; animation: none !important; caret-color: transparent !important; }`;

async function main() {
  const only = process.argv[2];
  const scenes = SCENES.filter((s) => !only || s.name.includes(only));
  if (!scenes.length) throw new Error(`no scene matches "${only}"`);
  mkdirSync(OUT, { recursive: true });

  const server = await createServer({ root: APP, logLevel: 'error', server: { port: 1499, strictPort: false } });
  await server.listen();
  const base = server.resolvedUrls.local[0];
  const browser = await chromium.launch(process.env.CHROME ? { executablePath: process.env.CHROME } : { channel: 'chrome' });
  try {
    for (const scene of scenes) {
      for (const theme of scene.themes ?? ['light']) {
        const context = await browser.newContext({
          viewport: SIZE,
          deviceScaleFactor: 2,
          colorScheme: theme,
          locale: 'en-US',
          timezoneId: 'America/New_York',
        });
        await context.route(/^https:\/\/picsum\.photos\//, (r) =>
          r.fulfill({ contentType: 'image/svg+xml', body: placeholder(r.request().url()) }),
        );
        await context.clock.setFixedTime(NOW);
        // Every page (popups too) without animations or a blinking caret.
        await context.addInitScript((css) => {
          addEventListener('DOMContentLoaded', () => {
            const s = document.createElement('style');
            s.textContent = css;
            document.head.append(s);
          });
        }, STILL);
        const page = await context.newPage();
        await page.goto(base + '?demo' + (scene.query ?? ''));
        await page.waitForSelector('.row');
        await page.waitForTimeout(600);
        const file = `${scene.name}${theme === 'dark' ? '-dark' : ''}.png`;
        const shot = async (target, clip) => {
          await target.waitForTimeout(250);
          await target.screenshot({ path: join(OUT, file), clip, animations: 'disabled', caret: 'hide' });
          console.log(`  ${file}`);
        };
        await scene.run({ page, context, shot, base });
        await context.close();
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
