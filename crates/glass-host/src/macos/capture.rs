//! Display capture with ScreenCaptureKit. Frames arrive as IOSurface-backed
//! pixel buffers and go straight to the hardware encoder without a copy.

use std::sync::{Arc, mpsc};

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayIsBuiltin, CGDisplayIsMain, CGDisplayPixelsHigh,
    CGDisplayPixelsWide,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVImageBuffer;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamErrorCode, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::audio::Packetizer;
use super::clock::{cmtime_us, now_us};
use super::encode::Encoder;
pub use crate::media::{DisplayInfo, WindowInfo};

/// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ('420v'), the encoder's native input.
const PIXEL_FORMAT_420V: u32 = u32::from_be_bytes(*b"420v");
/// kCVPixelFormatType_32BGRA: full colour, for the HEVC 4:2:2 sharp-text encoder.
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// The most recent captured frame, kept so a keyframe can be produced on demand: a static
/// screen yields no new frames, and a client that just connected would otherwise wait.
/// With its capture time, for lossless refresh.
struct LastFrame(Option<CFRetained<CVImageBuffer>>, u64);
// CVPixelBuffers are thread-safe, reference-counted CF objects.
unsafe impl Send for LastFrame {}

pub struct FrameState {
    encoder: Arc<Encoder>,
    last: std::sync::Mutex<LastFrame>,
    /// Frame-rate ceiling from the sharpness policy (fewer, sharper frames on a slow link).
    fps_cap: Arc<std::sync::atomic::AtomicU32>,
    /// Capture time of the last frame actually encoded.
    last_encoded_us: std::sync::atomic::AtomicU64,
    /// Bumped on every new image; a pending check that sees it unchanged knows the screen
    /// has gone still.
    generation: std::sync::atomic::AtomicU64,
    /// The newest image was skipped by the frame-rate ceiling and still needs sending.
    unsent: std::sync::atomic::AtomicBool,
    /// Idle refreshes done since the screen went still, and how many came out tiny.
    refreshes: std::sync::atomic::AtomicU32,
    tiny: std::sync::atomic::AtomicU32,
    queue: dispatch2::DispatchRetained<DispatchQueue>,
    /// ScreenCaptureKit ended the stream (e.g. its display slept); it won't resume by itself.
    stopped: std::sync::atomic::AtomicBool,
}

/// A still screen is re-encoded this often for a while, so it sharpens to near-lossless
/// after motion (each pass refines the previous one).
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(83);
/// At most this many refreshes (3 s), and stop after 3 tiny outputs (already sharp).
const MAX_REFRESHES: u32 = 36;
const TINY_FRAME: u64 = 1024;

impl FrameState {
    /// `refresh`: the image was encoded before (only refining it), not a new one.
    fn encode_last(&self, refresh: bool) {
        let Some(image) = self.last.lock().unwrap().0.clone() else {
            return;
        };
        let now = now_us();
        self.last_encoded_us
            .store(now, std::sync::atomic::Ordering::Relaxed);
        self.encoder
            .encode(&image, unsafe { CMTime::new(now as i64, 1_000_000) }, now, refresh);
    }

    /// After a frame (sent or skipped): if nothing newer arrives, send what was skipped,
    /// then keep refining the still picture for a while.
    fn schedule_idle_check(self: &Arc<Self>) {
        use std::sync::atomic::Ordering::Relaxed;
        let fps = self.fps_cap.load(Relaxed).max(1);
        let wait = REFRESH_INTERVAL.max(std::time::Duration::from_micros(1_000_000 / fps as u64));
        let generation = self.generation.load(Relaxed);
        let state = self.clone();
        let Ok(when) = dispatch2::DispatchTime::try_from(wait) else {
            return;
        };
        let _ = self.queue.after(when, move || {
            if state.generation.load(Relaxed) != generation {
                return; // newer frames took over
            }
            if state.unsent.swap(false, Relaxed) {
                state.encode_last(false);
            } else {
                if state.encoder.last_frame_size() < TINY_FRAME {
                    state.tiny.fetch_add(1, Relaxed);
                }
                if state.refreshes.fetch_add(1, Relaxed) >= MAX_REFRESHES
                    || state.tiny.load(Relaxed) >= 3
                {
                    return;
                }
                state.encode_last(true);
            }
            state.schedule_idle_check();
        });
    }
}

