//! Apps as UI Automation sees them: an app's window as a tree of elements (role, name,
//! value, where it is), and pressing or filling in one of them without moving the pointer
//! or typing. What `crate::elements` turns into snapshots and refs for agents, as
//! `macos/ax.rs` does with Accessibility.

use std::sync::{Once, OnceLock};
use std::time::Instant;

use windows::Win32::Foundation::{E_NOTIMPL, HWND, LPARAM, RECT};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, ExpandCollapseState_Collapsed, ExpandCollapseState_PartiallyExpanded, IUIAutomation,
    IUIAutomation2, IUIAutomationCacheRequest, IUIAutomationElement, IUIAutomationExpandCollapsePattern,
    IUIAutomationInvokePattern, IUIAutomationLegacyIAccessiblePattern, IUIAutomationRangeValuePattern,
    IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, IUIAutomationTreeWalker, IUIAutomationValuePattern,
    ToggleState_Indeterminate, ToggleState_On, UIA_BoundingRectanglePropertyId, UIA_ControlTypePropertyId,
    UIA_E_ELEMENTNOTAVAILABLE, UIA_E_ELEMENTNOTENABLED, UIA_E_NOTSUPPORTED, UIA_E_TIMEOUT,
    UIA_ExpandCollapseExpandCollapseStatePropertyId, UIA_ExpandCollapsePatternId, UIA_HasKeyboardFocusPropertyId,
    UIA_HelpTextPropertyId, UIA_InvokePatternId, UIA_IsEnabledPropertyId, UIA_IsPasswordPropertyId,
    UIA_LegacyIAccessiblePatternId, UIA_NamePropertyId, UIA_PATTERN_ID, UIA_PROPERTY_ID, UIA_RangeValuePatternId,
    UIA_RangeValueValuePropertyId, UIA_SelectionItemIsSelectedPropertyId, UIA_SelectionItemPatternId,
    UIA_TogglePatternId, UIA_ToggleToggleStatePropertyId, UIA_ValueIsReadOnlyPropertyId, UIA_ValuePatternId,
    UIA_ValueValuePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW, IsWindowVisible,
};
use windows::core::{BOOL, Interface};

use super::{convert, front};
use crate::elements::{Budget, Found, Node, Now, Outcome, Walk};

/// How long one question to an app may take before it counts as not answering.
const APP_TIMEOUT_MS: u32 = 1000;

/// What every element is asked, in one round trip.
const PROPERTIES: [UIA_PROPERTY_ID; 13] = [
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_IsEnabledPropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_IsPasswordPropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_HelpTextPropertyId,
    UIA_ValueValuePropertyId,
    UIA_ValueIsReadOnlyPropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId,
    UIA_RangeValueValuePropertyId,
];
const PATTERNS: [UIA_PATTERN_ID; 6] = [
    UIA_InvokePatternId,
    UIA_TogglePatternId,
    UIA_ValuePatternId,
    UIA_SelectionItemPatternId,
    UIA_ExpandCollapsePatternId,
    UIA_RangeValuePatternId,
];

/// Joins this thread to the multithreaded apartment, and keeps one thread there for good:
/// elements are released on whichever thread drops them, after the one that made them may
/// have gone.
fn com() {
    static KEEPER: Once = Once::new();
    KEEPER.call_once(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("glass-com".into()).spawn(move || {
            // SAFETY: initializes COM on this thread, which never leaves.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let _ = tx.send(());
            loop {
                std::thread::park();
            }
        });
        if spawned.is_ok() {
            let _ = rx.recv();
        }
    });
    // SAFETY: a no-op when this thread is already in an apartment.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// UI Automation's client object: free-threaded, so one serves every thread.
struct Automation(IUIAutomation);

// SAFETY: CUIAutomation is free-threaded, and every thread using it is in the MTA (`com`).
unsafe impl Send for Automation {}
unsafe impl Sync for Automation {}

