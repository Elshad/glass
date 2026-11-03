// Drives the Windows test computer over SSH (see windows-vm/README.md): PowerShell in the
// SSH session (session 0: no desktop, but administrator), or in the signed-in desktop
// session through a one-off interactive scheduled task, which is where the clipboard,
// windows and notifications are.

import { execFileSync, spawn } from 'node:child_process';
import { connect } from 'node:net';
import { homedir } from 'node:os';
import { join } from 'node:path';

const SSH_CONFIG = process.env.GLASS_VM_SSH_CONFIG ?? join(homedir(), 'VMs/win11/ssh/config');
const HOST = process.env.GLASS_VM_HOST ?? 'glass-win';
/** The VM's address as the browser reaches it, for WebTransport (UDP isn't tunnelled). */
export const VM_IP = process.env.GLASS_VM_IP ?? '192.168.64.2';
const USER = process.env.GLASS_VM_USER ?? 'glass';
/** Where the tests keep their files on the VM. */
export const WORK = 'C:\\glass';

const encode = (script) => Buffer.from(script, 'utf16le').toString('base64');
const quiet = (s) => s.split('\n').filter((l) => !/post-quantum|store now|may need to be upgraded/.test(l)).join('\n');

/** Runs PowerShell over SSH and returns its output. */
export function ps(script, { timeout = 120_000 } = {}) {
  try {
    // No progress records: they come back as CLIXML mixed into the output.
    const quietScript = `$ProgressPreference = 'SilentlyContinue'\n[Console]::OutputEncoding = [Text.Encoding]::UTF8\n${script}`;
    return execFileSync('ssh', ['-F', SSH_CONFIG, HOST, `powershell -NoProfile -NonInteractive -EncodedCommand ${encode(quietScript)}`], {
      encoding: 'utf8',
      timeout,
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
  } catch (e) {
    throw new Error(`VM command failed: ${quiet(String(e.stderr ?? e.message)).trim()}`);
  }
}

export function put(local, remote) {
  execFileSync('scp', ['-q', '-F', SSH_CONFIG, local, `${HOST}:/${remote.replaceAll('\\', '/')}`]);
}

export function get(remote, local) {
  execFileSync('scp', ['-q', '-F', SSH_CONFIG, `${HOST}:/${remote.replaceAll('\\', '/')}`, local]);
}

/** Runs PowerShell in the signed-in desktop session (single-threaded, for the clipboard). */
export function desk(script, { timeout = 60 } = {}) {
  return ps(
    `$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force '${WORK}' | Out-Null
Remove-Item '${WORK}\\desk.out', '${WORK}\\desk.done' -EA 0
Set-Content '${WORK}\\desk.cmd' -Encoding ascii -Value '@echo off', 'powershell -NoProfile -Sta -ExecutionPolicy Bypass -EncodedCommand ${encode(`$ProgressPreference = 'SilentlyContinue'\n[Console]::OutputEncoding = [Text.Encoding]::UTF8\n${script}`)} > ${WORK}\\desk.out 2>&1', 'echo %ERRORLEVEL% > ${WORK}\\desk.done'
# Hidden, so no console window takes the keyboard from the app a test is typing into.
Set-Content '${WORK}\\desk.vbs' -Encoding ascii -Value 'CreateObject("WScript.Shell").Run "cmd /c ${WORK}\\desk.cmd", 0, True'
schtasks /Create /TN glassdesk /TR 'wscript.exe ${WORK}\\desk.vbs' /SC ONCE /ST 23:59 /RU ${USER} /IT /F | Out-Null
schtasks /Run /TN glassdesk | Out-Null
$t = 0; while (-not (Test-Path '${WORK}\\desk.done') -and $t -lt ${timeout * 2}) { Start-Sleep -Milliseconds 500; $t++ }
Get-Content '${WORK}\\desk.out' -Encoding UTF8 -EA 0`,
    { timeout: (timeout + 30) * 1000 },
  );
}

/** Starts an app in the desktop session without waiting for it. */
export function launch(exe) {
  desk(`Start-Process '${exe}'`);
}

const BIN = '%LOCALAPPDATA%\\Programs\\Glass\\bin\\glass.exe';

/**
 * (Re)starts the per-user host in the desktop session with WebTransport pointed at the VM's
 * address, and returns the access token from its log. `exe` defaults to the per-user install.
 */
export function startHost(exe = BIN) {
  const out = ps(`
Set-NetFirewallProfile -Profile Public -Enabled False
New-Item -ItemType Directory -Force '${WORK}\\logs' | Out-Null
schtasks /End /TN glasshost 2>$null | Out-Null
Get-Process glass, glassw -EA 0 | Where-Object { $_.Path -notlike '*\\Glass\\Service\\*' } | Stop-Process -Force
Start-Sleep 1
Remove-Item '${WORK}\\logs\\stdout.log' -EA 0
Set-Content '${WORK}\\host.cmd' -Encoding ascii -Value '@echo off', 'set RUST_LOG=${process.env.GLASS_VM_RUST_LOG ?? 'info'}', '"${exe}" --wt-host ${VM_IP} > ${WORK}\\logs\\stdout.log 2>&1'
# Hidden: no console on the desktop for a test's keystrokes (Ctrl+C) to land in.
Set-Content '${WORK}\\host.vbs' -Encoding ascii -Value 'CreateObject("WScript.Shell").Run "cmd /c ${WORK}\\host.cmd", 0, False'
schtasks /Create /TN glasshost /TR 'wscript.exe ${WORK}\\host.vbs' /SC ONCE /ST 23:59 /RU ${USER} /IT /F | Out-Null
schtasks /Run /TN glasshost | Out-Null
$t = 0
while ($t -lt 60) {
  Start-Sleep -Milliseconds 500; $t++
  $line = Select-String -Path '${WORK}\\logs\\stdout.log' -Pattern '#t=([0-9a-f]+)' -EA 0 | Select-Object -Last 1
  if ($line) { $line.Matches[0].Groups[1].Value; break }
}`);
  const token = out.split('\n').pop().trim();
  if (!/^[0-9a-f]{16,}$/.test(token)) throw new Error(`the host didn't start: ${out}`);
  return token;
}

/** The host's log so far (what it printed since it started). */
export function hostLog() {
  return ps(`Get-Content '${WORK}\\logs\\stdout.log' -EA 0`);
}

/** Forwards a local port to the host's web server (8080) on the VM, once it's up; resolves
 * to a stop function. */
export async function tunnel(localPort) {
  const child = spawn('ssh', ['-F', SSH_CONFIG, '-N', '-L', `${localPort}:127.0.0.1:8080`, HOST], { stdio: 'ignore' });
  const open = () =>
    new Promise((resolve) => {
      const socket = connect(localPort, '127.0.0.1', () => {
        socket.end();
        resolve(true);
      });
      socket.on('error', () => resolve(false));
    });
  for (let i = 0; i < 50 && !(await open()); i++) await new Promise((r) => setTimeout(r, 200));
  return () => child.kill();
}