pub struct Ivars {
    state: Arc<FrameState>,
    audio: Option<Arc<std::sync::Mutex<Packetizer>>>,
    display: CGDirectDisplayID,
    started: std::time::Instant,
    frames: std::sync::atomic::AtomicU64,
    idle: std::sync::atomic::AtomicU64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "GlassStreamOutput"]
    #[ivars = Ivars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Audio {
                if let Some(audio) = &self.ivars().audio {
                    audio.lock().unwrap().push(sample);
                }
                return;
            }
            if kind != SCStreamOutputType::Screen {
                return;
            }
            use std::sync::atomic::Ordering::Relaxed;
            let ivars = self.ivars();
            // Idle frames (screen unchanged) carry no pixel buffer.
            let Some(image) = (unsafe { sample.image_buffer() }) else {
                if ivars.idle.fetch_add(1, Relaxed) == 0 && ivars.frames.load(Relaxed) == 0 {
                    tracing::info!("display {}: idle before any frame", ivars.display);
                }
                return;
            };
            if ivars.frames.fetch_add(1, Relaxed) == 0 {
                tracing::info!(
                    "display {}: first frame after {} ms",
                    ivars.display,
                    ivars.started.elapsed().as_millis()
                );
            }
            let pts = unsafe { sample.presentation_time_stamp() };
            let capture_us = match cmtime_us(pts) {
                0 => now_us(),
                us => us,
            };
            let state = &ivars.state;
            state.generation.fetch_add(1, Relaxed);
            state.refreshes.store(0, Relaxed);
            state.tiny.store(0, Relaxed);
            // Over the frame-rate ceiling: keep the image, send it later if it's the last.
            let fps = state.fps_cap.load(Relaxed);
            let since = capture_us.saturating_sub(state.last_encoded_us.load(Relaxed));
            let too_soon = fps > 0 && since < 900_000 / fps as u64;
            if too_soon && !state.encoder.keyframe_pending() {
                state.unsent.store(true, Relaxed);
            } else {
                state.unsent.store(false, Relaxed);
                state.last_encoded_us.store(capture_us, Relaxed);
                state.encoder.encode(&image, pts, capture_us, false);
            }
            *state.last.lock().unwrap() = LastFrame(Some(image), capture_us);
            state.schedule_idle_check();
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            let ivars = self.ivars();
            // Stopped from this Mac's screen-recording indicator: leave it stopped.
            if error.code() == SCStreamErrorCode::UserStopped.0 {
                tracing::info!("display {}: capture stopped from this Mac", ivars.display);
                return;
            }
            tracing::warn!(
                "display {}: capture stopped: {}",
                ivars.display,
                error.localizedDescription()
            );
            ivars
                .state
                .stopped
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
);

