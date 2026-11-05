//! The pointer on Windows, for viewers that draw it themselves (see `crate::cursor`): where
//! it is, whether it's showing and its image, from GetCursorInfo and GetIconInfo. The
//! process is per-monitor DPI aware, so positions are physical pixels, like the displays'.
//!
//! Adapted from voxl (MIT, github.com/tembo/voxl): its Windows cursor_loop and
//! bitmap_pixels.

use std::ffi::c_void;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW,
    HBITMAP, HGDIOBJ, ReleaseDC,
};
use windows::Win32::UI::WindowsAndMessaging::{CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetIconInfo, HICON, ICONINFO};

use crate::cursor::{Image, MAX_SIDE, Poll, Sample};

#[derive(Default)]
pub struct Poller {
    /// The last cursor handle sent: a new one is a new image.
    last: Option<isize>,
}

impl Poll for Poller {
    fn poll(&mut self, _shared: &crate::server::Shared) -> Option<Sample> {
        let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        // SAFETY: ci is a properly sized out-parameter.
        unsafe { GetCursorInfo(&mut ci) }.ok()?;
        let visible = ci.flags.0 & CURSOR_SHOWING.0 != 0 && !ci.hCursor.is_invalid();
        let mut image = None;
        let handle = ci.hCursor.0 as isize;
        if visible && self.last != Some(handle) {
            // SAFETY: a cursor handle is an icon handle; GetIconInfo copies its bitmaps.
            image = unsafe { cursor_image(HICON(ci.hCursor.0)) };
            if image.is_some() {
                self.last = Some(handle);
            }
        }
        Some(Sample { at: (ci.ptScreenPos.x as f64, ci.ptScreenPos.y as f64), visible, image })
    }
}

/// The cursor's image as straight RGBA, with its hotspot.
unsafe fn cursor_image(cursor: HICON) -> Option<Image> {
    let mut ii = ICONINFO::default();
    // SAFETY: (the caller's) a valid cursor handle; the bitmaps are deleted below.
    unsafe { GetIconInfo(cursor, &mut ii) }.ok()?;
    // SAFETY: the bitmaps GetIconInfo just made.
    let mask = unsafe { bitmap_pixels(ii.hbmMask) };
    let colour = if ii.hbmColor.is_invalid() { None } else { unsafe { bitmap_pixels(ii.hbmColor) } };
    // SAFETY: GetIconInfo hands the bitmaps over to the caller.
    unsafe {
        let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        }
    }
    let (hot_x, hot_y) = (ii.xHotspot as usize, ii.yHotspot as usize);
    // A mask pixel is set when it's white (GetDIBits expands the 1-bit palette).
    let bits = |m: &[u8], from: usize, n: usize| (from..from + n).map(|i| m.get(i * 4).is_some_and(|&b| b != 0)).collect::<Vec<_>>();
    let (width, height, rgba) = match colour {
        Some((w, h, px)) => {
            let and = mask.as_ref().filter(|(mw, mh, _)| *mw == w && *mh >= h).map(|(_, _, m)| bits(m, 0, w * h));
            (w, h, crate::cursor::colour_to_rgba(w, h, &px, and.as_deref()))
        }
        // Monochrome: the mask holds the AND image over the XOR image.
        None => {
            let (w, h2, m) = mask?;
            let h = h2 / 2;
            (w, h, crate::cursor::mono_to_rgba(w, h, &bits(&m, 0, w * h), &bits(&m, w * h, w * h)))
        }
    };
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    Some(Image {
        width,
        height,
        hot_x: hot_x.min(width - 1),
        hot_y: hot_y.min(height - 1),
        scale: 1.0,
        rgba,
    })
}

/// A bitmap's pixels as top-down BGRA.
unsafe fn bitmap_pixels(bm: HBITMAP) -> Option<(usize, usize, Vec<u8>)> {
    let mut b = BITMAP::default();
    // SAFETY: b is a BITMAP-sized out-parameter.
    if unsafe { GetObjectW(HGDIOBJ(bm.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut b as *mut _ as *mut c_void)) } == 0 {
        return None;
    }
    let (w, h) = (b.bmWidth as usize, b.bmHeight as usize);
    if w == 0 || h == 0 || w > 512 || h > 1024 {
        return None;
    }
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut px = vec![0u8; w * h * 4];
    // SAFETY: px holds h rows of w 32-bit pixels, as `info` describes.
    let n = unsafe {
        let dc = GetDC(None);
        let n = GetDIBits(dc, bm, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        n
    };
    (n > 0).then_some((w, h, px))
}