fn automation() -> Result<&'static IUIAutomation, String> {
    static AUTOMATION: OnceLock<Option<Automation>> = OnceLock::new();
    com();
    AUTOMATION
        .get_or_init(|| {
            // SAFETY: COM is initialized on this thread.
            unsafe {
                let a: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
                    .or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER))
                    .inspect_err(|e| tracing::warn!("UI Automation: {e}"))
                    .ok()?;
                // Windows 8 and later: a hung app costs a second, not twenty.
                if let Ok(a2) = a.cast::<IUIAutomation2>() {
                    let _ = a2.SetConnectionTimeout(APP_TIMEOUT_MS);
                    let _ = a2.SetTransactionTimeout(APP_TIMEOUT_MS);
                }
                Some(Automation(a))
            }
        })
        .as_ref()
        .map(|a| &a.0)
        .ok_or_else(|| "UI Automation isn't available on this computer".into())
}

fn cache_request(automation: &IUIAutomation) -> windows::core::Result<IUIAutomationCacheRequest> {
    // SAFETY: plain COM calls on a live object.
    unsafe {
        let cache = automation.CreateCacheRequest()?;
        for p in PROPERTIES {
            cache.AddProperty(p)?;
        }
        for p in PATTERNS {
            cache.AddPattern(p)?;
        }
        Ok(cache)
    }
}

/// An element of some app's interface, kept for acting on later.
pub struct Element(IUIAutomationElement);

// SAFETY: UI Automation's element proxies are free-threaded, and threads that use them
// are in the MTA (`com`).
unsafe impl Send for Element {}
unsafe impl Sync for Element {}

/// An element's cached properties, read once.
struct Seen {
    control: i32,
    password: bool,
    name: Option<String>,
    enabled: bool,
    focused: bool,
    frame: Option<[f64; 4]>,
    /// A value pattern's text, or a range's number.
    value: Option<String>,
    /// It has a value pattern that isn't read-only.
    writable: bool,
    toggle: Option<i32>,
    selected: Option<bool>,
    /// It has a pattern press can use.
    acts: bool,
}

fn text(s: windows::core::Result<windows::core::BSTR>) -> Option<String> {
    s.ok().map(|s| s.to_string()).filter(|s| !s.trim().is_empty())
}

/// What an element's cache says; `None` once it's gone.
fn seen(el: &IUIAutomationElement) -> Option<Seen> {
    // SAFETY: cached reads on a live element; patterns come back only if cached.
    unsafe {
        let control = el.CachedControlType().ok()?.0;
        let password = el.CachedIsPassword().is_ok_and(|b| b.as_bool());
        let value_pattern = el.GetCachedPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).ok();
        let range = el.GetCachedPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId).ok();
        let toggle = el.GetCachedPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId).ok();
        let selection = el.GetCachedPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId).ok();
        let invoke = el.GetCachedPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId).is_ok();
        let expand = el.GetCachedPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId).is_ok();
        let value = match (&value_pattern, &range) {
            (Some(v), _) if !password => text(v.CachedValue()),
            (None, Some(r)) => r.CachedValue().ok().map(|n| if n.fract() == 0.0 { format!("{n:.0}") } else { format!("{n}") }),
            _ => None,
        };
        let writable = value_pattern.as_ref().is_some_and(|v| v.CachedIsReadOnly().is_ok_and(|r| !r.as_bool()));
        let text_like = control == convert::control::TEXT;
        let name = text(el.CachedName())
            .or_else(|| text_like.then(|| value.clone()).flatten())
            .or_else(|| convert::editable(control).then(|| text(el.CachedHelpText()).map(|h| format!("{h} (placeholder)"))).flatten());
        let frame = el.CachedBoundingRectangle().ok().map(|r: RECT| {
            [r.left as f64, r.top as f64, (r.right - r.left) as f64, (r.bottom - r.top) as f64]
        });
        Some(Seen {
            control,
            password,
            name,
            enabled: el.CachedIsEnabled().map_or(true, |b| b.as_bool()),
            focused: el.CachedHasKeyboardFocus().is_ok_and(|b| b.as_bool()),
            frame,
            value: if text_like { None } else { value },
            writable,
            toggle: toggle.as_ref().and_then(|t| t.CachedToggleState().ok()).map(|s| s.0),
            selected: selection.as_ref().and_then(|s| s.CachedIsSelected().ok()).map(|b| b.as_bool()),
            acts: invoke || toggle.is_some() || selection.is_some() || expand,
        })
    }
}