impl StreamOutput {
    fn new(
        state: Arc<FrameState>,
        audio: Option<Arc<std::sync::Mutex<Packetizer>>>,
        display: CGDirectDisplayID,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            state,
            audio,
            display,
            started: std::time::Instant::now(),
            frames: Default::default(),
            idle: Default::default(),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// Keeps the stream and its output alive; capture stops when dropped.
pub struct Capture {
    stream: Retained<SCStream>,
    /// Kept to change the frame interval later (`set_max_fps`).
    config: Retained<SCStreamConfiguration>,
    state: Arc<FrameState>,
    _output: Retained<StreamOutput>,
    queue: dispatch2::DispatchRetained<DispatchQueue>,
    _audio_queue: dispatch2::DispatchRetained<DispatchQueue>,
}

// SCStream is safe to stop from any thread; the manager serializes access.
unsafe impl Send for Capture {}

impl Capture {
    /// Delivers up to `fps` frames a second from now on (a viewer with a faster or slower
    /// screen came or went), without restarting the stream.
    pub fn set_max_fps(&self, fps: u32) {
        unsafe {
            self.config.setMinimumFrameInterval(CMTime::new(1, fps.max(1) as i32));
            self.stream.updateConfiguration_completionHandler(&self.config, None);
        }
    }

    /// Draws the pointer into the picture from now on, or leaves it out (`crate::cursor`),
    /// without restarting the stream.
    pub fn set_shows_cursor(&self, on: bool) {
        unsafe {
            self.config.setShowsCursor(on);
            self.stream.updateConfiguration_completionHandler(&self.config, None);
        }
    }
}

/// A display's refresh rate (60 when it doesn't say).
pub fn refresh_rate(display: CGDirectDisplayID) -> u32 {
    let mode = objc2_core_graphics::CGDisplayCopyDisplayMode(display);
    let hz = objc2_core_graphics::CGDisplayMode::refresh_rate(mode.as_deref());
    if hz >= 1.0 { hz.round() as u32 } else { 60 }
}

/// Whether a display shows wide colour (Display P3), so capture shouldn't clip to sRGB.
pub fn wide_colour(display: CGDirectDisplayID) -> bool {
    objc2_core_graphics::CGDisplayCopyColorSpace(display).is_wide_gamut_rgb()
}

/// The Display P3 ICC profile, for tagging lossless pictures of a wide-colour display.
pub fn p3_icc() -> Option<Vec<u8>> {
    let space = objc2_core_graphics::CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceDisplayP3 }))?;
    let data = objc2_core_graphics::CGColorSpace::icc_data(Some(&space))?;
    Some(data.to_vec())
}

impl Capture {
    /// Re-encodes the last captured frame (on the capture queue, so it stays ordered with
    /// new frames). With a keyframe pending, this delivers it even if the screen is static.
    pub fn refresh(&self) {
        let state = self.state.clone();
        // A skipped (newer) image is news; otherwise it's the picture already sent.
        self.queue.exec_async(move || {
            let fresh = state.unsent.swap(false, std::sync::atomic::Ordering::Relaxed);
            state.encode_last(!fresh)
        });
    }

    /// Bumped on every new image.
    pub fn generation(&self) -> u64 {
        self.state.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The last image as packed BGRA, if it was captured in full colour (BGRA).
    pub fn read_bgra(&self) -> Option<crate::lossless::RawFrame> {
        let (image, capture_us) = {
            let last = self.state.last.lock().unwrap();
            (last.0.clone()?, last.1)
        };
        let (width, height, bgra) = copy_bgra(&image)?;
        Some(crate::lossless::RawFrame { width, height, bgra, capture_us })
    }

    /// ScreenCaptureKit ended this stream; only a new one brings frames back.
    pub fn stopped(&self) -> bool {
        self.state
            .stopped
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A BGRA pixel buffer's pixels, rows packed (`None` for other formats).
fn copy_bgra(image: &CVImageBuffer) -> Option<(usize, usize, Vec<u8>)> {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
        CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    if CVPixelBufferGetPixelFormatType(image) != PIXEL_FORMAT_BGRA {
        return None;
    }
    let (w, h) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
    if unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return None;
    }
    let base = CVPixelBufferGetBaseAddress(image) as *const u8;
    let stride = CVPixelBufferGetBytesPerRow(image);
    let mut bgra = Vec::with_capacity(w * h * 4);
    if !base.is_null() && stride >= w * 4 {
        for y in 0..h {
            // SAFETY: locked, and each row holds at least w * 4 bytes.
            let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w * 4) };
            bgra.extend_from_slice(row);
        }
    }
    unsafe { CVPixelBufferUnlockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) };
    (bgra.len() == w * h * 4).then_some((w, h, bgra))
}

