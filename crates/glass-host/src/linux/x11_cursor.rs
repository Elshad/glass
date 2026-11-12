//! The pointer on X11, for viewers that draw it themselves (`crate::cursor`): where it is
//! on the root window, and its image from XFIXES when that changes. Root-window pixels are
//! the coordinates the X11 capture gives its displays, so a position needs no converting.

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use crate::cursor::{Image, MAX_SIDE, Poll, Sample};
use crate::server::Shared;

#[derive(Default)]
pub struct Poller {
    conn: Option<(RustConnection, xproto::Window)>,
    /// The image needs fetching (at first, and after XFIXES says it changed).
    stale: bool,
    /// The last image was see-through all over: an app hid the pointer.
    blank: bool,
}

impl Poller {
    fn connect() -> Option<(RustConnection, xproto::Window)> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        conn.extension_information(xfixes::X11_EXTENSION_NAME).ok()??;
        conn.xfixes_query_version(5, 0).ok()?.reply().ok()?;
        let root = conn.setup().roots[screen].root;
        conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)
            .ok()?
            .check()
            .ok()?;
        Some((conn, root))
    }

    fn image(conn: &RustConnection) -> Option<(Image, bool)> {
        let c = conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
        let (w, h) = (c.width as usize, c.height as usize);
        if c.cursor_image.len() != w * h || w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return None;
        }
        // Premultiplied ARGB words to straight RGBA bytes.
        let mut rgba = Vec::with_capacity(w * h * 4);
        for p in &c.cursor_image {
            rgba.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8, (p >> 24) as u8]);
        }
        let blank = rgba.chunks_exact(4).all(|px| px[3] == 0);
        crate::cursor::unpremultiply(&mut rgba);
        let image = Image {
            width: w,
            height: h,
            hot_x: (c.xhot as usize).min(w - 1),
            hot_y: (c.yhot as usize).min(h - 1),
            scale: 1.0,
            rgba,
        };
        Some((image, blank))
    }

    fn look(&mut self) -> Option<Sample> {
        let (conn, root) = self.conn.as_ref()?;
        while let Some(event) = conn.poll_for_event().ok()? {
            if matches!(event, Event::XfixesCursorNotify(_)) {
                self.stale = true;
            }
        }
        let p = conn.query_pointer(*root).ok()?.reply().ok()?;
        let mut image = None;
        if self.stale
            && let Some((img, blank)) = Self::image(conn)
        {
            self.stale = false;
            self.blank = blank;
            image = Some(img);
        }
        Some(Sample {
            at: (p.root_x as f64, p.root_y as f64),
            visible: p.same_screen && !self.blank,
            image,
        })
    }
}

impl Poll for Poller {
    fn poll(&mut self, _shared: &Shared) -> Option<Sample> {
        if self.conn.is_none() {
            self.conn = Self::connect();
            self.stale = true;
        }
        let sample = self.look();
        if sample.is_none() {
            // The X server went away or refused: try a fresh connection next time.
            self.conn = None;
        }
        sample
    }
}
