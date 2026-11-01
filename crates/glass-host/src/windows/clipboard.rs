//! The Windows clipboard.

use tokio::sync::broadcast;

use crate::control::ServerMsg;

pub fn set_text(text: &str) {
    let _ = text;
}

/// Broadcasts clipboard changes.
pub fn watch(events: broadcast::Sender<ServerMsg>) {
    let _ = events;
}

/// The real system clipboard.
pub struct SystemClipboard;

impl crate::media::Clipboard for SystemClipboard {
    fn set_text(&self, text: &str) {
        set_text(text)
    }
}
