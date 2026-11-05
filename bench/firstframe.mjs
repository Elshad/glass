// Time to the first picture: page load to the first decoded frame, cold, on links where a
// large first keyframe is what takes the time. Every run is a fresh host and browser context.
//   node bench/firstframe.mjs [--link lan,cell3,cell1] [--runs 5] [--size 2560x1440] [--timeout 60]
import { createRequire } from 'node:module';
import { join, resolve } from 'node:path';
import { readFileSync } from 'node:fs';
import { Host, LINKS, shape } from './run.mjs';

const ROOT = resolve(import.meta.dirname, '..');
const { chromium } = createRequire(join(ROOT, 'tests/e2e/package.json'))('@playwright/test');
const o = { link: 'lan,cell3,cell1', runs: 5, size: '2560x1440', timeout: 60, workload: 'anim' };
const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i += 2) o[argv[i].replace(/^--/, '')] = argv[i + 1];

const median = (a) => { const s = [...a].sort((x, y) => x - y); return s.length ? s[s.length >> 1] : null; };

const browser = await chromium.launch({ channel: 'chrome', headless: !process.env.HEADFUL, args: ['--autoplay-policy=no-user-gesture-required'] });
const results = [];
for (const name of o.link.split(',')) {
  const times = [];
  let failed = 0;
  let last = {};
  for (let run = 0; run < Number(o.runs); run++) {
    const host = new Host({ GLASS_TEST_BENCH: o.workload, GLASS_TEST_SIZE: o.size });
    await host.start(60);
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const shaper = await shape({ shaper: 'user' }, LINKS[name], host.wt);
    try {
      const page = await context.newPage();
      const sessions = [];
      page.on('console', (m) => { if (/session killed|status: disconnected/.test(m.text())) sessions.push(m.text()); });
      await page.addInitScript(() => localStorage.setItem('glass.bench', '1'));
      if (shaper.port) {
        await page.route('**/config.json', async (route) => {
          const r = await route.fetch();
          route.fulfill({ response: r, json: { ...(await r.json()), wtPort: shaper.port } });
        });
      }
      await page.goto(`${host.base}/#t=${host.token}`);
      try {
        await page.waitForFunction(() => window.glassFirstFrame !== undefined, null, { timeout: Number(o.timeout) * 1000 });
        times.push(Math.round(await page.evaluate(() => window.glassFirstFrame)));
      } catch { failed++; }
      last = { reconnects: sessions.length };
    } finally {
      const counts = await shaper.stop().catch(() => undefined);
      if (counts) last.shaper = JSON.parse(counts);
      await context.close();
      await host.stop();
    }
  }
  const r = { link: name, medianMs: median(times), times, failed, ...last };
  console.log(JSON.stringify(r));
  results.push(r);
}
await browser.close();
