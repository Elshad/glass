//! Windows has no capture or input permission to grant: a process in the user's session
//! can duplicate the desktop and send input. What it can't do is drive windows of
//! elevated apps (UIPI) or the secure desktop (UAC prompts, Ctrl+Alt+Del).

pub fn has_screen_recording() -> bool {
    true
}

pub fn request_missing() {}