/// A small picture of one display, for previews (ScreenCaptureKit screenshots: macOS 14+).
pub fn screenshot(display_id: CGDirectDisplayID, max: [u32; 2]) -> Result<crate::lossless::RawFrame> {
    use objc2_screen_capture_kit::SCScreenshotManager;
    if objc2::runtime::AnyClass::get(c"SCScreenshotManager").is_none() {
        bail!("screenshots need macOS 14");
    }
    let display = list_displays()?
        .into_iter()
        .find(|d| d.info.id == display_id)
        .context("no such display")?;
    let (w, h) = crate::thumbs::fit(display.info.width as usize, display.info.height as usize, max);
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display.sc, &NSArray::new())
    };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(w);
        config.setHeight(h);
        config.setPixelFormat(PIXEL_FORMAT_BGRA);
        config.setShowsCursor(false);
    }
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |sample: *mut CMSampleBuffer, err: *mut NSError| {
        let result = match unsafe { sample.as_ref() } {
            Some(sample) => unsafe { sample.image_buffer() }
                .and_then(|image| copy_bgra(&image))
                .ok_or_else(|| anyhow!("no picture")),
            None => Err(match unsafe { err.as_ref() } {
                Some(e) => anyhow!("{}", e.localizedDescription()),
                None => anyhow!("no screenshot"),
            }),
        };
        let _ = tx.send(result);
    });
    unsafe {
        SCScreenshotManager::captureSampleBufferWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&handler),
        )
    };
    let (width, height, bgra) = rx.recv_timeout(std::time::Duration::from_secs(3))??;
    Ok(crate::lossless::RawFrame { width, height, bgra, capture_us: 0 })
}

