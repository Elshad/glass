//! Whether this session is locked, for treating keys as a password. While the lock screen
//! (or a UAC prompt) is up, the input desktop is Winlogon's, which this user can't open.
//! Checked at most once a second.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::System::StationsAndDesktops::{CloseDesktop, DESKTOP_READOBJECTS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop};

pub fn locked() -> bool {
    static LAST: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if let Some((at, locked)) = *last
        && at.elapsed() < Duration::from_secs(1)
    {
        return locked;
    }
    let locked = !input_desktop_is_ours();
    *last = Some((Instant::now(), locked));
    locked
}

/// The desktop getting input is one this user can open (the normal desktop).
pub fn input_desktop_is_ours() -> bool {
    // SAFETY: the handle is closed right after.
    unsafe {
        match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
            Ok(desk) => {
                let _ = CloseDesktop(desk);
                true
            }
            Err(_) => false,
        }
    }
}
