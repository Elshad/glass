//! The Windows clipboard: Unicode text, rich text ("HTML Format", CF_HTML) and images
//! (the "PNG" format browsers and Office use, and CF_DIB for everything else). Changes are
//! noticed through the clipboard's sequence number, which costs nothing to poll.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::broadcast;
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::{BI_BITFIELDS, BI_RGB, BITMAPINFOHEADER};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::core::w;

use crate::clip::{self, Clip, MAX_IMAGE};
use crate::control::ServerMsg;

const POLL: Duration = Duration::from_millis(300);

/// The sequence number after our own last write, so it isn't echoed back to viewers.
static OURS: AtomicU32 = AtomicU32::new(0);

fn html_format() -> u32 {
    // SAFETY: a constant string; registering an existing name returns its id.
    unsafe { RegisterClipboardFormatW(w!("HTML Format")) }
}

fn png_format() -> u32 {
    // SAFETY: as above.
    unsafe { RegisterClipboardFormatW(w!("PNG")) }
}

/// The clipboard, open; closed when dropped. Another app may hold it for a moment, so
/// opening retries briefly.
struct Open;

impl Open {
    fn new() -> Option<Self> {
        for _ in 0..20 {
            // SAFETY: no owner window: allowed, and EmptyClipboard then leaves it ownerless.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Some(Self);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        tracing::debug!("clipboard: busy");
        None
    }

    fn has(&self, format: u32) -> bool {
        // SAFETY: plain query.
        unsafe { IsClipboardFormatAvailable(format) }.is_ok()
    }

    /// A copy of the bytes of `format`, if the clipboard holds it.
    fn get(&self, format: u32) -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open; the handle is locked only while copying.
        unsafe {
            let handle = GetClipboardData(format).ok()?;
            let global = HGLOBAL(handle.0);
            let size = GlobalSize(global);
            let ptr = GlobalLock(global) as *const u8;
            if ptr.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
            let _ = GlobalUnlock(global);
            Some(bytes)
        }
    }

