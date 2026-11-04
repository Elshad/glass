// Latency and FPS bench: test-mode hosts (synthetic source, no screen capture) drawing a
// marker workload, Chrome viewing them through a shaped link, and the viewer's bench
// probe reading the marker back from every decoded frame. See bench/README.md.
//   node bench/run.mjs [--workload anim,scroll] [--link lan,10m40,5m60,1m150]
//     [--path wt|rtc] [--shaper user|kernel] [--secs 20] [--warmup 5] [--size 1280x720]
//     [--fps 60] [--json out.json]
// Adapted from voxl (MIT, github.com/tembo/voxl): bench/measure.mjs and netbench.sh.

import { execFileSync, spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { createServer } from 'node:net';
import { createSocket } from 'node:dgram';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const ROOT = resolve(import.meta.dirname, '..');
const BINARY = process.env.GLASS_BIN ?? join(ROOT, 'target/release/glass-host');
const SITE = join(ROOT, 'site');
// Playwright comes from the e2e tests' install (cd tests/e2e && npm ci).
const { chromium } = createRequire(join(ROOT, 'tests/e2e/package.json'))('@playwright/test');

/** Links: rate (Mbps) and loss on the host -> viewer side, rtt split across both. */
export const LINKS = {
  lan: null,
  '10m40': { mbps: 10, rtt: 40, loss: 0 },
  '5m60': { mbps: 5, rtt: 60, loss: 0 },
  // Tesla-like: a car's LTE link.
  '1m150': { mbps: 1, rtt: 150, loss: 1 },
};
const WORKLOADS = ['anim', 'scroll'];

function args() {
  const o = { workload: WORKLOADS.join(','), link: Object.keys(LINKS).join(','), path: 'wt', shaper: 'user', secs: 20, warmup: 5, size: '1280x720', fps: 60 };
  const argv = process.argv.slice(2);
  for (let i = 0; i < argv.length; i++) {
    const key = argv[i].replace(/^--/, '');
    if (!(key in o) && key !== 'json') throw new Error(`unknown option ${argv[i]}`);
    o[key] = argv[++i];
  }
  o.secs = Number(o.secs);
  o.warmup = Number(o.warmup);
  o.fps = Number(o.fps);
  o.workloads = o.workload.split(',');
  o.links = o.link.split(',');
  for (const w of o.workloads) if (!WORKLOADS.includes(w)) throw new Error(`unknown workload ${w}`);
  for (const l of o.links) if (!(l in LINKS)) throw new Error(`unknown link ${l}`);
  if (!['wt', 'rtc'].includes(o.path)) throw new Error(`unknown path ${o.path}`);
  if (!['user', 'kernel'].includes(o.shaper)) throw new Error(`unknown shaper ${o.shaper}`);
  return o;
}

// ---- Ports ----

function isFree(port, kind) {
  return new Promise((done) => {
    if (kind === 'udp') {
      const s = createSocket({ type: 'udp6', ipv6Only: false });
      s.once('error', () => done(false));
      s.bind(port, '::', () => s.close(() => done(true)));
    } else {
      const s = createServer();
      s.once('error', () => done(false));
      s.listen({ port, host: '::' }, () => s.close(() => done(true)));
    }
  });
}

// Its own range (the e2e tests start at 21000), below the ephemeral ports.
let nextPort = Number(process.env.GLASS_BENCH_PORT_BASE ?? 24000) + (process.pid % 200) * 5;
async function freePort(kind) {
  for (let i = 0; i < 1000; i++) {
    const port = nextPort++;
    if (await isFree(port, kind)) return port;
  }
  throw new Error('no free ports');
}

// ---- A test-mode host ----

class Host {
  constructor(env) {
    this.env = env;
    this.dir = mkdtempSync(join(tmpdir(), 'glass-bench-'));
    this.configDir = join(this.dir, 'config');
  }

  async start(fps) {
    this.http = await freePort('tcp');
    this.wt = await freePort('udp');
    this.base = `http://localhost:${this.http}`;
    this.proc = spawn(BINARY, [
      '--test', '--http-port', String(this.http), '--wt-port', String(this.wt),
      '--log-dir', join(this.dir, 'logs'), '--downloads-dir', join(this.dir, 'downloads'), '--fps', String(fps),
    ], { env: { ...process.env, GLASS_CONFIG_DIR: this.configDir, ...this.env }, stdio: 'ignore' });
    for (let i = 0; i < 100; i++) {
      try {
        if ((await fetch(`${this.base}/config.json`)).ok && existsSync(join(this.configDir, 'status.json'))) return this;
      } catch {}
      await sleep(100);
    }
    throw new Error(`host didn't start; see ${this.dir}/logs/host.log`);
  }

  cli(...a) {
    return execFileSync(BINARY, a, { env: { ...process.env, GLASS_CONFIG_DIR: this.configDir, ...this.env } }).toString();
  }

  get token() {
    return readFileSync(join(this.configDir, 'token'), 'utf8').trim();
  }

  async stop() {
    if (!this.proc || this.proc.exitCode !== null) return;
    const exited = new Promise((r) => this.proc.once('exit', r));
    this.proc.kill('SIGTERM');
    await exited;
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- The introducer, for the WebRTC path (as tests/e2e/remote.spec.mjs runs it) ----

async function startIntroducer() {
  execFileSync('node', ['build.mjs'], { cwd: SITE, stdio: 'ignore' });
  const persist = mkdtempSync(join(tmpdir(), 'glass-bench-site-'));
  execFileSync('npx', ['wrangler', 'd1', 'migrations', 'apply', 'getglass', '--local', '--persist-to', persist], { cwd: SITE, stdio: 'ignore' });
  const port = await freePort('tcp');
  const base = `http://localhost:${port}`;
  const proc = spawn('npx', ['wrangler', 'dev', '--local', '--persist-to', persist, '--port', String(port), '--local-upstream', `localhost:${port}`, '--inspector-port', String(await freePort('tcp'))], {
    cwd: SITE, stdio: 'ignore', detached: true,
  });
  for (let i = 0; i < 300; i++) {
    try {
      if ((await fetch(`${base}/app/`)).ok) return { base, stop: () => { try { process.kill(-proc.pid, 'SIGTERM'); } catch {} } };
    } catch {}
    await sleep(100);
  }
  throw new Error('wrangler dev did not start');
}

// ---- Shaping ----

async function shape(o, link, port) {
  if (!link) return { stop: async () => {} };
  if (o.shaper === 'kernel') {
    execFileSync('sudo', [join(ROOT, 'bench/shape.sh'), 'up', String(port), String(link.mbps), String(link.rtt), String(link.loss)], { stdio: 'inherit' });
    return { stop: async () => execFileSync('sudo', [join(ROOT, 'bench/shape.sh'), 'down'], { stdio: 'inherit' }) };
  }
  const listen = await freePort('udp');
  const proc = spawn('node', [join(ROOT, 'bench/udpshaper.mjs'), String(listen), String(port), String(link.mbps), String(link.rtt), String(link.loss)], { stdio: ['ignore', 'pipe', 'inherit'] });
  let out = '';
  proc.stdout.on('data', (d) => { out += d; });
  for (let i = 0; i < 50 && !out.includes('udpshaper :'); i++) await sleep(50);
  return {
    port: listen,
    stop: async () => {
      const exited = new Promise((r) => proc.once('exit', r));
      proc.kill('SIGTERM');
      await exited;
      return out.match(/udpshaper (\{.*\})/)?.[1];
    },
  };
}

// ---- One run ----

const quantile = (sorted, p) => (sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))] : null);
const mean = (a) => (a.length ? a.reduce((x, y) => x + y, 0) / a.length : null);
const round = (v, d = 1) => (v == null ? null : Number(v.toFixed(d)));

