//! Input on X11 through the XTEST extension. Unlike uinput it needs no device access, and
//! it reaches X servers that have no kernel input devices at all (xrdp, Xvnc, Xvfb).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

/// X keycodes are evdev codes plus this for the main block of the keyboard, under both
/// keycode sets in use (evdev, and the older xfree86 one that xrdp still has).
const EVDEV_OFFSET: u16 = 8;

/// From this evdev code up the two keycode sets disagree (arrows, Home, Delete, right-hand
/// modifiers, media keys), so those keys are found in the server's keymap by keysym.
const FIRST_EXTENDED: u16 = 96;

/// evdev code → the keysym(s) such a key carries, for every extended key in `keymap`.
const EXTENDED: &[(u16, &[u32])] = &[
    (96, &[0xff8d]),          // KP_Enter
    (97, &[0xffe4]),          // Control_R
    (98, &[0xffaf]),          // KP_Divide
    (100, &[0xffea, 0xfe03]), // Alt_R, or ISO_Level3_Shift where it is AltGr
    (102, &[0xff50]),         // Home
    (103, &[0xff52]),         // Up
    (104, &[0xff55]),         // Prior
    (105, &[0xff51]),         // Left
    (106, &[0xff53]),         // Right
    (107, &[0xff57]),         // End
    (108, &[0xff54]),         // Down
    (109, &[0xff56]),         // Next
    (110, &[0xff63]),         // Insert
    (111, &[0xffff]),         // Delete
    (113, &[0x1008_ff12]),    // XF86AudioMute
    (114, &[0x1008_ff11]),    // XF86AudioLowerVolume
    (115, &[0x1008_ff13]),    // XF86AudioRaiseVolume
    (117, &[0xffbd]),         // KP_Equal
    (125, &[0xffeb]),         // Super_L
    (126, &[0xffec]),         // Super_R
    (183, &[0xffca]),         // F13
    (184, &[0xffcb]),         // F14
    (185, &[0xffcc]),         // F15
    (186, &[0xffcd]),         // F16
    (187, &[0xffce]),         // F17
    (188, &[0xffcf]),         // F18
    (189, &[0xffd0]),         // F19
    (190, &[0xffd1]),         // F20
];

/// Modifiers and locks, by evdev code: held, they are pressed once, never repeated.
const NO_REPEAT: &[u16] = &[29, 42, 54, 56, 58, 69, 70, 97, 100, 125, 126];

/// The keymap is read again after this long: xrdp replaces it when a client connects.
const KEYMAP_AGE: Duration = Duration::from_secs(5);

/// Whether the X server accepts synthetic input.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| X11Input::open().is_some())
}

pub struct X11Input {
    conn: RustConnection,
    root: xproto::Window,
    /// Extended keys' X keycodes by evdev code, and when they were looked up.
    extended: Option<(Instant, HashMap<u16, u8>)>,
    /// Keys pressed and not yet released, by X keycode.
    held: HashSet<u8>,
}

