// The Windows host's basics, end to end: a browser viewer driving a real Windows computer.
// One host and one page for the whole file; the tests run in order and each leaves the
// desktop as the next expects (Notepad open and in front). See README.md.

import { mkdtempSync, writeFileSync } from 'node:fs';
import { crc32, deflateSync } from 'node:zlib';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test, expect } from '@playwright/test';
import { desk, hostLog, ps, put, startHost, tunnel, WORK } from './vm.mjs';

test.describe.configure({ mode: 'serial' });

const PORT = Number(process.env.GLASS_VM_PORT ?? 18090);
const files = mkdtempSync(join(tmpdir(), 'glass-vm-'));
let page;
let token;
let closeTunnel;

/** A small PNG of one colour. */
function png(width, height, [r, g, b]) {
  const chunk = (type, data) => {
    const out = Buffer.alloc(12 + data.length);
    out.writeUInt32BE(data.length, 0);
    out.write(type, 4, 'latin1');
    data.copy(out, 8);
    out.writeUInt32BE(crc32(Buffer.concat([Buffer.from(type, 'latin1'), data])), 8 + data.length);
    return out;
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header.set([8, 2, 0, 0, 0], 8);
  const row = Buffer.concat([Buffer.from([0]), Buffer.from(Array.from({ length: width }, () => [r, g, b]).flat())]);
  const pixels = deflateSync(Buffer.concat(Array.from({ length: height }, () => row)));
  return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk('IHDR', header), chunk('IDAT', pixels), chunk('IEND', Buffer.alloc(0))]);
}

/** A point on the streamed picture, as fractions of its width and height, in page pixels. */
async function at(fx, fy) {
  const canvas = page.locator('#screens canvas').first();
  const box = await canvas.boundingBox();
  const { w, h } = await canvas.evaluate((c) => ({ w: c.width, h: c.height }));
  const s = Math.min(box.width / w, box.height / h);
  return { x: box.x + (box.width - w * s) / 2 + fx * w * s, y: box.y + (box.height - h * s) / 2 + fy * h * s };
}

async function clickAt(fx, fy) {
  const p = await at(fx, fy);
  await page.mouse.click(p.x, p.y);
}

async function open() {
  await page.goto('about:blank');
  await page.goto(`http://localhost:${PORT}/#t=${token}`);
  await expect.poll(() => page.locator('#screens canvas').first().evaluate((c) => c.width).catch(() => 0), { timeout: 30_000 }).toBeGreaterThan(0);
  await page.waitForTimeout(1500);
}

const hostClipboard = () => desk('Get-Clipboard -Raw');
const setDeviceClipboard = (text) => page.evaluate((t) => navigator.clipboard.writeText(t), text);
const deviceClipboard = () => page.evaluate(() => navigator.clipboard.readText());

/** A fresh Notepad, maximized and in front, with the keyboard in it. */
async function notepad() {
  const file = `${WORK}\\vm-${Date.now()}.txt`;
  // Its saved tabs go too, or it reopens them (and asks about unsaved changes).
  // Start or Search left open by a failed test (they come back, closed) goes as well.
  desk(`Get-Process notepad, SearchHost, StartMenuExperienceHost -EA 0 | Stop-Process -Force; Start-Sleep -Milliseconds 300
Remove-Item "$env:LOCALAPPDATA\\Packages\\Microsoft.WindowsNotepad_8wekyb3d8bbwe\\LocalState\\TabState\\*" -EA 0
Set-Content '${file}' '' -NoNewline; Start-Process notepad -ArgumentList '${file}' -WindowStyle Maximized`);
  await page.waitForTimeout(2500);
  // Whatever a failed test left open (Start, a menu) closes first.
  await page.keyboard.press('Escape');
  await clickAt(0.5, 0.5);
  await page.waitForTimeout(500);
}

test.beforeAll(async ({ browser }) => {
  token = startHost();
  closeTunnel = await tunnel(PORT);
  const context = await browser.newContext();
  page = await context.newPage();
  page.on('dialog', (d) => d.accept());
  await open();
});

test.afterAll(async () => {
  await page?.context().close();
  closeTunnel?.();
});

