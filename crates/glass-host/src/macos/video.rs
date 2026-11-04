//! Owns the active capture + encoder pairs (one per streamed display) and switches
//! between showing one display and all of them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use glass_proto::MAX_STREAMS;
use objc2_core_graphics::CGDisplayIsAsleep;
use tokio::sync::broadcast;
use tracing::info;

use super::audio::{AudioPackets, Packetizer};
use super::capture::{self, Capture, Display, DisplayInfo, Source, Window};
use super::encode::{EncodedFrame, Encoder};
pub use crate::media::Layout;
use crate::media::VideoSource;

/// H.264 level 5.2 caps a frame at 36864 macroblocks (e.g. 4096x2304); HEVC (level 6.2)
/// takes 8192x4320, so a 5K display goes out at full size only in HEVC.
const MAX_MACROBLOCKS: f64 = 36864.0;
const MAX_HEVC_PIXELS: f64 = 8192.0 * 4320.0;

pub struct VideoConfig {
    pub fps: i32,
    pub bitrate: i32,
    pub scale: f64,
}

/// What to stream: one display (main when `None`) or every display.
struct Stream {
    capture: Capture,
    encoder: Arc<Encoder>,
    display: DisplayInfo,
    keyframe: Arc<AtomicBool>,
    /// Frame-rate ceiling the capture honours (from the sharpness policy).
    fps: Arc<std::sync::atomic::AtomicU32>,
    /// Recent output bytes (decaying), to share the bitrate by activity.
    activity: std::sync::atomic::AtomicU64,
    /// Encoded width (may be below native: dynamic resolution).
    width: usize,
    /// A single window rather than a display (`display` then describes the window).
    window: bool,
    /// The most frames its display can show (its refresh rate, capped by `--fps`).
    refresh: u32,
    /// Captured in Display P3 (a wide-colour display).
    p3: bool,
    /// Its display was asleep, which ends the capture: restart it once the display wakes.
    slept: AtomicBool,
}

/// What a stream captures.
enum Target<'a> {
    Display(&'a Display),
    Window(&'a Window),
}

impl Target<'_> {
    fn info(&self) -> DisplayInfo {
        match self {
            Target::Display(d) => d.info.clone(),
            Target::Window(w) => w.stream_info(),
        }
    }

    fn source(&self) -> Source<'_> {
        match self {
            Target::Display(d) => Source::Display(&d.sc),
            Target::Window(w) => Source::Window(&w.sc),
        }
    }
}

pub struct Video {
    config: VideoConfig,
    frames: broadcast::Sender<Arc<EncodedFrame>>,
    /// Shared across layout changes so audio sequence numbers stay continuous.
    audio: Arc<Mutex<Packetizer>>,
    streams: Mutex<Vec<Stream>>,
    /// Current adaptive target; new streams start at it.
    bitrate: std::sync::atomic::AtomicU32,
    /// Per stream: the width viewers need (`None` = native), from dynamic resolution.
    target_widths: Mutex<Vec<Option<u32>>>,
    /// Encode in HEVC rather than H.264 (crate::codec decides).
    hevc: AtomicBool,
    /// HEVC 4:2:2 for sharp text (crate::sharp decides; only while HEVC is on).
    rext: AtomicBool,
    /// Capture in BGRA for lossless refreshes (sharp text).
    full_colour: AtomicBool,
    /// The frame rate viewers want (crate::refresh): 60, or up to 120 for fast screens.
    max_fps: std::sync::atomic::AtomicU32,
    /// Draw the pointer into the picture (crate::cursor decides).
    cursor: AtomicBool,
    /// Displays an agent is waiting on that no stream shows, watched for changes alone.
    watches: Mutex<std::collections::HashMap<u32, capture::Watch>>,
}

impl Video {
    pub fn new(
        config: VideoConfig,
        frames: broadcast::Sender<Arc<EncodedFrame>>,
        audio: AudioPackets,
    ) -> Self {
        let audio = Arc::new(Mutex::new(Packetizer::new(audio)));
        let bitrate = std::sync::atomic::AtomicU32::new(config.bitrate as u32);
        Self {
            config,
            frames,
            audio,
            streams: Mutex::new(Vec::new()),
            bitrate,
            target_widths: Mutex::new(Vec::new()),
            hevc: AtomicBool::new(false),
            rext: AtomicBool::new(false),
            full_colour: AtomicBool::new(false),
            max_fps: std::sync::atomic::AtomicU32::new(crate::refresh::DEFAULT),
            cursor: AtomicBool::new(true),
            watches: Mutex::new(Default::default()),
        }
    }