impl X11Input {
    pub fn open() -> Option<Self> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        conn.extension_information(xtest::X11_EXTENSION_NAME).ok()??;
        conn.xtest_get_version(2, 2).ok()?.reply().ok()?;
        let root = conn.setup().roots[screen].root;
        Some(Self { conn, root, extended: None, held: HashSet::new() })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        let sent = self
            .conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map(drop)
            .map_err(anyhow::Error::from)
            .and_then(|()| Ok(self.conn.flush()?));
        if let Err(e) = sent {
            tracing::warn!("input: XTEST failed: {e:#}");
        }
    }

    /// Where the server's keymap puts each extended key (its first keysym decides).
    fn read_extended(&self) -> Option<HashMap<u16, u8>> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self.conn.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
        let per = map.keysyms_per_keycode.max(1) as usize;
        let firsts: Vec<u32> = map.keysyms.chunks(per).map(|k| k[0]).collect();
        Some(
            EXTENDED
                .iter()
                .filter_map(|&(evdev, keysyms)| {
                    let at = firsts.iter().position(|k| keysyms.contains(k))?;
                    Some((evdev, min + at as u8))
                })
                .collect(),
        )
    }

    fn keycode(&mut self, evdev: u16) -> Option<u8> {
        if evdev < FIRST_EXTENDED {
            return u8::try_from(evdev + EVDEV_OFFSET).ok();
        }
        if self.extended.as_ref().is_none_or(|(at, _)| at.elapsed() > KEYMAP_AGE) {
            self.extended = Some((Instant::now(), self.read_extended().unwrap_or_default()));
        }
        // A key the keymap doesn't have is dropped: guessing its code would press another.
        self.extended.as_ref()?.1.get(&evdev).copied()
    }

    /// Presses or releases a key, by evdev code.
    ///
    /// A key held on the viewer's side arrives as one press after another (the browser's
    /// own key repeat). X ignores a press of a key that's already down, and xrdp turns the
    /// server's auto repeat off (RDP clients repeat keys themselves), so each repeat is
    /// sent the way X's auto repeat sends it: a release, then a press.
    pub fn key(&mut self, evdev: u16, down: bool) {
        let Some(keycode) = self.keycode(evdev) else {
            return;
        };
        if !down {
            self.held.remove(&keycode);
            self.fake(xproto::KEY_RELEASE_EVENT, keycode, 0, 0);
            return;
        }
        if !self.held.insert(keycode) {
            if NO_REPEAT.contains(&evdev) {
                return;
            }
            self.fake(xproto::KEY_RELEASE_EVENT, keycode, 0, 0);
        }
        self.fake(xproto::KEY_PRESS_EVENT, keycode, 0, 0);
    }

    /// Presses or releases a pointer button (1 left, 2 middle, 3 right).
    pub fn button(&self, button: u8, down: bool) {
        let kind = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(kind, button, 0, 0);
    }

    /// Moves the pointer to a position on the root window.
    pub fn motion(&self, x: f64, y: f64) {
        let clamp = |v: f64| v.round().clamp(0.0, i16::MAX as f64) as i16;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, clamp(x), clamp(y));
    }

    /// Wheel notches: X has no scroll axis in the core protocol, a notch is a click of
    /// button 4 (up), 5 (down), 6 (left) or 7 (right).
    pub fn scroll(&self, up: i32, right: i32) {
        for (notches, positive, negative) in [(up, 4, 5), (right, 7, 6)] {
            let button = if notches > 0 { positive } else { negative };
            for _ in 0..notches.unsigned_abs().min(50) {
                self.button(button, true);
                self.button(button, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first keysym the server's keymap has for `keycode`.
    fn keysym(input: &X11Input, keycode: u8) -> u32 {
        input.conn.get_keyboard_mapping(keycode, 1).unwrap().reply().unwrap().keysyms[0]
    }

    #[test]
    #[ignore = "needs an X server; moves the pointer and puts it back"]
    fn reaches_the_x_server() {
        let mut input = X11Input::open().expect("an X server with XTEST");
        // Main-block keys sit at evdev + 8 under either keycode set.
        let a = input.keycode(30).unwrap();
        assert_eq!(keysym(&input, a), 'a' as u32);
        // Extended keys are wherever the keymap has their keysym.
        for (evdev, want) in [(103, 0xff52), (111, 0xffff), (125, 0xffeb)] {
            let keycode = input.keycode(evdev).unwrap_or_else(|| panic!("no key for evdev {evdev}"));
            assert_eq!(keysym(&input, keycode), want, "evdev {evdev}");
        }

        let at = |input: &X11Input| {
            let p = input.conn.query_pointer(input.root).unwrap().reply().unwrap();
            (p.root_x, p.root_y)
        };
        let before = at(&input);
        input.motion(123.0, 45.0);
        assert_eq!(at(&input), (123, 45));
        input.motion(before.0 as f64, before.1 as f64);
        assert_eq!(at(&input), before);
    }
}
