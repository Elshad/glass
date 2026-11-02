// Restarting from the viewer, end to end on the Windows VM: Glass, then the whole computer,
// with the same page reconnecting by itself each time. The computer comes back to the
// sign-in screen (automatic sign-in off), where the sign-in host (signin.rs) serves it until
// the viewer signs in. Needs the secure-desktop service, and GLASS_WT_HOST set machine-wide
// (the hosts are started by Windows, not the tests). Restarts the VM: run it on its own.

import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { test, expect } from '@playwright/test';
import { ps, tunnel } from './vm.mjs';

test.describe.configure({ mode: 'serial' });
const PORT = 18096;
let page;
let closeTunnel;

/** What's serving viewers now: the user's host, the sign-in host, or nothing. */
function serving() {
  try {
    const procs = ps(`Get-CimInstance Win32_Process | Where-Object Name -eq 'glass.exe' | ForEach-Object CommandLine`);
    if (procs.includes('--glass-signin-host')) return 'sign-in';
    if (/Programs\\\\Glass\\\\bin\\\\glass\.exe/i.test(procs) || /Programs.Glass.bin.glass\.exe/i.test(procs)) return 'user';
    return 'none';
  } catch {
    return 'down';
  }
}

/** The picture is live (a frame decoded since `since`). */
async function seesFrames() {
  return page.evaluate(() => document.querySelector('#screens canvas')?.width ?? 0);
}

async function typePassword() {
  const creds = readFileSync(`${homedir()}/VMs/win11/credentials.txt`, 'utf8');
  const password = creds.match(/password\s*[:=]\s*(\S+)/i)[1];
  const shifted = { '!': 'Digit1', '@': 'Digit2', '#': 'Digit3', $: 'Digit4', '%': 'Digit5', '^': 'Digit6', '&': 'Digit7', '*': 'Digit8', '(': 'Digit9', ')': 'Digit0', _: 'Minus', '+': 'Equal', '{': 'BracketLeft', '}': 'BracketRight', '|': 'Backslash', ':': 'Semicolon', '"': 'Quote', '<': 'Comma', '>': 'Period', '?': 'Slash', '~': 'Backquote' };
  const plain = { '-': 'Minus', '=': 'Equal', '[': 'BracketLeft', ']': 'BracketRight', '\\': 'Backslash', ';': 'Semicolon', "'": 'Quote', ',': 'Comma', '.': 'Period', '/': 'Slash', '`': 'Backquote' };
  for (const c of password) {
    if (/[a-z]/.test(c)) await page.keyboard.press(`Key${c.toUpperCase()}`);
    else if (/[A-Z]/.test(c)) await page.keyboard.press(`Shift+Key${c}`);
    else if (/[0-9]/.test(c)) await page.keyboard.press(`Digit${c}`);
    else if (shifted[c]) await page.keyboard.press(`Shift+${shifted[c]}`);
    else if (plain[c]) await page.keyboard.press(plain[c]);
    await page.waitForTimeout(40);
  }
}

test.beforeAll(async ({ browser }) => {
  expect(serving()).toBe('user');
  const token = ps(`Get-Content "$env:APPDATA\\Glass\\token"`).trim();
  closeTunnel = await tunnel(PORT);
  page = await (await browser.newContext()).newPage();
  page.on('dialog', (d) => d.accept());
  await page.goto(`http://localhost:${PORT}/#t=${token}`);
  await expect.poll(seesFrames, { timeout: 30_000 }).toBeGreaterThan(0);
});

test.afterAll(async () => {
  await page?.context().close();
  closeTunnel?.();
});

test('Restart Glass: the page reconnects by itself', async () => {
  const pid = ps(`(Get-Process glass | Where-Object Path -like '*Programs\\Glass\\bin*').Id`);
  await page.locator('#btn-more').click();
  await page.locator('[role=menuitem]', { hasText: 'Restart Glass' }).click();
  // A new host process, then the page live again.
  await expect.poll(() => ps(`(Get-Process glass -EA 0 | Where-Object Path -like '*Programs\\Glass\\bin*').Id`), { timeout: 60_000 }).not.toBe(pid);
  await page.waitForTimeout(8000);
  await page.screenshot({ path: test.info().outputPath('after-glass-restart.png') });
  await expect(page.getByText(/Can't reach|Connecting/)).toHaveCount(0, { timeout: 30_000 });
});

test('Restart computer: back at the sign-in screen, sign in, back on the desktop', async () => {
  test.setTimeout(600_000);
  await page.locator('#btn-more').click();
  await page.locator('[role=menuitem]', { hasText: 'Restart computer' }).click();
  // Down, then the sign-in host.
  await expect.poll(serving, { timeout: 120_000, intervals: [5000] }).toBe('down');
  await expect.poll(serving, { timeout: 300_000, intervals: [10_000] }).toBe('sign-in');
  // The tunnel to the VM went down with it: open it again; the page keeps retrying.
  closeTunnel();
  closeTunnel = await tunnel(PORT);
  await expect(page.getByText(/Can't reach|Connecting/)).toHaveCount(0, { timeout: 90_000 });
  await page.screenshot({ path: test.info().outputPath('sign-in-screen.png') });
  // Sign in through the viewer.
  const box = await page.locator('#screens canvas').first().boundingBox();
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  await page.keyboard.press('Space');
  await page.waitForTimeout(2500);
  await page.keyboard.press('Control+KeyA');
  await page.keyboard.press('Backspace');
  await typePassword();
  await page.keyboard.press('Enter');
  // The sign-in host steps aside for the user's own host, and the page follows.
  await expect.poll(serving, { timeout: 120_000, intervals: [5000] }).toBe('user');
  await expect(page.getByText(/Can't reach|Connecting/)).toHaveCount(0, { timeout: 90_000 });
  await page.waitForTimeout(5000);
  await page.screenshot({ path: test.info().outputPath('desktop.png') });
  // Owner again: the file buttons are back.
  await expect(page.locator('#btn-files')).toBeVisible();
});
