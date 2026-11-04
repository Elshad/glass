//! The pointer on macOS, for viewers that draw it themselves (see `crate::cursor`): where it
//! is from a CGEvent, and the system cursor's image (whichever app set it) from NSCursor.
//!
//! Adapted from voxl (MIT, github.com/tembo/voxl): its macOS cursor_loop and render.

use std::hash::{Hash, Hasher};

use objc2_app_kit::{NSCursor, NSImage};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGEvent, CGImageAlphaInfo, CGImageByteOrderInfo};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::cursor::{Image, MAX_SIDE, Poll, Sample};

/// Images are drawn at Retina density: crisp on a Retina display, and the viewer scales
/// them to what it shows anyway.
const SCALE: f64 = 2.0;

#[derive(Default)]
pub struct Poller {
    /// The last image's hash, to send it only when it changes.
    last: Option<u64>,
}

impl Poll for Poller {
    fn poll(&mut self, _shared: &crate::server::Shared) -> Option<Sample> {
        // Global points, top-left origin: the coordinates displays are placed in.
        let at = CGEvent::location(CGEvent::new(None).as_deref());
        let image = objc2::rc::autoreleasepool(|_| {
            // Deprecated, but the only way to the cursor another app set; Apple says it
            // will be nil one day, and then the plain arrow stands in.
            #[allow(deprecated)]
            let cursor = NSCursor::currentSystemCursor().unwrap_or_else(NSCursor::arrowCursor);
            let hot = cursor.hotSpot();
            let (w, h, rgba) = render(&cursor.image(), SCALE)?;
            let (hot_x, hot_y) = ((hot.x * SCALE).round().max(0.0) as usize, (hot.y * SCALE).round().max(0.0) as usize);
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (w, h, hot_x, hot_y, &rgba).hash(&mut hasher);
            let key = hasher.finish();
            if self.last == Some(key) {
                return None;
            }
            self.last = Some(key);
            Some(Image { width: w, height: h, hot_x: hot_x.min(w - 1), hot_y: hot_y.min(h - 1), scale: SCALE as f32, rgba })
        });
        // macOS doesn't say when another app hides the pointer (while typing, say).
        Some(Sample { at: (at.x, at.y), visible: true, image })
    }
}

/// The image at `scale` pixels per point, as straight RGBA.
fn render(image: &NSImage, scale: f64) -> Option<(usize, usize, Vec<u8>)> {
    let size: NSSize = image.size();
    let (w, h) = ((size.width * scale).round().max(1.0) as usize, (size.height * scale).round().max(1.0) as usize);
    if w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w as f64, h as f64));
    // SAFETY: `rect` outlives the call; no context or hints.
    let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
    let mut px = vec![0u8; w * h * 4];
    let space = CGColorSpace::new_device_rgb()?;
    // SAFETY: `px` holds w × h RGBA pixels and outlives the context, released at the end of
    // this block.
    unsafe {
        let ctx = CGBitmapContextCreate(
            px.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
        )?;
        CGContext::draw_image(
            Some(&ctx),
            CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } },
            Some(&cg),
        );
    }
    crate::cursor::unpremultiply(&mut px);
    Some((w, h, px))
}
