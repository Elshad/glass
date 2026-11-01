//! Injects client input with SendInput: keys as scan codes (so they follow the computer's
//! keyboard layout, like a real keyboard), the pointer in absolute desktop coordinates.
//! Windows refuses injected input into elevated windows (UIPI) and the secure desktop
//! (UAC prompts, the lock screen) unless the host itself runs elevated.
//!
//! A pen (an iPad's Pencil) is injected as a pen, with pressure and tilt, through a
//! synthetic pointer device (Windows 10 1809 and later): while it's near the screen, the
//! pointer moves and left button that follow are its hover and touches. Without one it
//! works as a mouse.

use std::collections::HashSet;

use glass_proto::InputEvent as Ev;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT,
    POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_BUTTON_CHANGE_TYPE, POINTER_FLAGS, POINTER_INFO, POINTER_PEN_INFO,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_FLAG_NONE, PEN_MASK_PRESSURE, PEN_MASK_ROTATION, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::convert::{PenPhase, pen_frame};
use super::keymap::{self, Key, LEFT_CTRL, LEFT_WIN, RIGHT_WIN, V};
use super::video::{self, Rect};

/// DOM wheel deltas are pixels; one wheel notch is 120 units, roughly 40 px.
const UNITS_PER_PIXEL: f32 = 3.0;

/// Windows needs no grant to send input to the user's own (non-elevated) windows.
pub fn has_accessibility() -> bool {
    true
}

pub struct Injector {
    /// The streamed displays, by stream number. Their rects are looked up for each move:
    /// displays can be moved, turned or plugged in while streaming.
    displays: Vec<u32>,
    held: HashSet<Key>,
    buttons: [bool; 3],
    /// Wheel movement too small to send yet (vertical, horizontal), in wheel units.
    wheel: (f32, f32),
    /// A streamed window's rect, which absolute moves map to instead of a display.
    window: Option<Rect>,
    /// The pen's latest state, while it's near the screen.
    pen: Option<Pen>,
    pen_device: PenDevice,
    /// Where the pointer was last put (desktop pixels): where pen frames go.
    at: Option<(i32, i32)>,
}

/// A synthetic pen device.
#[derive(Clone, Copy)]
struct Device(HSYNTHETICPOINTERDEVICE);

// SAFETY: a process-wide handle, not tied to the thread that made it; the injector using
// it is behind a mutex.
unsafe impl Send for Device {}

/// From `InputEvent::Pen`.
#[derive(Clone, Copy)]
struct Pen {
    pressure: f32,
    tilt: (i8, i8),
    twist: f32,
}

enum PenDevice {
    Untried,
    Ready(Device),
    /// Too old a Windows, or refused: the pen stays a mouse.
    Unavailable,
}

impl Injector {
    pub fn new(display: u32) -> Self {
        let mut this = Self {
            displays: Vec::new(),
            held: HashSet::new(),
            buttons: [false; 3],
            wheel: (0.0, 0.0),
            window: None,
            pen: None,
            pen_device: PenDevice::Untried,
            at: None,
        };
        this.set_displays(&[display]);
        this
    }

    pub fn set_displays(&mut self, displays: &[u32]) {
        self.release_all();
        self.displays = displays.to_vec();
    }

    /// Moves the pointer to `x`, `y` (0..1) of the rectangle `r` (desktop pixels), as the
    /// pen when it's near.
    fn move_in(&mut self, r: Rect, x: f32, y: f32) {
        let gx = r.x + x.clamp(0.0, 1.0) as f64 * (r.w - 1.0).max(0.0);
        let gy = r.y + y.clamp(0.0, 1.0) as f64 * (r.h - 1.0).max(0.0);
        self.at = Some((gx.round() as i32, gy.round() as i32));
        if self.pen_ready() {
            self.pen_frame(if self.buttons[0] { PenPhase::Drag } else { PenPhase::Hover });
            return;
        }
        let (ax, ay) = absolute(gx, gy, virtual_screen());
        send(&[mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
    }

    /// The pen is near and can be injected as a pen.
    fn pen_ready(&self) -> bool {
        self.pen.is_some() && matches!(self.pen_device, PenDevice::Ready(_))
    }

    /// Makes the synthetic pen the first time one comes near.
    fn ensure_pen_device(&mut self) {
        if !matches!(self.pen_device, PenDevice::Untried) {
            return;
        }
        // SAFETY: plain call; the device is destroyed on drop.
        self.pen_device = match unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) } {
            Ok(d) => PenDevice::Ready(Device(d)),
            Err(e) => {
                tracing::info!("input: no pen injection ({e}); the pen works as a mouse");
                PenDevice::Unavailable
            }
        };
    }