impl Seen {
    fn role(&self) -> &'static str {
        convert::short_role(self.control, self.password)
    }

    /// The value as a snapshot line shows it: checkboxes and radios as 1 or 0 (as on macOS).
    fn shown_value(&self) -> Option<String> {
        if self.password {
            return None;
        }
        match (self.role(), self.toggle, self.selected) {
            ("checkbox", Some(t), _) => Some(match t {
                t if t == ToggleState_On.0 => "1".into(),
                t if t == ToggleState_Indeterminate.0 => "2".into(),
                _ => "0".into(),
            }),
            ("radio" | "tab", _, Some(s)) => Some(if s { "1" } else { "0" }.into()),
            (_, Some(t), _) if self.value.is_none() => Some(if t == ToggleState_On.0 { "on" } else { "off" }.into()),
            _ => self.value.clone().filter(|v| !v.is_empty()),
        }
    }
}

struct Walker {
    walker: IUIAutomationTreeWalker,
    cache: IUIAutomationCacheRequest,
    budget: Budget,
    read: usize,
    partial: bool,
    handles: Vec<Element>,
}

impl Walker {
    fn out_of_budget(&self) -> bool {
        self.read >= self.budget.nodes || Instant::now() >= self.budget.until
    }

    fn first_child(&self, el: &IUIAutomationElement) -> Option<IUIAutomationElement> {
        // SAFETY: plain COM call; no child comes back as an error.
        unsafe { self.walker.GetFirstChildElementBuildCache(el, &self.cache) }.ok()
    }

    fn next_sibling(&self, el: &IUIAutomationElement) -> Option<IUIAutomationElement> {
        // SAFETY: as above.
        unsafe { self.walker.GetNextSiblingElementBuildCache(el, &self.cache) }.ok()
    }

    /// One element and what's under it; `None` when it's gone or not worth showing.
    fn node(&mut self, el: IUIAutomationElement, depth: usize) -> Option<Node> {
        self.read += 1;
        let s = seen(&el)?;
        let first = self.first_child(&el);
        // Nothing to see: zero-sized and empty.
        if s.frame.is_none_or(|f| f[2] <= 0.0 || f[3] <= 0.0) && first.is_none() {
            return None;
        }
        let editable = convert::editable(s.control) && s.writable;
        let press = !editable && (convert::interactive(s.control) || (s.acts && s.name.is_some()));
        // Unnamed panes and groups that do nothing are scaffolding: they don't count
        // against the depth limit.
        let scaffolding = s.name.is_none() && !press && !editable && convert::scaffolding(s.control);
        let next = if scaffolding { depth } else { depth + 1 };
        let mut node = Node {
            role: s.role().into(),
            name: s.name.clone(),
            value: s.shown_value(),
            secret: s.password,
            enabled: s.enabled,
            focused: s.focused,
            editable,
            cut: false,
            handle: None,
            children: Vec::new(),
        };
        if press || editable {
            node.handle = Some(self.handles.len());
            self.handles.push(Element(el));
        }
        if let Some(mut kid) = first {
            if next > self.budget.depth || self.out_of_budget() {
                node.cut = true;
                self.partial = true;
            } else {
                loop {
                    if self.out_of_budget() {
                        node.cut = true;
                        self.partial = true;
                        break;
                    }
                    let sibling = self.next_sibling(&kid);
                    if let Some(n) = self.node(kid, next) {
                        node.children.push(n);
                    }
                    match sibling {
                        Some(s) => kid = s,
                        None => break,
                    }
                }
            }
        }
        Some(node)
    }
}

/// Visible top-level windows, front to back.
fn top_windows() -> Vec<HWND> {
    unsafe extern "system" fn each(hwnd: HWND, l: LPARAM) -> BOOL {
        // SAFETY: l points at the Vec below, alive for the enumeration.
        let all = unsafe { &mut *(l.0 as *mut Vec<HWND>) };
        let mut r = RECT::default();
        // SAFETY: plain queries.
        let shown = unsafe { IsWindowVisible(hwnd).as_bool() && GetWindowRect(hwnd, &mut r).is_ok() };
        if shown && r.right > r.left && r.bottom > r.top {
            all.push(hwnd);
        }
        true.into()
    }
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: the callback only uses `all` during the call.
    let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut all as *mut Vec<HWND> as isize)) };
    all
}

