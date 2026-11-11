//! Linux. On Wayland: xdg-desktop-portal + PipeWire capture, uinput input and wl-clipboard,
//! built and tested against Hyprland (Omarchy). On X11 (`is_x11`): MIT-SHM capture of the
//! root window, XTEST input and xclip, built and tested against XFCE. Both: VA-API or
//! OpenH264 encode, PipeWire audio, and a systemd user service.

pub mod agent;
pub mod audio;
pub mod clipboard;
pub mod clock;
pub mod input;
pub mod keymap;
pub mod permissions;
pub mod portal;
pub mod privacy;
pub mod session;
pub mod setup;
#[cfg(feature = "vaapi")]
pub mod va_encode;
pub mod video;
mod x11_capture;
mod x11_input;

use std::process::{Command, Stdio};

pub const OS: &str = "linux";

pub fn os_version() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| "Linux".into())
}

pub fn log_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".local/state"))
        .join("glass")
}

/// Blocks idle and sleep while the host runs. `tail --pid` exits with this process,
/// which releases the inhibitor.
pub fn keep_awake() -> Option<std::process::Child> {
    Command::new("systemd-inhibit")
        .args([
            "--what=idle:sleep",
            "--who=Glass",
            "--why=Remote desktop host",
            "--mode=block",
        ])
        .args([
            "tail",
            &format!("--pid={}", std::process::id()),
            "-f",
            "/dev/null",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .inspect_err(|e| tracing::warn!("systemd-inhibit: {e}"))
        .ok()
}

/// Whether this is an X11 session: an X display and no Wayland compositor. Decided once,
/// when first asked.
pub fn is_x11() -> bool {
    static X11: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *X11.get_or_init(|| {
        let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        set("DISPLAY") && !set("WAYLAND_DISPLAY")
    })
}

/// Turns monitors back on if they were powered off (X11 DPMS, or Hyprland's).
pub fn wake_displays() {
    let (command, args): (_, &[&str]) = if is_x11() {
        ("xset", &["dpms", "force", "on"])
    } else {
        ("hyprctl", &["dispatch", "dpms", "on"])
    };
    let _ = Command::new(command)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn has_command(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether process `pid` is still running.
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