    /// One pen frame at the pointer, with the pen's pressure and tilt.
    fn pen_frame(&self, phase: PenPhase) {
        let (PenDevice::Ready(device), Some(pen), Some((x, y))) = (&self.pen_device, self.pen, self.at) else {
            return;
        };
        let f = pen_frame(phase, pen.pressure, pen.tilt, pen.twist);
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerFlags: POINTER_FLAGS(f.flags),
                        ptPixelLocation: POINT { x, y },
                        ButtonChangeType: POINTER_BUTTON_CHANGE_TYPE(f.button_change),
                        ..Default::default()
                    },
                    penFlags: PEN_FLAG_NONE,
                    penMask: PEN_MASK_PRESSURE | PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                    pressure: f.pressure,
                    rotation: f.rotation,
                    tiltX: f.tilt_x,
                    tiltY: f.tilt_y,
                },
            },
        };
        // SAFETY: a live device and one fully initialized pen frame.
        if let Err(e) = unsafe { InjectSyntheticPointerInput(device.0, &[info]) } {
            // Blocked like SendInput (an elevated window, the secure desktop), or a frame
            // out of sequence.
            tracing::debug!("input: pen {phase:?} not injected: {e}");
        }
    }

    pub fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::MouseMove { x, y, stream } => {
                let display = self.displays.get(stream as usize).or(self.displays.first());
                let Some(r) = self.window.or_else(|| display.and_then(|&id| video::rect_of(id))) else {
                    return;
                };
                self.move_in(r, x, y);
            }
            // Relative: what games and 3D apps reading raw input want.
            Ev::MouseMoveBy { dx, dy } => send(&[mouse(dx.round() as i32, dy.round() as i32, 0, MOUSEEVENTF_MOVE)]),
            // Routed to crate::gamepad by the server, never here.
            Ev::Gamepad { .. } => {}
            Ev::Pen { near, pressure, tilt_x, tilt_y, twist } => {
                let was_near = self.pen.is_some();
                if near {
                    self.ensure_pen_device();
                    self.pen = Some(Pen { pressure, tilt: (tilt_x, tilt_y), twist });
                    if !was_near && self.pen_ready() {
                        // A button held as a mouse ends before the pen takes over.
                        if self.buttons[0] {
                            self.buttons[0] = false;
                            send(&[mouse(0, 0, 0, MOUSEEVENTF_LEFTUP)]);
                        }
                        self.pen_frame(PenPhase::Hover);
                    }
                } else if was_near {
                    self.pen_leaves();
                }
            }
            Ev::MouseDown { button: 0 } | Ev::MouseUp { button: 0 } if self.pen_ready() => {
                let down = matches!(ev, Ev::MouseDown { .. });
                if down != self.buttons[0] {
                    self.buttons[0] = down;
                    self.pen_frame(if down { PenPhase::Down } else { PenPhase::Up });
                }
            }
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
                if is_modifier(key) {
                    settle();
                }
            }
        }
    }

    /// Pastes with Ctrl+V. The client triggers this from ⌘V, which arrives here as the
    /// Windows key, so that's lifted for the paste and restored after.
    pub fn paste(&mut self) {
        let wins: Vec<Key> = [LEFT_WIN, RIGHT_WIN].into_iter().filter(|k| self.held.contains(k)).collect();
        send(&wins.iter().map(|&k| keyboard(k, false)).collect::<Vec<_>>());
        send(&[keyboard(LEFT_CTRL, true)]);
        settle();
        send(&[keyboard(V, true), keyboard(V, false)]);
        settle();
        send(&[keyboard(LEFT_CTRL, false)]);
        send(&wins.iter().map(|&k| keyboard(k, true)).collect::<Vec<_>>());
    }

    /// The pen lifts (if touching) and goes out of range.
    fn pen_leaves(&mut self) {
        if self.pen_ready() {
            if self.buttons[0] {
                self.buttons[0] = false;
                self.pen_frame(PenPhase::Up);
            }
            self.pen_frame(PenPhase::Leave);
        }
        self.pen = None;
    }

    /// Releases anything still held, e.g. when a client disconnects mid-drag.
    pub fn release_all(&mut self) {
        if self.pen.is_some() {
            self.pen_leaves();
        }
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
        // Modifiers go as virtual keys (with their scan code): while any low-level keyboard
        // hook is installed (ours for privacy mode and agents, or another app's), Windows
        // drops the modifier state of scan-code-only modifiers, so Shift+H types "h".
        Key::Scan(code, extended) if let Some(vk) = modifier_vk(code, extended) => {
            (vk, code, up | if extended { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) })
        }
        Key::Scan(code, extended) => (
            0,
            code,
            KEYEVENTF_SCANCODE | up | if extended { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) },
        ),
        Key::Vk(vk) => (vk, 0, up),
    };
    keyboard_input(vk, scan, flags)
}

