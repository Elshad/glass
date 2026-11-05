// Regressions from a real failure: an iPhone on a cellular link, where the adaptive rate talked
// itself down to the floor and the viewer kept losing its session. Each check is a test host
// (synthetic source, no screen capture) viewed through a shaped link; exits non-zero on failure.
//   GLASS_BIN=target/debug/glass-host node bench/regress.mjs [--only cold,watchdog,pause]
// See bench/README.md. Shaping is userspace, the encoder OpenH264: VideoToolbox's own behaviour
// isn't covered here (see vt_at_low_bitrates in crates/glass-host/src/macos/encode.rs).

import { createRequire } from 'node:module';
import { existsSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { Host, LINKS, countWatchdog, shape, sleep } from './run.mjs';

const ROOT = resolve(import.meta.dirname, '..');
const BINARY = process.env.GLASS_BIN ?? join(ROOT, 'target/release/glass-host');
const { chromium } = createRequire(join(ROOT, 'tests/e2e/package.json'))('@playwright/test');
const SIZE = '1280x720';

const only = process.argv.includes('--only') ? process.argv[process.argv.indexOf('--only') + 1].split(',') : null;
const bitrate = async (host) => (await (await fetch(`${host.base}/test/bitrate`)).json()).bps / 1e6;

/** A viewer on a test host through `linkName`; `use({ page, host, samples })`, then all torn down. */
async function session(browser, workload, linkName, use, { phone = false } = {}) {
  const host = new Host({ GLASS_TEST_BENCH: workload, GLASS_TEST_SIZE: SIZE });
  await host.start(60);
  const context = await browser.newContext({ viewport: { width: 1600, height: 1000 } });
  const shaper = await shape({ shaper: 'user' }, LINKS[linkName], host.wt);
  const samples = [];
  const started = Date.now();
  const poll = setInterval(async () => {
    try {
      samples.push([(Date.now() - started) / 1000, await bitrate(host)]);
    } catch {}
  }, 250);
  try {
    const page = await context.newPage();
    await page.addInitScript((isPhone) => {
      localStorage.setItem('glass.bench', '1');
      // As an iPhone: a hidden page pauses video at once (others wait a minute).
      if (isPhone) Object.defineProperty(navigator, 'userAgent', { get: () => 'iPhone (bench)' });
    }, phone);
    if (shaper.port) {
      await page.route('**/config.json', async (route) => {
        const r = await route.fetch();
        route.fulfill({ response: r, json: { ...(await r.json()), wtPort: shaper.port } });
      });
    }
    await page.goto(`${host.base}/#t=${host.token}`);
    await page.waitForFunction(() => window.glassBench?.some((s) => s.unique > 0), null, { timeout: 45_000 });
    return await use({ page, host, samples, started });
  } finally {
    clearInterval(poll);
    await shaper.stop().catch(() => {});
    await context.close();
    await host.stop();
  }
}

function hostLog(host) {
  try {
    return readFileSync(join(host.dir, 'logs', 'host.log'), 'utf8');
  } catch {
    return '';
  }
}

const setHidden = (page, hidden) => page.evaluate((h) => {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => (h ? 'hidden' : 'visible') });
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => h });
  document.dispatchEvent(new Event('visibilitychange'));
}, hidden);

/** Pauses the page's worker (the one with the session) until `thaw()`. */
async function freezeWorker(page) {
  const cdp = await page.context().newCDPSession(page);
  const workers = [];
  cdp.on('Target.attachedToTarget', ({ sessionId, targetInfo }) => {
    if (targetInfo.type === 'worker') workers.push(sessionId);
  });
  await cdp.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: false });
  await sleep(100);
  const each = (method) => Promise.all(workers.map((sessionId) => cdp.send('Target.sendMessageToTarget', {
    sessionId,
    message: JSON.stringify({ id: Math.floor(Math.random() * 1e9), method }),
  })));
  if (!workers.length) throw new Error('no worker to freeze');
  await each('Debugger.enable');
  await each('Debugger.pause');
  return { thaw: async () => { await each('Debugger.resume'); await each('Debugger.disable'); } };
}

