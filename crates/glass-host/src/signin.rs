//! The sign-in host (Windows): Glass on the sign-in screen, before anyone has signed in,
//! so a viewer can sign in after a restart.
//!
//! The secure-desktop service (`glass service install`) starts it as SYSTEM in the console
//! session while nobody is signed in there, and stops it as soon as someone signs in (their
//! own host takes over). It serves the same viewers as the owner's host, from a copy of the
//! owner's token, settings, access rules and certificates (`%ProgramData%\Glass\signin`,
//! readable by SYSTEM and administrators only), but only the screen and keyboard and mouse:
//! as SYSTEM, a terminal, files, the clipboard or agents would be the whole computer, so
//! every viewer is treated as a guest and those stay off.

use std::sync::atomic::{AtomicBool, Ordering};

/// What the service starts the sign-in host with.
#[cfg_attr(not(windows), allow(dead_code))]
pub const ARG: &str = "--glass-signin-host";

/// What it keeps of the owner's configuration directory: enough to recognise the same
/// viewers (token, access rules, remote access) and present the same certificates.
#[cfg_attr(not(windows), allow(dead_code))]
pub const FILES: &[&str] = &["token", "settings.json", "acl.json", "certs", "remote"];

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// This process is the sign-in host.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn enable() {
    ACTIVE.store(true, Ordering::Relaxed);
}