    /// Frames per second for a stream now: what its display and its viewers can show.
    fn fps_for(&self, refresh: u32) -> u32 {
        refresh.min(self.max_fps.load(Ordering::Relaxed)).max(1)
    }

    /// Viewers' frame rate changed: each capture delivers up to the new rate and each
    /// encoder's policy follows.
    pub fn set_max_fps(&self, fps: u32) {
        if self.max_fps.swap(fps, Ordering::Relaxed) == fps {
            return;
        }
        for (i, s) in self.streams.lock().unwrap().iter().enumerate() {
            let now = self.fps_for(s.refresh);
            s.capture.set_max_fps(now);
            info!("stream {i}: up to {now} fps");
        }
        self.rebalance();
    }

    /// The displays being streamed; index = stream number.
    pub fn streams(&self) -> Vec<DisplayInfo> {
        self.streams
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.display.clone())
            .collect()
    }

    /// Requests a keyframe on one stream, or all of them.
    pub fn request_keyframe(&self, stream: Option<u8>) {
        for (i, s) in self.streams.lock().unwrap().iter().enumerate() {
            if stream.is_none_or(|n| n as usize == i) {
                s.keyframe.store(true, Ordering::Relaxed);
                s.capture.refresh();
            }
        }
    }

    /// Restarts capture with `layout`. Blocks on ScreenCaptureKit.
    pub fn set_layout(&self, layout: Layout) -> Result<Vec<DisplayInfo>> {
        let mut streams = self.streams.lock().unwrap();
        if let Layout::Window(id) = layout {
            let windows = capture::list_windows()?;
            let window = windows
                .iter()
                .find(|w| w.info.id == id)
                .context("that window is gone")?;
            streams.clear();
            streams.push(self.start(0, &Target::Window(window), Some(self.audio.clone()))?);
            return Ok(streams.iter().map(|s| s.display.clone()).collect());
        }
        let displays = capture::list_displays()?;
        let chosen: Vec<&Display> = match layout {
            Layout::Single(Some(id)) => {
                vec![
                    displays
                        .iter()
                        .find(|d| d.info.id == id)
                        .context("unknown display")?,
                ]
            }
            Layout::Single(None) => vec![&displays[0]],
            Layout::Window(_) => unreachable!("handled above"),
            Layout::All => {
                // Match System Settings › Displays › Arrange: left to right, then top to bottom.
                let mut all: Vec<&Display> = displays.iter().take(MAX_STREAMS).collect();
                all.sort_by(|a, b| {
                    a.info
                        .x
                        .total_cmp(&b.info.x)
                        .then(a.info.y.total_cmp(&b.info.y))
                });
                all
            }
        };
        // Stop the old streams first so a stream number never has two encoders feeding it.
        streams.clear();

        for (index, display) in chosen.into_iter().enumerate() {
            // Only the first stream carries audio, so it isn't heard once per display.
            let audio = (index == 0).then(|| self.audio.clone());
            streams.push(self.start(index as u8, &Target::Display(display), audio)?);
        }
        Ok(streams.iter().map(|s| s.display.clone()).collect())
    }

    fn start(
        &self,
        index: u8,
        target: &Target,
        audio: Option<Arc<Mutex<Packetizer>>>,
    ) -> Result<Stream> {
        let info = target.info();
        let (w, h) = (info.width as f64, info.height as f64);
        let hevc = self.hevc.load(Ordering::Relaxed);
        let rext = hevc && self.rext.load(Ordering::Relaxed);
        let max_pixels = if hevc {
            MAX_HEVC_PIXELS
        } else {
            MAX_MACROBLOCKS * 256.0
        };
        let wanted = self
            .target_widths
            .lock()
            .unwrap()
            .get(index as usize)
            .copied()
            .flatten();
        let scale = self
            .config
            .scale
            .min((max_pixels / (w * h)).sqrt())
            .min(wanted.map_or(1.0, |t| t as f64 / w));
        let width = ((w * scale) as usize) & !1;
        let height = ((h * scale) as usize) & !1;

        // The display's own refresh (120 Hz ProMotion, a 144 Hz monitor…), within --fps; a
        // window follows the main display.
        let display_id = match target {
            Target::Display(d) => d.info.id,
            Target::Window(_) => objc2_core_graphics::CGMainDisplayID(),
        };
        let refresh = capture::refresh_rate(display_id).min(self.config.fps.max(1) as u32);
        let fps = self.fps_for(refresh);
        let p3 = capture::wide_colour(display_id);

        let keyframe = Arc::new(AtomicBool::new(true));
        let frames = self.frames.clone();
        let encoder = Encoder::new(
            width as i32,
            height as i32,
            fps as i32,
            self.config.bitrate,
            hevc,
            rext,
            p3,
            keyframe.clone(),
            Arc::new(move |frame| {
                let _ = frames.send(Arc::new(EncodedFrame {
                    stream: index,
                    ..frame
                }));
            }),
        )?;
        let encoder = Arc::new(encoder);
        let fps_cap = Arc::new(std::sync::atomic::AtomicU32::new(
            encoder.apply_policy(self.bitrate.load(Ordering::Relaxed), fps),
        ));
        let capture = capture::start(
            target.source(),
            index,
            width,
            height,
            fps as i32,
            encoder.clone(),
            fps_cap.clone(),
            audio,
            self.full_colour.load(Ordering::Relaxed),
            p3,
            self.cursor.load(Ordering::Relaxed),
        )?;
        let id = info.id;
        let window = matches!(target, Target::Window(_));
        info!(
            "stream {index}: {} {id} at {width}x{height} @ {fps}fps (display {refresh} Hz{}, {})",
            if window { "window" } else { "display" },
            if p3 { ", Display P3" } else { "" },
            if rext {
                "HEVC 4:2:2"
            } else if hevc {
                "HEVC"
            } else {
                "H.264"
            }
        );
        Ok(Stream {
            capture,
            encoder,
            display: info,
            keyframe,
            fps: fps_cap,
            activity: Default::default(),
            width,
            window,
            refresh,
            p3,
            slept: Default::default(),
        })
    }

    /// Restarts every stream with the current codec settings (each starts on a keyframe).
    fn restart_all(&self, why: &str) {
        let Ok(displays) = capture::list_displays() else {
            return;
        };
        let mut streams = self.streams.lock().unwrap();
        let windows = if streams.iter().any(|s| s.window) {
            capture::list_windows().unwrap_or_default()
        } else {
            Vec::new()
        };
        for index in 0..streams.len() {
            let id = streams[index].display.id;
            let target = if streams[index].window {
                match windows.iter().find(|w| w.info.id == id) {
                    Some(w) => Target::Window(w),
                    None => continue,
                }
            } else {
                match displays.iter().find(|d| d.info.id == id) {
                    Some(d) => Target::Display(d),
                    None => continue,
                }
            };
            let audio = (index == 0).then(|| self.audio.clone());
            // Stop the old one first so the stream never has two encoders feeding it.
            let old = std::mem::replace(
                &mut streams[index],
                match self.start(index as u8, &target, audio) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("restarting stream {index} ({why}): {e:#}");
                        continue;
                    }
                },
            );
            drop(old);
        }
        drop(streams);
        self.rebalance();
    }

    pub fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(capture::list_displays()?
            .into_iter()
            .map(|d| d.info)
            .collect())
    }

    pub fn windows(&self) -> Result<Vec<crate::media::WindowInfo>> {
        Ok(capture::list_windows()?
            .into_iter()
            .map(|w| w.info)
            .collect())
    }
}

