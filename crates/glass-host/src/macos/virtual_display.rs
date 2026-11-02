//! A virtual display in the viewer's shape. "Fit this device": it stands in for a display,
//! so the desktop fills an iPad (4:3) or another screen with no black bars. "Use as a second
//! display": it stays its own display beside the main one, extending the desktop onto the
//! viewer. Dropping it (or this process exiting) removes it and macOS restores the displays.
//!
//! macOS has no public API for this; CGVirtualDisplay is CoreGraphics SPI (also used by
//! apps such as BetterDisplay and DeskPad), reached here through the Objective-C runtime
//! so a missing class fails cleanly instead of failing to link.

use anyhow::{Context, Result, bail};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CFArray, CFDictionary, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
    CGConfigureDisplayOrigin, CGConfigureDisplayWithDisplayMode, CGConfigureOption, CGDisplayCopyAllDisplayModes, CGDisplayMode, CGDirectDisplayID, CGDisplayBounds, CGDisplayConfigRef,
    CGError, CGGetActiveDisplayList, CGMainDisplayID,
};
use objc2_foundation::{NSArray, NSDictionary, NSString};


pub struct VirtualDisplay {
    /// Released on drop, which removes the display.
    display: Option<Retained<AnyObject>>,
    pub id: CGDirectDisplayID,
    /// Its size in pixels.
    pub width: u32,
    pub height: u32,
    hidpi: bool,
    /// The physical display this one stands in for, turned off until drop.
    replaced: Option<CGDirectDisplayID>,
    /// Where every display was, put back on drop (macOS rearranges the others when one is
    /// added, turned off or removed).
    /// Recorded before this display existed: adding one makes macOS rearrange the rest.
    arrangement: Vec<(CGDirectDisplayID, CGRect)>,
}

// The object is only messaged from whichever thread owns the VirtualDisplay.
unsafe impl Send for VirtualDisplay {}

fn class(name: &str) -> Result<&'static AnyClass> {
    AnyClass::get(&std::ffi::CString::new(name)?)
        .with_context(|| format!("{name} isn't available on this macOS"))
}

impl VirtualDisplay {
    /// `width`×`height` pixels; with `hidpi`, a Retina mode (half as many points, each two
    /// pixels wide), so text on an iPad is as crisp as on the iPad itself.
    pub fn create_scaled(width: u32, height: u32, hidpi: bool) -> Result<Self> {
        anyhow::ensure!(
            (640..=7680).contains(&width) && (480..=4320).contains(&height),
            "unsupported size {width}×{height}"
        );
        let arrangement: Vec<_> = active_displays().into_iter().map(|id| (id, CGDisplayBounds(id))).collect();
        unsafe {
            let descriptor: Retained<AnyObject> =
                msg_send![class("CGVirtualDisplayDescriptor")?, new];
            let _: () = msg_send![&descriptor, setName: &*NSString::from_str("Glass")];
            // Room to turn: the longer side either way, so a rotated viewer can resize it.
            let _: () = msg_send![&descriptor, setMaxPixelsWide: width.max(height)];
            let _: () = msg_send![&descriptor, setMaxPixelsHigh: width.max(height)];
            // A plausible physical size (~110 ppi) so macOS picks sensible UI scaling.
            let mm = CGSize {
                width: width as f64 / 110.0 * 25.4,
                height: height as f64 / 110.0 * 25.4,
            };
            let _: () = msg_send![&descriptor, setSizeInMillimeters: mm];
            let _: () = msg_send![&descriptor, setVendorID: 0xF1A5u32];
            let _: () = msg_send![&descriptor, setProductID: 0x0001u32];
            let _: () = msg_send![&descriptor, setSerialNum: 0x0001u32];
            let _: () = msg_send![&descriptor, setRedPrimary: CGPoint { x: 0.64, y: 0.33 }];
            let _: () = msg_send![&descriptor, setGreenPrimary: CGPoint { x: 0.30, y: 0.60 }];
            let _: () = msg_send![&descriptor, setBluePrimary: CGPoint { x: 0.15, y: 0.06 }];
            let _: () = msg_send![&descriptor, setWhitePoint: CGPoint { x: 0.3127, y: 0.3290 }];

            let alloc: Allocated<AnyObject> = msg_send![class("CGVirtualDisplay")?, alloc];
            let display: Option<Retained<AnyObject>> =
                msg_send![alloc, initWithDescriptor: &*descriptor];
            let display = display.context("couldn't create the virtual display")?;

            apply_mode(&display, width, height, hidpi)?;
            let id: CGDirectDisplayID = msg_send![&display, displayID];
            let _ = sel!(displayID);
            Ok(Self {
                display: Some(display),
                id,
                width,
                height,
                hidpi,
                replaced: None,
                arrangement,
            })
        }
    }

