//! Whether this session is locked, for treating keys as a password: logind's LockedHint
//! (GNOME, KDE and lockers that set it) or a screen locker running (hyprlock, swaylock and
//! others that don't). Checked at most once a second.

use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Screen lockers, by process name.
const LOCKERS: &[&str] = &["hyprlock", "swaylock", "gtklock", "waylock", "i3lock", "kscreenlocker_greet", "xsecurelock", "physlock"];

pub fn locked() -> bool {
    static LAST: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if let Some((at, locked)) = *last
        && at.elapsed() < Duration::from_secs(1)
    {
        return locked;
    }
    let locked = locker_running() || locked_hint();
    *last = Some((Instant::now(), locked));
    locked
}

fn locker_running() -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else { return false };
    procs.flatten().any(|p| {
        std::fs::read_to_string(p.path().join("comm")).is_ok_and(|name| LOCKERS.contains(&name.trim()))
    })
}

fn loginctl(args: &[&str]) -> Option<String> {
    let out = Command::new("loginctl").args(args).output().ok()?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !value.is_empty()).then_some(value)
}

/// This user's graphical session says it's locked.
fn locked_hint() -> bool {
    // SAFETY: a plain query.
    let uid = unsafe { libc::getuid() }.to_string();
    let session = std::env::var("XDG_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| loginctl(&["show-user", &uid, "-p", "Display", "--value"]));
    session.is_some_and(|s| loginctl(&["show-session", &s, "-p", "LockedHint", "--value"]).as_deref() == Some("yes"))
}