export function summarise(seconds) {
  const unique = seconds.map((s) => s.unique);
  const lat = seconds.flatMap((s) => s.lat).sort((a, b) => a - b);
  const glass = seconds.map((s) => s.glassLatency).filter((v) => v != null);
  return {
    fps: round(mean(unique)),
    worstSecond: unique.length ? Math.min(...unique) : null,
    latencyMs: { p50: round(quantile(lat, 0.5)), p99: round(quantile(lat, 0.99)), samples: lat.length },
    glassLatencyMs: round(mean(glass)),
    decodedFps: round(mean(seconds.map((s) => s.decoded))),
    mbps: round(mean(seconds.map((s) => s.mbps)), 2),
    dropped: seconds.reduce((n, s) => n + s.dropped, 0),
    badMarkers: seconds.reduce((n, s) => n + s.bad, 0),
    rttMs: round(mean(seconds.map((s) => s.rtt))),
  };
}

async function run(o, browser, introducer, workload, linkName) {
  const link = LINKS[linkName];
  if (o.path === 'rtc' && link && o.shaper === 'user') {
    return { skipped: 'the WebRTC path needs --shaper kernel to shape (ICE goes straight to the host)' };
  }
  const rtcPort = o.path === 'rtc' ? await freePort('udp') : undefined;
  const host = new Host({
    GLASS_TEST_BENCH: workload,
    GLASS_TEST_SIZE: o.size,
    ...(introducer && { GLASS_BASE: introducer.base, GLASS_FLEET_FAKE: '1', GLASS_REMOTE_RTP: '0', GLASS_TEST_RTC_PORT: String(rtcPort) }),
  });
  await host.start(o.fps);
  const [w, h] = o.size.split('x').map(Number);
  const context = await browser.newContext({ viewport: { width: w + 320, height: h + 280 } });
  let shaper;
  try {
    shaper = await shape(o, link, o.path === 'rtc' ? rtcPort : host.wt);
    const page = await context.newPage();
    await page.addInitScript((rtc) => {
      localStorage.setItem('glass.bench', '1');
      if (rtc) sessionStorage.setItem('glass.rtp', '0'); // video over the data channel
    }, o.path === 'rtc');
    let url;
    if (o.path === 'rtc') {
      for (let i = 0; i < 100 && !host.cli('status', '--json').includes('"running":true'); i++) await sleep(100);
      url = JSON.parse(host.cli('link', '--anywhere', '--json')).url;
    } else {
      url = `${host.base}/#t=${host.token}`;
      // Userspace shaping: point the viewer at the shaper instead of the host.
      if (shaper.port) {
        await page.route('**/config.json', async (route) => {
          const r = await route.fetch();
          route.fulfill({ response: r, json: { ...(await r.json()), wtPort: shaper.port } });
        });
      }
    }
    await page.goto(url);
    await page.waitForFunction(() => window.glassBench?.some((s) => s.unique > 0), null, { timeout: 30_000 });
    await sleep(o.warmup * 1000);
    const from = await page.evaluate(() => window.glassBench.length);
    await sleep(o.secs * 1000);
    const seconds = (await page.evaluate(() => window.glassBench)).slice(from);
    const shaperCounts = await shaper.stop();
    shaper = undefined;
    return { ...summarise(seconds), ...(shaperCounts && { shaper: JSON.parse(shaperCounts) }) };
  } finally {
    await shaper?.stop().catch(() => {});
    await context.close();
    await host.stop();
  }
}

