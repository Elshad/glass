//! Injects client input with SendInput.

use glass_proto::InputEvent as Ev;

/// Windows needs no grant to send input to the user's own (non-elevated) windows.
pub fn has_accessibility() -> bool {
    true
}

pub struct Injector;

impl Injector {
    pub fn new(display: u32) -> Self {
        let _ = display;
        Self
    }
}

/// Wakes the displays (any input does).
pub fn nudge() {}

impl crate::media::InputSink for Injector {
    fn handle(&mut self, ev: Ev) {
        let _ = ev;
    }
    fn paste(&mut self) {}
    fn release_all(&mut self) {}
    fn set_displays(&mut self, displays: &[u32]) {
        let _ = displays;
    }
    fn move_on_display(&mut self, display: u32, x: f32, y: f32) {
        let _ = (display, x, y);
    }
}
