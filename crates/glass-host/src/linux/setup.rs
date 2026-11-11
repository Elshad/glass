//! Linux-specific setup: desktop dependencies, input access (uinput), and approving
//! screen sharing in the compositor's picker. An X11 session needs only xclip: there is
//! no portal, no picker, and input goes through XTEST.

use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use cliclack::{confirm, log, note, spinner};
use console::style;

use super::{has_command, input, is_x11};
use crate::setup::ask_retry;
use crate::status::{self, Status};

const UDEV_RULE: &str = "/etc/udev/rules.d/60-glass-uinput.rules";
const MODULES_LOAD: &str = "/etc/modules-load.d/glass-uinput.conf";

pub async fn prepare() -> Result<()> {
    dependencies_step()?;
    input_access_step()
}

fn dependencies_step() -> Result<()> {
    log::step("Desktop integration")?;
    loop {
        let missing = missing_dependencies();
        if missing.is_empty() {
            if is_x11() {
                log::success("X11 session: PipeWire and xclip found")?;
            } else {
                log::success("PipeWire, screen-sharing portal and wl-clipboard found")?;
            }
            return Ok(());
        }
        let Some(pm) = PackageManager::detect() else {
            log::warning(format!(
                "Missing: {}. Install them with your package manager.",
                missing.join(", ")
            ))?;
            if !ask_retry("Installed them?", "Continue anyway")? {
                return Ok(());
            }
            continue;
        };
        let command = pm.install(&missing, false);
        log::warning(format!("Missing: {}", missing.join(", ")))?;
        if confirm(format!(
            "Install them now? (runs {})",
            style(&command).bold()
        ))
        .initial_value(true)
        .interact()?
        {
            let _ = Command::new("sh").args(["-c", &command]).status();
            continue;
        }
        if !ask_retry("Installed them?", "Continue anyway")? {
            return Ok(());
        }
    }
}

/// The distro's package manager, for install commands that work as written.
#[derive(Clone, Copy)]
enum PackageManager {
    Pacman,
    Apt,
    Dnf,
    Zypper,
}

impl PackageManager {
    fn detect() -> Option<Self> {
        [
            ("pacman", Self::Pacman),
            ("apt-get", Self::Apt),
            ("dnf", Self::Dnf),
            ("zypper", Self::Zypper),
        ]
        .into_iter()
        .find(|(cmd, _)| has_command(cmd))
        .map(|(_, pm)| pm)
    }

    fn install(self, packages: &[&str], unattended: bool) -> String {
        let packages = packages.join(" ");
        let (cmd, yes) = match self {
            Self::Pacman => ("pacman -S --needed", "--noconfirm"),
            Self::Apt => ("apt-get install", "-y"),
            Self::Dnf => ("dnf install", "-y"),
            Self::Zypper => ("zypper install", "-y"),
        };
        let sudo = if unattended { "sudo -n" } else { "sudo" };
        if unattended {
            format!("{sudo} {cmd} {yes} {packages}")
        } else {
            format!("{sudo} {cmd} {packages}")
        }
    }
}

/// The screen-sharing portal backend for this desktop (package names match across distros).
fn portal_package() -> &'static str {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_lowercase();
    if desktop.contains("hyprland") {
        "xdg-desktop-portal-hyprland"
    } else if desktop.contains("gnome") || desktop.contains("unity") {
        "xdg-desktop-portal-gnome"
    } else if desktop.contains("kde") {
        "xdg-desktop-portal-kde"
    } else if desktop.contains("cosmic") {
        "xdg-desktop-portal-cosmic"
    } else {
        // sway, river, niri, Wayfire and other wlroots compositors.
        "xdg-desktop-portal-wlr"
    }
}

