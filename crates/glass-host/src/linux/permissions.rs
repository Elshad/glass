//! On Linux, "screen recording" is the portal's screen-cast approval and "accessibility"
//! is write access to /dev/uinput (see `input::has_accessibility`). X11 asks for neither:
//! any client may read the screen and, through XTEST, send input.

use std::sync::atomic::{AtomicBool, Ordering};

static SCREEN_CAST_GRANTED: AtomicBool = AtomicBool::new(false);

pub fn has_screen_recording() -> bool {
    SCREEN_CAST_GRANTED.load(Ordering::Relaxed)
}

pub(crate) fn set_screen_cast_granted(granted: bool) {
    SCREEN_CAST_GRANTED.store(granted, Ordering::Relaxed);
}

/// The portal asks when capture starts; nothing to request up front.
pub fn request_missing() {}
