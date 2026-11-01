//! Windows-specific setup: letting Glass through Windows Defender Firewall.

use anyhow::Result;
use cliclack::{confirm, log, note};

use super::firewall::{self, Inbound};
use crate::status::Status;

pub async fn prepare() -> Result<()> {
    firewall_step()
}

/// Without a rule, Windows asks the first time the host listens, on this screen, and
/// "Cancel" there leaves Glass blocked. Adding the rule now asks once, here, with context.
fn firewall_step() -> Result<()> {
    log::step("Windows Defender Firewall")?;
    let exe = std::env::current_exe()?;
    match firewall::inbound(&exe) {
        Some(Inbound::Allowed) => {
            log::success("Glass can accept connections on private networks")?;
            return Ok(());
        }
        Some(Inbound::Blocked) => {
            log::warning(
                "Windows Defender Firewall is blocking Glass (maybe its prompt was turned down). In\n\
                 Windows Security › Firewall & network protection › Allow an app through firewall,\n\
                 tick Private for Glass, then run `glass setup` again.",
            )?;
            return Ok(());
        }
        Some(Inbound::Missing) => {}
        None => {
            log::warning("Couldn't read the firewall's rules; Windows may ask about Glass when it starts.")?;
            return Ok(());
        }
    }
    note(
        "Let your other devices connect",
        "Windows Defender Firewall blocks incoming connections until a program is allowed.\n\
         Glass can add a rule for itself on private networks (home, work and Tailscale),\n\
         not public ones like a café's. Windows asks for administrator approval once.",
    )?;
    if !confirm("Add the rule now?").initial_value(true).interact()? {
        log::warning("Skipped: Windows will ask when Glass first starts. Choose Allow there.")?;
        return Ok(());
    }
    if firewall::add_rule_elevated(&exe) && firewall::inbound(&exe) == Some(Inbound::Allowed) {
        log::success("Glass can accept connections on private networks")?;
    } else {
        log::warning(format!(
            "The rule wasn't added. As administrator, run:\n  netsh {}",
            firewall::add_rule_args(&exe)
        ))?;
    }
    Ok(())
}

/// Adds the firewall rule when already running as administrator; reports it otherwise.
pub fn prepare_unattended() -> Vec<crate::setup::Need> {
    let mut needs = Vec::new();
    let Ok(exe) = std::env::current_exe() else { return needs };
    let action = match firewall::inbound(&exe) {
        Some(Inbound::Missing) if firewall::add_rule_quietly(&exe) => None,
        Some(Inbound::Missing) => Some(format!(
            "Allow Glass through Windows Defender Firewall on private networks: run `glass setup` on the computer (asks for administrator approval once), or as administrator: netsh {}",
            firewall::add_rule_args(&exe)
        )),
        Some(Inbound::Blocked) => Some(
            "Windows Defender Firewall blocks Glass: in Windows Security › Firewall & network protection › Allow an app through firewall, tick Private for Glass".into(),
        ),
        Some(Inbound::Allowed) | None => None,
    };
    if let Some(action) = action {
        needs.push(crate::setup::Need { step: "firewall", action, on_screen: false });
    }
    needs
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
