//! The Glass icon in the notification area (the Mac's menu bar item): lit while anyone is
//! viewing, with who's viewing in its tooltip; click to open the app, right-click for a
//! menu listing viewers (choose one to disconnect it) and the same controls as on a Mac.
//! Runs on a thread of its own with a hidden window for the icon's messages.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, HICON, ICONINFO, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL,
    WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use crate::viewers;

/// The icon's own messages, and "redraw" from the host's runtime.
const WM_TRAY: u32 = WM_APP + 1;
const WM_REFRESH: u32 = WM_APP + 2;

// Menu commands; viewers get VIEWER_BASE + their position in the list shown.
const OPEN_APP: u32 = 1;
const UPDATE: u32 = 2;
const MESSAGE: u32 = 3;
const PRIVACY: u32 = 4;
const OPEN_BROWSER: u32 = 5;
const COPY_LINK: u32 = 6;
const DISCONNECT_ALL: u32 = 7;
const REMOTE: u32 = 8;
const ASK_GUESTS: u32 = 9;
const REVOKE_LINKS: u32 = 10;
const HISTORY: u32 = 11;
const LOGS: u32 = 12;
const QUIT: u32 = 13;
const VIEWER_BASE: u32 = 1000;

/// The hidden window, for posting a redraw from other threads.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();
/// Set once a self-update has started; the restart ends this process.
static UPDATING: AtomicBool = AtomicBool::new(false);

/// Shows the icon for as long as the host runs.
pub fn start(rt: tokio::runtime::Handle) {
    let _ = RUNTIME.set(rt.clone());
    if let Err(e) = std::thread::Builder::new().name("glass-tray".into()).spawn(run) {
        tracing::warn!("tray icon: {e}");
        return;
    }
    // Redraw when viewers come and go, and each minute (for the "3 min" labels).
    rt.spawn(async {
        let mut rx = viewers::subscribe();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { return },
                _ = tick.tick() => {}
            }
            post(WM_REFRESH);
        }
    });
}

fn post(msg: u32) {
    let hwnd = WINDOW.load(Ordering::Relaxed);
    if hwnd != 0 {
        // SAFETY: posting to our own window; harmless if it's gone.
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(0)) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Copies `text` into a fixed UTF-16 field, cut to fit with its NUL.
fn fill(field: &mut [u16], text: &str) {
    let units: Vec<u16> = text.encode_utf16().take(field.len() - 1).collect();
    field[..units.len()].copy_from_slice(&units);
    field[units.len()] = 0;
}

/// Taskbar icons, made once: the Glass icon, and the same with a blue dot while viewed.
struct Icons {
    idle: HICON,
    viewing: HICON,
}

// SAFETY: icon handles are process-wide and only used from the tray thread.
unsafe impl Send for Icons {}
unsafe impl Sync for Icons {}

fn icons() -> &'static Icons {
    static ICONS: OnceLock<Icons> = OnceLock::new();
    ICONS.get_or_init(|| {
        let png = include_bytes!("../../assets/icon.png");
        let small = decode(png).and_then(|(rgba, w, h)| {
            // To BGRA, then down to the notification area's size.
            let bgra: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
            crate::thumbs::downscale(&bgra, w, h, w * 4, false, [32, 32])
        });
        let make = |dot: bool| {
            let Some(frame) = small.as_ref() else { return HICON::default() };
            let mut bgra = frame.bgra.clone();
            if dot {
                badge(&mut bgra, frame.width, frame.height);
            }
            icon_from_bgra(&bgra, frame.width, frame.height)
        };
        Icons { idle: make(false), viewing: make(true) }
    })
}

/// An RGBA picture from a PNG.
fn decode(png: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    (info.color_type == png::ColorType::Rgba).then(|| (buf[..info.buffer_size()].to_vec(), info.width as usize, info.height as usize))
}

/// A blue dot in the bottom-right corner, with a white ring so it shows on any taskbar.
fn badge(bgra: &mut [u8], w: usize, h: usize) {
    let r = w as f32 * 0.22;
    let (cx, cy) = (w as f32 - r - 0.5, h as f32 - r - 0.5);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            let colour = if d <= r - 1.5 {
                [0xF6, 0x82, 0x3B, 0xFF] // blue, as BGRA
            } else if d <= r {
                [0xFF, 0xFF, 0xFF, 0xFF]
            } else {
                continue;
            };
            let at = (y * w + x) * 4;
            bgra[at..at + 4].copy_from_slice(&colour);
        }
    }
}

