//! What an agent is about to touch: the app that owns the window under a point (or that
//! has the keyboard), asked at the moment of input; whether a password field has the
//! keyboard; and whether the person at the computer is using its own keyboard or mouse.
//!
//! Apps are named by their executable (see `convert::app_id`). Glass's own SendInput
//! counts as input to Windows, so local input is watched with low-level hooks that skip
//! injected events, in a helper process; privacy mode uses the same hooks to drop local
//! input.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

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

pub fn class_name(hwnd: HWND) -> String {
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
/// does). Classic edit boxes say so in their style; anything else, through UI Automation.
pub fn secure_input() -> bool {
    if super::session::locked() {
        return true;
    }
    let Some(focus) = focus_window() else { return false };
    let class = class_name(focus).to_ascii_lowercase();
    // SAFETY: plain query.
    let style = unsafe { GetWindowLongW(focus, GWL_STYLE) };
    ((class == "edit" || class.starts_with("richedit")) && style & ES_PASSWORD != 0) || super::uia::focus_is_password()
}

// ---- Local input ----
//
// The hooks run in a helper process (`glass --glass-input-hook <host pid>`), never in the
// host: in the VM, a process holding a low-level keyboard hook that also injects input lost
// its own modifiers (Shift+H typed "h", paste typed "v"). The two share a few counters in
// named shared memory.

/// What the host and the hook helper share.
#[repr(C)]
struct Shared {
    /// When someone last used this computer's own keyboard or mouse (ms since the epoch;
    /// 0 until they do).
    real_at: AtomicU64,
    /// Privacy mode: drop local keyboard and mouse input (Glass's own still goes through).
    block: AtomicU64,
    /// Set by the helper: 1 once its hooks are in, 2 if Windows refused them.
    ready: AtomicU64,
}

/// The flag that starts the helper instead of Glass.
pub const HOOK_HELPER_ARG: &str = "--glass-input-hook";

fn shared_name(host_pid: u32) -> windows::core::HSTRING {
    windows::core::HSTRING::from(format!("Local\\GlassLocalInput-{host_pid}"))
}

/// Maps the shared counters, creating them (the host) or opening the host's (the helper).
fn map_shared(host_pid: u32, create: bool) -> Option<&'static Shared> {
    use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW, PAGE_READWRITE};
    let name = shared_name(host_pid);
    // SAFETY: a fresh (zeroed) or existing mapping of at least size_of::<Shared>() bytes,
    // mapped for the life of the process and only touched through atomics.
    unsafe {
        let mapping = if create {
            CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, std::mem::size_of::<Shared>() as u32, &name).ok()?
        } else {
            OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &name).ok()?
        };
        let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, std::mem::size_of::<Shared>());
        (!view.Value.is_null()).then(|| &*(view.Value as *const Shared))
    }
}

static SHARED: OnceLock<&'static Shared> = OnceLock::new();

fn epoch_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// In the helper: a local event, noted, and dropped while privacy mode is on.
fn local() -> bool {
    let Some(shared) = SHARED.get() else { return false };
    shared.real_at.store(epoch_ms(), Ordering::Relaxed);
    shared.block.load(Ordering::Relaxed) != 0
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

/// The helper's whole life: hooks, then a message loop (Windows drops hooks whose thread
/// stops pumping) until the host is gone.
pub fn run_hook_helper(host_pid: &str) -> anyhow::Result<()> {
    use windows::Win32::System::Threading::{INFINITE, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    let host_pid: u32 = host_pid.parse()?;
    let shared = map_shared(host_pid, false).ok_or_else(|| anyhow::anyhow!("no host {host_pid}"))?;
    let _ = SHARED.set(shared);
    // SAFETY: the handle lives as long as this thread, which ends the process.
    let host = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, host_pid)? };
    let host = host.0 as usize;
    std::thread::spawn(move || {
        // SAFETY: waiting on the host's process handle.
        unsafe { WaitForSingleObject(windows::Win32::Foundation::HANDLE(host as *mut _), INFINITE) };
        std::process::exit(0);
    });
    // SAFETY: hooks set on this thread, which then runs its message loop for good.
    unsafe {
        let module = GetModuleHandleW(None).ok().map(Into::into);
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(on_mouse), module, 0);
        let keys = SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), module, 0);
        if mouse.is_err() || keys.is_err() {
            shared.ready.store(2, Ordering::SeqCst);
            anyhow::bail!("Windows refused the input hooks");
        }
        shared.ready.store(1, Ordering::SeqCst);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Starts watching local input (once) through the hook helper. False if it couldn't start
/// or Windows refused its hooks.
pub fn watch() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let pid = std::process::id();
        let Some(shared) = map_shared(pid, true) else {
            tracing::warn!("local input: no shared memory for the hook helper");
            return false;
        };
        let _ = SHARED.set(shared);
        let mut cmd = match std::env::current_exe() {
            Ok(exe) => std::process::Command::new(exe),
            Err(_) => return false,
        };
        cmd.args([HOOK_HELPER_ARG, &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        super::no_window(&mut cmd);
        if let Err(e) = cmd.spawn() {
            tracing::warn!("local input: couldn't start the hook helper: {e}");
            return false;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match shared.ready.load(Ordering::SeqCst) {
                1 => return true,
                2 => break,
                _ => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        tracing::warn!("local input: the hook helper didn't set its hooks");
        false
    })
}

/// Privacy mode: drop (or stop dropping) local keyboard and mouse input.
pub fn set_block_local(on: bool) {
    if let Some(shared) = SHARED.get() {
        shared.block.store(on as u64, Ordering::SeqCst);
    }
}

pub fn block_local() -> bool {
    SHARED.get().is_some_and(|s| s.block.load(Ordering::SeqCst) != 0)
}

/// Seconds since someone used this computer's own keyboard or mouse (not input Glass
/// injected), once that's been seen.
pub fn local_input_age() -> Option<f64> {
    if !watch() {
        return None;
    }
    let at = SHARED.get()?.real_at.load(Ordering::Relaxed);
    (at != 0).then(|| epoch_ms().saturating_sub(at) as f64 / 1000.0)
}
