# Host features by platform

What each host OS can do, to track what's done and what's left. Viewer-side features
(the web client, PWA, keyboard profiles, push) work the same for every host and aren't listed.

✅ works · 🧪 built, not yet run on real hardware · 🚧 being built · ❌ not yet · ? unverified · — doesn't apply

Windows is in a stack of PRs (#75 onwards); a ✅ there means it ran in a Windows 11 Arm VM (UTM, no GPU, Glass as x64 under emulation), driven from a browser.

## Picture

| Feature | macOS | Linux | Windows | Notes |
|---|---|---|---|---|
| Capture every display | ✅ ScreenCaptureKit | ✅ portal + PipeWire | ✅ DXGI Desktop Duplication | |
| Pointer in the picture | ✅ | ✅ | ✅ drawn on the CPU | |
| Hardware H.264 | ✅ VideoToolbox | ✅ VA-API, opt-in | 🧪 Media Foundation | Windows falls back to OpenH264 |
| Software H.264 (OpenH264) | — | ✅ | ✅ | |
| Up to 120 fps | ✅ | ❌ 60 | 🧪 with hardware encode | |
| HEVC, sharp text 4:2:2 | ✅ | ❌ | ❌ | |
| Lossless 4:4:4 still picture | ✅ | ✅ | 🧪 | |
| Display P3 | ✅ | — | ❌ | |
| Display previews, agent screenshots | ✅ | ✅ | 🧪 | |
| Rotated displays | ✅ | ✅ (Hyprland) | 🧪 | Windows sends them unrotated today |
| Displays plugged in later | ✅ | ❌ restart | 🧪 | |
| Stream one window | ✅ | ❌ | 🧪 | |
| Device as an extra display | ✅ virtual display | ❌ | ❌ | Windows needs an IddCx driver |
| Reshape the desktop to the viewer | ✅ | ❌ | ❌ | |

## Control

| Feature | macOS | Linux | Windows | Notes |
|---|---|---|---|---|
| Mouse, keyboard, scroll | ✅ CGEvent | ✅ uinput | ✅ SendInput | Windows: keys by scan code |
| Relative mouse (games) | ✅ | ✅ | 🧪 | |
| Type any text (agents) | ✅ | ❌ | 🧪 Unicode key events | |
| Pen pressure and tilt | ✅ | ❌ | 🧪 synthetic pen | |
| Game controllers | ❌ | ✅ uinput | ❌ | Windows needs ViGEmBus |
| Unlock from the lock screen | ✅ | ? | ❌ | Windows needs a SYSTEM service |
| Control elevated windows, UAC prompts | — | — | ❌ | Same: SYSTEM service |
| Wake on LAN, restart Glass/computer | ✅ | ✅ | 🧪 | |
| Stay awake with the lid closed | ✅ | ✅ | ❌ | |

## Sound, clipboard, files

| Feature | macOS | Linux | Windows | Notes |
|---|---|---|---|---|
| System audio | ✅ | ✅ PipeWire | 🧪 WASAPI loopback | |
| Viewer's microphone | ✅ Glass Microphone driver | ✅ virtual source | 🧪 | Windows: through VB-CABLE if installed |
| Now playing + media controls | ✅ | ✅ playerctl | 🧪 media session API | |
| Clipboard text | ✅ | ✅ | ✅ | |
| Clipboard rich text (HTML) | ✅ | ❌ | ✅ CF_HTML | |
| Clipboard images | ✅ | ✅ | 🧪 PNG + bitmap | |
| File transfer, folders | ✅ | ✅ | 🧪 | |
| File browser | ✅ | ✅ | 🧪 | |
| Move to Trash | ✅ | ✅ gio | 🧪 Recycle Bin | |
| File previews | ✅ Quick Look | ❌ | ❌ | |
| Pick files on the host | ✅ | ✅ portal | 🧪 Open dialog | |

## People and agents

| Feature | macOS | Linux | Windows | Notes |
|---|---|---|---|---|
| Chat, notifications, prompts | ✅ | ✅ | 🧪 toasts, message boxes | |
| "Someone is viewing" notice | ✅ app | ✅ | 🧪 | |
| Drawings shown on the host | ✅ overlay | ❌ | ❌ | Windows could reuse the privacy overlay trick |
| Privacy screen | ✅ | ❌ | ✅ capture-excluded overlay | |
| Quick support (`glass support`) | ✅ | ✅ | 🧪 | Windows has no `help.ps1` one-liner yet |
| Terminal in the viewer | ✅ | ✅ | ✅ ConPTY | |
| Agent screenshots, clicks, keys | ✅ | ✅ | ✅ | |
| Agent `snapshot` (window as text) | ✅ Accessibility | ❌ | ✅ UI Automation | |
| Agents wait while you use it | ✅ | ❌ | 🧪 | |
| Agents kept out of password fields | ✅ | ✅ lock screen only | 🧪 lock screen only | |
| Front-app rules for agents | ✅ | ❌ | 🧪 | |

## Running it

| Feature | macOS | Linux | Windows | Notes |
|---|---|---|---|---|
| One-line install | ✅ install.sh | ✅ install.sh | ✅ install.ps1 | `irm glassdesk.dev \| iex` |
| Release builds | ✅ universal | ✅ x86_64 | 🧪 x64 + Arm64 zips | |
| Background service | ✅ LaunchAgent | ✅ systemd user | ✅ glassw at sign-in | |
| Self-update | ✅ | ✅ | ✅ | |
| Guided setup | ✅ | ✅ | ✅ (unattended) | Windows adds a firewall rule (one UAC prompt) |
| Desktop app | ✅ Glass.app | ❌ | ❌ | |
| CLI ↔ host channel | ✅ Unix socket | ✅ Unix socket | ✅ named pipe | |
| Fleet reports | ✅ | ✅ | 🧪 | |
| Fleet scripts | ✅ | ✅ | 🧪 .ps1/.cmd/.bat/.exe | |
| Headless (no monitor) | — | 🚧 | ❌ | |
| Recording to MP4, port forwarding, links, SSO/LDAP/SCIM, posture, relay, console | ✅ | ✅ | 🧪 | Platform-independent code |