/// An icon from a top-down BGRA picture.
fn icon_from_bgra(bgra: &[u8], w: usize, h: usize) -> HICON {
    use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
    // CreateBitmap takes rows top-down; the mask goes unused with an alpha channel.
    let mask = vec![0u8; w.div_ceil(16) * 2 * h];
    // SAFETY: bitmaps sized for w×h at 32 and 1 bits per pixel; deleted once the icon has
    // its own copies.
    unsafe {
        let colour = CreateBitmap(w as i32, h as i32, 1, 32, Some(bgra.as_ptr().cast()));
        let mono = CreateBitmap(w as i32, h as i32, 1, 1, Some(mask.as_ptr().cast()));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mono, hbmColor: colour };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(colour.into());
        let _ = DeleteObject(mono.into());
        icon
    }
}

/// The tooltip and icon for the current viewers.
fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let list = viewers::subscribe().borrow().clone();
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WM_TRAY,
        hIcon: if list.is_empty() { icons().idle } else { icons().viewing },
        ..Default::default()
    };
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let tip = match list.len() {
        0 => "Glass: no one is viewing this computer".to_string(),
        1 => format!("Glass: {} is viewing", list[0].describe()),
        n => format!("Glass: {n} viewing this computer"),
    };
    fill(&mut data.szTip, &tip);
    data
}

fn add_icon(hwnd: HWND) {
    let data = notify_data(hwnd);
    // SAFETY: a filled NOTIFYICONDATAW for our window.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
}

fn run() {
    // SAFETY: a window class and hidden window owned by this thread, which then pumps its
    // messages until the process ends.
    unsafe {
        let class = w!("GlassTray");
        let wc = WNDCLASSW { lpfnWndProc: Some(window_proc), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("Glass"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, None, None) else {
            tracing::warn!("tray icon: no window");
            return;
        };
        WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);
        add_icon(hwnd);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let data = notify_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

/// Sent to every top-level window when Explorer (and so the taskbar) restarts.
fn taskbar_created() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    // SAFETY: a constant name.
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => match (l.0 & 0xFFFF) as u32 {
            WM_LBUTTONUP => crate::app::open_app(),
            WM_CONTEXTMENU => show_menu(hwnd),
            _ => {}
        },
        WM_REFRESH => {
            let data = notify_data(hwnd);
            // SAFETY: as in add_icon.
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        }
        m if m == taskbar_created() => add_icon(hwnd),
        WM_COMMAND => {}
        // SAFETY: everything else goes to the default handler.
        _ => return unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
    LRESULT(0)
}

/// Builds the menu as things are now, shows it at the pointer, and runs the choice.
fn show_menu(hwnd: HWND) {
    let list = viewers::subscribe().borrow().clone();
    let link = crate::status::current().and_then(|s| s.url);
    let update = crate::update::available();
    // SAFETY: a popup menu built, shown and destroyed here, on the window's thread.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let add = |id: u32, text: &str, checked: bool, enabled: bool| {
            let text = wide(text);
            let mut flags = MF_STRING;
            if checked {
                flags |= MF_CHECKED;
            }
            if !enabled {
                flags |= MF_GRAYED;
            }
            let _ = AppendMenuW(menu, flags, id as usize, PCWSTR(text.as_ptr()));
        };
        let separator = || {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        };
        add(0, &format!("Glass {}", crate::update::CURRENT), false, false);
        add(OPEN_APP, "Open Glass…", false, true);
        if UPDATING.load(Ordering::Relaxed) {
            add(0, "Updating…", false, false);
        } else if let Some(v) = &update {
            add(UPDATE, &format!("Update to {v}"), false, true);
        }
        separator();
        if list.is_empty() {
            add(0, "No one is viewing this computer", false, false);
        } else {
            add(0, &if list.len() == 1 { "1 viewer (choose to disconnect)".into() } else { format!("{} viewers (choose one to disconnect)", list.len()) }, false, false);
            for (i, v) in list.iter().enumerate() {
                add(VIEWER_BASE + i as u32, &format!("    {}", v.describe()), false, true);
            }
            add(MESSAGE, "Send a message…", false, true);
            if crate::privacy::supported() {
                add(PRIVACY, "Privacy screen (blank this computer)", crate::privacy::active(), true);
            }
        }
        separator();
        add(OPEN_BROWSER, "Open in browser", false, link.is_some());
        add(COPY_LINK, "Copy link", false, link.is_some());
        separator();
        add(DISCONNECT_ALL, "Disconnect everyone", false, !list.is_empty());
        add(REMOTE, "Remote access from anywhere", crate::remote::enabled(), true);
        add(ASK_GUESTS, "Ask before guests connect", crate::settings::load().ask_guests, true);
        add(REVOKE_LINKS, "Revoke all guest links", false, true);
        add(HISTORY, "Connection history", false, true);
        separator();
        add(LOGS, "Open logs", false, true);
        separator();
        add(QUIT, "Quit Glass", false, true);

        // The menu only closes on a click elsewhere if its window is in front.
        let _ = SetForegroundWindow(hwnd);
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, at.x, at.y, None, hwnd, None).0 as u32;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        choose(chosen, &list, link);
    }
}