// ---- Main ----

if (import.meta.main ?? process.argv[1] === import.meta.filename) {
  const o = args();
  if (!existsSync(BINARY)) throw new Error(`no ${BINARY}: cargo build --release -p glass-host (or set GLASS_BIN)`);
  const browser = await chromium.launch({
    // Google Chrome, not bundled Chromium: WebCodecs H.264 needs the proprietary codecs.
    channel: process.env.CHROME_CHANNEL ?? 'chrome',
    headless: !process.env.HEADFUL,
    args: ['--autoplay-policy=no-user-gesture-required'],
  });
  const introducer = o.path === 'rtc' ? await startIntroducer() : undefined;
  const results = [];
  try {
    console.log(`path ${o.path}, shaper ${o.shaper}, ${o.size} @ ${o.fps} fps, ${o.secs} s per run`);
    console.log('workload  link    fps  worst  p50 ms  p99 ms  glass ms  Mbps  dropped');
    for (const workload of o.workloads) {
      for (const link of o.links) {
        let r;
        try {
          r = await run(o, browser, introducer, workload, link);
        } catch (e) {
          r = { error: e.message ?? String(e) };
        }
        results.push({ path: o.path, shaper: LINKS[link] ? o.shaper : 'none', workload, link, ...r });
        const cell = (v, n) => String(v ?? '-').padStart(n);
        console.log(r.skipped || r.error
          ? `${workload.padEnd(9)} ${link.padEnd(6)} ${r.skipped ? 'skipped' : 'error'}: ${r.skipped ?? r.error}`
          : `${workload.padEnd(9)} ${link.padEnd(6)}${cell(r.fps, 5)}${cell(r.worstSecond, 7)}${cell(r.latencyMs.p50, 8)}${cell(r.latencyMs.p99, 8)}${cell(r.glassLatencyMs, 10)}${cell(r.mbps, 6)}${cell(r.dropped, 9)}`);
      }
    }
  } finally {
    introducer?.stop();
    await browser.close();
  }
  if (o.json) {
    mkdirSync(resolve(o.json, '..'), { recursive: true });
    writeFileSync(o.json, JSON.stringify(results, null, 2));
  }
}
