//! Game controllers from the viewer (browser Gamepad API, standard mapping), as virtual
//! controllers on the host.
//!
//! Linux: one uinput device per pad, shaped like an Xbox 360 controller (same ids and
//! codes as the xpad driver), so games and SDL pick it up with no setup. Windows: one
//! virtual Xbox 360 controller per pad from the ViGEmBus driver, when it's installed (Windows
//! has no virtual controller of its own for an unsigned app). macOS has no public way to
//! create a virtual HID controller (that needs a DriverKit extension with a special
//! entitlement), so controllers aren't offered there.

/// Linux input codes, kept here (not from evdev) so the mapping is testable everywhere.
pub mod code {
    pub const BTN_SOUTH: u16 = 0x130;
    pub const BTN_EAST: u16 = 0x131;
    /// xpad's BTN_X / BTN_Y (0x133 / 0x134): the left and top face buttons.
    pub const BTN_X: u16 = 0x133;
    pub const BTN_Y: u16 = 0x134;
    pub const BTN_TL: u16 = 0x136;
    pub const BTN_TR: u16 = 0x137;
    pub const BTN_SELECT: u16 = 0x13a;
    pub const BTN_START: u16 = 0x13b;
    pub const BTN_MODE: u16 = 0x13c;
    pub const BTN_THUMBL: u16 = 0x13d;
    pub const BTN_THUMBR: u16 = 0x13e;
    pub const ABS_X: u16 = 0x00;
    pub const ABS_Y: u16 = 0x01;
    pub const ABS_Z: u16 = 0x02;
    pub const ABS_RX: u16 = 0x03;
    pub const ABS_RY: u16 = 0x04;
    pub const ABS_RZ: u16 = 0x05;
    pub const ABS_HAT0X: u16 = 0x10;
    pub const ABS_HAT0Y: u16 = 0x11;
}

/// Where a standard-mapping button goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Key(u16),
    /// An analog trigger: an absolute axis, 0..=TRIGGER_MAX.
    Trigger(u16),
    /// A d-pad direction: hat axis and -1 / +1.
    Hat(u16, i32),
}

pub const STICK_MAX: i32 = 32767;
pub const TRIGGER_MAX: i32 = 255;

/// https://w3c.github.io/gamepad/#remapping
pub fn map_button(index: u16) -> Option<Button> {
    use code::*;
    Some(match index {
        0 => Button::Key(BTN_SOUTH),
        1 => Button::Key(BTN_EAST),
        2 => Button::Key(BTN_X),
        3 => Button::Key(BTN_Y),
        4 => Button::Key(BTN_TL),
        5 => Button::Key(BTN_TR),
        6 => Button::Trigger(ABS_Z),
        7 => Button::Trigger(ABS_RZ),
        8 => Button::Key(BTN_SELECT),
        9 => Button::Key(BTN_START),
        10 => Button::Key(BTN_THUMBL),
        11 => Button::Key(BTN_THUMBR),
        12 => Button::Hat(ABS_HAT0Y, -1),
        13 => Button::Hat(ABS_HAT0Y, 1),
        14 => Button::Hat(ABS_HAT0X, -1),
        15 => Button::Hat(ABS_HAT0X, 1),
        16 => Button::Key(BTN_MODE),
        _ => return None,
    })
}

/// Stick axes: left x/y, right x/y (-1..1, +y down, as in both the browser and evdev).
pub fn map_axis(index: u16) -> Option<u16> {
    use code::*;
    [ABS_X, ABS_Y, ABS_RX, ABS_RY].get(index as usize).copied()
}

/// Scales a browser value to the device's range.
pub fn scale(value: f32, max: i32, signed: bool) -> i32 {
    let v = if signed {
        value.clamp(-1.0, 1.0)
    } else {
        value.clamp(0.0, 1.0)
    };
    (v * max as f32).round() as i32
}