fn choose(id: u32, list: &[viewers::Viewer], link: Option<String>) {
    match id {
        OPEN_APP => crate::app::open_app(),
        UPDATE => match crate::update::start_self_update() {
            Ok(()) => UPDATING.store(true, Ordering::Relaxed),
            Err(e) => tracing::warn!("update from the tray: {e:#}"),
        },
        MESSAGE => {
            if let Some(rt) = RUNTIME.get() {
                rt.spawn(async {
                    if let Some(text) = crate::notify::ask_text("Glass", "Message to everyone viewing this computer:").await
                        && let Err(e) = crate::server::host_chat(&text)
                    {
                        tracing::warn!("message from the tray: {e:#}");
                    }
                });
            }
        }
        PRIVACY => {
            if let Err(e) = crate::privacy::set_forced(!crate::privacy::active()) {
                tracing::warn!("privacy from the tray: {e:#}");
            }
        }
        OPEN_BROWSER => {
            if let Some(url) = link {
                super::setup::open_url(&url);
            }
        }
        COPY_LINK => {
            if let Some(url) = link {
                super::clipboard::set_text(&url);
            }
        }
        DISCONNECT_ALL => viewers::disconnect_all(),
        REMOTE => {
            let _ = crate::remote::set_enabled(!crate::remote::enabled());
        }
        ASK_GUESTS => {
            let mut s = crate::settings::load();
            s.ask_guests = !s.ask_guests;
            if let Err(e) = crate::settings::save(&s) {
                tracing::warn!("settings: {e:#}");
            }
        }
        REVOKE_LINKS => {
            let file = crate::access::file_for(&crate::default_token_file());
            match crate::access::revoke_all(&file) {
                Ok(n) => tracing::info!("revoked links from the tray ({n} guest passes)"),
                Err(e) => tracing::warn!("revoking links: {e:#}"),
            }
        }
        HISTORY => {
            // A readable copy, opened in Notepad.
            let path = crate::config_dir().join("connections.txt");
            let mut text = String::from("Glass connection history (newest last)\r\n\r\n");
            for e in crate::connlog::recent(500) {
                text.push_str(&crate::connlog::describe(&e));
                text.push_str("\r\n");
            }
            if std::fs::write(&path, text).is_ok() {
                let _ = std::process::Command::new("notepad.exe").arg(&path).spawn();
            }
        }
        LOGS => super::setup::open_url(&super::log_dir().display().to_string()),
        QUIT => {
            // A clean exit: glassw starts Glass again only after a crash, so it stays quit
            // until the next sign-in (or `glass setup`). Viewers see it disconnect.
            tracing::info!("quit from the tray");
            viewers::disconnect_all();
            std::thread::sleep(std::time::Duration::from_millis(200));
            std::process::exit(0);
        }
        id if id >= VIEWER_BASE => {
            if let Some(v) = list.get((id - VIEWER_BASE) as usize) {
                viewers::disconnect(v.id);
            }
        }
        _ => {}
    }
    post(WM_REFRESH);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tips_are_cut_to_fit() {
        let mut field = [1u16; 8];
        fill(&mut field, "a long tooltip");
        assert_eq!(&field[..7], &"a long ".encode_utf16().collect::<Vec<_>>()[..]);
        assert_eq!(field[7], 0);
    }

    #[test]
    fn the_badge_is_a_dot_in_the_corner() {
        let mut bgra = vec![0u8; 32 * 32 * 4];
        badge(&mut bgra, 32, 32);
        let px = |x: usize, y: usize| &bgra[(y * 32 + x) * 4..][..4];
        assert_eq!(px(25, 25), &[0xF6, 0x82, 0x3B, 0xFF]);
        assert_eq!(px(2, 2), &[0, 0, 0, 0]);
    }
}
