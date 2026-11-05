// An agent using the computer through `glass mcp`: screenshots, clicks, typing and keys
// land on the host; the viewer shows each step, can pause and stop it, and answers its
// questions.

import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { join } from 'node:path';
import { BINARY, Host, expectVideo, test, expect } from './harness.mjs';

const it = test.extend({
  host: async ({}, use) => {
    const host = await new Host().start();
    await use(host);
    await host.stop();
  },
});

/** `glass mcp` against `host`, spoken to over stdio like an MCP client does. */
function mcp(host) {
  const child = spawn(BINARY, ['mcp', '--url', host.base, '--token-file', join(host.configDir, 'token')], {
    env: { ...process.env, GLASS_CONFIG_DIR: host.configDir },
    stdio: ['pipe', 'pipe', 'inherit'],
  });
  const waiting = new Map();
  createInterface({ input: child.stdout }).on('line', (line) => {
    const msg = JSON.parse(line);
    waiting.get(msg.id)?.(msg);
    waiting.delete(msg.id);
  });
  let next = 0;
  const rpc = (method, params = {}) =>
    new Promise((resolve) => {
      const id = ++next;
      waiting.set(id, resolve);
      child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
    });
  const tool = async (name, args = {}) => (await rpc('tools/call', { name, arguments: args })).result;
  return { rpc, tool, close: () => child.kill() };
}

