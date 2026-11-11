//! Screen capture on X11 (XFCE, i3, xrdp sessions and anything else without a Wayland
//! compositor): each monitor's rectangle of the root window is read through MIT-SHM and
//! handed to that monitor's encoder, exactly where the PipeWire path hands over its frames.
//!
//! X11 has no frame clock to follow, so the DAMAGE extension stands in for one: a grab
//! happens as soon as the server reports that something was drawn, at most once per frame
//! interval, and never while the screen is still. The mailbox keeps only the newest
//! picture, so a frame the encoder was too busy for is dropped, not queued. The pointer
//! isn't part of a root-window grab; XFIXES supplies its image and it is drawn in.
//!
//! What changed is also passed on to `crate::damage`, which agents wait on: the region
//! DAMAGE collected since the last grab, plus where the pointer was and is.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use tracing::{info, warn};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use super::clock::now_us;
use super::video::{CaptureSink, Rect};
use crate::soft_encode::{Frame, PixelFormat};

/// A monitor's rectangle on the root window, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    x: i16,
    y: i16,
    w: u16,
    h: u16,
}

impl Area {
    fn rect(self) -> Rect {
        Rect {
            x: self.x as f64,
            y: self.y as f64,
            w: self.w as f64,
            h: self.h as f64,
        }
    }
}

/// Connects to the X server, calls `make_sink` once per monitor (the main one first) and
/// starts the thread that feeds them.
pub fn start(fps: u32, mut make_sink: impl FnMut(usize, Rect) -> Result<CaptureSink>) -> Result<()> {
    let (conn, screen) = x11rb::connect(None).context("connecting to the X server")?;
    let root = conn.setup().roots[screen].root;
    let format = pixel_format(&conn, screen)?;
    ensure!(
        conn.extension_information(shm::X11_EXTENSION_NAME)?.is_some(),
        "the X server has no MIT-SHM extension"
    );
    conn.shm_query_version()?.reply().context("MIT-SHM")?;

    let has_randr = conn.extension_information(randr::X11_EXTENSION_NAME)?.is_some()
        && conn.randr_query_version(1, 5)?.reply().is_ok_and(|v| (v.major_version, v.minor_version) >= (1, 5));
    let areas = monitors(&conn, root, has_randr)?;
    ensure!(!areas.is_empty(), "the X server reports no screen");
    let mut targets = Vec::new();
    for (i, &area) in areas.iter().enumerate() {
        info!("screen {i}: {}x{} at {},{} (X11)", area.w, area.h, area.x, area.y);
        targets.push(Target {
            sink: make_sink(i, area.rect())?,
            area,
            segment: None,
        });
    }

    std::thread::Builder::new()
        .name("glass-x11".into())
        .spawn(move || {
            if let Err(e) = run(conn, root, format, targets, fps, has_randr) {
                warn!("X11 capture stopped: {e:#}");
            }
        })
        .context("starting capture thread")?;
    Ok(())
}

/// The byte order of a root-window grab: 32 bits per pixel, alpha unused.
fn pixel_format(conn: &RustConnection, screen: usize) -> Result<PixelFormat> {
    let setup = conn.setup();
    let root = &setup.roots[screen];
    let bpp = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == root.root_depth)
        .map(|f| f.bits_per_pixel);
    ensure!(
        bpp == Some(32) && setup.image_byte_order == xproto::ImageOrder::LSB_FIRST,
        "unsupported X11 screen format: depth {}, {bpp:?} bits per pixel",
        root.root_depth
    );
    let visual = root
        .allowed_depths
        .iter()
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == root.root_visual)
        .context("root visual not found")?;
    match (visual.red_mask, visual.blue_mask) {
        (0x00ff_0000, 0x0000_00ff) => Ok(PixelFormat::Bgrx),
        (0x0000_00ff, 0x00ff_0000) => Ok(PixelFormat::Rgbx),
        (red, blue) => bail!("unsupported X11 colour masks: red {red:#x}, blue {blue:#x}"),
    }
}

