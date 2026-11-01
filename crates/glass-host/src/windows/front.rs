//! What an agent is about to touch: the app that owns the window under a point (or that
//! has the keyboard), asked at the moment of input; whether a password field has the
//! keyboard; and whether the person at the computer is using its own keyboard or mouse.
//!
//! Apps are named by their executable (see `convert::app_id`). Glass's own SendInput
//! counts as input to Windows, so local input is watched with low-level hooks that skip
//! injected events; privacy mode uses the same hooks to drop local input.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, ES_PASSWORD, EnumChildWindows, GA_ROOT, GUITHREADINFO, GWL_STYLE, GetAncestor,
    GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetWindowLongW, GetWindowThreadProcessId,
    HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED, LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED, MSG,
    MSLLHOOKSTRUCT, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL, WindowFromPoint,
};
use windows::core::{BOOL, PCWSTR, PWSTR, w};

use super::convert;

/// Where a process's executable is, if this user may ask.
fn exe_path(pid: u32) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the handle is closed right after; buf and len describe a writable buffer.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// A process's app id ("notepad").
pub fn bundle_of(pid: i32) -> Option<String> {
    convert::app_id(&exe_path(u32::try_from(pid).ok()?)?)
}

/// What to call a process's app: its executable's description ("Notepad"), else its name.
pub fn name_of(pid: u32) -> Option<String> {
    let path = exe_path(pid)?;
    description(&path).or_else(|| {
        let file = path.rsplit('\\').next()?;
        Some(file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string())
    })
}