const CHECKS = {
  /** From a cold start, with more to send than any link carries, the rate finds the link's. */
  cold: async (browser) => {
    const out = [];
    for (const [link, mbps] of [['10m40', 10], ['phone', 5]]) {
      out.push(await session(browser, 'noise', link, async ({ samples, page, host }) => {
        await sleep(15_000);
        const settled = samples.slice(-20).map((s) => s[1]);
        const low = Math.min(...settled);
        const received = (await page.evaluate(() => window.glassBench.slice(-5))).map((s) => s.mbps);
        const mean = received.reduce((a, b) => a + b, 0) / received.length;
        return {
          ok: low >= mbps * 0.4 && mean >= mbps * 0.25 && countWatchdog(host) === 0,
          detail: `${link}: rate over the last 5 s stays at or above ${low.toFixed(1)} Mbps of ${mbps}, viewer receives ${mean.toFixed(1)} Mbps`,
        };
      }));
    }
    return { ok: out.every((r) => r.ok), detail: out.map((r) => r.detail).join('; ') };
  },
  /** A viewer on a queueing, slightly lossy link never goes 3 s without data. */
  watchdog: async (browser) => {
    const out = [];
    for (const workload of ['still', 'anim']) {
      out.push(await session(browser, workload, 'phoneloss', async ({ host }) => {
        await sleep(25_000);
        const n = countWatchdog(host);
        return { ok: n === 0, detail: `${workload}: tripped ${n} times in 25 s` };
      }));
    }
    return { ok: out.every((r) => r.ok), detail: out.map((r) => r.detail).join('; ') };
  },
  /** Pausing video (a hidden tab) leaves the rate alone, and video comes back after. */
  pause: async (browser) => session(browser, 'anim', 'phone', async ({ page, host, samples }) => {
    await sleep(6_000);
    const before = await bitrate(host);
    const from = samples.length;
    await setHidden(page, true);
    // A hidden iPhone tab is frozen right away: frames on their way are never acknowledged.
    // Stop the viewer's worker in the debugger, which is that without closing its session.
    const worker = await freezeWorker(page);
    await sleep(8_000);
    await worker.thaw();
    const during = samples.slice(from).map((s) => s[1]);
    const low = Math.min(...during);
    const seen = await page.evaluate(() => window.glassBench.length);
    await setHidden(page, false);
    await page.waitForFunction((n) => window.glassBench.slice(n).some((s) => s.unique > 0), seen, { timeout: 15_000 });
    const log = hostLog(host);
    const stuck = (log.match(/video link stuck/g) ?? []).length;
    const paused = log.includes('video paused');
    return {
      ok: paused && low >= before * 0.99 && stuck === 0 && countWatchdog(host) === 0,
      detail: `${paused ? 'paused' : "didn't pause"}: rate ${before.toFixed(1)} Mbps before, lowest ${low.toFixed(1)} while paused; "stuck" logged ${stuck} times; video back`,
    };
  }, { phone: true }),
};

if (!existsSync(BINARY)) throw new Error(`no ${BINARY}: build glass-host (or set GLASS_BIN)`);
const browser = await chromium.launch({
  channel: process.env.CHROME_CHANNEL ?? 'chrome',
  headless: !process.env.HEADFUL,
  args: ['--autoplay-policy=no-user-gesture-required'],
});
let failed = 0;
try {
  for (const [name, check] of Object.entries(CHECKS)) {
    if (only && !only.includes(name)) continue;
    let r;
    try {
      r = await check(browser);
    } catch (e) {
      r = { ok: false, detail: e.message ?? String(e) };
    }
    if (!r.ok) failed++;
    console.log(`${r.ok ? 'ok  ' : 'FAIL'} ${name}: ${r.detail}`);
  }
} finally {
  await browser.close();
}
process.exit(failed ? 1 : 0);