impl Drop for Capture {
    fn drop(&mut self) {
        let (tx, rx) = mpsc::channel();
        let handler = RcBlock::new(move |_err: *mut NSError| {
            let _ = tx.send(());
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
    }
}

pub struct Display {
    pub sc: Retained<SCDisplay>,
    pub info: DisplayInfo,
}

unsafe impl Send for Display {}

/// A window to stream on its own (single-app streaming).
pub struct Window {
    pub sc: Retained<SCWindow>,
    pub info: WindowInfo,
}

unsafe impl Send for Window {}

impl Window {
    /// The window as a stream: its pixel size, and its frame on the desktop.
    pub fn stream_info(&self) -> DisplayInfo {
        DisplayInfo {
            id: self.info.id,
            width: self.info.width,
            height: self.info.height,
            x: self.info.x,
            y: self.info.y,
            points_width: self.info.points_width,
            points_height: self.info.points_height,
            main: false,
            builtin: false,
        }
    }
}

/// What a capture shows: a whole display, or one window.
pub enum Source<'a> {
    Display(&'a SCDisplay),
    Window(&'a SCWindow),
}

/// Windows smaller than this (points) aren't worth listing: palettes, badges, menus.
const MIN_WINDOW: f64 = 100.0;

/// Windows that can be streamed on their own: on screen, titled, normal level, belonging to
/// another app. Front to back.
pub fn list_windows() -> Result<Vec<Window>> {
    let content = shareable_content()?;
    let own = std::process::id() as i32;
    let displays: Vec<(objc2_core_foundation::CGRect, f64)> = unsafe { content.displays() }
        .iter()
        .map(|d| {
            let id = unsafe { d.displayID() };
            let b = CGDisplayBounds(id);
            (b, CGDisplayPixelsWide(id) as f64 / b.size.width.max(1.0))
        })
        .collect();
    let mut windows = Vec::new();
    for sc in unsafe { content.windows() }.iter() {
        let (frame, layer, on_screen) =
            unsafe { (sc.frame(), sc.windowLayer(), sc.isOnScreen()) };
        if layer != 0
            || !on_screen
            || frame.size.width < MIN_WINDOW
            || frame.size.height < MIN_WINDOW
        {
            continue;
        }
        let Some(app) = (unsafe { sc.owningApplication() }) else {
            continue;
        };
        if unsafe { app.processID() } == own {
            continue;
        }
        let title = unsafe { sc.title() }
            .map(|t| t.to_string())
            .unwrap_or_default();
        let app_name = unsafe { app.applicationName() }.to_string();
        if title.is_empty() || app_name.is_empty() {
            continue;
        }
        // Pixels per point of the display the window is mostly on (Retina: 2).
        let (cx, cy) = (
            frame.origin.x + frame.size.width / 2.0,
            frame.origin.y + frame.size.height / 2.0,
        );
        let scale = displays
            .iter()
            .find(|(b, _)| {
                cx >= b.origin.x
                    && cx < b.origin.x + b.size.width
                    && cy >= b.origin.y
                    && cy < b.origin.y + b.size.height
            })
            .or(displays.first())
            .map_or(1.0, |(_, s)| *s);
        let info = WindowInfo {
            id: unsafe { sc.windowID() },
            app: app_name,
            title,
            width: ((frame.size.width * scale) as usize) & !1,
            height: ((frame.size.height * scale) as usize) & !1,
            x: frame.origin.x,
            y: frame.origin.y,
            points_width: frame.size.width,
            points_height: frame.size.height,
        };
        windows.push(Window { sc, info });
    }
    Ok(windows)
}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let result = if content.is_null() {
            Err(match unsafe { err.as_ref() } {
                Some(e) => anyhow!("{}", e.localizedDescription()),
                None => anyhow!("no shareable content"),
            })
        } else {
            Ok(unsafe { Retained::retain(content).unwrap() })
        };
        let _ = tx.send(result);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    answer(&rx, "listing displays")?.context(
        "Screen Recording permission is required (System Settings → Privacy & Security)",
    )
}

/// How long ScreenCaptureKit gets to answer a request. It normally takes milliseconds; it
/// doesn't answer at all when replayd (the system's capture service) is stuck.
const SCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Waits for ScreenCaptureKit's completion handler, but not forever: when it doesn't
/// answer, restarts replayd (see `recover_capture_service`) and fails, so the caller's
/// retry finds a working service instead of blocking Glass for good.
fn answer<T>(rx: &mpsc::Receiver<T>, what: &str) -> Result<T> {
    match rx.recv_timeout(SCK_TIMEOUT) {
        Ok(v) => Ok(v),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            recover_capture_service();
            bail!("{what}: ScreenCaptureKit didn't answer in {}s", SCK_TIMEOUT.as_secs())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("{what}: ScreenCaptureKit dropped the request"),
    }
}

/// Restarts replayd, the per-user daemon behind ScreenCaptureKit, when it has stopped
/// answering (seen after a host restart: "application connection being interrupted", then
/// silence). Only this user's copy, at most every 5 minutes; launchd starts a fresh one on
/// the next request. Other apps recording the screen lose a moment of capture, which
/// beats a remote desktop that's down until someone at the Mac notices.
fn recover_capture_service() {
    use std::sync::Mutex;
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(300)) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() }.to_string();
    let killed = std::process::Command::new("/usr/bin/pkill")
        .args(["-KILL", "-x", "-U", &uid, "replayd"])
        .status()
        .is_ok_and(|s| s.success());
    if killed {
        tracing::warn!("screen capture service (replayd) wasn't answering; restarted it");
    } else {
        tracing::warn!("screen capture service (replayd) isn't answering and couldn't be restarted");
    }
}

/// All capturable displays, main display first. Sleeping displays are not listed.
pub fn list_displays() -> Result<Vec<Display>> {
    let content = shareable_content()?;
    let mut displays: Vec<Display> = unsafe { content.displays() }
        .iter()
        .map(|sc| {
            let id = unsafe { sc.displayID() };
            let bounds = CGDisplayBounds(id);
            let info = DisplayInfo {
                id,
                width: CGDisplayPixelsWide(id),
                height: CGDisplayPixelsHigh(id),
                x: bounds.origin.x,
                y: bounds.origin.y,
                points_width: bounds.size.width,
                points_height: bounds.size.height,
                main: CGDisplayIsMain(id),
                builtin: CGDisplayIsBuiltin(id),
            };
            Display { sc, info }
        })
        .collect();
    displays.sort_by_key(|d| !d.info.main);
    if displays.is_empty() {
        bail!("no displays");
    }
    Ok(displays)
}