/// The hat value from the two opposing buttons (both held: centred).
pub fn hat_value(negative: bool, positive: bool) -> i32 {
    positive as i32 - negative as i32
}

/// Where Windows gets the driver controllers need.
pub const VIGEM_DOWNLOAD: &str = "https://github.com/nefarius/ViGEmBus/releases";

pub fn supported() -> bool {
    crate::testmode::enabled() || cfg!(target_os = "linux") || windows_supported()
}

#[cfg(windows)]
fn windows_supported() -> bool {
    vigem::present()
}

#[cfg(not(windows))]
fn windows_supported() -> bool {
    false
}

#[cfg(target_os = "linux")]
pub use linux::{handle, release_all};

#[cfg(windows)]
pub use vigem::{handle, release_all};

#[cfg(not(any(target_os = "linux", windows)))]
pub fn handle(ev: glass_proto::InputEvent) {
    let _ = ev;
}

/// A session ended: let go of its pads.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn release_all() {}

/// Xbox 360 controller (XUSB) reports, as the ViGEmBus driver takes them. Built from the
/// same standard mapping as Linux, so both hosts treat a pad alike.
#[cfg_attr(not(windows), allow(dead_code))]
pub mod xusb {
    use glass_proto::{GAMEPAD_AXIS, GAMEPAD_BUTTON};

    use super::{Button, STICK_MAX, TRIGGER_MAX, code, hat_value, map_axis, map_button, scale};

    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;

    /// The XUSB bit for a button code from `map_button`.
    pub fn button(c: u16) -> Option<u16> {
        use code::*;
        Some(match c {
            BTN_SOUTH => A,
            BTN_EAST => B,
            BTN_X => X,
            BTN_Y => Y,
            BTN_TL => LEFT_SHOULDER,
            BTN_TR => RIGHT_SHOULDER,
            BTN_SELECT => BACK,
            BTN_START => START,
            BTN_MODE => GUIDE,
            BTN_THUMBL => LEFT_THUMB,
            BTN_THUMBR => RIGHT_THUMB,
            _ => return None,
        })
    }