test('streams the desktop', async () => {
  expect(hostLog()).toMatch(/capturing \d+x\d+/);
});

test('typing reaches the app in front, Shift and symbols included', async () => {
  await notepad();
  for (const key of ['Shift+KeyH', 'KeyI', 'Space', 'Shift+Backslash', 'Space', 'Shift+Digit1', 'Digit2', 'Shift+Digit3']) {
    await page.keyboard.press(key);
  }
  await page.keyboard.press('Control+KeyA');
  await page.keyboard.press('Control+KeyC');
  await expect.poll(hostClipboard, { timeout: 10_000 }).toBe('Hi | !2#');
});

test('copying on the computer puts it on this device', async () => {
  // The Ctrl+C above went to the computer; its clipboard came back here.
  await expect.poll(deviceClipboard, { timeout: 10_000 }).toBe('Hi | !2#');
});

test('pasting from this device (⌘V) types it there, and Start stays closed', async () => {
  await setDeviceClipboard('pasted from the viewer ✓');
  await page.keyboard.press('Control+KeyA');
  // A paste travels apart from the keys and can overtake them; people don't press ⌘V
  // within milliseconds of the key before it.
  await page.waitForTimeout(300);
  await page.keyboard.press('Meta+KeyV');
  await page.waitForTimeout(1500);
  const front = desk(`Add-Type -Name W -Namespace U -MemberDefinition '[DllImport("user32.dll")] public static extern System.IntPtr GetForegroundWindow(); [DllImport("user32.dll")] public static extern int GetWindowThreadProcessId(System.IntPtr h, out int p);'; $p = 0; [void][U.W]::GetWindowThreadProcessId([U.W]::GetForegroundWindow(), [ref]$p); (Get-Process -Id $p).Name`);
  expect(front.trim()).toBe('Notepad');
  await page.keyboard.press('Control+KeyA');
  await page.keyboard.press('Control+KeyC');
  await expect.poll(hostClipboard, { timeout: 10_000 }).toBe('pasted from the viewer ✓');
});

test("another viewer leaving doesn't let go of this one's keys", async ({ browser }) => {
  await notepad();
  await page.keyboard.down('Shift');
  const count = (re) => (hostLog().match(re) ?? []).length;
  const [opened, closed] = [count(/input stream open/g), count(/client disconnected/g)];
  const other = await (await browser.newContext()).newPage();
  await other.goto(`http://localhost:${PORT}/#t=${token}`);
  await expect.poll(() => count(/input stream open/g), { timeout: 20_000 }).toBeGreaterThan(opened);
  await other.context().close();
  await expect.poll(() => count(/client disconnected/g), { timeout: 30_000 }).toBeGreaterThan(closed);
  await page.keyboard.press('Backslash');
  await page.keyboard.up('Shift');
  await page.keyboard.press('Control+KeyA');
  await page.keyboard.press('Control+KeyC');
  await expect.poll(hostClipboard, { timeout: 10_000 }).toBe('|');
});

test('images go both ways on the clipboard', async () => {
  // This device → the computer.
  await page.evaluate(async () => {
    const c = Object.assign(document.createElement('canvas'), { width: 80, height: 60 });
    const g = c.getContext('2d');
    g.fillStyle = '#22aa77';
    g.fillRect(0, 0, 80, 60);
    const png = await new Promise((r) => c.toBlob(r, 'image/png'));
    await navigator.clipboard.write([new ClipboardItem({ 'image/png': png })]);
  });
  await page.keyboard.press('End');
  await page.keyboard.press('Meta+KeyV');
  const image = () => desk('Add-Type -AssemblyName System.Windows.Forms; $i = [System.Windows.Forms.Clipboard]::GetImage(); if ($i) { "$($i.Width)x$($i.Height) " + $i.GetPixel(5, 5).Name } else { "none" }');
  await expect.poll(image, { timeout: 15_000 }).toBe('80x60 ff22aa77');

  // The computer → this device: a new image there, then Ctrl+C (nothing selected, so the
  // clipboard stays the image) brings it here.
  await setDeviceClipboard('reset');
  await page.waitForTimeout(1500);
  desk('Add-Type -AssemblyName System.Windows.Forms, System.Drawing; $b = New-Object System.Drawing.Bitmap 64, 48; $g = [System.Drawing.Graphics]::FromImage($b); $g.Clear([System.Drawing.Color]::OrangeRed); $g.Dispose(); [System.Windows.Forms.Clipboard]::SetImage($b)');
  await page.waitForTimeout(1500);
  await page.keyboard.press('Control+KeyC');
  const types = () =>
    page.evaluate(async () => {
      const out = [];
      for (const item of await navigator.clipboard.read()) out.push(...item.types);
      return out.join(',');
    });
  await expect.poll(types, { timeout: 15_000 }).toContain('image/png');
});