fn is_modifier(key: Key) -> bool {
    matches!(key, Key::Scan(code, extended) if modifier_vk(code, extended).is_some())
}

/// Gives a modifier's press or release time to take hold before the next key: keys
/// injected a fraction of a millisecond after Shift or Ctrl can reach the app before the
/// modifier change does (in the Windows VM, Shift+H typed "h", Shift released early typed
/// "I", and paste typed "v"). Keystrokes that arrive bunched together, as they can over a
/// network, would do the same.
fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(5));
}

/// The left/right virtual key of a modifier's scan code (same on every layout).
fn modifier_vk(scan: u16, extended: bool) -> Option<u16> {
    Some(match (scan, extended) {
        (0x2A, false) => 0xA0, // left Shift
        (0x36, false) => 0xA1, // right Shift
        (0x1D, false) => 0xA2, // left Ctrl
        (0x1D, true) => 0xA3,  // right Ctrl
        (0x38, false) => 0xA4, // left Alt
        (0x38, true) => 0xA5,  // right Alt
        (0x5B, true) => 0x5B,  // left Windows
        (0x5C, true) => 0x5C,  // right Windows
        _ => return None,
    })
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

impl Drop for Injector {
    fn drop(&mut self) {
        if let PenDevice::Ready(device) = self.pen_device {
            // SAFETY: made by this injector and used by nothing else.
            unsafe { DestroySyntheticPointerDevice(device.0) };
        }
    }
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
    fn modifiers_go_as_virtual_keys_and_other_keys_as_scan_codes() {
        let flags = |i: INPUT| unsafe { (i.Anonymous.ki.wVk.0, i.Anonymous.ki.wScan, i.Anonymous.ki.dwFlags) };
        assert_eq!(flags(keyboard(Key::Scan(0x2A, false), true)), (0xA0, 0x2A, KEYBD_EVENT_FLAGS(0)));
        assert_eq!(flags(keyboard(Key::Scan(0x1D, true), false)), (0xA3, 0x1D, KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY));
        assert_eq!(flags(keyboard(LEFT_WIN, true)).0, 0x5B);
        assert_eq!(flags(keyboard(V, true)), (0, 0x2F, KEYEVENTF_SCANCODE));
        // Keypad Enter shares Enter's scan code but isn't a modifier.
        assert_eq!(modifier_vk(0x1C, true), None);
    }

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