    /// XUSB_REPORT: 12 bytes, little-endian, sticks with +y up.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Report {
        pub buttons: u16,
        pub left_trigger: u8,
        pub right_trigger: u8,
        pub lx: i16,
        pub ly: i16,
        pub rx: i16,
        pub ry: i16,
    }

    impl Report {
        pub fn to_bytes(self) -> [u8; 12] {
            let mut b = [0u8; 12];
            b[0..2].copy_from_slice(&self.buttons.to_le_bytes());
            b[2] = self.left_trigger;
            b[3] = self.right_trigger;
            for (i, v) in [self.lx, self.ly, self.rx, self.ry].into_iter().enumerate() {
                b[4 + i * 2..6 + i * 2].copy_from_slice(&v.to_le_bytes());
            }
            b
        }
    }

    /// One pad: its report, and which d-pad buttons (12..=15) are held, so two opposite
    /// ones held count as neither (as the Linux hat does).
    #[derive(Debug, Default)]
    pub struct Pad {
        pub report: Report,
        dpad: [bool; 4],
    }

    impl Pad {
        /// Applies one browser event; true if the report changed.
        pub fn apply(&mut self, kind: u8, index: u16, value: f32) -> bool {
            let before = self.report;
            let r = &mut self.report;
            match kind {
                GAMEPAD_BUTTON => match map_button(index) {
                    Some(Button::Key(c)) => {
                        let Some(bit) = button(c) else { return false };
                        if value >= 0.5 {
                            r.buttons |= bit;
                        } else {
                            r.buttons &= !bit;
                        }
                    }
                    Some(Button::Trigger(c)) => {
                        let v = scale(value, TRIGGER_MAX, false) as u8;
                        if c == code::ABS_Z {
                            r.left_trigger = v;
                        } else {
                            r.right_trigger = v;
                        }
                    }
                    Some(Button::Hat(..)) => {
                        self.dpad[(index - 12) as usize] = value >= 0.5;
                        let y = hat_value(self.dpad[0], self.dpad[1]);
                        let x = hat_value(self.dpad[2], self.dpad[3]);
                        r.buttons &= !(DPAD_UP | DPAD_DOWN | DPAD_LEFT | DPAD_RIGHT);
                        for (on, bit) in [(y < 0, DPAD_UP), (y > 0, DPAD_DOWN), (x < 0, DPAD_LEFT), (x > 0, DPAD_RIGHT)] {
                            if on {
                                r.buttons |= bit;
                            }
                        }
                    }
                    None => return false,
                },
                GAMEPAD_AXIS => {
                    let Some(c) = map_axis(index) else { return false };
                    let v = scale(value, STICK_MAX, true) as i16;
                    match c {
                        code::ABS_X => r.lx = v,
                        // The browser's +y is down, XInput's up.
                        code::ABS_Y => r.ly = -v,
                        code::ABS_RX => r.rx = v,
                        _ => r.ry = -v,
                    }
                }
                _ => return false,
            }
            self.report != before
        }
    }

    /// ViGEmBus's control codes: CTL_CODE(FILE_DEVICE_BUS_EXTENDER, 0x801 + n,
    /// METHOD_BUFFERED, FILE_WRITE_DATA).
    pub const fn ioctl(n: u32) -> u32 {
        (0x2a << 16) | (0x2 << 14) | ((0x801 + n) << 2)
    }
    pub const PLUGIN_TARGET: u32 = ioctl(0x000);
    pub const UNPLUG_TARGET: u32 = ioctl(0x001);
    pub const CHECK_VERSION: u32 = ioctl(0x002);
    pub const WAIT_DEVICE_READY: u32 = ioctl(0x003);
    pub const SUBMIT_REPORT: u32 = ioctl(0x201);

    /// VIGEM_PLUGIN_TARGET for an Xbox 360 controller (target type 0) with its usual ids.
    pub fn plugin(serial: u32) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&16u32.to_le_bytes());
        b[4..8].copy_from_slice(&serial.to_le_bytes());
        b[12..14].copy_from_slice(&0x045eu16.to_le_bytes());
        b[14..16].copy_from_slice(&0x028eu16.to_le_bytes());
        b
    }

    /// The {Size, SerialNo} (or {Size, Version}) body of the other requests.
    pub fn serial(value: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&8u32.to_le_bytes());
        b[4..8].copy_from_slice(&value.to_le_bytes());
        b
    }

    /// XUSB_SUBMIT_REPORT.
    pub fn submit(serial: u32, report: Report) -> [u8; 20] {
        let mut b = [0u8; 20];
        b[0..4].copy_from_slice(&20u32.to_le_bytes());
        b[4..8].copy_from_slice(&serial.to_le_bytes());
        b[8..20].copy_from_slice(&report.to_bytes());
        b
    }
}