pub fn start(
    source: Source<'_>,
    width: usize,
    height: usize,
    fps: i32,
    encoder: Arc<Encoder>,
    fps_cap: Arc<std::sync::atomic::AtomicU32>,
    audio: Option<Arc<std::sync::Mutex<Packetizer>>>,
    // Full colour: for the 4:2:2 encoder, or for lossless refreshes (sharp text).
    bgra: bool,
    // Capture in Display P3 (a wide-colour display) instead of sRGB.
    p3: bool,
    // Draw the pointer in; viewers that draw their own leave it out (`crate::cursor`).
    shows_cursor: bool,
) -> Result<Capture> {
    let captures_audio = audio.is_some();
    let (filter, source_id) = match source {
        Source::Display(display) => (
            unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    display,
                    &NSArray::new(),
                )
            },
            unsafe { display.displayID() },
        ),
        Source::Window(window) => (
            unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
            },
            unsafe { window.windowID() },
        ),
    };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width);
        config.setHeight(height);
        config.setMinimumFrameInterval(CMTime::new(1, fps));
        config.setPixelFormat(if bgra || encoder.wants_bgra() {
            PIXEL_FORMAT_BGRA
        } else {
            PIXEL_FORMAT_420V
        });
        if p3 {
            config.setColorSpaceName(objc2_core_graphics::kCGColorSpaceDisplayP3);
        }
        config.setShowsCursor(shows_cursor);
        // One buffer is held as the last frame, so leave the pool room for new ones.
        config.setQueueDepth(4);
        config.setCapturesAudio(captures_audio);
        config.setExcludesCurrentProcessAudio(true);
        config.setSampleRate(glass_proto::AUDIO_SAMPLE_RATE as isize);
        config.setChannelCount(2);
    }

    let queue = DispatchQueue::new("glass.capture", DispatchQueueAttr::SERIAL);
    let state = Arc::new(FrameState {
        encoder,
        last: std::sync::Mutex::new(LastFrame(None, 0)),
        fps_cap,
        last_encoded_us: Default::default(),
        generation: Default::default(),
        unsent: Default::default(),
        refreshes: Default::default(),
        tiny: Default::default(),
        queue: queue.clone(),
        stopped: Default::default(),
    });
    let output = StreamOutput::new(state.clone(), audio, source_id);
    // The output is also the delegate, to hear when ScreenCaptureKit ends the stream.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        )
    };
    // Separate queue so audio never waits behind a frame being encoded.
    let audio_queue = DispatchQueue::new("glass.audio", DispatchQueueAttr::SERIAL);
    unsafe {
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|e| anyhow!("addStreamOutput: {}", e.localizedDescription()))?;
        if captures_audio {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Audio,
                    Some(&audio_queue),
                )
                .map_err(|e| anyhow!("addStreamOutput(audio): {}", e.localizedDescription()))?;
        }
    }

    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.send(unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string()));
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    let started = answer(&rx, "starting capture");
    if started.is_err() {
        // Should it still start later, stop it: no stream runs without a Capture owning it.
        unsafe { stream.stopCaptureWithCompletionHandler(None) };
    }
    if let Some(err) = started? {
        bail!("startCapture: {err}");
    }

    Ok(Capture {
        stream,
        config,
        state,
        _output: output,
        queue,
        _audio_queue: audio_queue,
    })
}

#[cfg(test)]
mod tests {
    /// Needs Screen Recording permission: `cargo test -- --ignored screenshot`.
    #[test]
    #[ignore]
    fn screenshot_of_main_display() {
        let id = super::list_displays().unwrap()[0].info.id;
        let shot = super::screenshot(id, crate::thumbs::MAX).unwrap();
        assert!(shot.width <= 320 && shot.height <= 200 && shot.width > 0);
        assert_eq!(shot.bgra.len(), shot.width * shot.height * 4);
        assert!(shot.bgra.chunks_exact(4).any(|p| p[..3] != [0, 0, 0]), "all black");
    }
}