test('a file sent from here lands in Downloads', async () => {
  const name = `vm-upload-${Date.now()}.txt`;
  writeFileSync(join(files, name), 'hello from the viewer');
  const chooser = page.waitForEvent('filechooser');
  await page.locator('#btn-upload').click();
  await (await chooser).setFiles(join(files, name));
  await expect.poll(() => ps(`Get-Content "$env:USERPROFILE\\Downloads\\${name}" -EA 0`), { timeout: 20_000 }).toBe('hello from the viewer');
});

test('the file browser shows what Explorer shows, previews pictures and uses the Recycle Bin', async () => {
  const picture = join(files, 'vm-preview.png');
  writeFileSync(picture, png(160, 120, [40, 120, 220]));
  put(picture, 'C:\\Users\\glass\\Pictures\\vm-preview.png');
  await page.locator('#btn-files').click();
  const sheet = page.locator('#files');
  await expect(sheet.getByText('Documents', { exact: true })).toBeVisible();
  for (const hidden of ['AppData', 'NetHood', 'Application Data', 'desktop.ini']) {
    await expect(sheet.getByText(hidden, { exact: true })).toHaveCount(0);
  }
  await sheet.getByText('Pictures', { exact: true }).first().dblclick();
  await sheet.locator('button[title="Grid view"]').click();
  await expect(sheet.locator('img.thumb')).toHaveCount(1, { timeout: 15_000 });
  await sheet.getByText('vm-preview.png', { exact: true }).click();
  await sheet.getByRole('button', { name: 'Move to Recycle Bin' }).click();
  await expect
    .poll(() => desk('(New-Object -ComObject Shell.Application).Namespace(10).Items() | ForEach-Object Name'), { timeout: 15_000 })
    .toContain('vm-preview');
  await sheet.locator('button[title="Close"]').click();
});

test('chat shows as a notification from Glass', async () => {
  const text = `vm chat ${Date.now()}`;
  await page.locator('button[title="Chat"]').first().click();
  await page.getByPlaceholder('Message').fill(text);
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  const toasts = () =>
    desk(`$m = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]; foreach ($n in $m::History.GetHistory('Glassdesk.Glass')) { ($n.Content.GetElementsByTagName('text') | ForEach-Object { $_.InnerText }) -join ' / ' }`);
  await expect.poll(toasts, { timeout: 15_000 }).toContain(text);
  await page.keyboard.press('Escape');
});