#[cfg(windows)]
mod vigem {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use glass_proto::GAMEPAD_CONNECTED;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW, CR_SUCCESS,
    };
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::core::{GUID, PCWSTR};

    use super::xusb;

    /// GUID_DEVINTERFACE_BUSENUM_VIGEM.
    const BUS: GUID = GUID::from_u128(0x96e42b22_f5e9_42f8_b043_ed0f932f014f);
    /// The interface version this speaks (VIGEM_COMMON_VERSION).
    const VERSION: u32 = 1;
    /// The bus's own limit on controllers (serials 1..=16).
    const MAX_SERIAL: u32 = 16;
    const MAX_PADS: u8 = 4;

    /// The ViGEmBus device's path, when the driver is installed and running.
    fn bus_path() -> Option<Vec<u16>> {
        let mut len = 0u32;
        // SAFETY: len is a valid out pointer; BUS lives for the call.
        let r = unsafe { CM_Get_Device_Interface_List_SizeW(&mut len, &BUS, PCWSTR::null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) };
        if r != CR_SUCCESS || len <= 1 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        // SAFETY: buf is writable and its length is passed with it.
        let r = unsafe { CM_Get_Device_Interface_ListW(&BUS, PCWSTR::null(), &mut buf, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) };
        if r != CR_SUCCESS {
            return None;
        }
        // A list of NUL-terminated paths: the first one, with its NUL.
        let end = buf.iter().position(|&c| c == 0)?;
        (end > 0).then(|| buf[..=end].to_vec())
    }

    pub fn present() -> bool {
        bus_path().is_some()
    }

    /// An open handle to the bus. Closing it unplugs every controller it plugged in.
    struct Bus(HANDLE);

    // SAFETY: a file handle may be used from any thread; it's only used under PADS' lock.
    unsafe impl Send for Bus {}

    impl Drop for Bus {
        fn drop(&mut self) {
            // SAFETY: the handle is open and owned here.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    impl Bus {
        fn open() -> anyhow::Result<Bus> {
            let path = bus_path().ok_or_else(|| anyhow::anyhow!("ViGEmBus isn't installed ({})", super::VIGEM_DOWNLOAD))?;
            // SAFETY: path is NUL-terminated and outlives the call. Not overlapped: each
            // request waits for its answer, one at a time under PADS' lock.
            let h = unsafe {
                CreateFileW(
                    PCWSTR(path.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            }?;
            let bus = Bus(h);
            bus.call(xusb::CHECK_VERSION, &xusb::serial(VERSION))
                .map_err(|e| anyhow::anyhow!("this ViGEmBus version doesn't match ({e}); install the latest from {}", super::VIGEM_DOWNLOAD))?;
            Ok(bus)
        }

        fn call(&self, code: u32, body: &[u8]) -> windows::core::Result<()> {
            let mut got = 0u32;
            // SAFETY: body is readable for its length; there's no output buffer.
            unsafe { DeviceIoControl(self.0, code, Some(body.as_ptr().cast()), body.len() as u32, None, 0, Some(&mut got), None) }
        }

        /// Plugs in a new controller, on the first free serial.
        fn plug(&self) -> anyhow::Result<u32> {
            let mut last = None;
            for serial in 1..=MAX_SERIAL {
                match self.call(xusb::PLUGIN_TARGET, &xusb::plugin(serial)) {
                    Ok(()) => {
                        // Waits until it takes reports (drivers before 1.17 don't have
                        // this request, and are ready already).
                        if let Err(e) = self.call(xusb::WAIT_DEVICE_READY, &xusb::serial(serial)) {
                            tracing::debug!("gamepad: wait-ready: {e}");
                        }
                        return Ok(serial);
                    }
                    // Taken (by this or another app): the next one.
                    Err(e) => last = Some(e),
                }
            }
            Err(anyhow::anyhow!("no free controller slot ({})", last.map(|e| e.to_string()).unwrap_or_default()))
        }

        fn unplug(&self, serial: u32) {
            if let Err(e) = self.call(xusb::UNPLUG_TARGET, &xusb::serial(serial)) {
                tracing::warn!("gamepad: unplug {serial}: {e}");
            }
        }
    }

    struct Pad {
        serial: u32,
        state: xusb::Pad,
    }

    #[derive(Default)]
    struct State {
        bus: Option<Bus>,
        pads: HashMap<u8, Pad>,
    }

    static PADS: Mutex<Option<State>> = Mutex::new(None);

    pub fn handle(ev: glass_proto::InputEvent) {
        let glass_proto::InputEvent::Gamepad { pad, kind, index, value } = ev else { return };
        if pad >= MAX_PADS {
            return;
        }
        let mut state = PADS.lock().unwrap();
        let state = state.get_or_insert_with(State::default);
        if kind == GAMEPAD_CONNECTED && value == 0.0 {
            if let Some(p) = state.pads.remove(&pad) {
                if let Some(bus) = &state.bus {
                    bus.unplug(p.serial);
                }
                tracing::info!("gamepad {pad}: disconnected");
            }
            if state.pads.is_empty() {
                state.bus = None;
            }
            return;
        }
        if !state.pads.contains_key(&pad) {
            if state.bus.is_none() {
                match Bus::open() {
                    Ok(bus) => state.bus = Some(bus),
                    Err(e) => {
                        tracing::warn!("gamepad {pad}: {e}");
                        return;
                    }
                }
            }
            let Some(bus) = &state.bus else { return };
            match bus.plug() {
                Ok(serial) => {
                    tracing::info!("gamepad {pad}: virtual Xbox 360 controller ready");
                    state.pads.insert(pad, Pad { serial, state: xusb::Pad::default() });
                }
                Err(e) => {
                    tracing::warn!("gamepad {pad}: can't plug in a ViGEmBus controller ({e})");
                    return;
                }
            }
        }
        let (Some(bus), Some(p)) = (&state.bus, state.pads.get_mut(&pad)) else { return };
        if p.state.apply(kind, index, value)
            && let Err(e) = bus.call(xusb::SUBMIT_REPORT, &xusb::submit(p.serial, p.state.report))
        {
            tracing::warn!("gamepad {pad}: report failed: {e}");
        }
    }

    pub fn release_all() {
        let mut state = PADS.lock().unwrap();
        let Some(state) = state.as_mut() else { return };
        if !state.pads.is_empty() {
            tracing::info!("gamepads: released {}", state.pads.len());
        }
        if let Some(bus) = &state.bus {
            for p in state.pads.values() {
                bus.unplug(p.serial);
            }
        }
        state.pads.clear();
        state.bus = None;
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use evdev::uinput::VirtualDevice;
    use evdev::{
        AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId,
        KeyCode, UinputAbsSetup,
    };
    use glass_proto::{GAMEPAD_AXIS, GAMEPAD_BUTTON, GAMEPAD_CONNECTED};

    use super::{Button, STICK_MAX, TRIGGER_MAX, code, hat_value, map_axis, map_button, scale};

    /// Browsers report at most a handful of pads; more are ignored.
    const MAX_PADS: u8 = 4;

    struct Pad {
        device: VirtualDevice,
        /// D-pad buttons 12..=15 held.
        dpad: [bool; 4],
    }

    static PADS: Mutex<Option<HashMap<u8, Pad>>> = Mutex::new(None);

    pub fn handle(ev: glass_proto::InputEvent) {
        let glass_proto::InputEvent::Gamepad {
            pad,
            kind,
            index,
            value,
        } = ev
        else {
            return;
        };
        if pad >= MAX_PADS {
            return;
        }
        let mut pads = PADS.lock().unwrap();
        let pads = pads.get_or_insert_with(HashMap::new);
        if kind == GAMEPAD_CONNECTED && value == 0.0 {
            if pads.remove(&pad).is_some() {
                tracing::info!("gamepad {pad}: disconnected");
            }
            return;
        }
        if !pads.contains_key(&pad) {
            match build(pad) {
                Ok(device) => {
                    tracing::info!("gamepad {pad}: virtual controller ready");
                    pads.insert(
                        pad,
                        Pad {
                            device,
                            dpad: [false; 4],
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!("gamepad {pad}: can't create a uinput device ({e})");
                    return;
                }
            }
        }
        let Some(p) = pads.get_mut(&pad) else { return };
        let events = match kind {
            GAMEPAD_BUTTON => match map_button(index) {
                Some(Button::Key(c)) => vec![InputEvent::new(
                    EventType::KEY.0,
                    c,
                    (value >= 0.5) as i32,
                )],
                Some(Button::Trigger(c)) => vec![InputEvent::new(
                    EventType::ABSOLUTE.0,
                    c,
                    scale(value, TRIGGER_MAX, false),
                )],
                Some(Button::Hat(axis, _)) => {
                    p.dpad[(index - 12) as usize] = value >= 0.5;
                    let (neg, pos) = if axis == code::ABS_HAT0Y {
                        (p.dpad[0], p.dpad[1])
                    } else {
                        (p.dpad[2], p.dpad[3])
                    };
                    vec![InputEvent::new(
                        EventType::ABSOLUTE.0,
                        axis,
                        hat_value(neg, pos),
                    )]
                }
                None => return,
            },
            GAMEPAD_AXIS => match map_axis(index) {
                Some(c) => vec![InputEvent::new(
                    EventType::ABSOLUTE.0,
                    c,
                    scale(value, STICK_MAX, true),
                )],
                None => return,
            },
            _ => return,
        };
        if let Err(e) = p.device.emit(&events) {
            tracing::warn!("gamepad {pad}: emit failed: {e}");
        }
    }

    pub fn release_all() {
        if let Some(pads) = PADS.lock().unwrap().as_mut()
            && !pads.is_empty()
        {
            tracing::info!("gamepads: released {}", pads.len());
            pads.clear();
        }
    }

    fn build(pad: u8) -> std::io::Result<VirtualDevice> {
        use code::*;
        let keys: AttributeSet<KeyCode> = [
            BTN_SOUTH, BTN_EAST, BTN_X, BTN_Y, BTN_TL, BTN_TR, BTN_SELECT, BTN_START, BTN_MODE,
            BTN_THUMBL, BTN_THUMBR,
        ]
        .into_iter()
        .map(KeyCode::new)
        .collect();
        let abs = |c: u16, min, max, flat| {
            UinputAbsSetup::new(AbsoluteAxisCode(c), AbsInfo::new(0, min, max, 16, flat, 0))
        };
        let stick = |c| abs(c, -STICK_MAX - 1, STICK_MAX, 128);
        let trigger = |c| abs(c, 0, TRIGGER_MAX, 0);
        let hat = |c| abs(c, -1, 1, 0);
        VirtualDevice::builder()?
            .name(&format!("Glass controller {}", pad + 1))
            // An Xbox 360 controller's ids, so SDL and Steam map it with no setup.
            .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
            .with_keys(&keys)?
            .with_absolute_axis(&stick(ABS_X))?
            .with_absolute_axis(&stick(ABS_Y))?
            .with_absolute_axis(&stick(ABS_RX))?
            .with_absolute_axis(&stick(ABS_RY))?
            .with_absolute_axis(&trigger(ABS_Z))?
            .with_absolute_axis(&trigger(ABS_RZ))?
            .with_absolute_axis(&hat(ABS_HAT0X))?
            .with_absolute_axis(&hat(ABS_HAT0Y))?
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_mapping() {
        assert_eq!(map_button(0), Some(Button::Key(code::BTN_SOUTH)));
        assert_eq!(map_button(3), Some(Button::Key(code::BTN_Y)));
        assert_eq!(map_button(6), Some(Button::Trigger(code::ABS_Z)));
        assert_eq!(map_button(7), Some(Button::Trigger(code::ABS_RZ)));
        assert_eq!(map_button(12), Some(Button::Hat(code::ABS_HAT0Y, -1)));
        assert_eq!(map_button(15), Some(Button::Hat(code::ABS_HAT0X, 1)));
        assert_eq!(map_button(16), Some(Button::Key(code::BTN_MODE)));
        assert_eq!(map_button(17), None);
        assert_eq!(map_axis(0), Some(code::ABS_X));
        assert_eq!(map_axis(3), Some(code::ABS_RY));
        assert_eq!(map_axis(4), None);
    }

    #[test]
    fn values_scale_and_clamp() {
        assert_eq!(scale(1.0, STICK_MAX, true), STICK_MAX);
        assert_eq!(scale(-2.0, STICK_MAX, true), -STICK_MAX);
        assert_eq!(scale(0.5, TRIGGER_MAX, false), 128);
        assert_eq!(scale(-0.5, TRIGGER_MAX, false), 0);
        assert_eq!(hat_value(true, false), -1);
        assert_eq!(hat_value(false, true), 1);
        assert_eq!(hat_value(true, true), 0);
    }

    #[test]
    fn xusb_reports_follow_the_standard_mapping() {
        use glass_proto::{GAMEPAD_AXIS, GAMEPAD_BUTTON, GAMEPAD_CONNECTED};
        let mut p = xusb::Pad::default();
        assert!(p.apply(GAMEPAD_BUTTON, 0, 1.0));
        assert!(p.apply(GAMEPAD_BUTTON, 16, 1.0));
        assert_eq!(p.report.buttons, xusb::A | xusb::GUIDE);
        assert!(!p.apply(GAMEPAD_BUTTON, 0, 0.9), "no change, no report");
        assert!(p.apply(GAMEPAD_BUTTON, 0, 0.0));
        assert_eq!(p.report.buttons, xusb::GUIDE);
        p.apply(GAMEPAD_BUTTON, 8, 1.0);
        p.apply(GAMEPAD_BUTTON, 9, 1.0);
        assert_eq!(p.report.buttons, xusb::GUIDE | xusb::BACK | xusb::START);
        p.apply(GAMEPAD_BUTTON, 6, 0.5);
        p.apply(GAMEPAD_BUTTON, 7, 1.0);
        assert_eq!((p.report.left_trigger, p.report.right_trigger), (128, 255));
        // Up and down at once: neither.
        p.apply(GAMEPAD_BUTTON, 12, 1.0);
        assert_eq!(p.report.buttons & 0xf, xusb::DPAD_UP);
        p.apply(GAMEPAD_BUTTON, 13, 1.0);
        p.apply(GAMEPAD_BUTTON, 15, 1.0);
        assert_eq!(p.report.buttons & 0xf, xusb::DPAD_RIGHT);
        // The browser's +y is down; XInput's is up.
        p.apply(GAMEPAD_AXIS, 0, -1.0);
        p.apply(GAMEPAD_AXIS, 1, 1.0);
        p.apply(GAMEPAD_AXIS, 3, -0.5);
        assert_eq!((p.report.lx, p.report.ly, p.report.ry), (-32767, -32767, 16384));
        assert!(!p.apply(GAMEPAD_AXIS, 4, 1.0));
        assert!(!p.apply(GAMEPAD_BUTTON, 17, 1.0));
        assert!(!p.apply(GAMEPAD_CONNECTED, 0, 1.0));
        // Every Linux button has an XUSB one.
        for i in (0..=11).chain([16]) {
            if let Some(Button::Key(c)) = map_button(i) {
                assert!(xusb::button(c).is_some(), "button {i}");
            }
        }
    }

    #[test]
    fn vigem_requests_are_laid_out_as_the_driver_reads_them() {
        assert_eq!(xusb::PLUGIN_TARGET, 0x2a_a004);
        assert_eq!(xusb::UNPLUG_TARGET, 0x2a_a008);
        assert_eq!(xusb::CHECK_VERSION, 0x2a_a00c);
        assert_eq!(xusb::WAIT_DEVICE_READY, 0x2a_a010);
        assert_eq!(xusb::SUBMIT_REPORT, 0x2a_a808);
        assert_eq!(xusb::plugin(3), [16, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0x5e, 0x04, 0x8e, 0x02]);
        assert_eq!(xusb::serial(1), [8, 0, 0, 0, 1, 0, 0, 0]);
        let r = xusb::Report { buttons: 0x1001, left_trigger: 1, right_trigger: 2, lx: -1, ly: 1, rx: 0, ry: 256 };
        assert_eq!(r.to_bytes(), [0x01, 0x10, 1, 2, 0xff, 0xff, 1, 0, 0, 0, 0, 1]);
        let s = xusb::submit(2, r);
        assert_eq!(&s[..8], &[20, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(&s[8..], &r.to_bytes());
    }
}
