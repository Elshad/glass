//! Top-level windows that can be streamed on their own (what Alt+Tab shows), and where
//! they are now. Ids are window handles, which fit in 32 bits on 64-bit Windows too.

use std::collections::HashMap;

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PWSTR};

use crate::media::WindowInfo;
use crate::win_capture::{WindowFacts, app_name, listable};

fn hwnd(id: u32) -> HWND {
    // Handles are sign-extended from their 32 significant bits.
    HWND(id as i32 as isize as *mut _)
}

fn id_of(hwnd: HWND) -> u32 {
    hwnd.0 as isize as u32
}

/// The window's visible frame (left, top, right, bottom; desktop pixels, without the
/// invisible resize borders), or `None` when it's gone or minimized.
pub fn frame_rect(id: u32) -> Option<[i32; 4]> {
    let hwnd = hwnd(id);
    let mut r = RECT::default();
    // SAFETY: IsWindow and IsIconic take any handle; DwmGetWindowAttribute writes a RECT
    // into r, whose size it's given.
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, (&raw mut r).cast(), size_of::<RECT>() as u32).ok()?;
    }
    (r.right > r.left && r.bottom > r.top).then_some([r.left, r.top, r.right, r.bottom])
}

/// Windows worth streaming, front to back.
pub fn list() -> Vec<WindowInfo> {
    unsafe extern "system" fn add(hwnd: HWND, list: LPARAM) -> BOOL {
        // SAFETY: list is the Vec passed to EnumWindows below, alive for its duration.
        unsafe { (*(list.0 as *mut Vec<HWND>)).push(hwnd) };
        BOOL(1)
    }
    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: the callback only runs during the call, while handles is borrowed.
    let _ = unsafe { EnumWindows(Some(add), LPARAM(&raw mut handles as isize)) };
    let own = std::process::id();
    let mut apps: HashMap<u32, String> = HashMap::new();
    let mut out = Vec::new();
    for hwnd in handles {
        let id = id_of(hwnd);
        let title = title(hwnd);
        let rect = frame_rect(id);
        let [l, t, r, b] = rect.unwrap_or_default();
        let mut pid = 0;
        let mut cloaked = 0u32;
        // SAFETY: plain queries on a window handle (harmless if it has just closed);
        // DwmGetWindowAttribute writes a u32 into cloaked, whose size it's given.
        let facts = unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size_of::<u32>() as u32);
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            WindowFacts {
                visible: IsWindowVisible(hwnd).as_bool(),
                cloaked: cloaked != 0,
                minimized: rect.is_none(),
                tool: ex & WS_EX_TOOLWINDOW.0 != 0,
                app_window: ex & WS_EX_APPWINDOW.0 != 0,
                owned: GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()),
                own_process: pid == own,
                title: &title,
                width: r - l,
                height: b - t,
            }
        };
        if !listable(&facts) {
            continue;
        }
        let app = apps.entry(pid).or_insert_with(|| process_name(pid).unwrap_or_default()).clone();
        let (w, h) = ((r - l) as f64, (b - t) as f64);
        out.push(WindowInfo {
            id,
            app,
            title,
            width: (r - l) as usize & !1,
            height: (b - t) as usize & !1,
            x: l as f64,
            y: t as f64,
            points_width: w,
            points_height: h,
        });
    }
    out
}

fn title(hwnd: HWND) -> String {
    // SAFETY: the buffer holds len + 1 UTF-16 units, as GetWindowTextW is told.
    unsafe {
        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..n.min(len)])
    }
}

/// The app's name from its executable (`Code` for Code.exe).
fn process_name(pid: u32) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the handle is closed right after the query, which writes at most len units.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let named = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        named.ok()?;
    }
    Some(app_name(&String::from_utf16_lossy(&buf[..len as usize])))
}
