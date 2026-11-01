//! Injects client input with SendInput: keys as scan codes (so they follow the computer's
//! keyboard layout, like a real keyboard), the pointer in absolute desktop coordinates.
//! Windows refuses injected input into elevated windows (UIPI) and the secure desktop
//! (UAC prompts, the lock screen) unless the host itself runs elevated.

use std::collections::HashSet;

use glass_proto::InputEvent as Ev;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::keymap::{self, Key, LEFT_CTRL, LEFT_WIN, RIGHT_WIN, V};
use super::video::{self, Rect};

/// DOM wheel deltas are pixels; one wheel notch is 120 units, roughly 40 px.
const UNITS_PER_PIXEL: f32 = 3.0;

/// Windows needs no grant to send input to the user's own (non-elevated) windows.
pub fn has_accessibility() -> bool {
    true
}

pub struct Injector {
    /// Desktop rect of each streamed display, by stream number.
    bounds: Vec<Rect>,
    held: HashSet<Key>,
    buttons: [bool; 3],
    /// Wheel movement too small to send yet (vertical, horizontal), in wheel units.
    wheel: (f32, f32),
    /// A streamed window's rect, which absolute moves map to instead of a display.
    window: Option<Rect>,
}

impl Injector {
    pub fn new(display: u32) -> Self {
        let mut this = Self {
            bounds: Vec::new(),
            held: HashSet::new(),
            buttons: [false; 3],
            wheel: (0.0, 0.0),
            window: None,
        };
        this.set_displays(&[display]);
        this
    }

    pub fn set_displays(&mut self, displays: &[u32]) {
        self.release_all();
        self.bounds = displays.iter().filter_map(|&id| video::rect_of(id)).collect();
    }

    /// Moves the pointer to `x`, `y` (0..1) of the rectangle `r` (desktop pixels).
    fn move_in(&mut self, r: Rect, x: f32, y: f32) {
        let gx = r.x + x.clamp(0.0, 1.0) as f64 * (r.w - 1.0).max(0.0);
        let gy = r.y + y.clamp(0.0, 1.0) as f64 * (r.h - 1.0).max(0.0);
        let (ax, ay) = absolute(gx, gy, virtual_screen());
        send(&[mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
    }

    pub fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::MouseMove { x, y, stream } => {
                let Some(r) = self.window.or_else(|| self.bounds.get(stream as usize).or(self.bounds.first()).copied()) else {
                    return;
                };
                self.move_in(r, x, y);
            }
            // Relative: what games and 3D apps reading raw input want.
            Ev::MouseMoveBy { dx, dy } => send(&[mouse(dx.round() as i32, dy.round() as i32, 0, MOUSEEVENTF_MOVE)]),
            // Routed to crate::gamepad by the server, never here.
            Ev::Gamepad { .. } => {}
            // Pressure needs pen injection; the pen still works as a mouse.
            Ev::Pen { .. } => {}
            Ev::MouseDown { button } | Ev::MouseUp { button } => {
                let down = matches!(ev, Ev::MouseDown { .. });
                let flags = match (button, down) {
                    (0, true) => MOUSEEVENTF_LEFTDOWN,
                    (0, false) => MOUSEEVENTF_LEFTUP,
                    (1, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (1, false) => MOUSEEVENTF_MIDDLEUP,
                    (2, true) => MOUSEEVENTF_RIGHTDOWN,
                    (2, false) => MOUSEEVENTF_RIGHTUP,
                    _ => return,
                };
                self.buttons[button as usize] = down;
                send(&[mouse(0, 0, 0, flags)]);
            }
            Ev::Scroll { dx, dy } => {
                // DOM: +dy scrolls down, +dx right. Windows: +wheel scrolls up, +hwheel right.
                self.wheel.0 -= dy * UNITS_PER_PIXEL;
                self.wheel.1 += dx * UNITS_PER_PIXEL;
                let (v, h) = (self.wheel.0.trunc(), self.wheel.1.trunc());
                self.wheel.0 -= v;
                self.wheel.1 -= h;
                let mut inputs = Vec::new();
                if v != 0.0 {
                    inputs.push(mouse(0, 0, v as i32 as u32, MOUSEEVENTF_WHEEL));
                }
                if h != 0.0 {
                    inputs.push(mouse(0, 0, h as i32 as u32, MOUSEEVENTF_HWHEEL));
                }
                send(&inputs);
            }
            Ev::KeyDown { keycode } | Ev::KeyUp { keycode } => {
                let down = matches!(ev, Ev::KeyDown { .. });
                let Some(key) = keymap::to_key(keycode) else {
                    return;
                };
                if down {
                    self.held.insert(key);
                } else {
                    self.held.remove(&key);
                }
                send(&[keyboard(key, down)]);
            }
        }
    }

    /// Pastes with Ctrl+V. The client triggers this from ⌘V, which arrives here as the
    /// Windows key, so that's lifted for the paste and restored after.
    pub fn paste(&mut self) {
        let wins: Vec<Key> = [LEFT_WIN, RIGHT_WIN].into_iter().filter(|k| self.held.contains(k)).collect();
        let mut inputs: Vec<INPUT> = wins.iter().map(|&k| keyboard(k, false)).collect();
        inputs.extend([keyboard(LEFT_CTRL, true), keyboard(V, true), keyboard(V, false), keyboard(LEFT_CTRL, false)]);
        inputs.extend(wins.iter().map(|&k| keyboard(k, true)));
        send(&inputs);
    }

    /// Releases anything still held, e.g. when a client disconnects mid-drag.
    pub fn release_all(&mut self) {
        for button in 0..3u8 {
            if self.buttons[button as usize] {
                self.handle(Ev::MouseUp { button });
            }
        }
        let keys: Vec<INPUT> = std::mem::take(&mut self.held).into_iter().map(|k| keyboard(k, false)).collect();
        send(&keys);
    }
}

/// The bounding box of every display (what absolute SendInput coordinates span).
fn virtual_screen() -> Rect {
    // SAFETY: plain queries.
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
            y: GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as f64,
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as f64,
        }
    }
}