fn found(pid: u32) -> Found {
    let bundle = front::bundle_of(pid as i32).unwrap_or_default();
    let name = front::name_of(pid).unwrap_or_else(|| bundle.clone());
    Found { pid: pid as i32, bundle, name }
}

/// The app meant: by executable name ("notepad", "notepad.exe") or by name ("Notepad"), or
/// the one in front.
pub fn find_app(query: Option<&str>) -> Result<Found, String> {
    if super::session::locked() {
        return Err("The screen is locked: UI Automation can't read apps. Use screenshot.".into());
    }
    match query.map(str::trim).filter(|q| !q.is_empty()) {
        None => {
            // SAFETY: plain query.
            let fg = unsafe { GetForegroundWindow() };
            if fg.is_invalid() {
                return Err("no app is in front".into());
            }
            Ok(found(front::window_pid(fg)))
        }
        Some(q) => {
            let lower = q.to_lowercase();
            let lower = lower.strip_suffix(".exe").unwrap_or(&lower);
            let mut seen = Vec::new();
            for hwnd in top_windows() {
                let pid = front::window_pid(hwnd);
                if seen.contains(&pid) {
                    continue;
                }
                seen.push(pid);
                let app = found(pid);
                if app.bundle == lower || app.name.to_lowercase() == lower {
                    return Ok(app);
                }
            }
            Err(format!("no app called {q:?} has a window open"))
        }
    }
}

/// Popup menus: their own top-level windows, which go with whichever window is read.
fn is_menu(hwnd: HWND) -> bool {
    matches!(front::class_name(hwnd).as_str(), "#32768" | "Xaml_WindowedPopupClass")
}

/// An app's focused window (or every window) and any menu it has open, as a tree.
pub fn walk(app: &Found, all_windows: bool, budget: Budget) -> Result<Walk, String> {
    let automation = automation()?;
    let name = if app.name.is_empty() { "That app" } else { &app.name };
    let pid = app.pid as u32;
    let windows: Vec<HWND> = top_windows().into_iter().filter(|&h| front::window_pid(h) == pid).collect();
    let chosen: Vec<HWND> = if all_windows {
        windows
    } else {
        // SAFETY: plain queries.
        let fg = unsafe { GetForegroundWindow() };
        let main = if windows.contains(&fg) {
            Some(fg)
        } else {
            let titled = windows.iter().copied().find(|&h| !is_menu(h) && unsafe { GetWindowTextLengthW(h) } > 0);
            titled.or_else(|| windows.iter().copied().find(|&h| !is_menu(h)))
        };
        main.into_iter().chain(windows.iter().copied().filter(|&h| is_menu(h) && Some(h) != main)).collect()
    };
    if chosen.is_empty() {
        return Err(format!("{name} has no window open that UI Automation can see (minimized ones count). Use screenshot."));
    }
    let busy = || format!("{name} isn't answering UI Automation right now (busy, or just starting). Try again in a moment, or use screenshot.");
    let cache = cache_request(automation).map_err(|e| format!("UI Automation: {e}"))?;
    // SAFETY: plain COM call.
    let walker = unsafe { automation.ControlViewWalker() }.map_err(|e| format!("UI Automation: {e}"))?;
    let mut walker = Walker { walker, cache, budget, read: 0, partial: false, handles: Vec::new() };
    let mut roots = Vec::new();
    let mut timed_out = false;
    for hwnd in chosen {
        if walker.out_of_budget() {
            walker.partial = true;
            break;
        }
        // SAFETY: plain COM call.
        match unsafe { automation.ElementFromHandleBuildCache(hwnd, &walker.cache) } {
            Ok(el) => {
                if let Some(n) = walker.node(el, 0) {
                    roots.push(n);
                }
            }
            Err(e) if e.code().0 as u32 == UIA_E_TIMEOUT => timed_out = true,
            Err(_) => {}
        }
    }
    if roots.is_empty() {
        return Err(if timed_out { busy() } else { format!("{name} has no window UI Automation can read. Use screenshot.") });
    }
    Ok(Walk { roots, handles: walker.handles, partial: walker.partial })
}

