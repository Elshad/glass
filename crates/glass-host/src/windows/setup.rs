//! Windows-specific setup.

use anyhow::Result;

use crate::status::Status;

pub async fn prepare() -> Result<()> {
    Ok(())
}

pub fn prepare_unattended() -> Vec<crate::setup::Need> {
    Vec::new()
}

pub const TAILSCALE_INSTALL: &str = "Install Tailscale: winget install Tailscale.Tailscale (or https://tailscale.com/download/windows),\n\
     then sign in from its tray icon. Install it on your other devices too.";
pub const TAILSCALE_START: &str = "Tailscale is installed but its service isn't running. Start it from the Start menu, or run:\n  net start Tailscale";
pub const TAILSCALE_SIGN_IN: &str =
    "Tailscale isn't signed in. Click its tray icon and log in, or run: tailscale up";

pub fn open_url(url: &str) {
    // `start` treats its first quoted argument as a title, hence the empty one.
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "start", "", url]);
    super::no_window(&mut cmd);
    let _ = cmd.status();
}

/// Where to allow screen capture, for messages.
pub fn screen_settings() -> String {
    "nothing (Windows doesn't ask); make sure someone is signed in at the computer".into()
}

/// Where to allow remote input, for messages.
pub fn input_settings() -> String {
    "nothing (Windows doesn't ask)".into()
}

/// Tailscale on Windows lets any local user run its CLI.
pub fn try_tailscale_operator() -> bool {
    false
}

pub fn permission_needs(status: &Status) -> Vec<crate::setup::Need> {
    let mut needs = Vec::new();
    if !status.capturing {
        needs.push(crate::setup::Need {
            step: "screen",
            action: "Sign in at the computer: Glass captures the signed-in desktop".into(),
            on_screen: true,
        });
    }
    needs
}

pub fn permissions(status: Status) -> Result<()> {
    let _ = status;
    Ok(())
}