/// Desktop pixels → SendInput's 0..65535 across the virtual screen (pixel centres).
fn absolute(gx: f64, gy: f64, screen: Rect) -> (i32, i32) {
    let scale = |v: f64, origin: f64, size: f64| (((v - origin + 0.5) * 65536.0 / size).floor() as i32).clamp(0, 65535);
    (scale(gx, screen.x, screen.w), scale(gy, screen.y, screen.h))
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn keyboard(key: Key, down: bool) -> INPUT {
    let up = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    let (vk, scan, flags) = match key {
        Key::Scan(code, extended) => (
            0,
            code,
            KEYEVENTF_SCANCODE | up | if extended { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) },
        ),
        Key::Vk(vk) => (vk, 0, up),
    };
    keyboard_input(vk, scan, flags)
}

fn keyboard_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    // SAFETY: a slice of fully initialized INPUTs.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if (sent as usize) < inputs.len() {
        // Blocked: an elevated window has focus, or the secure desktop is up.
        tracing::debug!("input: SendInput sent {sent} of {}", inputs.len());
    }
}

/// Wakes the displays: a pointer nudge there and back counts as input.
pub fn nudge() {
    send(&[mouse(1, 0, 0, MOUSEEVENTF_MOVE), mouse(-1, 0, 0, MOUSEEVENTF_MOVE)]);
}

impl crate::media::InputSink for Injector {
    fn handle(&mut self, ev: Ev) {
        Injector::handle(self, ev)
    }
    fn paste(&mut self) {
        Injector::paste(self)
    }
    fn release_all(&mut self) {
        Injector::release_all(self)
    }
    fn set_displays(&mut self, displays: &[u32]) {
        Injector::set_displays(self, displays)
    }
    fn set_window_rect(&mut self, rect: Option<[f64; 4]>) {
        self.window = rect.map(|[x, y, w, h]| Rect { x, y, w, h });
    }
    fn move_on_display(&mut self, display: u32, x: f32, y: f32) {
        if let Some(r) = video::rect_of(display) {
            self.move_in(r, x, y);
        }
    }
    /// Any text, as Unicode key events (UTF-16 units), independent of the layout.
    fn type_text(&mut self, text: &str) -> bool {
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            // Newlines as Enter: apps read Return from the key, not the character.
            if unit == '\n' as u16 {
                inputs.extend([keyboard(Key::Scan(0x1C, false), true), keyboard(Key::Scan(0x1C, false), false)]);
                continue;
            }
            inputs.push(keyboard_input(0, unit, KEYEVENTF_UNICODE));
            inputs.push(keyboard_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
        send(&inputs);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_coordinates_span_the_virtual_screen() {
        // Two 1920-wide displays, the second left of the primary.
        let screen = Rect { x: -1920.0, y: 0.0, w: 3840.0, h: 1080.0 };
        assert_eq!(absolute(-1920.0, 0.0, screen), (8, 30));
        assert_eq!(absolute(0.0, 539.5, screen).0, 32776);
        assert_eq!(absolute(1919.0, 1079.0, screen), (65527, 65505));
        assert_eq!(absolute(5000.0, -10.0, screen), (65535, 0));
    }
}
