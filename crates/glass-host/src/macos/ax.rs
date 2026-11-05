//! Apps as Accessibility sees them: an app's window as a tree of elements (role, name,
//! value, where it is), and pressing or filling in one of them without moving the pointer
//! or typing. What `crate::elements` turns into snapshots and refs for agents.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Instant;

use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{CFArray, CFBoolean, CFNumber, CFRetained, CFString};

use crate::elements::{Budget, Found, Node, Now, Outcome, Walk};

type AXError = i32;
const AX_OK: AXError = 0;
const AX_INVALID_ELEMENT: AXError = -25202;
const AX_CANNOT_COMPLETE: AXError = -25204;
const AX_ACTION_UNSUPPORTED: AXError = -25206;
const AX_API_DISABLED: AXError = -25211;

/// AXValue types.
const AX_POINT: u32 = 1;
const AX_SIZE: u32 = 2;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementCopyAttributeValue(element: *const c_void, attribute: *const c_void, value: *mut *const c_void) -> AXError;
    fn AXUIElementCopyMultipleAttributeValues(element: *const c_void, attributes: *const c_void, options: u32, values: *mut *const c_void) -> AXError;
    fn AXUIElementCopyActionNames(element: *const c_void, names: *mut *const c_void) -> AXError;
    fn AXUIElementPerformAction(element: *const c_void, action: *const c_void) -> AXError;
    fn AXUIElementSetAttributeValue(element: *const c_void, attribute: *const c_void, value: *const c_void) -> AXError;
    fn AXUIElementIsAttributeSettable(element: *const c_void, attribute: *const c_void, settable: *mut u8) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, seconds: f32) -> AXError;
    fn AXUIElementGetPid(element: *const c_void, pid: *mut i32) -> AXError;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetType(value: *const c_void) -> u32;
    fn AXValueGetValue(value: *const c_void, kind: u32, out: *mut c_void) -> u8;
    fn AXValueCreate(kind: u32, value: *const c_void) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFEqual(a: *const c_void, b: *const c_void) -> bool;
    fn CFStringGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
}

/// A CoreFoundation object we hold a reference to.
struct Cf(NonNull<c_void>);

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { CFRelease(self.0.as_ptr()) }
    }
}

impl Cf {
    /// Takes over a reference the caller owns (a Copy/Create result).
    fn owned(ptr: *const c_void) -> Option<Cf> {
        NonNull::new(ptr as *mut c_void).map(Cf)
    }

    /// Retains a reference someone else owns (an array's item).
    fn retained(ptr: *const c_void) -> Option<Cf> {
        // SAFETY: a valid CF object, or null (skipped).
        NonNull::new(ptr as *mut c_void).map(|p| unsafe { Cf(NonNull::new_unchecked(CFRetain(p.as_ptr()) as *mut c_void)) })
    }

    fn ptr(&self) -> *const c_void {
        self.0.as_ptr()
    }

    fn is(&self, type_id: usize) -> bool {
        // SAFETY: a valid CF object.
        unsafe { CFGetTypeID(self.ptr()) == type_id }
    }

    fn string(&self) -> Option<String> {
        // SAFETY: checked to be a CFString first.
        self.is(unsafe { CFStringGetTypeID() }).then(|| unsafe { &*(self.ptr() as *const CFString) }.to_string())
    }

    fn boolean(&self) -> Option<bool> {
        // SAFETY: checked to be a CFBoolean first.
        self.is(unsafe { CFBooleanGetTypeID() }).then(|| unsafe { &*(self.ptr() as *const CFBoolean) }.as_bool())
    }

    fn number(&self) -> Option<f64> {
        // SAFETY: checked to be a CFNumber first.
        if self.is(unsafe { CFNumberGetTypeID() }) { unsafe { &*(self.ptr() as *const CFNumber) }.as_f64() } else { None }
    }