test('one window can be streamed on its own', async () => {
  await page.locator('#btn-displays').click();
  await page.locator('.panel:not(.hidden) button', { hasText: 'Notepad' }).first().click();
  await expect.poll(() => hostLog(), { timeout: 15_000 }).toMatch(/layout Window\(/);
  await page.locator('#btn-displays').click();
  await page.locator('.panel:not(.hidden) button', { hasText: 'Display' }).first().click();
  await page.waitForTimeout(1500);
});

test('recording saves an MP4 on the computer', async () => {
  await page.locator('#btn-record').click();
  await page.waitForTimeout(1000);
  await clickAt(0.5, 0.5);
  await page.waitForTimeout(3000);
  await page.locator('#btn-record').click();
  await expect(page.getByText(/Recording saved on .*\.mp4/)).toBeVisible({ timeout: 15_000 });
  const size = ps(`(Get-ChildItem "$env:USERPROFILE\\Videos\\Glass\\*.mp4" | Sort-Object LastWriteTime | Select-Object -Last 1).Length`);
  expect(Number(size)).toBeGreaterThan(1000);
});

test('turning sound on works, or says why there is none', async () => {
  await page.locator('#btn-audio').click();
  if (/no sound output/.test(hostLog())) {
    await expect(page.getByText('this computer has no sound output')).toBeVisible({ timeout: 10_000 });
  } else {
    await expect.poll(() => hostLog(), { timeout: 10_000 }).toMatch(/audio on/);
  }
  await page.locator('#btn-audio').click();
});

test('a one-time link lets a guest in', async ({ browser }) => {
  const link = ps(`& "$env:LOCALAPPDATA\\Programs\\Glass\\bin\\glass.exe" link`).match(/#l=[0-9a-f]+/)?.[0];
  expect(link).toBeTruthy();
  const guest = await (await browser.newContext()).newPage();
  await guest.goto(`http://localhost:${PORT}/${link}`);
  await expect.poll(() => hostLog(), { timeout: 20_000 }).toMatch(/authenticated via magic link/);
  await guest.context().close();
});

test('Ctrl+Alt+Del opens the secure screen (with the secure-desktop service)', async () => {
  test.skip(!/RUNNING/.test(ps('sc.exe query GlassSecureDesktop')), 'the secure-desktop service is not installed');
  await page.locator('#btn-more').click();
  await page.locator('[role=menuitem]', { hasText: 'Send Ctrl+Alt+Del' }).click();
  await expect.poll(() => hostLog(), { timeout: 15_000 }).toMatch(/secure desktop up/);
  await page.waitForTimeout(2000);
  await page.keyboard.press('Escape');
  await page.waitForTimeout(2000);
});

test('an agent can type any text, through glass mcp', async () => {
  await notepad();
  // Agents wait while a person is using the computer: give them the keyboard.
  await page.waitForTimeout(4000);
  // Not real words: Notepad's autocorrect would "fix" them (Ünïcödé became Unicode).
  const text = 'agent typed: Ü1ï2ö3é ✓ 日本 #42!';
  const calls = [
    { name: 'screenshot', arguments: {} },
    { name: 'type_text', arguments: { text } },
  ];
  const out = desk(
    `$p = New-Object System.Diagnostics.Process
$p.StartInfo.FileName = "$env:LOCALAPPDATA\\Programs\\Glass\\bin\\glass.exe"
$p.StartInfo.Arguments = 'mcp'
$p.StartInfo.UseShellExecute = $false
$p.StartInfo.RedirectStandardInput = $true
$p.StartInfo.RedirectStandardOutput = $true
$p.StartInfo.StandardOutputEncoding = [Text.Encoding]::UTF8
[void]$p.Start()
$in = New-Object System.IO.StreamWriter($p.StandardInput.BaseStream, (New-Object System.Text.UTF8Encoding $false))
function Ask($m) { $in.WriteLine($m); $in.Flush(); $line = $p.StandardOutput.ReadLine(); if ($line.Length -gt 300) { $line.Substring(0, 120) + '...' } else { $line } }
Ask '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"vm-test","version":"1"}}}'
$in.WriteLine('{"jsonrpc":"2.0","method":"notifications/initialized"}'); $in.Flush()
${calls.map((c, i) => `Ask '${JSON.stringify({ jsonrpc: '2.0', id: 10 + i, method: 'tools/call', params: c })}'`).join('\n')}
$p.Kill()`,
    { timeout: 90 },
  );
  expect(out).toContain('"id":11');
  await page.keyboard.press('Control+KeyA');
  await page.keyboard.press('Control+KeyC');
  await expect.poll(hostClipboard, { timeout: 10_000 }).toBe(text);
});

test('policies turn features off', async () => {
  ps(`New-Item HKLM:\\SOFTWARE\\Policies\\Glass -Force | Out-Null; New-ItemProperty HKLM:\\SOFTWARE\\Policies\\Glass -Name FileTransfer -Value 0 -PropertyType DWord -Force | Out-Null`);
  try {
    token = startHost();
    await open();
    await expect(page.locator('#btn-more')).toBeVisible();
    await expect(page.locator('#btn-upload')).toBeHidden();
    await expect(page.locator('#btn-files')).toBeHidden();
  } finally {
    ps('Remove-Item HKLM:\\SOFTWARE\\Policies\\Glass -Recurse -Force -EA 0');
    token = startHost();
  }
});
