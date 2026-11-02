# Windows basics, on a real Windows computer

`basics.vm.mjs` checks the Windows host end to end: a browser viewer (Playwright) driving
a real Windows desktop. It covers typing, copy and paste both ways (text and images),
files and the file browser, notifications, streaming one window, recording, sound,
one-time links, Ctrl+Alt+Del and policies. The other e2e specs use a `--test` host with a
fake desktop; these need the real thing, so they aren't part of the normal run or CI.

```sh
cd tests/e2e
npx playwright test -c windows-vm.config.mjs
```

## The computer

A Windows 10/11 VM (or PC) the tests can reach over SSH:

- OpenSSH Server on, key login, for a user who is an administrator and signed in at the
  desktop (auto sign-in is easiest). Its SSH shell must be PowerShell.
- Glass installed for that user (`irm glassdesk.dev/install.ps1 | iex`, or a build copied
  to `%LOCALAPPDATA%\Programs\Glass`).
- Optional: `glass service install` for the Ctrl+Alt+Del test (skipped without it).

Settings (environment variables, with defaults):

| Variable | Default | |
|---|---|---|
| `GLASS_VM_HOST` | `glass-win` | SSH host (alias in the config below) |
| `GLASS_VM_SSH_CONFIG` | `~/VMs/win11/ssh/config` | SSH config file |
| `GLASS_VM_IP` | `192.168.64.2` | The computer's address, for WebTransport (UDP isn't tunnelled) |
| `GLASS_VM_USER` | `glass` | The signed-in user |
| `GLASS_VM_PORT` | `18090` | Local port for the tunnel to its web server |
| `GLASS_VM_RUST_LOG` | `info` | The host's log level, e.g. `info,glass_host::platform::input=trace` |

The tests start the host themselves (hidden, in the desktop session, with `--wt-host`)
and tunnel its web server to localhost, so the page is a secure context. They turn off
the Windows Firewall's public profile on the VM so WebTransport gets through.

## Restarting (`restart.vm.mjs`)

Restarts Glass, then the computer, from the viewer, and checks the same page comes back
each time: through the sign-in screen (served by the sign-in host) to the desktop. It
restarts the VM, so it only runs when asked: `GLASS_VM_REBOOT=1 npx playwright test -c windows-vm.config.mjs restart`.
It needs the secure-desktop service, automatic sign-in off (Winlogon `AutoAdminLogon` 0),
and `GLASS_WT_HOST` set machine-wide to the VM's address, since Windows starts the hosts.
It types the password from `~/VMs/win11/credentials.txt` (`password: …`).

## Notes

- Commands that need the desktop (the clipboard, windows, notifications) run there through
  a one-off scheduled task (`desk()` in `vm.mjs`), hidden so no console takes the keyboard.
- The tests type into Notepad, started fresh and maximized for each one that needs it.
- The test VM may have no sound device: the sound test then checks the viewer says so.
- Host log: `C:\glass\logs\stdout.log` on the VM.