fn input_access_step() -> Result<()> {
    log::step("Mouse and keyboard access")?;
    if is_x11() {
        if input::has_accessibility() {
            log::success("Glass can send input through the X server (XTEST)")?;
        } else {
            log::warning(
                "The X server has no XTEST extension: viewing works, but remote mouse and keyboard won't.",
            )?;
        }
        return Ok(());
    }
    if input::has_accessibility() {
        log::success("Glass can create its virtual mouse and keyboard")?;
        return Ok(());
    }
    note(
        "Allow Glass to control the mouse and keyboard",
        format!(
            "Glass injects input through /dev/uinput. A one-line udev rule gives your\n\
             logged-in session access ({UDEV_RULE}). This needs your password once."
        ),
    )?;
    if !confirm("Install the rule now?")
        .initial_value(true)
        .interact()?
    {
        log::warning(
            "Skipped: viewing works, but remote mouse and keyboard won't until this is set up.",
        )?;
        return Ok(());
    }
    let script = format!(
        "echo 'KERNEL==\"uinput\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"' > {UDEV_RULE} && \
         echo uinput > {MODULES_LOAD} && modprobe uinput && \
         udevadm control --reload-rules && udevadm trigger --name-match=uinput"
    );
    let ok = Command::new("sudo")
        .args(["sh", "-c", &script])
        .status()
        .is_ok_and(|s| s.success());
    std::thread::sleep(Duration::from_millis(500));
    if ok && input::has_accessibility() {
        log::success("Input access granted")?;
    } else {
        log::warning(
            "Still no access to /dev/uinput. Log out and back in, then run `glass setup` again.",
        )?;
    }
    Ok(())
}

fn sudo_n(script: &str) -> bool {
    Command::new("sudo")
        .args(["-n", "sh", "-c", script])
        .status()
        .is_ok_and(|s| s.success())
}