    /// Turns `display` off and puts this one in its place (the main display stays main, menu
    /// bar and all; the windows move here). Unlike mirroring, `display` never has to show our
    /// shape, which some monitors can't (they lose the signal every few seconds), so this one
    /// can be any size. For this process only: if it exits, macOS turns `display` back on.
    pub fn replace(&mut self, display: CGDirectDisplayID) -> Result<()> {
        let enable = display_enabler()?;
        configure_with(CGConfigureOption::ForAppOnly, |cfg| unsafe { enable(cfg, display, false) })?;
        self.replaced = Some(display);
        self.settle()
    }

    /// A new size (a viewer turned): same display, so windows stay on it.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let display = self.display.as_ref().context("the virtual display is gone")?;
        apply_mode(display, width, height, self.hidpi)?;
        self.width = width;
        self.height = height;
        self.settle()
    }

    /// Our sharpest mode (Retina when there is one), in the place of the display we stand
    /// in for, the others beside it as they were.
    fn settle(&self) -> Result<()> {
        let mode = retina_mode(self.id, self.width, self.height);
        let size = mode.as_deref().map_or(CGDisplayBounds(self.id).size, |m| CGSize {
            width: CGDisplayMode::width(Some(m)) as f64,
            height: CGDisplayMode::height(Some(m)) as f64,
        });
        let target = self.replaced.and_then(|d| self.arrangement.iter().find(|(id, _)| *id == d)).map(|(_, b)| *b);
        let mut moves = Vec::new();
        if let Some(target) = target {
            moves.push((self.id, target.origin.x.round() as i32, target.origin.y.round() as i32));
            let others: Vec<_> = self.arrangement.iter().filter(|(id, _)| Some(*id) != self.replaced).copied().collect();
            moves.extend(keep_beside(target, size, &others));
        }
        configure(|cfg| {
            if let Some(mode) = mode.as_deref() {
                let err = unsafe { CGConfigureDisplayWithDisplayMode(cfg, self.id, Some(mode), None) };
                if err != CGError::Success {
                    return err;
                }
            }
            for &(id, x, y) in &moves {
                let err = unsafe { CGConfigureDisplayOrigin(cfg, id, x, y) };
                if err != CGError::Success {
                    return err;
                }
            }
            CGError::Success
        })
    }

    /// The physical display this one stands in for.
    pub fn replaced(&self) -> Option<CGDirectDisplayID> {
        self.replaced
    }

    /// Places this display just right of the main one, top edges aligned, so the pointer
    /// moves onto it past the main display's right edge.
    pub fn place_right_of_main(&self) -> Result<()> {
        let main = CGDisplayBounds(CGMainDisplayID());
        let (x, y) = right_of(main.origin.x, main.origin.y, main.size.width);
        configure(|cfg| unsafe { CGConfigureDisplayOrigin(cfg, self.id, x, y) })
    }
}

/// The display's Retina mode (two pixels a point) at its full pixel size, if it has one.
fn retina_mode(display: CGDirectDisplayID, width: u32, height: u32) -> Option<CFRetained<CGDisplayMode>> {
    // Retina modes are only listed as "duplicate low resolution" ones.
    let key: &NSString = unsafe { &*(objc2_core_graphics::kCGDisplayShowDuplicateLowResolutionModes as *const objc2_core_foundation::CFString as *const NSString) };
    let options: Retained<NSDictionary<NSString, objc2_foundation::NSNumber>> =
        NSDictionary::from_retained_objects(&[key], &[objc2_foundation::NSNumber::new_bool(true)]);
    let options: &CFDictionary = unsafe { &*(Retained::as_ptr(&options) as *const CFDictionary) };
    let modes = unsafe { CGDisplayCopyAllDisplayModes(display, Some(options)) }?;
    (0..CFArray::count(&modes))
        .filter_map(|i| {
            let mode = unsafe { (CFArray::value_at_index(&modes, i) as *const CGDisplayMode).as_ref() }?;
            Some(unsafe { CFRetained::retain(std::ptr::NonNull::from(mode)) })
        })
        .find(|m| {
            let (pw, ph) = (CGDisplayMode::pixel_width(Some(m)), CGDisplayMode::pixel_height(Some(m)));
            (pw, ph) == (width as usize, height as usize) && CGDisplayMode::width(Some(m)) * 2 == pw
        })
}

