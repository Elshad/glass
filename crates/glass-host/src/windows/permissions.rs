//! Windows has no capture or input permission to grant: a process in the user's session
//! can duplicate the desktop and send input. What it can't do is drive windows of
//! elevated apps (UIPI) or the secure desktop (UAC prompts, Ctrl+Alt+Del).

pub fn has_screen_recording() -> bool {
    true
}

pub fn request_missing() {}

/// Settings pages the app links to if a check ever fails (Windows itself has no capture or
/// input grant to give).
pub const SCREEN_RECORDING_PANE: &str = "ms-settings:privacy";
pub const ACCESSIBILITY_PANE: &str = "ms-settings:easeofaccess";

pub fn open_pane(pane: &str) {
    super::setup::open_url(pane)
}