    /// An array's items (nothing for anything else).
    fn items(&self) -> Vec<Cf> {
        // SAFETY: checked to be a CFArray first; items are retained before use.
        unsafe {
            if !self.is(CFArrayGetTypeID()) {
                return Vec::new();
            }
            (0..CFArrayGetCount(self.ptr())).filter_map(|i| Cf::retained(CFArrayGetValueAtIndex(self.ptr(), i))).collect()
        }
    }

    /// An AXValue holding a point or size, as (x, y) or (width, height).
    fn pair(&self, kind: u32) -> Option<(f64, f64)> {
        // SAFETY: checked to be an AXValue of `kind` (a CGPoint or CGSize: two f64s).
        unsafe {
            if !self.is(AXValueGetTypeID()) || AXValueGetType(self.ptr()) != kind {
                return None;
            }
            let mut p = Point::default();
            (AXValueGetValue(self.ptr(), kind, (&mut p as *mut Point).cast()) != 0).then_some((p.x, p.y))
        }
    }

    /// Text for a value: a string, a number (sliders, checkboxes) or a yes/no.
    fn text(&self) -> Option<String> {
        self.string().or_else(|| self.number().map(|n| if n.fract() == 0.0 { format!("{n:.0}") } else { format!("{n}") }))
            .or_else(|| self.boolean().map(|b| if b { "1".into() } else { "0".into() }))
    }
}

