//! What an agent is about to touch: the app that owns the element under a point (or that
//! has the keyboard), asked of Accessibility at the moment of input, since Spotlight,
//! Control Center, menu extras and the Dock take input without becoming the front app;
//! whether a password field has the keyboard (secure input); and whether the person at the
//! computer is using its own keyboard or mouse.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::NSString;

type AXUIElementRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyElementAtPosition(app: AXUIElementRef, x: f32, y: f32, element: *mut AXUIElementRef) -> i32;
    fn AXUIElementCopyAttributeValue(element: AXUIElementRef, attribute: *const c_void, value: *mut *const c_void) -> i32;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> i32;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

const HID_SYSTEM_STATE: i32 = 1;
const ANY_INPUT: u32 = u32::MAX;

pub fn bundle_of(pid: i32) -> Option<String> {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .and_then(|a| a.bundleIdentifier())
        .map(|id| id.to_string())
}

/// The front app's bundle id.
pub fn frontmost_app() -> Option<String> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|a| a.bundleIdentifier())
        .map(|id| id.to_string())
}

/// The app owning what's under global point (`x`, `y`, in points): what a click there
/// reaches. Falls back to the front app if Accessibility can't say.
pub fn app_at(x: f64, y: f64) -> Option<String> {
    // SAFETY: AX calls on the system-wide element; every returned reference is released.
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return frontmost_app();
        }
        let mut element: AXUIElementRef = std::ptr::null();
        let found = AXUIElementCopyElementAtPosition(system, x as f32, y as f32, &mut element) == 0 && !element.is_null();
        let mut pid = 0;
        let ok = found && AXUIElementGetPid(element, &mut pid) == 0;
        if !element.is_null() {
            CFRelease(element);
        }
        CFRelease(system);
        if ok { bundle_of(pid).or_else(frontmost_app) } else { frontmost_app() }
    }
}

/// The app that has the keyboard (Spotlight's field counts, though it's never the front app).
pub fn key_app() -> Option<String> {
    // SAFETY: as above; the attribute name is an NSString, toll-free bridged to CFString.
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return frontmost_app();
        }
        let name = NSString::from_str("AXFocusedApplication");
        let mut app: *const c_void = std::ptr::null();
        let found = AXUIElementCopyAttributeValue(system, (&*name as *const NSString).cast(), &mut app) == 0 && !app.is_null();
        let mut pid = 0;
        let ok = found && AXUIElementGetPid(app, &mut pid) == 0;
        if !app.is_null() {
            CFRelease(app);
        }
        CFRelease(system);
        if ok { bundle_of(pid).or_else(frontmost_app) } else { frontmost_app() }
    }
}

/// Some app turned on secure input (a password field has focus): typing is private.
pub fn secure_input() -> bool {
    // SAFETY: a plain query with no arguments.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// When Glass last posted input (ms since the epoch): its own events count as input to
/// macOS too, so they're told apart from the person's by time.
static INJECTED_AT: AtomicU64 = AtomicU64::new(0);

fn epoch_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub fn injected() {
    INJECTED_AT.store(epoch_ms(), Ordering::Relaxed);
}

/// Seconds since someone used this computer's own keyboard, mouse or trackpad (not input
/// Glass posted), if that's recent enough to tell.
pub fn local_input_age() -> Option<f64> {
    // SAFETY: a plain query.
    let idle = unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, ANY_INPUT) };
    let since_injected = epoch_ms().saturating_sub(INJECTED_AT.load(Ordering::Relaxed)) as f64 / 1000.0;
    // The last input came well after Glass's last event: a person.
    (idle >= 0.0 && since_injected > idle + 0.25).then_some(idle)
}
