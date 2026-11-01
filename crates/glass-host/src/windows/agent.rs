//! Runs the host at logon in the user's session.

use anyhow::Result;

pub const LOG_HINT: &str = " Check the logs in %LOCALAPPDATA%\\Glass\\Logs.";

pub fn is_installed() -> bool {
    false
}

pub fn install(start_at_login: bool) -> Result<()> {
    let _ = start_at_login;
    anyhow::bail!("the background service isn't available on Windows yet")
}

pub fn restart() -> Result<()> {
    anyhow::bail!("the background service isn't available on Windows yet")
}

pub fn uninstall() -> Result<()> {
    Ok(())
}