impl VideoSource for Video {
    fn streams(&self) -> Vec<DisplayInfo> {
        Video::streams(self)
    }
    fn request_keyframe(&self, stream: Option<u8>) {
        Video::request_keyframe(self, stream)
    }
    fn set_layout(&self, layout: Layout) -> Result<Vec<DisplayInfo>> {
        Video::set_layout(self, layout)
    }
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Video::displays(self)
    }
    fn supports_hevc(&self) -> bool {
        super::encode::supports_hevc()
    }
    fn supports_rext(&self) -> bool {
        super::encode::supports_rext()
    }
    fn supports_lossless(&self) -> bool {
        true
    }
    /// BGRA capture, so a still screen can go out losslessly (VideoToolbox converts).
    fn set_full_colour(&self, on: bool) {
        if self.full_colour.swap(on, Ordering::Relaxed) != on {
            self.restart_all("full colour");
            info!("capture colour: {}", if on { "BGRA (lossless refresh)" } else { "4:2:0" });
        }
    }
    fn set_cursor_baked(&self, on: bool) {
        if self.cursor.swap(on, Ordering::Relaxed) != on {
            for s in self.streams.lock().unwrap().iter() {
                s.capture.set_shows_cursor(on);
            }
        }
    }
    /// A streamed display reports its changes already; another is watched while agents
    /// want it (`heal` stops it after).
    fn track_damage(&self, id: u32) -> bool {
        if self.streams.lock().unwrap().iter().any(|s| !s.window && s.display.id == id) {
            self.watches.lock().unwrap().remove(&id);
            return true;
        }
        let mut watches = self.watches.lock().unwrap();
        if watches.get(&id).is_some_and(|w| !w.stopped()) {
            return true;
        }
        watches.remove(&id);
        let watched = capture::list_displays()
            .and_then(|all| all.into_iter().find(|d| d.info.id == id).context("no such display"))
            .and_then(|d| capture::watch(&d));
        match watched {
            Ok(w) => {
                watches.insert(id, w);
                true
            }
            Err(e) => {
                tracing::debug!("display {id}: can't watch it for changes: {e:#}");
                false
            }
        }
    }
    fn picture(&self, id: u32, max: [u32; 2]) -> Option<crate::lossless::RawFrame> {
        capture::screenshot(id, max)
            .inspect_err(|e| tracing::debug!("preview of display {id}: {e:#}"))
            .ok()
    }
    fn frame_generation(&self, stream: u8) -> Option<u64> {
        let streams = self.streams.lock().unwrap();
        streams.get(stream as usize).map(|s| s.capture.generation())
    }
    fn set_max_fps(&self, fps: u32) {
        Video::set_max_fps(self, fps)
    }
    fn icc(&self, stream: u8) -> Option<Arc<Vec<u8>>> {
        static P3: std::sync::OnceLock<Option<Arc<Vec<u8>>>> = std::sync::OnceLock::new();
        let p3 = self.streams.lock().unwrap().get(stream as usize).is_some_and(|s| s.p3);
        p3.then(|| P3.get_or_init(|| capture::p3_icc().map(Arc::new)).clone()).flatten()
    }
    fn raw_frame(&self, stream: u8) -> Option<Arc<crate::lossless::RawFrame>> {
        let streams = self.streams.lock().unwrap();
        streams.get(stream as usize)?.capture.read_bgra().map(Arc::new)
    }
    /// Sharp text: HEVC 4:2:2 instead of 4:2:0 (applies while HEVC is on).
    fn set_rext(&self, on: bool) -> bool {
        let on = on && super::encode::supports_rext();
        if self.rext.swap(on, Ordering::Relaxed) != on && self.hevc.load(Ordering::Relaxed) {
            self.restart_all("chroma");
            info!("video chroma: {}", if on { "4:2:2" } else { "4:2:0" });
        }
        on && self.hevc.load(Ordering::Relaxed)
    }
    /// Restarts every stream in the new codec (each starts on a keyframe).
    fn set_hevc(&self, hevc: bool) -> bool {
        let hevc = hevc && super::encode::supports_hevc();
        if self.hevc.swap(hevc, Ordering::Relaxed) == hevc {
            return hevc;
        }
        self.restart_all("codec");
        info!("video codec: {}", if hevc { "HEVC" } else { "H.264" });
        hevc
    }
    fn supports_windows(&self) -> bool {
        true
    }
    fn windows(&self) -> Result<Vec<crate::media::WindowInfo>> {
        Video::windows(self)
    }
    /// `bps` is the total for all displays (not each): shared out by `rebalance`.
    fn set_bitrate(&self, bps: u32) {
        self.bitrate.store(bps, Ordering::Relaxed);
        self.rebalance();
    }

    /// Restarts any stream whose encoded size should change (dynamic resolution).
    fn set_target_widths(&self, widths: &[Option<u32>]) {
        *self.target_widths.lock().unwrap() = widths.to_vec();
        let Ok(displays) = capture::list_displays() else {
            return;
        };
        let mut streams = self.streams.lock().unwrap();
        let windows = if streams.iter().any(|s| s.window) {
            capture::list_windows().unwrap_or_default()
        } else {
            Vec::new()
        };
        for (index, want) in widths.iter().enumerate() {
            let Some(current) = streams.get(index) else {
                continue;
            };
            let native = current.display.width;
            let want = want.map_or(native, |w| (w as usize).min(native));
            // Within a few pixels is the same size (rounding to even, macroblock caps).
            if current.width.abs_diff(want) <= 16 {
                continue;
            }
            let id = current.display.id;
            let target = if current.window {
                match windows.iter().find(|w| w.info.id == id) {
                    Some(w) => Target::Window(w),
                    None => continue,
                }
            } else {
                match displays.iter().find(|d| d.info.id == id) {
                    Some(d) => Target::Display(d),
                    None => continue,
                }
            };
            let audio = (index == 0).then(|| self.audio.clone());
            // Stop the old one first so the stream never has two encoders feeding it.
            let old = std::mem::replace(
                &mut streams[index],
                match self.start(index as u8, &target, audio) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("resizing stream {index}: {e:#}");
                        continue;
                    }
                },
            );
            drop(old);
            info!(
                "stream {index}: now {}px wide for its viewers",
                streams[index].width
            );
        }
        drop(streams);
        self.rebalance();
    }

    /// Restarts streams that ended. ScreenCaptureKit stops a stream when its display sleeps
    /// (closing the lid, even for a second) and never resumes it, which froze the picture
    /// while input still worked.
    fn heal(&self) {
        // Watches no agent has wanted for a while end.
        self.watches.lock().unwrap().retain(|id, _| crate::damage::wanted(*id));
        let mut streams = self.streams.lock().unwrap();
        let mut displays = None;
        for index in 0..streams.len() {
            let s = &streams[index];
            // Only displays sleep; a window stream has no display to wait for.
            if s.window {
                continue;
            }
            let id = s.display.id;
            if CGDisplayIsAsleep(id) {
                s.slept.store(true, Ordering::Relaxed);
                continue;
            }
            if !s.slept.swap(false, Ordering::Relaxed) && !s.capture.stopped() {
                continue;
            }
            let list = displays.get_or_insert_with(capture::list_displays);
            let restarted = match list {
                Ok(list) => match list.iter().find(|d| d.info.id == id) {
                    Some(display) => {
                        let audio = (index == 0).then(|| self.audio.clone());
                        self.start(index as u8, &Target::Display(display), audio)
                    }
                    None => Err(anyhow::anyhow!("display {id} isn't back yet")),
                },
                Err(e) => Err(anyhow::anyhow!("listing displays: {e:#}")),
            };
            match restarted {
                Ok(stream) => {
                    // Same display and stream number, so viewers just get a new keyframe.
                    drop(std::mem::replace(&mut streams[index], stream));
                    info!("stream {index}: display {id} capture restarted");
                }
                Err(e) => {
                    // Try again on the next check.
                    streams[index].slept.store(true, Ordering::Relaxed);
                    tracing::debug!("stream {index}: restart: {e:#}");
                }
            }
        }
    }

    /// Splits the total between displays by recent activity, and applies the sharpness
    /// policy (bitrate, frame-rate ceiling, quality cap) to each.
    fn rebalance(&self) {
        let streams = self.streams.lock().unwrap();
        let activity: Vec<u64> = streams
            .iter()
            .map(|s| {
                let recent = s.encoder.take_bytes() + s.activity.load(Ordering::Relaxed) / 2;
                s.activity.store(recent, Ordering::Relaxed);
                recent
            })
            .collect();
        let shares = crate::rate::split(self.bitrate.load(Ordering::Relaxed), &activity);
        for (s, bps) in streams.iter().zip(shares) {
            let fps = s.encoder.apply_policy(bps, self.fps_for(s.refresh));
            s.fps.store(fps, Ordering::Relaxed);
        }
    }
}