/// Gives the display one mode, `width`×`height` pixels at 60 Hz (Retina with `hidpi`).
fn apply_mode(display: &AnyObject, width: u32, height: u32, hidpi: bool) -> Result<()> {
    let (mode_w, mode_h) = if hidpi { (width / 2, height / 2) } else { (width, height) };
    unsafe {
        let mode_alloc: Allocated<AnyObject> = msg_send![class("CGVirtualDisplayMode")?, alloc];
        let mode: Retained<AnyObject> = msg_send![mode_alloc, initWithWidth: mode_w, height: mode_h, refreshRate: 60.0f64];
        let settings: Retained<AnyObject> = msg_send![class("CGVirtualDisplaySettings")?, new];
        let _: () = msg_send![&settings, setHiDPI: u32::from(hidpi)];
        let modes = NSArray::from_retained_slice(&[mode]);
        let _: () = msg_send![&settings, setModes: &*modes];
        let applied: Bool = msg_send![display, applySettings: &*settings];
        if !applied.as_bool() {
            bail!("macOS rejected the virtual display's mode ({width}×{height})");
        }
    }
    Ok(())
}

fn active_displays() -> Vec<CGDirectDisplayID> {
    let mut ids = [0; 16];
    let mut count = 0;
    let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        return Vec::new();
    }
    ids[..count as usize].to_vec()
}

/// New origins for the displays around `target` once it's `size` points instead: those to
/// its right or below move by the change, so they stay flush on the same side.
fn keep_beside(
    target: CGRect,
    size: CGSize,
    others: &[(CGDirectDisplayID, CGRect)],
) -> Vec<(CGDirectDisplayID, i32, i32)> {
    let dw = size.width - target.size.width;
    let dh = size.height - target.size.height;
    let (right, bottom) = (target.origin.x + target.size.width, target.origin.y + target.size.height);
    others
        .iter()
        .map(|(id, b)| {
            let x = if b.origin.x >= right { b.origin.x + dw } else { b.origin.x };
            let y = if b.origin.y >= bottom { b.origin.y + dh } else { b.origin.y };
            (*id, x.round() as i32, y.round() as i32)
        })
        .collect()
}

/// The origin that puts a display flush against the right edge of one at (`x`, `y`) that is
/// `width` points wide.
fn right_of(x: f64, y: f64, width: f64) -> (i32, i32) {
    ((x + width).round() as i32, y.round() as i32)
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } }
    }

    #[test]
    fn neighbours_keep_their_side() {
        let target = rect(0.0, 0.0, 2560.0, 1440.0);
        let size = CGSize { width: 1590.0, height: 1160.0 };
        let others = [
            (1, rect(-2560.0, 0.0, 2560.0, 1440.0)), // left: stays
            (2, rect(2560.0, 0.0, 1920.0, 1080.0)),  // right: follows the new right edge
            (3, rect(0.0, 1440.0, 1920.0, 1080.0)),  // below: follows the new bottom
        ];
        assert_eq!(
            super::keep_beside(target, size, &others),
            vec![(1, -2560, 0), (2, 1590, 0), (3, 0, 1160)]
        );
    }

    #[test]
    fn sits_right_of_the_main_display() {
        assert_eq!(super::right_of(0.0, 0.0, 1512.0), (1512, 0));
        assert_eq!(super::right_of(-1920.0, -200.0, 1920.0), (0, -200));
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        if let Some(off) = self.replaced.take()
            && let Err(e) = display_enabler()
                .and_then(|enable| configure_with(CGConfigureOption::ForAppOnly, |cfg| unsafe { enable(cfg, off, true) }))
        {
            tracing::warn!("turning display {off} back on: {e:#}");
        }
        // Gone first: removing a display rearranges the others too.
        drop(self.display.take());
        let arrangement = std::mem::take(&mut self.arrangement);
        if !arrangement.is_empty()
            && let Err(e) = configure(|cfg| {
                for (id, b) in arrangement.iter().filter(|(id, _)| *id != self.id) {
                    let (x, y) = (b.origin.x.round() as i32, b.origin.y.round() as i32);
                    let err = unsafe { CGConfigureDisplayOrigin(cfg, *id, x, y) };
                    if err != CGError::Success {
                        return err;
                    }
                }
                CGError::Success
            })
        {
            tracing::warn!("putting the displays back where they were: {e:#}");
        }
        tracing::info!("virtual display {} removed", self.id);
    }
}