/// An executable's FileDescription, in the first language it lists.
fn description(path: &str) -> Option<String> {
    let path = windows::core::HSTRING::from(path);
    // SAFETY: the version block is a buffer of the size Windows asked for; VerQueryValueW
    // returns pointers into it, read only while it lives.
    unsafe {
        let size = GetFileVersionInfoSizeW(&path, None);
        if size == 0 {
            return None;
        }
        let mut block = vec![0u8; size as usize];
        GetFileVersionInfoW(&path, None, size, block.as_mut_ptr().cast()).ok()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(block.as_ptr().cast(), w!("\\VarFileInfo\\Translation"), &mut ptr, &mut len).as_bool() || len < 4 {
            return None;
        }
        let lang = *(ptr as *const u16);
        let codepage = *(ptr as *const u16).add(1);
        let key = windows::core::HSTRING::from(format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\FileDescription"));
        if !VerQueryValueW(block.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut ptr, &mut len).as_bool() || len == 0 {
            return None;
        }
        let text = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let text = String::from_utf16_lossy(text).trim_end_matches('\0').trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: a writable buffer.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn thread_pid(hwnd: HWND) -> u32 {
    let mut pid = 0;
    // SAFETY: plain query; an invalid window leaves pid 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// The process whose app a window shows: its top-level window's, except for Store apps,
/// whose frame belongs to ApplicationFrameHost and whose content is a child window of
/// the app's own process.
pub fn window_pid(hwnd: HWND) -> u32 {
    // SAFETY: plain queries.
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let root = if root.is_invalid() { hwnd } else { root };
    let frame = thread_pid(root);
    if class_name(root) != "ApplicationFrameWindow" {
        return frame;
    }
    struct Search {
        frame: u32,
        found: u32,
    }
    unsafe extern "system" fn each(child: HWND, l: LPARAM) -> BOOL {
        // SAFETY: l points at the Search below, alive for the enumeration.
        let s = unsafe { &mut *(l.0 as *mut Search) };
        let pid = thread_pid(child);
        if pid != s.frame && class_name(child) == "Windows.UI.Core.CoreWindow" {
            s.found = pid;
            return false.into();
        }
        true.into()
    }
    let mut s = Search { frame, found: 0 };
    // SAFETY: the callback only uses `s` during the call.
    unsafe { let _ = EnumChildWindows(Some(root), Some(each), LPARAM(&mut s as *mut Search as isize)); }
    if s.found != 0 { s.found } else { frame }
}

/// The front app's id.
pub fn frontmost_app() -> Option<String> {
    // SAFETY: plain query.
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() {
        return None;
    }
    bundle_of(window_pid(fg) as i32)
}

/// The app owning the window under desktop point (`x`, `y`, physical pixels): what a click
/// there reaches. Falls back to the front app if that can't be told.
pub fn app_at(x: f64, y: f64) -> Option<String> {
    // SAFETY: plain query. Privacy mode's overlay is click-through, so it's never the answer.
    let hwnd = unsafe { WindowFromPoint(POINT { x: x.round() as i32, y: y.round() as i32 }) };
    if hwnd.is_invalid() {
        return frontmost_app();
    }
    bundle_of(window_pid(hwnd) as i32).or_else(frontmost_app)
}

/// The window with the keyboard focus (in the foreground thread), if there is one.
fn focus_window() -> Option<HWND> {
    // SAFETY: plain queries into a GUITHREADINFO sized for them.
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return None;
        }
        let tid = GetWindowThreadProcessId(fg, None);
        let mut info = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        match GetGUIThreadInfo(tid, &mut info) {
            Ok(()) if !info.hwndFocus.is_invalid() => Some(info.hwndFocus),
            _ => Some(fg),
        }
    }
}

/// The app that has the keyboard.
pub fn key_app() -> Option<String> {
    focus_window().and_then(|h| bundle_of(window_pid(h) as i32))
}

/// A password field has the keyboard, or the session is locked (the lock screen's field
/// does). Classic edit boxes say so in their style.
pub fn secure_input() -> bool {
    if super::session::locked() {
        return true;
    }
    let Some(focus) = focus_window() else { return false };
    let class = class_name(focus).to_ascii_lowercase();
    // SAFETY: plain query.
    let style = unsafe { GetWindowLongW(focus, GWL_STYLE) };
    (class == "edit" || class.starts_with("richedit")) && style & ES_PASSWORD != 0
}

// ---- Local input ----

/// When someone last used this computer's own keyboard or mouse (ms since the epoch; 0
/// until they do).
static REAL_AT: AtomicU64 = AtomicU64::new(0);
/// Privacy mode: drop local keyboard and mouse input (Glass's own still goes through).
pub static BLOCK_LOCAL: AtomicBool = AtomicBool::new(false);

fn epoch_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A local event: noted, and dropped while privacy mode is on.
fn local() -> bool {
    REAL_AT.store(epoch_ms(), Ordering::Relaxed);
    BLOCK_LOCAL.load(Ordering::Relaxed)
}

unsafe extern "system" fn on_mouse(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, l points at an MSLLHOOKSTRUCT.
        let info = unsafe { &*(l.0 as *const MSLLHOOKSTRUCT) };
        if info.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) == 0 && local() {
            return LRESULT(1);
        }
    }
    // SAFETY: passing the event on, unchanged.
    unsafe { CallNextHookEx(None, code, w, l) }
}

unsafe extern "system" fn on_key(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, l points at a KBDLLHOOKSTRUCT.
        let info = unsafe { &*(l.0 as *const KBDLLHOOKSTRUCT) };
        if (info.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0)) == 0 && local() {
            return LRESULT(1);
        }
    }
    // SAFETY: as above.
    unsafe { CallNextHookEx(None, code, w, l) }
}

/// Starts watching local input (once): low-level mouse and keyboard hooks on a thread of
/// their own, which must keep pumping messages or Windows drops them. False if they
/// couldn't be set.
pub fn watch() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("glass-local-input".into()).spawn(move || {
            // SAFETY: hooks set on this thread, which then runs its message loop for good.
            unsafe {
                let module = GetModuleHandleW(None).ok().map(Into::into);
                let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(on_mouse), module, 0);
                let keys = SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), module, 0);
                if let Err(e) = mouse.as_ref().and(keys.as_ref()) {
                    tracing::warn!("local input: couldn't set hooks: {e}");
                    let _ = tx.send(false);
                    return;
                }
                let _ = tx.send(true);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });
        spawned.is_ok() && rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or(false)
    })
}

/// Seconds since someone used this computer's own keyboard or mouse (not input Glass
/// injected), once that's been seen.
pub fn local_input_age() -> Option<f64> {
    if !watch() {
        return None;
    }
    let at = REAL_AT.load(Ordering::Relaxed);
    (at != 0).then(|| epoch_ms().saturating_sub(at) as f64 / 1000.0)
}