    /// Puts `bytes` on the clipboard as `format` (after `EmptyClipboard`).
    fn set(&self, format: u32, bytes: &[u8]) {
        // SAFETY: a fresh movable block, filled then handed to the clipboard, which owns it
        // from SetClipboardData on.
        unsafe {
            let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else { return };
            let ptr = GlobalLock(global) as *mut u8;
            if ptr.is_null() {
                return;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let _ = GlobalUnlock(global);
            if let Err(e) = SetClipboardData(format, Some(HANDLE(global.0))) {
                tracing::warn!("clipboard: {e}");
            }
        }
    }

    fn text(&self) -> Option<String> {
        let bytes = self.get(CF_UNICODETEXT.0 as u32)?;
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Some(String::from_utf16_lossy(&units[..len]))
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: opened in `new`.
        let _ = unsafe { CloseClipboard() };
    }
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

pub fn set_text(text: &str) {
    set(&Clip { text: text.to_owned(), ..Default::default() });
}

/// Text, rich text and an image together, as a viewer copied them.
pub fn set(c: &Clip) {
    let Some(open) = Open::new() else { return };
    // SAFETY: the clipboard is open.
    if unsafe { EmptyClipboard() }.is_err() {
        return;
    }
    if !c.text.is_empty() {
        open.set(CF_UNICODETEXT.0 as u32, &utf16z(&c.text));
    }
    if let Some(html) = &c.html {
        open.set(html_format(), &to_cf_html(html));
    }
    if let Some(image) = &c.image {
        if clip::is_png(image) {
            open.set(png_format(), image);
            if let Some(dib) = png_to_dib(image) {
                open.set(CF_DIB.0 as u32, &dib);
            }
        } else {
            // SAFETY: a constant string.
            open.set(unsafe { RegisterClipboardFormatW(w!("JFIF")) }, image);
        }
    }
    drop(open);
    // SAFETY: plain query.
    OURS.store(unsafe { GetClipboardSequenceNumber() }, Ordering::Relaxed);
}

/// The clipboard's image as PNG: its PNG if it has one, else its bitmap converted.
pub fn image_png() -> Option<Vec<u8>> {
    let open = Open::new()?;
    if let Some(png) = open.get(png_format()).filter(|p| clip::is_png(p)) {
        return (png.len() <= MAX_IMAGE).then_some(png);
    }
    let dib = open.get(CF_DIB.0 as u32)?;
    drop(open);
    let frame = dib_to_bgra(&dib)?;
    crate::lossless::encode_png(&frame, [0, 0, frame.width, frame.height]).ok().filter(|p| p.len() <= MAX_IMAGE)
}

/// What a viewer hears about a change: text, rich text, and whether there's an image.
fn snapshot() -> Option<ServerMsg> {
    let open = Open::new()?;
    let text = open.text().unwrap_or_default();
    let html = clip::html_for_viewer(open.get(html_format()).and_then(|b| from_cf_html(&b)));
    let image = open.has(png_format()) || open.has(CF_DIB.0 as u32);
    Some(ServerMsg::Clipboard { text, html, image })
}

/// Broadcasts the clipboard whenever it changes on the computer.
pub fn watch(events: broadcast::Sender<ServerMsg>) {
    std::thread::spawn(move || {
        // SAFETY: plain query.
        let mut last = unsafe { GetClipboardSequenceNumber() };
        loop {
            std::thread::sleep(POLL);
            // SAFETY: plain query.
            let seq = unsafe { GetClipboardSequenceNumber() };
            if seq == last {
                continue;
            }
            last = seq;
            if seq == OURS.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(msg @ ServerMsg::Clipboard { .. }) = snapshot()
                && matches!(&msg, ServerMsg::Clipboard { text, html, image } if !text.is_empty() || html.is_some() || *image)
            {
                let _ = events.send(msg);
            }
        }
    });
}

/// CF_HTML: a header of byte offsets into the UTF-8 that follows, and the fragment marked.
fn to_cf_html(html: &str) -> Vec<u8> {
    const HEADER: &str = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let start_html = HEADER.len();
    let prefix = "<html><body><!--StartFragment-->";
    let start_fragment = start_html + prefix.len();
    let end_fragment = start_fragment + html.len();
    let end_html = end_fragment + "<!--EndFragment--></body></html>".len();
    let mut out = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n{prefix}{html}<!--EndFragment--></body></html>"
    )
    .into_bytes();
    out.push(0);
    out
}

/// The fragment of a CF_HTML clip (what was copied, without the page around it).
fn from_cf_html(bytes: &[u8]) -> Option<String> {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let bytes = &bytes[..len];
    let header = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let offset = |key: &str| -> Option<usize> {
        let at = header.find(&format!("{key}:"))? + key.len() + 1;
        header[at..].split(['\r', '\n']).next()?.trim().parse().ok()
    };
    let (start, end) = match (offset("StartFragment"), offset("EndFragment")) {
        (Some(s), Some(e)) => (s, e),
        _ => (offset("StartHTML")?, offset("EndHTML")?),
    };
    let fragment = bytes.get(start..end.min(bytes.len()))?;
    Some(String::from_utf8_lossy(fragment).into_owned())
}

/// A bottom-up 32-bit CF_DIB of a PNG.
fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let channels = info.color_type.samples();
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        biHeight: h as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: (w * h * 4) as u32,
        ..Default::default()
    };
    // SAFETY: BITMAPINFOHEADER is plain old data.
    let mut dib = unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, std::mem::size_of::<BITMAPINFOHEADER>()) }.to_vec();
    dib.reserve(w * h * 4);
    for y in (0..h).rev() {
        for px in buf[y * info.line_size..][..w * channels].chunks_exact(channels) {
            let (r, g, b, a) = match channels {
                4 => (px[0], px[1], px[2], px[3]),
                2 => (px[0], px[0], px[0], px[1]),
                _ => return None,
            };
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some(dib)
}

/// A CF_DIB (24- or 32-bit, either way up) as a top-down BGRA picture.
fn dib_to_bgra(dib: &[u8]) -> Option<crate::lossless::RawFrame> {
    let size = std::mem::size_of::<BITMAPINFOHEADER>();
    if dib.len() < size {
        return None;
    }
    // SAFETY: at least a header's worth of bytes; read unaligned.
    let header: BITMAPINFOHEADER = unsafe { std::ptr::read_unaligned(dib.as_ptr() as *const BITMAPINFOHEADER) };
    let (w, h) = (header.biWidth.unsigned_abs() as usize, header.biHeight.unsigned_abs() as usize);
    let bottom_up = header.biHeight > 0;
    let bpp = header.biBitCount as usize;
    if w == 0 || h == 0 || !(bpp == 24 || bpp == 32) {
        return None;
    }
    // BI_BITFIELDS keeps three colour masks after a plain header (assumed BGRA order).
    let mut offset = header.biSize as usize;
    if header.biCompression == BI_BITFIELDS.0 && header.biSize as usize == size {
        offset += 12;
    } else if header.biCompression != BI_RGB.0 && header.biCompression != BI_BITFIELDS.0 {
        return None;
    }
    let stride = (w * bpp).div_ceil(32) * 4;
    let pixels = dib.get(offset..offset + stride * h)?;
    let mut bgra = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = &pixels[if bottom_up { h - 1 - y } else { y } * stride..][..w * bpp / 8];
        for px in row.chunks_exact(bpp / 8) {
            bgra.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    Some(crate::lossless::RawFrame { width: w, height: h, bgra, capture_us: 0 })
}

/// The real system clipboard.
pub struct SystemClipboard;

impl crate::media::Clipboard for SystemClipboard {
    fn set_text(&self, text: &str) {
        set_text(text)
    }

    fn set(&self, c: &Clip) {
        set(c)
    }

    fn image_png(&self) -> Option<Vec<u8>> {
        image_png()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_round_trips() {
        let html = "<b>héllo</b> <i>world</i>";
        let cf = to_cf_html(html);
        let text = String::from_utf8_lossy(&cf);
        assert!(text.starts_with("Version:0.9\r\nStartHTML:0000000105\r\n"), "{text}");
        assert_eq!(from_cf_html(&cf).as_deref(), Some(html));
    }

    #[test]
    fn reads_fragments_from_browsers() {
        // As Chrome writes it: offsets without padding, a fragment inside a page.
        let body = "<html><body><!--StartFragment--><p>hi</p><!--EndFragment--></body></html>";
        let header_len = "Version:0.9\r\nStartHTML:000\r\nEndHTML:000\r\nStartFragment:000\r\nEndFragment:000\r\n".len();
        let start = header_len + body.find("<p>").unwrap();
        let end = header_len + body.find("<!--End").unwrap();
        let cf = format!(
            "Version:0.9\r\nStartHTML:{header_len:03}\r\nEndHTML:{:03}\r\nStartFragment:{start:03}\r\nEndFragment:{end:03}\r\n{body}",
            header_len + body.len()
        );
        assert_eq!(from_cf_html(cf.as_bytes()).as_deref(), Some("<p>hi</p>"));
    }

    #[test]
    fn bitmaps_convert_both_ways() {
        // A 2×1 PNG (red, blue) → DIB → BGRA.
        let frame = crate::lossless::RawFrame { width: 2, height: 1, bgra: vec![0, 0, 255, 255, 255, 0, 0, 255], capture_us: 0 };
        let png = crate::lossless::encode_png(&frame, [0, 0, 2, 1]).unwrap();
        let dib = png_to_dib(&png).unwrap();
        let back = dib_to_bgra(&dib).unwrap();
        assert_eq!((back.width, back.height), (2, 1));
        assert_eq!(back.bgra, frame.bgra);
    }

    #[test]
    fn top_down_24_bit_bitmaps() {
        let header = BITMAPINFOHEADER {
            biSize: 40,
            biWidth: 1,
            biHeight: -2,
            biPlanes: 1,
            biBitCount: 24,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        // SAFETY: plain old data.
        let mut dib = unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, 40) }.to_vec();
        // Rows padded to 4 bytes: blue then green.
        dib.extend_from_slice(&[255, 0, 0, 0, 0, 255, 0, 0]);
        let frame = dib_to_bgra(&dib).unwrap();
        assert_eq!(frame.bgra, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }
}
