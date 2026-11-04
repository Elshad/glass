// Starts isolated `glass --test` hosts and gives tests helpers for the page and the
// host's /test/* hooks. Each host gets its own ports, config dir, downloads and logs.

import { execFileSync, spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { createSocket } from 'node:dgram';
import { mkdtempSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { test as base, expect } from '@playwright/test';

// Every spec's `test`: closes the browser contexts a test opened, even when it fails, so
// pages left behind don't keep reconnecting to stopped hosts and slow the run.
export const test = base.extend({
  _closeContexts: [
    async ({ browser }, use) => {
      const before = new Set(browser.contexts());
      await use();
      await Promise.all(browser.contexts().filter((c) => !before.has(c)).map((c) => c.close().catch(() => {})));
    },
    { auto: true },
  ],
});
export { expect };

const ROOT = resolve(import.meta.dirname, '../..');
export const BINARY = process.env.GLASS_BIN ?? join(ROOT, 'target/release/glass-host');

// Each parallel worker gets its own block of ports, below the OS's ephemeral range (so the
// browser's own sockets can't take one between our check and the host binding it).
const PORTS_PER_WORKER = 400;
// GLASS_E2E_PORT_BASE moves the whole range, so two test runs on one machine don't collide.
const firstPort =
  Number(process.env.GLASS_E2E_PORT_BASE ?? 21000) + Number(process.env.TEST_PARALLEL_INDEX ?? 0) * PORTS_PER_WORKER;
// A worker restarted after a failure keeps its index: start somewhere else in the block so
// it doesn't reuse ports its predecessor's hosts (and Chrome's QUIC sessions) just had.
let nextOffset = (process.pid * 37) % PORTS_PER_WORKER;

// Free on both IPv4 and IPv6: the host binds 0.0.0.0, which an IPv6-only check misses.
function isFreeOn(port, kind, family) {
  return new Promise((resolve) => {
    const address = family === 6 ? '::' : '0.0.0.0';
    if (kind === 'udp') {
      const sock = createSocket({ type: family === 6 ? 'udp6' : 'udp4', ipv6Only: family === 6 });
      sock.once('error', () => resolve(false));
      sock.bind(port, address, () => sock.close(() => resolve(true)));
    } else {
      const server = createServer();
      server.once('error', () => resolve(false));
      server.listen({ port, host: address, ipv6Only: family === 6 }, () => server.close(() => resolve(true)));
    }
  });
}

async function isFree(port, kind) {
  return (await isFreeOn(port, kind, 4)) && (await isFreeOn(port, kind, 6));
}

/** The next port in this worker's block that's free for `kind` (tcp or udp). */
export async function freePort(kind) {
  for (let i = 0; i < PORTS_PER_WORKER; i++) {
    const port = firstPort + (nextOffset++ % PORTS_PER_WORKER);
    if (await isFree(port, kind)) return port;
  }
  throw new Error('no free test ports');
}

export class Host {
  /** `os`: what the host reports itself as (macos or linux). */
  /** `fps`: the test displays' refresh rate (viewers get up to this). */
  constructor({ configDir, os = 'macos', latest, webDir, env, fps = 30 } = {}) {
    this.fps = fps;
    this.webDir = webDir;
    this.env = env;
    this.os = os;
    this.latest = latest;
    this.dir = mkdtempSync(join(tmpdir(), 'glass-e2e-'));
    this.configDir = configDir ?? join(this.dir, 'config');
  }

  async start() {
    // Ports are picked once and kept, so a restarted host comes back at the same address.
    if (!this.http) {
      this.http = await freePort('tcp');
      this.wt = await freePort('udp');
      this.base = `http://localhost:${this.http}`;
    }
    this.proc = spawn(
      BINARY,
      [
        '--test',
        '--http-port', String(this.http),
        '--wt-port', String(this.wt),
        '--log-dir', join(this.dir, 'logs'),
        '--downloads-dir', join(this.dir, 'downloads'),
        '--fps', String(this.fps),
        ...(this.webDir ? ['--web-dir', this.webDir] : []),
      ],
      {
        env: { ...process.env, GLASS_CONFIG_DIR: this.configDir, GLASS_TEST_OS: this.os, ...(this.latest && { GLASS_TEST_LATEST: this.latest }), ...this.env },
        stdio: 'ignore',
      },
    );
    for (let i = 0; i < 100; i++) {
      try {
        const r = await fetch(`${this.base}/config.json`);
        // Ready once it has also written its status, which `glass link` and friends read.
        if (r.ok && existsSync(join(this.configDir, 'status.json'))) return this;
      } catch {}
      await new Promise((r) => setTimeout(r, 100));
    }
    throw new Error(`host on :${this.http} didn't start; see ${this.dir}/logs/host.log`);
  }

  async stop() {
    if (!this.proc) return;
    const exited = new Promise((r) => this.proc.once('exit', r));
    this.proc.kill('SIGTERM');
    await exited;
    this.proc = undefined;
  }

  /** Runs the glass CLI against this host's config (and environment). */
  cli(...args) {
    return execFileSync(BINARY, args, { env: { ...process.env, GLASS_CONFIG_DIR: this.configDir, ...this.env } }).toString();
  }

  get token() {
    return readFileSync(join(this.configDir, 'token'), 'utf8').trim();
  }

  url({ token = true } = {}) {
    return token ? `${this.base}/#t=${this.token}` : `${this.base}/`;
  }

  async get(path) {
    const r = await fetch(`${this.base}${path}`);
    const type = r.headers.get('content-type') ?? '';
    return type.includes('json') ? r.json() : r.text();
  }

  async post(path, body = '') {
    const r = await fetch(`${this.base}${path}`, { method: 'POST', body });
    expect(r.ok, `POST ${path}`).toBeTruthy();
  }

  async clearInput() {
    await fetch(`${this.base}/test/input`, { method: 'DELETE' });
  }

  /** Recorded input events, optionally filtered by type. */
  async input(type) {
    const { events, pastes } = await this.get('/test/input');
    return { events: type ? events.filter((e) => e.t === type) : events, pastes };
  }

  /** Client log lines (forwarded by the page) matching `msg`. */
  clientLog(msg) {
    const file = join(this.dir, 'logs', 'client.log');
    if (!existsSync(file)) return [];
    return readFileSync(file, 'utf8')
      .split('\n')
      .filter(Boolean)
      .map((l) => JSON.parse(l.slice(l.indexOf(' ') + 1)))
      .filter((e) => (msg instanceof RegExp ? msg.test(e.msg) : e.msg === msg));
  }
}

/** Waits until the HUD shows `count` decoded streams (each "640×360"). */
export async function expectVideo(page, count = 1) {
  await expect
    .poll(async () => ((await hudText(page)).split('\n')[0].match(/640×360/g) ?? []).length, { timeout: 15_000 })
    .toBe(count);
}

export async function hudText(page) {
  return (await page.locator('#hud').textContent()) ?? '';
}

/** Parses the HUD's stats lines. */
export async function stats(page) {
  const hud = await hudText(page);
  const num = (re) => Number(hud.match(re)?.[1] ?? NaN);
  return {
    latency: num(/latency ([\d.]+) ms/),
    fps: num(/(\d+) fps/),
    dropped: num(/dropped (\d+)/),
    audioPackets: num(/audio (\d+) pkt\/s/),
    audioCodec: hud.match(/audio \d+ pkt\/s .* (pcm|opus)/)?.[1],
    connected: /connected/.test(hud.split('\n')[0]),
  };
}

/** Clicks a toolbar button (showing the toolbar first if it was hidden). */
export async function toolbarClick(page, id) {
  if (await page.locator('#toolbar-show').isVisible()) await page.locator('#toolbar-show').click();
  // On a phone-sized screen the button may be in its group's dropdown.
  const button = page.locator(`#${id}`);
  if (!(await button.isVisible())) {
    const trigger = page.locator(`.tb-group:has(#${id}) .tb-trigger`);
    if (await trigger.isVisible()) await trigger.click();
  }
  await button.click();
}

/** Opens a toolbar menu and clicks the item whose label starts with `label`. */
export async function pickMenuItem(page, button, label) {
  await toolbarClick(page, button);
  await page.locator('.panel:not(.hidden) button', { hasText: label }).first().click();
}

/** Dispatches pointer events like a finger would, at a fraction of the canvas. */
export async function touch(page, canvasIndex, points) {
  await page.evaluate(
    ({ canvasIndex, points }) => {
      const canvas = document.querySelectorAll('#screens canvas')[canvasIndex];
      const r = canvas.getBoundingClientRect();
      // Touch ids distinct from the mouse's (1), as on real hardware.
      for (const { type, id: finger = 1, fx, fy } of points) {
        const id = 100 + finger;
        canvas.dispatchEvent(
          new PointerEvent(type, {
            bubbles: true,
            cancelable: true,
            pointerType: 'touch',
            pointerId: id,
            clientX: r.left + r.width * fx,
            clientY: r.top + r.height * fy,
          }),
        );
      }
    },
    { canvasIndex, points },
  );
}