/// Every monitor's area, the primary first; the whole screen when RandR can't say.
fn monitors(conn: &RustConnection, root: xproto::Window, has_randr: bool) -> Result<Vec<Area>> {
    let screen = conn.get_geometry(root)?.reply()?;
    let mut areas = Vec::new();
    if has_randr && let Ok(reply) = conn.randr_get_monitors(root, true)?.reply() {
        let mut list = reply.monitors;
        list.sort_by_key(|m| !m.primary);
        for m in list {
            // Clipped to the screen: a grab that reaches past it is refused.
            let (x, y) = (m.x.max(0), m.y.max(0));
            let w = (m.width as i32).min(screen.width as i32 - x as i32);
            let h = (m.height as i32).min(screen.height as i32 - y as i32);
            if w > 0 && h > 0 {
                areas.push(Area { x, y, w: w as u16, h: h as u16 });
            }
        }
    }
    if areas.is_empty() && screen.width > 0 && screen.height > 0 {
        areas.push(Area { x: 0, y: 0, w: screen.width, h: screen.height });
    }
    Ok(areas)
}

struct Target {
    sink: CaptureSink,
    area: Area,
    segment: Option<Segment>,
}

/// Shared memory the X server writes a grab into, so pixels never cross the socket.
struct Segment {
    id: shm::Seg,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping belongs to whichever thread holds the `Segment`; nothing else has
// the pointer.
unsafe impl Send for Segment {}

impl Segment {
    fn new(conn: &RustConnection, len: usize) -> Result<Self> {
        // SAFETY: plain System V calls; the segment is marked for removal as soon as both
        // sides are attached, so it can't outlive them.
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            ensure!(shmid >= 0, "shmget: {}", std::io::Error::last_os_error());
            let ptr = libc::shmat(shmid, std::ptr::null(), 0);
            if ptr as isize == -1 {
                let e = std::io::Error::last_os_error();
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                bail!("shmat: {e}");
            }
            let attached = conn
                .generate_id()
                .map_err(anyhow::Error::from)
                .and_then(|id| Ok((id, conn.shm_attach(id, shmid as u32, false)?.check()?)));
            libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            match attached {
                Ok((id, ())) => Ok(Self { id, ptr: ptr.cast(), len }),
                Err(e) => {
                    libc::shmdt(ptr);
                    Err(e.context("attaching shared memory to the X server"))
                }
            }
        }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` bytes until `release`. The server only writes while a
        // grab is in flight, and `grab` reads after its reply.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn release(self, conn: &RustConnection) {
        let _ = conn.shm_detach(self.id);
        // SAFETY: mapped by `shmat` in `new`, and not used after this.
        unsafe { libc::shmdt(self.ptr.cast()) };
    }
}

/// The pointer's picture: premultiplied ARGB, one `u32` per pixel.
struct Cursor {
    w: i32,
    h: i32,
    hot: (i32, i32),
    pixels: Vec<u32>,
}

impl Cursor {
    fn fetch(conn: &RustConnection) -> Option<Self> {
        let c = conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
        (c.cursor_image.len() == c.width as usize * c.height as usize).then(|| Self {
            w: c.width as i32,
            h: c.height as i32,
            hot: (c.xhot as i32, c.yhot as i32),
            pixels: c.cursor_image,
        })
    }

    /// The root-window rectangle the cursor covers with its hotspot at `at`.
    fn area(&self, at: (i16, i16)) -> xproto::Rectangle {
        xproto::Rectangle {
            x: (at.0 as i32 - self.hot.0).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            y: (at.1 as i32 - self.hot.1).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            width: self.w as u16,
            height: self.h as u16,
        }
    }

    /// Blends the cursor into a `w`×`h` frame with its hotspot at `at`.
    fn draw(&self, frame: &mut [u8], w: i32, h: i32, at: (i32, i32), format: PixelFormat) {
        let (left, top) = (at.0 - self.hot.0, at.1 - self.hot.1);
        for cy in 0..self.h {
            let y = top + cy;
            if y < 0 || y >= h {
                continue;
            }
            for cx in 0..self.w {
                let x = left + cx;
                if x < 0 || x >= w {
                    continue;
                }
                let p = self.pixels[(cy * self.w + cx) as usize];
                let alpha = p >> 24;
                if alpha == 0 {
                    continue;
                }
                let (r, g, b) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff);
                let src = if matches!(format, PixelFormat::Rgbx) { [r, g, b] } else { [b, g, r] };
                let o = (y * w + x) as usize * 4;
                for (d, s) in frame[o..o + 3].iter_mut().zip(src) {
                    *d = (s + *d as u32 * (255 - alpha) / 255).min(255) as u8;
                }
            }
        }
    }
}

fn run(
    conn: RustConnection,
    root: xproto::Window,
    format: PixelFormat,
    mut targets: Vec<Target>,
    fps: u32,
    has_randr: bool,
) -> Result<()> {
    let fd = conn.stream().as_raw_fd();
    let interval = Duration::from_secs_f64(1.0 / fps.clamp(1, 240) as f64);

    // Without DAMAGE every interval is a grab; without XFIXES the pointer isn't drawn.
    let damage = if conn.extension_information(damage::X11_EXTENSION_NAME)?.is_some()
        && conn.damage_query_version(1, 1)?.reply().is_ok()
    {
        let id = conn.generate_id()?;
        conn.damage_create(id, root, damage::ReportLevel::NON_EMPTY)?.check()?;
        Some(id)
    } else {
        warn!("the X server has no DAMAGE extension: capturing every frame, even when nothing changes");
        None
    };
    let has_xfixes = conn.extension_information(xfixes::X11_EXTENSION_NAME)?.is_some()
        && conn.xfixes_query_version(5, 0)?.reply().is_ok();
    if has_xfixes {
        conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    } else {
        warn!("the X server has no XFIXES extension: the pointer won't show in the stream");
    }
    if has_randr {
        conn.randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE)?;
    }
    let mut cursor = has_xfixes.then(|| Cursor::fetch(&conn)).flatten();
    // Receives what DAMAGE collected, so its rectangles can be read back.
    let parts = match damage {
        Some(_) if has_xfixes => {
            let id = conn.generate_id()?;
            conn.xfixes_create_region(id, &[])?.check()?;
            Some(id)
        }
        _ => None,
    };

    let mut pointer = (0i16, 0i16);
    // Pointer positions since the last grab, for damage: where it was drawn, where it is.
    let mut pointer_trail: Vec<(i16, i16)> = Vec::new();
    let (mut dirty, mut resized, mut first) = (true, false, true);
    let mut last = Instant::now() - interval;
    loop {
        // Pointer moves aren't damage, so its position is asked for each pass.
        if has_xfixes {
            let p = conn.query_pointer(root)?.reply()?;
            if (p.root_x, p.root_y) != pointer {
                pointer_trail.push(pointer);
                pointer = (p.root_x, p.root_y);
                dirty = true;
            }
        }
        // Drained last before waiting: a reply read above may have buffered events, and
        // the socket alone wouldn't wake for those.
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::DamageNotify(_) => dirty = true,
                Event::XfixesCursorNotify(_) => {
                    cursor = Cursor::fetch(&conn);
                    dirty = true;
                }
                Event::RandrScreenChangeNotify(_) => resized = true,
                _ => {}
            }
        }
        if resized {
            resized = false;
            dirty = true;
            relayout(&conn, root, has_randr, &mut targets)?;
        }
        if damage.is_none() {
            dirty = true;
        }
        if dirty && last.elapsed() >= interval {
            // Cleared before the grab: anything drawn after it is reported again.
            let mut changed = match (damage, parts) {
                (Some(id), Some(parts)) => {
                    conn.damage_subtract(id, x11rb::NONE, parts)?;
                    Some(conn.xfixes_fetch_region(parts)?.reply()?.rectangles)
                }
                (Some(id), None) => {
                    conn.damage_subtract(id, x11rb::NONE, x11rb::NONE)?;
                    None
                }
                _ => None,
            };
            if let (Some(rects), Some(c)) = (changed.as_mut(), cursor.as_ref()) {
                rects.extend(pointer_trail.drain(..).chain([pointer]).map(|p| c.area(p)));
            }
            pointer_trail.clear();
            last = Instant::now();
            dirty = false;
            let mut failed = false;
            for (i, target) in targets.iter_mut().enumerate() {
                match grab(&conn, root, target, format, cursor.as_ref(), pointer) {
                    Ok(()) => {
                        if first {
                            info!("screen {i}: first frame");
                        }
                        note_damage(i, target.area, first, changed.as_deref());
                    }
                    // Usually a grab that raced a resolution change.
                    Err(e) => {
                        tracing::debug!("screen {i}: grab failed: {e:#}");
                        failed = true;
                    }
                }
            }
            if !failed {
                first = false;
                continue;
            }
            (resized, dirty) = (true, true);
        }
        conn.flush()?;
        let timeout = if dirty { interval.saturating_sub(last.elapsed()) } else { interval };
        wait(fd, timeout);
    }
}

/// Tells `crate::damage` what changed on monitor `i`: `rects` are on the root window,
/// `None` when the server can't say (every pixel, then). The first frame shows how the
/// monitor looks rather than a change.
fn note_damage(i: usize, area: Area, first: bool, rects: Option<&[xproto::Rectangle]>) {
    let size = (area.w as usize, area.h as usize);
    let display = i as u32;
    if first {
        return crate::damage::note(display, size, &[]);
    }
    let Some(rects) = rects else {
        return crate::damage::note_all(display, size);
    };
    let local: Vec<_> = rects.iter().filter_map(|r| clip(area, r)).collect();
    crate::damage::note(display, size, &local);
}

/// `r`'s part inside `area`, as (x0, y0, x1, y1) in `area`'s pixels.
fn clip(area: Area, r: &xproto::Rectangle) -> Option<(u32, u32, u32, u32)> {
    let (ax, ay) = (area.x as i32, area.y as i32);
    let x0 = (r.x as i32).max(ax);
    let y0 = (r.y as i32).max(ay);
    let x1 = (r.x as i32 + r.width as i32).min(ax + area.w as i32);
    let y1 = (r.y as i32 + r.height as i32).min(ay + area.h as i32);
    (x0 < x1 && y0 < y1).then(|| ((x0 - ax) as u32, (y0 - ay) as u32, (x1 - ax) as u32, (y1 - ay) as u32))
}

/// Follows a resolution or arrangement change (xrandr, or an xrdp client resizing its
/// window). Monitors are matched by position in the list; ones plugged in later aren't
/// picked up until the host restarts.
fn relayout(conn: &RustConnection, root: xproto::Window, has_randr: bool, targets: &mut [Target]) -> Result<()> {
    let areas = monitors(conn, root, has_randr)?;
    for (i, (target, &area)) in targets.iter_mut().zip(&areas).enumerate() {
        if target.area != area {
            info!("screen {i}: now {}x{} at {},{}", area.w, area.h, area.x, area.y);
            target.area = area;
            target.sink.moved(area.rect());
        }
    }
    Ok(())
}

fn grab(
    conn: &RustConnection,
    root: xproto::Window,
    target: &mut Target,
    format: PixelFormat,
    cursor: Option<&Cursor>,
    pointer: (i16, i16),
) -> Result<()> {
    let Area { x, y, w, h } = target.area;
    let len = w as usize * h as usize * 4;
    if target.segment.as_ref().is_none_or(|s| s.len != len) {
        if let Some(old) = target.segment.take() {
            old.release(conn);
        }
        target.segment = Some(Segment::new(conn, len)?);
    }
    let segment = target.segment.as_ref().unwrap();

    let captured_us = now_us();
    conn.shm_get_image(root, x, y, w, h, !0, xproto::ImageFormat::Z_PIXMAP.into(), segment.id, 0)?
        .reply()?;
    let mut pixels = target.sink.buffer();
    pixels.clear();
    pixels.extend_from_slice(segment.bytes());
    if let Some(cursor) = cursor {
        let at = (pointer.0 as i32 - x as i32, pointer.1 as i32 - y as i32);
        cursor.draw(&mut pixels, w as i32, h as i32, at, format);
    }
    // Replaces a frame the encoder hasn't started on: it always gets the newest.
    target.sink.put(
        Frame {
            width: w as usize,
            height: h as usize,
            stride: w as usize * 4,
            format,
            data: pixels,
        },
        captured_us,
    );
    Ok(())
}

/// Sleeps until the X server sends something or `timeout` passes.
fn wait(fd: std::os::fd::RawFd, timeout: Duration) {
    if timeout.is_zero() {
        return;
    }
    let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let ms = timeout.as_millis().clamp(1, 1000) as i32;
    // SAFETY: one valid pollfd; the connection owns `fd` for the life of the thread.
    unsafe { libc::poll(&mut poll, 1, ms) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(pixel: u32) -> Cursor {
        Cursor { w: 1, h: 1, hot: (0, 0), pixels: vec![pixel] }
    }

    #[test]
    fn cursor_blends_into_the_frame() {
        // A 2x1 white frame; an opaque red pixel lands on the second one only.
        let mut frame = vec![255u8; 8];
        cursor(0xffff_0000).draw(&mut frame, 2, 1, (1, 0), PixelFormat::Bgrx);
        assert_eq!(frame, [255, 255, 255, 255, 0, 0, 255, 255]);
        // Half-transparent black (premultiplied) halves what's under it.
        let mut frame = vec![200u8; 4];
        cursor(0x8000_0000).draw(&mut frame, 1, 1, (0, 0), PixelFormat::Bgrx);
        assert_eq!(frame[..3], [99, 99, 99]);
        // RGBx frames take the channels the other way round.
        let mut frame = vec![0u8; 4];
        cursor(0xffff_0000).draw(&mut frame, 1, 1, (0, 0), PixelFormat::Rgbx);
        assert_eq!(frame[..3], [255, 0, 0]);
    }

    #[test]
    fn damage_is_clipped_to_each_monitor() {
        let right = Area { x: 1920, y: 0, w: 1280, h: 1024 };
        let rect = |x, y, width, height| xproto::Rectangle { x, y, width, height };
        // Straddling the two monitors: only the right part, in that monitor's pixels.
        assert_eq!(clip(right, &rect(1900, 10, 40, 20)), Some((0, 10, 20, 30)));
        assert_eq!(clip(right, &rect(0, 0, 100, 100)), None);
        assert_eq!(clip(right, &rect(3190, 1020, 50, 50)), Some((1270, 1020, 1280, 1024)));
    }

    #[test]
    fn cursor_is_clipped_at_the_edges() {
        let big = Cursor { w: 3, h: 3, hot: (1, 1), pixels: vec![0xffff_ffff; 9] };
        let mut frame = vec![0u8; 2 * 2 * 4];
        // Hotspot on the top-left pixel, and well outside: neither may write out of bounds.
        big.draw(&mut frame, 2, 2, (0, 0), PixelFormat::Bgrx);
        assert_eq!(frame[..3], [255, 255, 255]);
        big.draw(&mut frame, 2, 2, (-10, 50), PixelFormat::Bgrx);
    }
}
