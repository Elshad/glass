---
path: /guides/wayland-remote-desktop
title: Remote desktop on Wayland: GNOME, KDE, Hyprland and wlroots
label: Wayland remote desktop
description: Which remote desktops work on Linux with Wayland: GNOME Remote Desktop, KRdp, wayvnc, RustDesk and Glass, what each supports, and how to set one up.
order: 5
---

# Remote desktop on Wayland

**Short answer:** on Wayland, a remote desktop has to capture the screen through the desktop's ScreenCast portal and PipeWire, so what works depends on your desktop. GNOME has **GNOME Remote Desktop** (RDP) built in. KDE Plasma has **KRdp** (RDP). Sway and other wlroots compositors have **wayvnc** (VNC). **Glass** works on any of them, Hyprland included, through the portal, and you view it from a browser instead of an RDP or VNC client.

Wayland removed the trick X11 tools relied on (any app reading the whole screen and injecting input), which is why older VNC servers and many remote desktop apps don't work, or work only on the login screen's X11 fallback.

## The options

| | Desktops | Protocol / viewer | Login screen | License |
|---|---|---|---|---|
| GNOME Remote Desktop | GNOME | RDP client (Windows' Remote Desktop, Remmina, …) | Yes, with Remote Login (GNOME 46+) | GPL-2.0+ |
| KRdp | KDE Plasma 6.1+ | RDP client that supports H.264 | No: needs a running session or autologin | Open source (KDE) |
| wayvnc | wlroots compositors (Sway, …); not GNOME, KDE or Weston | Any VNC client | Headless sessions supported | ISC |
| RustDesk | Wayland is experimental | RustDesk app | Not on Wayland; switch GDM to X11 | AGPL-3.0 |
| Glass (beta on Linux) | Any compositor with the ScreenCast portal: Hyprland, GNOME, KDE, wlroots | Any browser | No: needs a running session | MIT |

Sources: [GNOME Remote Desktop](https://help.gnome.org/gnome-help/sharing-desktop.html) and [Remote Login](https://help.gnome.org/gnome-help/remote-login.html), [KRdp's README](https://invent.kde.org/plasma/krdp), [wayvnc](https://github.com/any1/wayvnc), [RustDesk's Linux notes](https://rustdesk.com/docs/en/client/linux/), checked October 2026.

## GNOME: GNOME Remote Desktop

Built in to GNOME. **Settings › System › Remote Desktop** offers two modes:
- **Desktop Sharing** shares the session you're logged in to, over RDP.
- **Remote Login** (GNOME 46 and later) connects to the computer when nobody is using it, through the login screen.

It encodes H.264 with NVENC on NVIDIA GPUs. Use any RDP client to connect.

## KDE Plasma: KRdp

KDE's RDP server for a Plasma Wayland session, with a settings page since Plasma 6.1. Its README lists its limits:
- video and input only
- one username and password
- clients that support H.264 only
- hardware encoding through VA-API only (NVIDIA falls back to software)
- no RDP at the SDDM login screen, so you need an existing session or autologin

## Sway and wlroots: wayvnc

A VNC server for wlroots-based compositors, including headless ones. Its README says GNOME, KDE and Weston aren't supported.

## Hyprland, or any desktop, from a browser: Glass

[Glass](/) captures the screen through xdg-desktop-portal and PipeWire, so it works with any compositor that has a ScreenCast portal: `xdg-desktop-portal-hyprland`, `-gnome`, `-kde` or `-wlr`. You view and control it from a browser on an iPad, phone or another computer:
- keyboard and pointer, through `/dev/uinput`
- system audio, and your microphone as an input
- clipboard, through wl-clipboard
- game controllers, as a virtual Xbox 360 controller

Setup on Arch or Omarchy:

```sh
curl -fsSL glassdesk.dev | sh
```

`glass setup` checks for PipeWire, the portal and wl-clipboard, and offers to install what's missing with pacman, apt, dnf or zypper. It installs a one-line udev rule for input, which needs `sudo` once, and runs `tailscale serve` so the machine has an HTTPS address on your tailnet. On Hyprland the share picker asks once which screen to share, and Glass remembers the answer.

**Glass's limits on Linux (beta):**
- x86_64 and ARM64 (aarch64) only; needs glibc 2.39+ (Ubuntu 24.04+, Fedora 40+, Arch).
- Software H.264 encoding (hardware VA-API is planned), and screens over 2560×1600 are halved to keep the frame rate up.
- It needs a running graphical session, not the login screen.

## Which to choose

- **Windows' Remote Desktop app or another RDP client already in use:** GNOME Remote Desktop on GNOME, KRdp on Plasma.
- **Sway or another wlroots compositor with a VNC client:** wayvnc.
- **Hyprland, or reaching any of these from an iPad or phone with nothing to install:** Glass.
- **Windows hosts as well as Linux:** RustDesk or Moonlight with Sunshine, but expect Wayland to be the rough edge.

## Questions

### Does VNC work on Wayland?

Only through a server written for Wayland: wayvnc on wlroots compositors, or KRFB on KDE through the portal. Classic X11 VNC servers such as x11vnc don't capture a Wayland session.

### How do I remote into Hyprland?

Hyprland uses xdg-desktop-portal-hyprland for screen capture. Glass works with it: run `curl -fsSL glassdesk.dev | sh`, approve the share picker once, and open the computer's address in any browser.

### Can I reach the Linux login screen remotely on Wayland?

With GNOME's Remote Login, yes (GNOME 46 and later). KRdp, wayvnc and Glass need a session that's already running.