fn missing_dependencies() -> Vec<&'static str> {
    if is_x11() {
        return [
            (!has_command("pipewire")).then_some("pipewire"),
            (!has_command("xclip")).then_some("xclip"),
        ]
        .into_iter()
        .flatten()
        .collect();
    }
    let portal = std::path::Path::new("/usr/share/xdg-desktop-portal/portals")
        .read_dir()
        .is_ok_and(|mut d| {
            d.any(|e| {
                e.is_ok_and(|e| {
                    std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains("ScreenCast"))
                })
            })
        });
    [
        (!has_command("pipewire")).then_some("pipewire"),
        (!portal).then_some(portal_package()),
        (!has_command("wl-copy")).then_some("wl-clipboard"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Installs what it can with passwordless sudo; reports the rest.
pub fn prepare_unattended() -> Vec<crate::setup::Need> {
    let mut needs = Vec::new();
    let missing = missing_dependencies();
    if !missing.is_empty() {
        let pm = PackageManager::detect();
        let installed = pm.is_some_and(|pm| {
            Command::new("sh")
                .args(["-c", &pm.install(&missing, true)])
                .status()
                .is_ok_and(|s| s.success())
        });
        if !installed {
            needs.push(crate::setup::Need {
                step: "dependencies",
                action: match pm {
                    Some(pm) => format!("Install: {}", pm.install(&missing, false)),
                    None => format!("Install with the package manager: {}", missing.join(" ")),
                },
                on_screen: false,
            });
        }
    }
    if !input::has_accessibility() && is_x11() {
        needs.push(crate::setup::Need {
            step: "input-access",
            action: "Enable the XTEST extension in the X server (remote input needs it)".into(),
            on_screen: false,
        });
    } else if !input::has_accessibility() {
        let script = format!(
            "echo 'KERNEL==\"uinput\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"' > {UDEV_RULE} && \
             echo uinput > {MODULES_LOAD} && modprobe uinput && \
             udevadm control --reload-rules && udevadm trigger --name-match=uinput"
        );
        if !(sudo_n(&script) && input::has_accessibility()) {
            needs.push(crate::setup::Need {
                step: "input-access",
                action: "Grant /dev/uinput access: run `glass setup` on the machine (asks for sudo once)".into(),
                on_screen: false,
            });
        }
    }
    needs
}

pub const TAILSCALE_INSTALL: &str = "Install Tailscale: curl -fsSL https://tailscale.com/install.sh | sh\n\
     then sign in with: sudo tailscale up. Install it on your other devices too.";
pub const TAILSCALE_START: &str = "Tailscale is installed but its service isn't running. Start it with:\n  sudo systemctl enable --now tailscaled";
pub const TAILSCALE_SIGN_IN: &str =
    "Tailscale isn't signed in. Run: sudo tailscale up (it prints a link to log in).";

pub fn open_url(url: &str) {
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Where to allow screen capture, for messages.
pub fn screen_settings() -> String {
    if is_x11() {
        return "the X session (the service needs DISPLAY and XAUTHORITY; run `glass setup` there)".into();
    }
    "the desktop's screen-share picker (run `glass setup` on the machine)".into()
}

/// Where to allow remote input, for messages.
pub fn input_settings() -> String {
    if is_x11() {
        return "the X server's configuration (the XTEST extension is missing)".into();
    }
    "`glass setup` (grants /dev/uinput access, then log out and back in)".into()
}

pub fn try_tailscale_operator() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    !user.is_empty() && sudo_n(&format!("tailscale set --operator={user}"))
}

pub fn permission_needs(status: &Status) -> Vec<crate::setup::Need> {
    let mut needs = Vec::new();
    if !status.capturing && is_x11() {
        needs.push(crate::setup::Need {
            step: "screen-share",
            action: "The service can't reach the X server: run `glass setup` inside the desktop session".into(),
            on_screen: false,
        });
    } else if !status.capturing {
        needs.push(crate::setup::Need {
            step: "screen-share",
            action: "Approve screen sharing in the desktop's share picker (shown on the machine's screen)".into(),
            on_screen: true,
        });
    }
    if !status.accessibility && is_x11() {
        needs.push(crate::setup::Need {
            step: "input-access",
            action: "Enable the XTEST extension in the X server (remote input needs it)".into(),
            on_screen: false,
        });
    } else if !status.accessibility {
        needs.push(crate::setup::Need {
            step: "input-access",
            action: "Log out and back in so the uinput rule applies, or run `glass setup` there"
                .into(),
            on_screen: false,
        });
    }
    needs
}

pub fn permissions(status: Status) -> Result<()> {
    log::step("Screen sharing")?;
    if status.capturing {
        log::success("Already sharing")?;
    } else if is_x11() {
        // Nothing to approve on X11: capture starts as soon as the service is up.
        let sp = spinner();
        sp.start("Waiting for screen capture to start");
        let started = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(500));
            status::read().is_some_and(|s| s.is_fresh() && s.capturing)
        });
        if started {
            sp.stop("Capturing the X11 screen");
        } else {
            sp.stop("Screen capture hasn't started");
            log::warning(format!("The service isn't capturing yet.{}", super::agent::LOG_HINT))?;
        }
    } else {
        note(
            "Choose what to share",
            "Your desktop will ask which screen Glass may share. Pick the screen(s)\n\
             you want to control. Glass remembers the choice, so this is a one-time step.",
        )?;
        let sp = spinner();
        sp.start("Waiting for screen sharing to be approved");
        loop {
            if status::read().is_some_and(|s| s.is_fresh() && s.capturing) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        sp.stop("Screen sharing approved");
    }
    if !is_x11() && !status::read().is_some_and(|s| s.accessibility) {
        log::warning(
            "The service can't open /dev/uinput yet, so remote input is off. Re-run `glass setup` after logging out and in.",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_commands_match_the_distro() {
        let pkgs = ["pipewire", "wl-clipboard"];
        assert_eq!(
            PackageManager::Pacman.install(&pkgs, false),
            "sudo pacman -S --needed pipewire wl-clipboard"
        );
        assert_eq!(
            PackageManager::Apt.install(&pkgs, true),
            "sudo -n apt-get install -y pipewire wl-clipboard"
        );
        assert_eq!(
            PackageManager::Dnf.install(&pkgs, false),
            "sudo dnf install pipewire wl-clipboard"
        );
    }
}
