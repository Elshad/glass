//! Linux (Wayland): xdg-desktop-portal + PipeWire capture, VA-API or OpenH264 encode, uinput input,
//! wl-clipboard, and a systemd user service. Built and tested against Hyprland (Omarchy).

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

/// Turns monitors back on if the compositor powered them off (Hyprland).
pub fn wake_displays() {
    let _ = Command::new("hyprctl")
        .args(["dispatch", "dpms", "on"])
        .stdout(Stdio::null())
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