type EnableDisplay = unsafe extern "C" fn(CGDisplayConfigRef, CGDirectDisplayID, bool) -> CGError;

/// CGSConfigureDisplayEnabled: SPI (what BetterDisplay and displayplacer use to turn a
/// display off), looked up at run time so a macOS without it fails cleanly.
fn display_enabler() -> Result<EnableDisplay> {
    let name = c"CGSConfigureDisplayEnabled";
    let f = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    anyhow::ensure!(!f.is_null(), "turning a display off isn't available on this macOS");
    Ok(unsafe { std::mem::transmute::<*mut libc::c_void, EnableDisplay>(f) })
}

/// Applies one display configuration change for this login session.
fn configure(change: impl FnOnce(CGDisplayConfigRef) -> CGError) -> Result<()> {
    configure_with(CGConfigureOption::ForSession, change)
}

fn configure_with(option: CGConfigureOption, change: impl FnOnce(CGDisplayConfigRef) -> CGError) -> Result<()> {
    let mut cfg: CGDisplayConfigRef = std::ptr::null_mut();
    let err = unsafe { CGBeginDisplayConfiguration(&mut cfg) };
    anyhow::ensure!(
        err == CGError::Success,
        "CGBeginDisplayConfiguration: {err:?}"
    );
    let err = change(cfg);
    if err != CGError::Success {
        unsafe { CGCancelDisplayConfiguration(cfg) };
        bail!("display configuration: {err:?}");
    }
    let err = unsafe { CGCompleteDisplayConfiguration(cfg, option) };
    anyhow::ensure!(
        err == CGError::Success,
        "CGCompleteDisplayConfiguration: {err:?}"
    );
    Ok(())
}

/// `glass display-test WxH`: creates a virtual display, stands it in for the main display for
/// a few seconds, then restores, and checks the displays came back as they were.
pub fn self_test(width: u32, height: u32, seconds: u64) -> Result<()> {
    let before = super::capture::list_displays()?
        .into_iter()
        .map(|d| d.info)
        .collect::<Vec<_>>();
    println!("Displays now: {}", describe(&before));
    {
        let mut vd = VirtualDisplay::create_scaled(width, height, true)?;
        println!("Created virtual display {} at {width}×{height}.", vd.id);
        vd.replace(CGMainDisplayID())?;
        println!("It stands in for the main display; turning it in {} s, restoring {seconds} s later…", seconds / 2);
        std::thread::sleep(std::time::Duration::from_secs(seconds / 2));
        vd.resize(height, width)?;
        println!("Turned: {height}×{width}.");
        std::thread::sleep(std::time::Duration::from_secs(seconds - seconds / 2));
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    let after = super::capture::list_displays()?
        .into_iter()
        .map(|d| d.info)
        .collect::<Vec<_>>();
    println!("Displays after: {}", describe(&after));
    let same = before.len() == after.len()
        && before.iter().all(|b| {
            after
                .iter()
                .any(|a| a.id == b.id && a.width == b.width && a.height == b.height)
        });
    if same {
        println!("Restored: the displays are exactly as before.");
        Ok(())
    } else {
        bail!("the displays differ from before; check System Settings › Displays")
    }
}

fn describe(list: &[crate::media::DisplayInfo]) -> String {
    list.iter()
        .map(|d| {
            format!(
                "{} {}×{}{}",
                d.id,
                d.width,
                d.height,
                if d.main { " (main)" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}
