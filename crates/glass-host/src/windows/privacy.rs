//! Privacy screen on Windows: a black, topmost, click-through window over every display,
//! marked to be left out of capture (WDA_EXCLUDEFROMCAPTURE, Windows 10 2004 and later),
//! so the panels show black while Desktop Duplication still sees the desktop; and local
//! keyboard and mouse input is dropped by the hooks in `front` (Glass's own injected input
//! passes).
//!
//! The windows and hooks die with the process, so a crash can't leave the computer dark
//! or locked; Ctrl+Alt+Del always gets through.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, EnumDisplayMonitors, GetMonitorInfoW, GetStockObject, HBRUSH, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HTTRANSPARENT, HWND_TOPMOST,
    LWA_ALPHA, MA_NOACTIVATE, MSG, PostThreadMessageW, RegisterClassW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity, SetWindowPos,
    ShowWindow, WDA_EXCLUDEFROMCAPTURE, WM_APP, WM_DISPLAYCHANGE, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_QUIT, WM_TIMER,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BOOL, w};

use super::front;

/// WDA_EXCLUDEFROMCAPTURE needs Windows 10 2004 (build 19041); earlier, the overlay would
/// be captured too, and viewers would see black.
pub fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        super::registry_string(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "CurrentBuildNumber")
            .and_then(|b| b.parse::<u32>().ok())
            .is_some_and(|build| build >= 19041)
    })
}

/// The overlay thread, while it runs.
static THREAD: Mutex<Option<u32>> = Mutex::new(None);

/// Posted to the overlay thread when the displays change.
const REBUILD: u32 = WM_APP + 1;

pub fn set(on: bool) -> anyhow::Result<()> {
    if on {
        engage()
    } else {
        release();
        Ok(())
    }
}

fn engage() -> anyhow::Result<()> {
    let mut thread = THREAD.lock().unwrap();
    if thread.is_some() {
        return Ok(());
    }
    anyhow::ensure!(supported(), "privacy mode needs Windows 10 version 2004 or later");
    // Blocking first: a dark screen the person can still type into isn't private.
    anyhow::ensure!(front::watch(), "couldn't block local input (keyboard and mouse hooks refused)");
    front::set_block_local(true);
    let (tx, rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let spawned = std::thread::Builder::new().name("glass-privacy".into()).spawn(move || overlay(tx));
    let started = match spawned {
        Ok(_) => rx.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|_| Err("the privacy screen didn't start".into())),
        Err(e) => Err(e.to_string()),
    };
    match started {
        Ok(tid) => {
            *thread = Some(tid);
            install_panic_restore();
            Ok(())
        }
        Err(e) => {
            front::set_block_local(false);
            anyhow::bail!("couldn't blank the screen: {e}")
        }
    }
}

fn release() {
    front::set_block_local(false);
    // try_lock: a panic while engaging holds the lock.
    let Ok(mut thread) = THREAD.try_lock() else { return };
    if let Some(tid) = thread.take() {
        // SAFETY: posting to a thread id; harmless if it has gone.
        let _ = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

/// Every display's rectangle, in desktop pixels.
fn monitors() -> Vec<RECT> {
    unsafe extern "system" fn each(monitor: HMONITOR, _: HDC, _: *mut RECT, l: LPARAM) -> BOOL {
        // SAFETY: l points at the Vec below, alive for the enumeration.
        let all = unsafe { &mut *(l.0 as *mut Vec<RECT>) };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: info is sized for the call.
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            all.push(info.rcMonitor);
        }
        true.into()
    }
    let mut all: Vec<RECT> = Vec::new();
    // SAFETY: the callback only uses `all` during the call.
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut all as *mut Vec<RECT> as isize)) };
    all
}

unsafe extern "system" fn on_message(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        // Clicks (Glass's own) go through to what's underneath, and it never takes focus.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DISPLAYCHANGE => {
            // SAFETY: posting to this very thread.
            let _ = unsafe { PostThreadMessageW(GetCurrentThreadId(), REBUILD, WPARAM(0), LPARAM(0)) };
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// A black window over `r`, left out of capture. Never shown unless it's left out.
fn cover(r: RECT) -> Result<HWND, String> {
    // SAFETY: the class is registered before any window is made; a window that fails a
    // step is destroyed before returning.
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            w!("GlassPrivacyScreen"),
            w!("Glass privacy screen"),
            WS_POPUP,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        let ready = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)
            .and_then(|_| SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE));
        if let Err(e) = ready {
            let _ = DestroyWindow(hwnd);
            return Err(format!("can't keep it out of capture ({e})"));
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok(hwnd)
    }
}

fn cover_all(rects: &[RECT]) -> Result<Vec<HWND>, String> {
    let mut made = Vec::new();
    for &r in rects {
        match cover(r) {
            Ok(h) => made.push(h),
            Err(e) => {
                uncover(&mut made);
                return Err(e);
            }
        }
    }
    Ok(made)
}

fn uncover(windows: &mut Vec<HWND>) {
    for h in windows.drain(..) {
        // SAFETY: windows this thread made.
        let _ = unsafe { DestroyWindow(h) };
    }
}

/// The overlay thread: makes the windows, keeps them on top (others come and go) and
/// matching the displays, until told to quit.
fn overlay(started: std::sync::mpsc::Sender<Result<u32, String>>) {
    // SAFETY: a class and windows on this thread, which runs their message loop.
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(on_message),
            hInstance: GetModuleHandleW(None).map(Into::into).unwrap_or_default(),
            lpszClassName: w!("GlassPrivacyScreen"),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        // Fails harmlessly when already registered by an earlier engage.
        RegisterClassW(&class);
        let mut rects = monitors();
        let mut windows = match cover_all(&rects) {
            Ok(w) if !w.is_empty() => w,
            Ok(_) => {
                let _ = started.send(Err("no displays".into()));
                return;
            }
            Err(e) => {
                let _ = started.send(Err(e));
                return;
            }
        };
        SetTimer(None, 0, 1000, None);
        let _ = started.send(Ok(GetCurrentThreadId()));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            match (msg.hwnd.is_invalid(), msg.message) {
                (true, WM_TIMER | REBUILD) => {
                    let now = monitors();
                    if now != rects || windows.is_empty() || msg.message == REBUILD {
                        uncover(&mut windows);
                        match cover_all(&now) {
                            Ok(w) => windows = w,
                            // Tried again on the next tick.
                            Err(e) => tracing::warn!("privacy: couldn't cover the displays: {e}"),
                        }
                        rects = now;
                    }
                    for &h in &windows {
                        let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
                    }
                }
                _ => {
                    DispatchMessageW(&msg);
                }
            }
        }
        uncover(&mut windows);
    }
}

/// A panic elsewhere shouldn't leave the screen dark and the keyboard dead until the
/// process exits.
fn install_panic_restore() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if front::block_local() {
                release();
            }
            previous(info);
        }));
    });
}