fn name(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

fn cf_ptr<T>(r: &CFRetained<T>) -> *const c_void {
    let r: &T = r;
    (r as *const T).cast()
}

/// An element of some app's interface, kept for acting on later.
pub struct Element(Cf);

// SAFETY: AXUIElements are immutable CF objects (retain counts are atomic), and the
// Accessibility API may be called from any thread.
unsafe impl Send for Element {}
unsafe impl Sync for Element {}

/// How long one question to an app may take before it counts as not answering.
const APP_TIMEOUT: f32 = 1.0;

impl Element {
    fn from(cf: Cf) -> Element {
        // SAFETY: an AXUIElement; the timeout is a setting kept on this reference.
        unsafe { AXUIElementSetMessagingTimeout(cf.ptr(), APP_TIMEOUT) };
        Element(cf)
    }

    fn attr(&self, attribute: &str) -> Result<Cf, AXError> {
        let mut out: *const c_void = std::ptr::null();
        let n = name(attribute);
        // SAFETY: valid element and attribute name; a returned value is ours to release.
        let err = unsafe { AXUIElementCopyAttributeValue(self.0.ptr(), cf_ptr(&n), &mut out) };
        if err == AX_OK { Cf::owned(out).ok_or(err) } else { Err(err) }
    }

    /// Several attributes in one round trip; `None` for each the element doesn't have.
    fn attrs(&self, names: &CFRetained<CFArray<CFString>>, count: usize) -> Result<Vec<Option<Cf>>, AXError> {
        let mut out: *const c_void = std::ptr::null();
        // SAFETY: as `attr`; the array of values is ours to release.
        let err = unsafe { AXUIElementCopyMultipleAttributeValues(self.0.ptr(), cf_ptr(names), 0, &mut out) };
        if err != AX_OK {
            return Err(err);
        }
        let values = Cf::owned(out).ok_or(err)?.items();
        // A missing attribute comes back as an AXValue holding an error.
        let missing = |v: &Cf| v.is(unsafe { AXValueGetTypeID() }) && unsafe { AXValueGetType(v.ptr()) } == 5;
        let mut values: Vec<Option<Cf>> = values.into_iter().map(|v| (!missing(&v)).then_some(v)).collect();
        values.resize_with(count, || None);
        Ok(values)
    }

    fn actions(&self) -> Vec<String> {
        let mut out: *const c_void = std::ptr::null();
        // SAFETY: as `attr`.
        let err = unsafe { AXUIElementCopyActionNames(self.0.ptr(), &mut out) };
        if err != AX_OK {
            return Vec::new();
        }
        Cf::owned(out).map(|a| a.items().iter().filter_map(Cf::string).collect()).unwrap_or_default()
    }

    fn settable(&self, attribute: &str) -> bool {
        let mut yes = 0u8;
        let n = name(attribute);
        // SAFETY: valid element and attribute name.
        unsafe { AXUIElementIsAttributeSettable(self.0.ptr(), cf_ptr(&n), &mut yes) == AX_OK && yes != 0 }
    }

    fn perform(&self, action: &str) -> AXError {
        let n = name(action);
        // SAFETY: valid element and action name.
        unsafe { AXUIElementPerformAction(self.0.ptr(), cf_ptr(&n)) }
    }

    /// Sets a point or size attribute.
    fn set_pair(&self, attribute: &str, kind: u32, a: f64, b: f64) -> AXError {
        let p = Point { x: a, y: b };
        // SAFETY: a CGPoint or CGSize (two f64s) for `kind`; the new AXValue is released here.
        let Some(value) = Cf::owned(unsafe { AXValueCreate(kind, (&p as *const Point).cast()) }) else {
            return AX_CANNOT_COMPLETE;
        };
        self.set(attribute, value.ptr())
    }

    fn set(&self, attribute: &str, value: *const c_void) -> AXError {
        let n = name(attribute);
        // SAFETY: valid element, attribute name and CF value.
        unsafe { AXUIElementSetAttributeValue(self.0.ptr(), cf_ptr(&n), value) }
    }

    fn role(&self) -> Option<String> {
        self.attr("AXRole").ok().and_then(|r| r.string())
    }

    /// A window its app still lists among its windows.
    fn listed(&self) -> bool {
        let mut pid = 0;
        // SAFETY: valid element; a new reference to the app's element, released on drop.
        if unsafe { AXUIElementGetPid(self.0.ptr(), &mut pid) } != AX_OK {
            return true;
        }
        let Some(app) = Cf::owned(unsafe { AXUIElementCreateApplication(pid) }).map(Element::from) else { return true };
        match app.attr("AXWindows") {
            // SAFETY: both valid CF objects.
            Ok(list) => list.items().iter().any(|w| unsafe { CFEqual(w.ptr(), self.0.ptr()) }),
            // Can't tell: don't call it gone.
            Err(_) => true,
        }
    }

    fn children(&self) -> Vec<Element> {
        self.attr("AXChildren").map(|c| c.items().into_iter().map(Element::from).collect()).unwrap_or_default()
    }
}

/// What every node is asked, in one round trip.
const ATTRS: [&str; 11] = [
    "AXRole", "AXSubrole", "AXTitle", "AXDescription", "AXValue", "AXEnabled", "AXFocused",
    "AXPosition", "AXSize", "AXChildren", "AXPlaceholderValue",
];

/// Roles an agent can act on whatever they say about themselves.
const INTERACTIVE: &[&str] = &[
    "AXButton", "AXCheckBox", "AXRadioButton", "AXPopUpButton", "AXMenuButton", "AXMenuItem",
    "AXMenuBarItem", "AXLink", "AXTextField", "AXTextArea", "AXComboBox", "AXSlider",
    "AXIncrementor", "AXDisclosureTriangle", "AXColorWell", "AXSegmentedControl", "AXDockItem",
];

/// Roles whose value is text someone can replace.
const EDITABLE: &[&str] = &["AXTextField", "AXTextArea", "AXComboBox"];

/// Roles that only act if they say so (web pages put a press on clickable groups).
const MAYBE_ACTIVE: &[&str] = &["AXGroup", "AXStaticText", "AXImage", "AXCell", "AXRow", "AXOutlineRow", "AXHeading"];

/// The short role name agents see: "AXPopUpButton" is "popupbutton".
pub fn short_role(role: &str, subrole: Option<&str>) -> String {
    match (role, subrole) {
        (_, Some("AXSecureTextField")) => "passwordfield".into(),
        (_, Some("AXSearchField")) => "searchfield".into(),
        (_, Some("AXSwitch")) => "switch".into(),
        (_, Some("AXTabButton")) => "tab".into(),
        (_, Some("AXCloseButton")) => "closebutton".into(),
        (_, Some("AXMinimizeButton")) => "minimizebutton".into(),
        (_, Some("AXFullScreenButton" | "AXZoomButton")) => "zoombutton".into(),
        ("AXStaticText", _) => "text".into(),
        ("AXRadioButton", _) => "radio".into(),
        (r, _) => r.trim_start_matches("AX").to_lowercase(),
    }
}

struct Walker {
    names: CFRetained<CFArray<CFString>>,
    budget: Budget,
    read: usize,
    partial: bool,
    handles: Vec<Element>,
}

impl Walker {
    fn new(budget: Budget) -> Walker {
        let names: Vec<CFRetained<CFString>> = ATTRS.iter().map(|a| name(a)).collect();
        Walker { names: CFArray::from_retained_objects(&names), budget, read: 0, partial: false, handles: Vec::new() }
    }

    fn out_of_budget(&self) -> bool {
        self.read >= self.budget.nodes || Instant::now() >= self.budget.until
    }

    /// One element and what's under it; `None` when it's gone or not worth showing.
    fn node(&mut self, el: Element, depth: usize) -> Option<Node> {
        self.read += 1;
        let v = el.attrs(&self.names, ATTRS.len()).ok()?;
        let s = |i: usize| v[i].as_ref().and_then(Cf::string).filter(|s| !s.trim().is_empty());
        let role = s(0)?;
        let subrole = s(1);
        let title = s(2);
        let description = s(3);
        let value = v[4].as_ref().and_then(Cf::text);
        let enabled = v[5].as_ref().and_then(Cf::boolean).unwrap_or(true);
        let focused = v[6].as_ref().and_then(Cf::boolean).unwrap_or(false);
        let position = v[7].as_ref().and_then(|p| p.pair(AX_POINT));
        let size = v[8].as_ref().and_then(|p| p.pair(AX_SIZE));
        let kids: Vec<Element> = v[9].as_ref().map(|c| c.items().into_iter().map(Element::from).collect()).unwrap_or_default();
        let placeholder = s(10);

        let secret = subrole.as_deref() == Some("AXSecureTextField");
        let text_like = matches!(role.as_str(), "AXStaticText" | "AXHeading");
        let name = title
            .or(description)
            .or_else(|| text_like.then(|| value.clone()).flatten())
            .or_else(|| placeholder.map(|p| format!("{p} (placeholder)")));
        let frame = position.zip(size).map(|((x, y), (w, h))| [x, y, w, h]);
        // Nothing to see: zero-sized and empty.
        if frame.is_some_and(|f| f[2] <= 0.0 || f[3] <= 0.0) && kids.is_empty() {
            return None;
        }

        let editable = EDITABLE.contains(&role.as_str()) && el.settable("AXValue");
        let mut press = INTERACTIVE.contains(&role.as_str()) && !EDITABLE.contains(&role.as_str());
        if !press && name.is_some() && MAYBE_ACTIVE.contains(&role.as_str()) {
            let actions = el.actions();
            press = actions.iter().any(|a| a == "AXPress") || (matches!(role.as_str(), "AXRow" | "AXCell" | "AXOutlineRow") && el.settable("AXSelected"));
        }
        let shown_value = if text_like || secret { None } else { value.filter(|v| !v.is_empty()) };

        // Unnamed groups that do nothing are scaffolding (web pages nest dozens): they don't
        // count against the depth limit.
        let scaffolding = name.is_none() && !press && !editable && matches!(role.as_str(), "AXGroup" | "AXSplitGroup" | "AXScrollArea" | "AXLayoutArea" | "AXUnknown");
        let next = if scaffolding { depth } else { depth + 1 };
        let mut node = Node {
            role: short_role(&role, subrole.as_deref()),
            name,
            value: shown_value,
            secret,
            enabled,
            focused,
            editable,
            cut: false,
            handle: None,
            children: Vec::new(),
        };
        if press || editable {
            node.handle = Some(self.handles.len());
            self.handles.push(el);
        }
        // A menu bar's menus only while one is open (closed, they'd list every command).
        let closed_menu = role == "AXMenuBarItem" && !node.handle.and_then(|h| self.handles[h].attr("AXSelected").ok()).and_then(|s| s.boolean()).unwrap_or(false);
        if !kids.is_empty() && !closed_menu {
            if next > self.budget.depth || self.out_of_budget() {
                node.cut = true;
                self.partial = true;
            } else {
                for kid in kids {
                    if self.out_of_budget() {
                        node.cut = true;
                        self.partial = true;
                        break;
                    }
                    if let Some(n) = self.node(kid, next) {
                        node.children.push(n);
                    }
                }
            }
        }
        Some(node)
    }
}

/// Shrinks and moves every window that sits on `display` (its center there) but doesn't
/// fit inside `area` (the display less the menu bar), so nothing hangs off a display that
/// just got smaller. Global points; returns how many windows changed.
pub fn fit_windows(display: [f64; 4], area: [f64; 4]) -> usize {
    let [dx, dy, dw, dh] = display;
    let mut moved = 0;
    for app in NSWorkspace::sharedWorkspace().runningApplications().iter() {
        if app.activationPolicy() != objc2_app_kit::NSApplicationActivationPolicy::Regular {
            continue;
        }
        let Some(root) = Cf::owned(unsafe { AXUIElementCreateApplication(app.processIdentifier()) }).map(Element::from) else {
            continue;
        };
        let Ok(windows) = root.attr("AXWindows") else { continue };
        for window in windows.items().into_iter().map(Element::from) {
            let flag = |a: &str| window.attr(a).ok().and_then(|v| v.boolean()).unwrap_or(false);
            if flag("AXMinimized") || flag("AXFullScreen") {
                continue;
            }
            let pos = window.attr("AXPosition").ok().and_then(|p| p.pair(AX_POINT));
            let size = window.attr("AXSize").ok().and_then(|p| p.pair(AX_SIZE));
            let Some(((x, y), (w, h))) = pos.zip(size) else { continue };
            let (cx, cy) = (x + w / 2.0, y + h / 2.0);
            if cx < dx || cy < dy || cx >= dx + dw || cy >= dy + dh {
                continue;
            }
            let Some([fx, fy, fw, fh]) = fitted([x, y, w, h], area) else { continue };
            if (fw, fh) != (w, h) {
                window.set_pair("AXSize", AX_SIZE, fw, fh);
            }
            window.set_pair("AXPosition", AX_POINT, fx, fy);
            moved += 1;
        }
    }
    moved
}

/// Where a window at `frame` goes to fit inside `area`: no bigger than it, and moved in
/// just enough. `None` when it already fits.
fn fitted(frame: [f64; 4], area: [f64; 4]) -> Option<[f64; 4]> {
    let [x, y, w, h] = frame;
    let [ax, ay, aw, ah] = area;
    let (fw, fh) = (w.min(aw), h.min(ah));
    let fx = x.clamp(ax, ax + aw - fw);
    let fy = y.clamp(ay, ay + ah - fh);
    let out = [fx, fy, fw, fh];
    (out != frame).then_some(out)
}

/// The app meant: by bundle id or name, or the one in front.
pub fn find_app(query: Option<&str>) -> Result<Found, String> {
    let workspace = NSWorkspace::sharedWorkspace();
    let app = match query.map(str::trim).filter(|q| !q.is_empty()) {
        None => workspace.frontmostApplication().ok_or("no app is in front")?,
        Some(q) => {
            let lower = q.to_lowercase();
            workspace
                .runningApplications()
                .iter()
                .find(|a| {
                    a.bundleIdentifier().is_some_and(|b| b.to_string().to_lowercase() == lower)
                        || a.localizedName().is_some_and(|n| n.to_string().to_lowercase() == lower)
                })
                .ok_or_else(|| format!("no app called {q:?} is running"))?
        }
    };
    let pid = app.processIdentifier();
    Ok(Found {
        pid,
        bundle: app.bundleIdentifier().map(|b| b.to_string()).unwrap_or_default(),
        name: app.localizedName().map(|n| n.to_string()).unwrap_or_default(),
    })
}

/// An app's focused window (or every window), any menu it has open, and its menu bar, as
/// a tree.
pub fn walk(app: &Found, all_windows: bool, budget: Budget) -> Result<Walk, String> {
    // SAFETY: a new reference to the app's element, ours to release.
    let root = Element::from(Cf::owned(unsafe { AXUIElementCreateApplication(app.pid) }).ok_or("can't read that app")?);
    // Chromium and Electron apps build their tree only when asked for it.
    root.set("AXManualAccessibility", (CFBoolean::new(true) as *const CFBoolean).cast());
    let name = if app.name.is_empty() { "That app" } else { &app.name };
    // Busy, or just launched: it can take a while to answer.
    let busy = || format!("{name} isn't answering Accessibility right now (busy, or just starting). Try again in a moment, or use screenshot.");
    let windows: Vec<Element> = if all_windows {
        match root.attr("AXWindows") {
            Ok(w) => w.items().into_iter().map(Element::from).collect(),
            Err(AX_CANNOT_COMPLETE) => return Err(busy()),
            Err(e) => return Err(why(e)),
        }
    } else {
        match root.attr("AXFocusedWindow").or_else(|_| root.attr("AXMainWindow")) {
            Ok(w) => vec![Element::from(w)],
            Err(AX_API_DISABLED) => return Err(why(AX_API_DISABLED)),
            Err(AX_CANNOT_COMPLETE) => return Err(busy()),
            Err(_) => match root.attr("AXWindows") {
                Ok(w) => w.items().into_iter().take(1).map(Element::from).collect(),
                Err(AX_CANNOT_COMPLETE) => return Err(busy()),
                Err(_) => Vec::new(),
            },
        }
    };
    // While the screen is locked (and for windows on another Space), the app answers with
    // itself in place of its windows.
    let windows = windows.into_iter().filter(|w| w.role().is_some_and(|r| r != "AXApplication"));
    // Context menus and pop-up menus hang off the app, not a window.
    let menus: Vec<Element> = root.children().into_iter().filter(|c| c.role().as_deref() == Some("AXMenu")).collect();
    let mut walker = Walker::new(budget);
    let mut roots = Vec::new();
    for el in windows.chain(menus) {
        if walker.out_of_budget() {
            walker.partial = true;
            break;
        }
        if let Some(n) = walker.node(el, 0) {
            roots.push(n);
        }
    }
    if roots.is_empty() {
        return Err(format!("{name} has no window Accessibility can see: none open, on another Space, or the screen is locked. Use screenshot."));
    }
    // Last, so the window comes first when there's more than fits.
    if let Ok(bar) = root.attr("AXMenuBar")
        && !walker.out_of_budget()
        && let Some(n) = walker.node(Element::from(bar), 0)
    {
        roots.push(n);
    }
    Ok(Walk { roots, handles: walker.handles, partial: walker.partial })
}

fn why(err: AXError) -> String {
    match err {
        AX_API_DISABLED => "Glass isn't allowed to use Accessibility (System Settings › Privacy & Security › Accessibility)".into(),
        AX_CANNOT_COMPLETE => "the app isn't answering".into(),
        e => format!("Accessibility error {e}"),
    }
}

/// What a kept element is now; `None` once it's gone.
pub fn look(el: &Element) -> Option<Now> {
    let role = el.attr("AXRole").ok()?.string()?;
    // A closed window's elements go on answering for themselves, and so, while its app
    // holds on to it, does the window: it's gone once its app no longer lists it.
    if let Ok(window) = el.attr("AXWindow") {
        let window = Element::from(window);
        match window.role().as_deref() {
            None => return None,
            Some("AXWindow") if !window.listed() => return None,
            _ => {}
        }
    }
    let subrole = el.attr("AXSubrole").ok().and_then(|s| s.string());
    let text = |a: &str| el.attr(a).ok().and_then(|v| v.string()).filter(|s| !s.trim().is_empty());
    let text_like = matches!(role.as_str(), "AXStaticText" | "AXHeading");
    let name = text("AXTitle")
        .or_else(|| text("AXDescription"))
        .or_else(|| if text_like { text("AXValue") } else { None })
        .or_else(|| text("AXPlaceholderValue").map(|p| format!("{p} (placeholder)")));
    let enabled = el.attr("AXEnabled").ok().and_then(|e| e.boolean()).unwrap_or(true);
    let pos = el.attr("AXPosition").ok().and_then(|p| p.pair(AX_POINT));
    let size = el.attr("AXSize").ok().and_then(|p| p.pair(AX_SIZE));
    let frame = pos.zip(size).map(|((x, y), (w, h))| [x, y, w, h]);
    Some(Now { role: short_role(&role, subrole.as_deref()), name, enabled, frame })
}

/// Presses an element: its press action, or whichever it has of confirm, pick and open, or
/// selecting it (rows and cells).
pub fn press(el: &Element) -> Outcome {
    let actions = el.actions();
    for action in ["AXPress", "AXConfirm", "AXPick", "AXOpen"] {
        if actions.iter().any(|a| a == action) {
            return outcome(el.perform(action));
        }
    }
    if el.settable("AXSelected") {
        return outcome(el.set("AXSelected", (CFBoolean::new(true) as *const CFBoolean).cast()));
    }
    Outcome::Unsupported
}

/// Replaces a field's text, focusing it first as typing would.
pub fn set_value(el: &Element, text: &str) -> Outcome {
    if el.settable("AXFocused") {
        el.set("AXFocused", (CFBoolean::new(true) as *const CFBoolean).cast());
    }
    let value = name(text);
    outcome(el.set("AXValue", cf_ptr(&value)))
}

/// A field's text now (to check what was set), unless it's a password.
pub fn value(el: &Element) -> Option<String> {
    el.attr("AXValue").ok().and_then(|v| v.text())
}

/// What an Accessibility call's result says about whether the app did it.
fn outcome(err: AXError) -> Outcome {
    match err {
        AX_OK => Outcome::Done,
        AX_ACTION_UNSUPPORTED => Outcome::Unsupported,
        AX_INVALID_ELEMENT => Outcome::Gone,
        // The app took too long to reply: it may be busy doing it (a dialog it opened
        // blocks the reply), or it may not have.
        AX_CANNOT_COMPLETE => Outcome::Unconfirmed,
        e => Outcome::Failed(why(e)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn windows_shrink_and_move_inside() {
        let area = [0.0, 25.0, 1590.0, 1135.0];
        // Fits already: left alone.
        assert_eq!(super::fitted([100.0, 100.0, 800.0, 600.0], area), None);
        // Bigger than the display: as big as it, at its corner.
        assert_eq!(super::fitted([0.0, 25.0, 2560.0, 1415.0], area), Some([0.0, 25.0, 1590.0, 1135.0]));
        // Hanging off the right and bottom: moved in, same size.
        assert_eq!(super::fitted([1200.0, 900.0, 800.0, 400.0], area), Some([790.0, 760.0, 800.0, 400.0]));
    }

    use super::*;

    #[test]
    fn roles_read_short() {
        assert_eq!(short_role("AXPopUpButton", None), "popupbutton");
        assert_eq!(short_role("AXTextField", Some("AXSecureTextField")), "passwordfield");
        assert_eq!(short_role("AXStaticText", None), "text");
        assert_eq!(short_role("AXRadioButton", Some("AXTabButton")), "tab");
    }
}