/// What a kept element is now; `None` once it's gone.
pub fn look(el: &Element) -> Option<Now> {
    let automation = automation().ok()?;
    let cache = cache_request(automation).ok()?;
    // SAFETY: plain COM call; a gone element answers with an error.
    let now = unsafe { el.0.BuildUpdatedCache(&cache) }.ok()?;
    let s = seen(&now)?;
    let frame = s.frame.filter(|f| f[2] > 0.0 && f[3] > 0.0);
    Some(Now { role: s.role().into(), name: s.name, enabled: s.enabled, frame })
}

fn gone(e: &windows::core::Error) -> bool {
    e.code().0 as u32 == UIA_E_ELEMENTNOTAVAILABLE
}

/// Presses an element: invokes it, or toggles, selects, expands (or collapses) it, or does
/// its default action, whichever it can.
pub fn press(el: &Element) -> Outcome {
    if automation().is_err() {
        return Outcome::Failed("UI Automation isn't available".into());
    }
    let el = &el.0;
    // SAFETY: plain COM calls on a live element; patterns it lacks come back as errors.
    unsafe {
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
            return outcome(p.Invoke());
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) {
            return outcome(p.Toggle());
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId) {
            return outcome(p.Select());
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId) {
            let state = p.CurrentExpandCollapseState();
            return match state {
                Ok(s) if s == ExpandCollapseState_Collapsed || s == ExpandCollapseState_PartiallyExpanded => outcome(p.Expand()),
                Ok(_) => outcome(p.Collapse()),
                Err(e) => outcome(Err(e)),
            };
        }
        match el.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(UIA_LegacyIAccessiblePatternId) {
            Ok(p) if p.CurrentDefaultAction().is_ok_and(|a| !a.is_empty()) => outcome(p.DoDefaultAction()),
            Err(e) if gone(&e) => Outcome::Gone,
            _ => Outcome::Unsupported,
        }
    }
}

/// Replaces a field's text, focusing it first as typing would.
pub fn set_value(el: &Element, text: &str) -> Outcome {
    if automation().is_err() {
        return Outcome::Failed("UI Automation isn't available".into());
    }
    // SAFETY: plain COM calls on a live element.
    unsafe {
        let _ = el.0.SetFocus();
        match el.0.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
            Ok(p) => outcome(p.SetValue(&windows::core::BSTR::from(text))),
            Err(e) if gone(&e) => Outcome::Gone,
            Err(_) => Outcome::Unsupported,
        }
    }
}

/// A field's text now (to check what was set), unless it's a password.
pub fn value(el: &Element) -> Option<String> {
    automation().ok()?;
    // SAFETY: plain COM calls on a live element.
    unsafe {
        let p = el.0.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).ok()?;
        p.CurrentValue().ok().map(|v| v.to_string())
    }
}

/// The focused control is a password field (web pages' too, as browsers report it).
pub fn focus_is_password() -> bool {
    let Ok(automation) = automation() else { return false };
    // SAFETY: plain COM calls.
    unsafe { automation.GetFocusedElement().and_then(|el| el.CurrentIsPassword()).is_ok_and(|b| b.as_bool()) }
}

/// What a UI Automation call's result says about whether the app did it.
fn outcome(r: windows::core::Result<()>) -> Outcome {
    match r {
        Ok(()) => Outcome::Done,
        Err(e) if gone(&e) => Outcome::Gone,
        Err(e) => match e.code() {
            // The app took too long to reply: it may be busy doing it (a dialog it opened
            // blocks the reply), or it may not have.
            c if c.0 as u32 == UIA_E_TIMEOUT => Outcome::Unconfirmed,
            c if c.0 as u32 == UIA_E_NOTSUPPORTED || c == E_NOTIMPL => Outcome::Unsupported,
            c if c.0 as u32 == UIA_E_ELEMENTNOTENABLED => Outcome::Failed("it's disabled".into()),
            _ => Outcome::Failed(e.message()),
        },
    }
}