it('an agent sees, clicks, types and asks, and the viewer steers it', async ({ page, host }) => {
  const agent = mcp(host);
  try {
    const init = await agent.rpc('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'test', version: '1' } });
    expect(init.result.serverInfo.name).toBe('glass');
    expect(init.result.instructions).toContain('confirm');
    const names = (await agent.rpc('tools/list')).result.tools.map((t) => t.name);
    expect(names).toEqual(expect.arrayContaining(['screenshot', 'click', 'type_text', 'press_keys', 'request_help', 'confirm']));

    await page.goto(host.url());
    await expectVideo(page);

    const shot = await agent.tool('screenshot');
    expect(shot.content[0]).toMatchObject({ type: 'image', mimeType: 'image/png' });
    expect(shot.content[1].text).toContain('640×360');

    await host.clearInput();
    expect((await agent.tool('click', { x: 320, y: 180 })).isError).toBeFalsy();
    const { events } = await host.input();
    expect(events[0]).toMatchObject({ t: 'move', x: 0.5, y: 0.5 });
    expect(events.slice(1).map((e) => e.t)).toEqual(['down', 'up']);
    // The viewer shows what it did, and where.
    await expect(page.locator('#agent-bar')).toBeVisible();
    await expect(page.locator('#agent-label')).toContainText('click at 320, 180');

    await host.clearInput();
    await agent.tool('type_text', { text: 'héllo' });
    await agent.tool('press_keys', { keys: 'cmd+s' });
    const typed = (await host.input()).events;
    expect(typed[0]).toEqual({ t: 'text', text: 'héllo' });
    expect(typed.slice(1).map((e) => [e.t, e.keycode])).toEqual([['keydown', 0x37], ['keydown', 0x01], ['keyup', 0x01], ['keyup', 0x37]]);

    // Paused, its next step waits until the owner resumes.
    await page.locator('#agent-pause').click();
    await expect(page.locator('#agent-label')).toHaveText('Agent paused');
    let clicked = false;
    const pending = agent.tool('click', { x: 10, y: 10 }).then((r) => { clicked = true; return r; });
    await page.waitForTimeout(800);
    expect(clicked).toBe(false);
    await page.locator('#agent-pause').click(); // Resume
    expect((await pending).isError).toBeFalsy();

    // Approval: the card asks the owner, the note comes back.
    const asking = agent.tool('confirm', { action: 'Send the email to Ana' });
    const card = page.locator('.agent-ask');
    await expect(card).toContainText('Send the email to Ana');
    await card.locator('input').fill('cc Bo too');
    await card.getByRole('button', { name: 'Allow' }).click();
    expect((await asking).content[0].text).toBe('Approved. Note: cc Bo too');
    await expect(card).toHaveCount(0);

    const helping = agent.tool('request_help', { message: 'Type the 2FA code' });
    await page.locator('.agent-ask').getByRole('button', { name: "Can't" }).click();
    expect((await helping).content[0].text).toBe('The person declined.');

    // Stopped, everything is refused until the owner allows it again.
    await page.locator('#agent-stop').click();
    const refused = await agent.tool('click', { x: 1, y: 1 });
    expect(refused.isError).toBe(true);
    expect(refused.content[0].text).toContain('stopped');
    await page.locator('#agent-stop').click(); // Allow again
    expect((await agent.tool('click', { x: 1, y: 1 })).isError).toBeFalsy();
  } finally {
    agent.close();
  }
});

it('keys pressed into a password field reach it, but viewers and the log are not told which', async ({ page }) => {
  const host = await new Host({ env: { GLASS_TEST_SECURE_INPUT: '1' } }).start();
  const agent = mcp(host);
  try {
    await agent.rpc('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'test', version: '1' } });
    await page.goto(host.url());
    await expectVideo(page);
    await host.clearInput();
    expect((await agent.tool('press_keys', { keys: 'n' })).isError).toBeFalsy();
    expect((await host.input()).events.map((e) => [e.t, e.keycode])).toEqual([['keydown', 0x2d], ['keyup', 0x2d]]);
    await expect(page.locator('#agent-label')).toContainText('a key, into a password field');
  } finally {
    agent.close();
    await host.stop();
  }
});

it('drives another machine on the tailnet by name', async ({ browser, host }) => {
  const other = await new Host({ env: { GLASS_TEST_TRUST_LOOPBACK: '1' } }).start();
  const machines = [
    { name: 'This Mac', dns: 'this.example.ts.net', glass: true, online: true, os: 'macOS', self: true },
    { name: 'Mac mini', dns: 'mac-mini.example.ts.net', glass: true, online: true, os: 'macOS', self: false, url: other.base },
    { name: 'Old laptop', dns: 'old.example.ts.net', glass: false, online: true, os: 'Linux', self: false },
  ];
  await host.stop();
  const home = await new Host({ configDir: host.configDir, env: { GLASS_TEST_MACHINES: JSON.stringify(machines) } }).start();
  const agent = mcp(home);
  try {
    const list = JSON.parse((await agent.tool('list_machines')).content[0].text);
    expect(list.map((m) => m.name)).toEqual(['This Mac', 'Mac mini', 'Old laptop']);

    // The viewer on the other machine sees the agent at work there.
    const page = await (await browser.newContext()).newPage();
    await page.goto(other.url());
    await expectVideo(page);
    await other.clearInput();
    await home.clearInput();
    expect((await agent.tool('click', { machine: 'mac-mini', x: 320, y: 180 })).isError).toBeFalsy();
    expect((await other.input()).events.map((e) => e.t)).toEqual(['move', 'down', 'up']);
    expect((await home.input()).events).toHaveLength(0);
    await expect(page.locator('#agent-label')).toContainText('click');

    const shot = await agent.tool('screenshot', { machine: 'Mac mini' });
    expect(shot.content[0].type).toBe('image');
    // And one that can't be driven says why.
    const missing = await agent.tool('screenshot', { machine: 'Old laptop' });
    expect(missing.isError).toBe(true);
    expect(missing.content[0].text).toContain("Glass isn't running on Old laptop");
    expect((await agent.tool('click', { machine: 'nope', x: 1, y: 1 })).content[0].text).toContain('no machine called');
  } finally {
    agent.close();
    await home.stop();
    await other.stop();
  }
});

it('only the owner can drive it', async ({ host }) => {
  const r = await fetch(`${host.base}/agent/v1/displays`);
  expect(r.status).toBe(403);
  const ok = await fetch(`${host.base}/agent/v1/displays`, { headers: { authorization: `Bearer ${host.token}` } });
  expect((await ok.json()).displays).toHaveLength(2);
});
