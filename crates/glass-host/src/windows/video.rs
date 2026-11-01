//! Captures each display with DXGI Desktop Duplication (one thread per display), draws the
//! pointer in (duplication leaves it out), and encodes each on its own thread: on the GPU
//! through Media Foundation where there's a hardware encoder, else with OpenH264.
//! Coordinates are physical pixels: the process is per-monitor DPI aware.

mod mf;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use glass_proto::MAX_STREAMS;
use tokio::sync::broadcast;
use tracing::{info, warn};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::clock::now_us;
use crate::mailbox::{FrameSender, Mailbox};
use crate::media::VideoSource;
pub use crate::media::{AudioPackets, DisplayInfo, EncodedFrame, Layout};
use crate::soft_encode::{Encoder, Frame, PixelFormat};

/// How long an encoder waits for a new frame before checking for a keyframe request it
/// can satisfy by re-encoding the last one (static screens produce no new frames).
const IDLE_POLL: Duration = Duration::from_millis(100);
/// How long a capture thread waits for the desktop to change before looking again.
const ACQUIRE_MS: u32 = 100;
/// OpenH264 keeps up with 60 frames a second at most; the hardware encoder takes what
/// viewers ask for (crate::refresh).
const SOFTWARE_MAX_FPS: u32 = 60;

pub struct VideoConfig {
    pub fps: i32,
    pub bitrate: i32,
    pub scale: f64,
}

/// A rectangle on the desktop, in physical pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

struct Monitor {
    info: Mutex<DisplayInfo>,
    /// The output's GDI name (\\.\DISPLAY1), which survives re-enumeration.
    device: String,
    /// Stream number this monitor is sent as, or -1 when not shown.
    slot: AtomicI32,
    keyframe: AtomicBool,
    /// Adaptive target bitrate; the encoder thread applies changes.
    bitrate: AtomicU32,
    /// The latest picture in BGRA, kept only while sharp text wants lossless refreshes.
    latest: Mutex<Option<Arc<crate::lossless::RawFrame>>>,
    /// Bumped with each new captured picture.
    generation: AtomicU64,
    /// A small picture for display previews, refreshed while viewers want them.
    thumb: Mutex<Option<(Instant, Arc<crate::lossless::RawFrame>)>>,
}

/// Pictures (display previews, agents' screenshots) are wanted until then; each request
/// extends it. Kept at up to PICTURE_MAX and scaled down further on request.
static THUMBS_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
const THUMB_EVERY: Duration = Duration::from_millis(300);
const PICTURE_MAX: [u32; 2] = [1600, 1600];

/// Lossless refreshes wanted (sharp text): keep a BGRA copy of each new picture.
static FULL_COLOUR: AtomicBool = AtomicBool::new(false);

/// The frame rate viewers want (crate::refresh): 60, or up to 120 for fast screens.
static VIEWER_FPS: AtomicU32 = AtomicU32::new(crate::refresh::DEFAULT);

/// Monitor rects for input mapping, by display id.
static RECTS: Mutex<Vec<Rect>> = Mutex::new(Vec::new());

pub fn rect_of(id: u32) -> Option<Rect> {
    RECTS.lock().unwrap().get(id as usize).copied()
}

/// The outputs attached to the desktop, in adapter order: (GDI name, desktop rect).
fn outputs() -> Result<Vec<(String, Rect)>> {
    let mut out = Vec::new();
    for_each_output(|_, _, desc| {
        out.push((device_name(&desc), desktop_rect(&desc)));
        false
    })?;
    Ok(out)
}

/// Calls `f` for each output attached to the desktop until it returns true.
fn for_each_output(mut f: impl FnMut(&IDXGIAdapter1, &IDXGIOutput1, DXGI_OUTPUT_DESC) -> bool) -> Result<()> {
    // SAFETY: plain COM calls; every interface is reference counted by windows-rs.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        for a in 0.. {
            let adapter = match factory.EnumAdapters1(a) {
                Ok(adapter) => adapter,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e).context("EnumAdapters1"),
            };
            for o in 0.. {
                let Ok(output) = adapter.EnumOutputs(o) else { break };
                let Ok(desc) = output.GetDesc() else { continue };
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let Ok(output) = output.cast::<IDXGIOutput1>() else { continue };
                if f(&adapter, &output, desc) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn device_name(desc: &DXGI_OUTPUT_DESC) -> String {
    let len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
    String::from_utf16_lossy(&desc.DeviceName[..len])
}

fn desktop_rect(desc: &DXGI_OUTPUT_DESC) -> Rect {
    let r = desc.DesktopCoordinates;
    Rect {
        x: r.left as f64,
        y: r.top as f64,
        w: (r.right - r.left) as f64,
        h: (r.bottom - r.top) as f64,
    }
}

/// The pointer, shared by every display's capture: duplication sends a new shape only to
/// the display the pointer was on when it changed.
#[derive(Default)]
struct Pointer {
    shape: Option<Arc<PointerShape>>,
}

static POINTER: Mutex<Pointer> = Mutex::new(Pointer { shape: None });

struct PointerShape {
    kind: u32,
    width: usize,
    height: usize,
    pitch: usize,
    data: Vec<u8>,
}

const SHAPE_MONOCHROME: u32 = 1;
const SHAPE_COLOR: u32 = 2;
const SHAPE_MASKED_COLOR: u32 = 4;

/// Draws `shape` with its top-left at (`x`, `y`) into a BGRA picture.
fn draw_pointer(pixels: &mut [u8], width: usize, height: usize, stride: usize, shape: &PointerShape, x: i32, y: i32) {
    // Monochrome shapes hold an AND mask over an XOR mask, each `height / 2` rows.
    let rows = if shape.kind == SHAPE_MONOCHROME { shape.height / 2 } else { shape.height };
    for row in 0..rows {
        let py = y + row as i32;
        if py < 0 || py as usize >= height {
            continue;
        }
        for col in 0..shape.width {
            let px = x + col as i32;
            if px < 0 || px as usize >= width {
                continue;
            }
            let at = py as usize * stride + px as usize * 4;
            let Some(dst) = pixels.get_mut(at..at + 4) else { continue };
            match shape.kind {
                SHAPE_MONOCHROME => {
                    let bit = |r: usize| shape.data.get(r * shape.pitch + col / 8).is_some_and(|b| b & (0x80 >> (col % 8)) != 0);
                    let (and, xor) = (bit(row), bit(row + rows));
                    for c in &mut dst[..3] {
                        *c = (if and { *c } else { 0 }) ^ if xor { 0xFF } else { 0 };
                    }
                }
                SHAPE_COLOR | SHAPE_MASKED_COLOR => {
                    let s = row * shape.pitch + col * 4;
                    let Some(src) = shape.data.get(s..s + 4) else { continue };
                    if shape.kind == SHAPE_COLOR {
                        let a = src[3] as u32;
                        for i in 0..3 {
                            dst[i] = ((src[i] as u32 * a + dst[i] as u32 * (255 - a)) / 255) as u8;
                        }
                    } else if src[3] == 0 {
                        dst[..3].copy_from_slice(&src[..3]);
                    } else {
                        for i in 0..3 {
                            dst[i] ^= src[i];
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// One display's duplication, with a CPU-readable copy of its latest desktop image.
struct Duplicator {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    width: usize,
    height: usize,
    /// Where the pointer is on this display (its top-left), if it's here and visible.
    pointer: Option<(i32, i32)>,
    /// The staging copy holds a desktop image (not just a pointer move) yet.
    has_image: bool,
}

impl Duplicator {
    fn new(device_name: &str) -> Result<Self> {
        let mut found = None;
        for_each_output(|adapter, output, desc| {
            if self::device_name(&desc) == device_name {
                found = Some((adapter.clone(), output.clone()));
                return true;
            }
            false
        })?;
        let (adapter, output) = found.with_context(|| format!("{device_name} isn't attached"))?;
        // SAFETY: plain COM calls on interfaces we hold.
        unsafe {
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let (device, context) = (device.context("no device")?, context.context("no context")?);
            let dup = output.DuplicateOutput(&device).context("DuplicateOutput")?;
            let desc = dup.GetDesc();
            if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
                warn!("{device_name} is rotated; its picture is sent unrotated for now");
            }
            Ok(Self {
                _device: device,
                context,
                dup,
                staging: None,
                width: desc.ModeDesc.Width as usize,
                height: desc.ModeDesc.Height as usize,
                pointer: None,
                has_image: false,
            })
        }
    }

    /// Waits for the desktop or pointer to change; copies the picture into `tx` if either
    /// did. Errors (DXGI_ERROR_ACCESS_LOST on a mode change, the lock screen or a UAC
    /// prompt) mean the duplication must be made again.
    fn next(&mut self, tx: &FrameSender) -> windows::core::Result<()> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: COM calls; the acquired frame is released before returning.
        unsafe {
            match self.dup.AcquireNextFrame(ACQUIRE_MS, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
                Err(e) => return Err(e),
            }
            let captured_us = now_us();
            let result = self.take(&info, resource);
            let released = self.dup.ReleaseFrame();
            let changed = result?;
            released?;
            if changed && self.has_image {
                self.copy_out(tx, captured_us)?;
            }
        }
        Ok(())
    }

    /// Reads the acquired frame: the pointer's shape and position, and the desktop image
    /// into the staging texture. Whether anything visible changed.
    unsafe fn take(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, resource: Option<IDXGIResource>) -> windows::core::Result<bool> {
        let mut changed = false;
        if info.PointerShapeBufferSize > 0 {
            let mut data = vec![0u8; info.PointerShapeBufferSize as usize];
            let (mut needed, mut shape) = (0u32, DXGI_OUTDUPL_POINTER_SHAPE_INFO::default());
            // SAFETY: data holds PointerShapeBufferSize bytes.
            unsafe { self.dup.GetFramePointerShape(data.len() as u32, data.as_mut_ptr().cast(), &mut needed, &mut shape)? };
            POINTER.lock().unwrap().shape = Some(Arc::new(PointerShape {
                kind: shape.Type,
                width: shape.Width as usize,
                height: shape.Height as usize,
                pitch: shape.Pitch as usize,
                data,
            }));
            changed = true;
        }
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            let at = p.Visible.as_bool().then_some((p.Position.x, p.Position.y));
            changed |= at != self.pointer;
            self.pointer = at;
        }
        if info.LastPresentTime != 0
            && let Some(resource) = resource
        {
            let texture: ID3D11Texture2D = resource.cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: desc is a plain out-parameter.
            unsafe { texture.GetDesc(&mut desc) };
            if self.staging.is_none() || (desc.Width as usize, desc.Height as usize) != (self.width, self.height) {
                self.staging = Some(self.make_staging(desc.Width, desc.Height)?);
                (self.width, self.height) = (desc.Width as usize, desc.Height as usize);
            }
            // SAFETY: both textures are the same size and format.
            unsafe { self.context.CopyResource(self.staging.as_ref().unwrap(), &texture) };
            self.has_image = true;
            changed = true;
        }
        Ok(changed)
    }

    fn make_staging(&self, width: u32, height: u32) -> windows::core::Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: desc describes a valid staging texture.
        unsafe { self._device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        Ok(texture.expect("CreateTexture2D returned no texture"))
    }

    /// Copies the staging image, with the pointer drawn in, into the next mailbox frame.
    unsafe fn copy_out(&mut self, tx: &FrameSender, captured_us: u64) -> windows::core::Result<()> {
        let staging = self.staging.as_ref().expect("has_image without staging");
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture is CPU-readable; it's unmapped below.
        unsafe { self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
        let (width, height, stride) = (self.width, self.height, mapped.RowPitch as usize);
        let mut pixels = tx.0.buffer();
        pixels.clear();
        // SAFETY: a mapped staging texture is RowPitch × height readable bytes.
        pixels.extend_from_slice(unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, stride * height) });
        // SAFETY: mapped above.
        unsafe { self.context.Unmap(staging, 0) };
        let shape = POINTER.lock().unwrap().shape.clone();
        if let (Some((x, y)), Some(shape)) = (self.pointer, shape) {
            draw_pointer(&mut pixels, width, height, stride, &shape, x, y);
        }
        tx.0.put(
            Frame {
                width,
                height,
                stride,
                format: PixelFormat::Bgrx,
                data: pixels,
            },
            captured_us,
        );
        Ok(())
    }
}

/// Duplicates one display for as long as the host runs, making the duplication again
/// whenever it's lost (resolution changes, the lock screen, UAC prompts, sleep).
fn capture_loop(id: usize, device: String, tx: FrameSender) {
    let mut failing_since: Option<Instant> = None;
    loop {
        let mut dup = match Duplicator::new(&device) {
            Ok(d) => {
                info!("screen {id} ({device}): capturing {}x{}", d.width, d.height);
                failing_since = None;
                d
            }
            Err(e) => {
                // The secure desktop (lock screen, UAC) can't be duplicated by this user:
                // say so once, then keep trying quietly until it's gone.
                if failing_since.is_none() {
                    warn!("screen {id} ({device}): {e:#}; retrying");
                    failing_since = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        loop {
            if let Err(e) = dup.next(&tx) {
                info!("screen {id}: duplication ended ({e}); starting it again");
                break;
            }
        }
    }
}

pub struct Video {
    config: VideoConfig,
    frames: broadcast::Sender<Arc<EncodedFrame>>,
    monitors: Mutex<Option<Vec<Arc<Monitor>>>>,
    shown: Mutex<Vec<u32>>,
}

impl Video {
    pub fn new(
        config: VideoConfig,
        frames: broadcast::Sender<Arc<EncodedFrame>>,
        audio: AudioPackets,
    ) -> Self {
        super::dpi_aware();
        super::audio::start(audio);
        Self {
            config,
            frames,
            monitors: Mutex::new(None),
            shown: Mutex::new(Vec::new()),
        }
    }

    pub fn streams(&self) -> Vec<DisplayInfo> {
        let monitors = self.monitors.lock().unwrap();
        let Some(monitors) = monitors.as_ref() else {
            return Vec::new();
        };
        self.shown
            .lock()
            .unwrap()
            .iter()
            .filter_map(|&id| monitors.get(id as usize))
            .map(|m| m.info.lock().unwrap().clone())
            .collect()
    }

    pub fn request_keyframe(&self, stream: Option<u8>) {
        if let Some(monitors) = self.monitors.lock().unwrap().as_ref() {
            for m in monitors {
                let slot = m.slot.load(Ordering::Relaxed);
                if slot >= 0 && stream.is_none_or(|s| s as i32 == slot) {
                    m.keyframe.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn set_layout(&self, layout: Layout) -> Result<Vec<DisplayInfo>> {
        let monitors = self.ensure_started()?;
        let ids: Vec<u32> = match layout {
            Layout::Single(Some(id)) => {
                anyhow::ensure!((id as usize) < monitors.len(), "unknown display");
                vec![id]
            }
            Layout::Single(None) => vec![monitors.iter().position(|m| m.info.lock().unwrap().main).unwrap_or(0) as u32],
            Layout::Window(_) => anyhow::bail!("streaming one window isn't available on Windows yet"),
            Layout::All => {
                // Left to right, then top to bottom, like the desktop arrangement.
                let mut all: Vec<(u32, f64, f64)> = monitors
                    .iter()
                    .enumerate()
                    .take(MAX_STREAMS)
                    .map(|(i, m)| {
                        let info = m.info.lock().unwrap();
                        (i as u32, info.x, info.y)
                    })
                    .collect();
                all.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.2.total_cmp(&b.2)));
                all.into_iter().map(|(i, ..)| i).collect()
            }
        };
        for m in &monitors {
            m.slot.store(-1, Ordering::Relaxed);
        }
        for (slot, &id) in ids.iter().enumerate() {
            monitors[id as usize].slot.store(slot as i32, Ordering::Relaxed);
            monitors[id as usize].keyframe.store(true, Ordering::Relaxed);
        }
        *self.shown.lock().unwrap() = ids.clone();
        Ok(ids.iter().map(|&id| monitors[id as usize].info.lock().unwrap().clone()).collect())
    }

    pub fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(self.ensure_started()?.iter().map(|m| m.info.lock().unwrap().clone()).collect())
    }

    /// Starts capturing every display the first time it's called. Displays added later
    /// need a restart of the host for now.
    fn ensure_started(&self) -> Result<Vec<Arc<Monitor>>> {
        let mut guard = self.monitors.lock().unwrap();
        if let Some(m) = guard.as_ref() {
            return Ok(m.clone());
        }
        let outputs = outputs()?;
        anyhow::ensure!(!outputs.is_empty(), "no displays to capture (is anyone signed in at the computer?)");
        info!("{} display(s)", outputs.len());
        let mut rects = Vec::new();
        let mut monitors = Vec::new();
        for (i, (device, r)) in outputs.into_iter().enumerate() {
            rects.push(r);
            let monitor = Arc::new(Monitor {
                info: Mutex::new(DisplayInfo {
                    id: i as u32,
                    width: r.w as usize,
                    height: r.h as usize,
                    x: r.x,
                    y: r.y,
                    points_width: r.w,
                    points_height: r.h,
                    // The primary display has the desktop's origin.
                    main: r.x == 0.0 && r.y == 0.0,
                    builtin: false,
                }),
                device: device.clone(),
                slot: AtomicI32::new(-1),
                keyframe: AtomicBool::new(true),
                bitrate: AtomicU32::new(self.config.bitrate as u32),
                latest: Mutex::new(None),
                generation: AtomicU64::new(0),
                thumb: Mutex::new(None),
            });
            let rx = Arc::new(Mailbox::default());
            let tx = FrameSender(rx.clone());
            let (fps, scale) = (self.config.fps as u32, self.config.scale);
            let (frames, m) = (self.frames.clone(), monitor.clone());
            std::thread::Builder::new()
                .name(format!("glass-encode-{i}"))
                .spawn(move || encode_loop(m, rx, frames, fps, scale))?;
            std::thread::Builder::new()
                .name(format!("glass-capture-{i}"))
                .spawn(move || capture_loop(i, device, tx))
                .context("starting capture thread")?;
            monitors.push(monitor);
        }
        *RECTS.lock().unwrap() = rects;
        *guard = Some(monitors.clone());
        Ok(monitors)
    }
}

/// Encodes a monitor's newest picture whenever it's shown and a frame is due: at most the
/// encoder's frame rate, and again on a keyframe request (static screens send nothing).
fn encode_loop(
    monitor: Arc<Monitor>,
    rx: Arc<Mailbox>,
    frames: broadcast::Sender<Arc<EncodedFrame>>,
    fps: u32,
    scale: f64,
) {
    let mut encoder: Option<VideoEncoder> = None;
    let mut applied_bitrate = 0;
    let mut last: Option<(Frame, u64)> = None;
    // The latest picture hasn't been encoded yet (it came sooner than the frame rate allows).
    let mut unsent = false;
    let mut next_due = Instant::now();
    loop {
        let wait = if unsent { next_due.saturating_duration_since(Instant::now()) } else { IDLE_POLL };
        let fresh = match rx.recv_timeout(wait) {
            Ok(f) => Some(f),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let has_fresh = fresh.is_some();
        if let Some(f) = fresh
            && let Some((old, _)) = last.replace(f)
        {
            rx.recycle(old.data);
        }
        unsent |= has_fresh;
        // Previews cover every monitor, shown or not. The first one comes from the last
        // frame: a still screen sends no new ones, and screenshots would never work.
        if (has_fresh || monitor.thumb.lock().unwrap().is_none())
            && THUMBS_UNTIL.lock().unwrap().is_some_and(|t| t > Instant::now())
            && monitor.thumb.lock().unwrap().as_ref().is_none_or(|(at, _)| at.elapsed() >= THUMB_EVERY)
            && let Some((frame, _)) = last.as_ref()
            && let Some(small) = crate::thumbs::downscale(&frame.data, frame.width, frame.height, frame.stride, false, PICTURE_MAX)
        {
            *monitor.thumb.lock().unwrap() = Some((Instant::now(), Arc::new(small)));
        }
        let slot = monitor.slot.load(Ordering::Relaxed);
        if slot < 0 {
            continue;
        }
        let Some((frame, captured_us)) = last.as_ref() else {
            continue;
        };
        if has_fresh {
            monitor.generation.fetch_add(1, Ordering::Relaxed);
            // Duplication hands over BGRA as is: full colour, nothing to convert back
            // from YUV. Copied only while someone wants it.
            if FULL_COLOUR.load(Ordering::Relaxed)
                && let Some(bgra) = crate::lossless::to_bgra(&frame.data, frame.width, frame.height, frame.stride, false)
            {
                *monitor.latest.lock().unwrap() = Some(Arc::new(crate::lossless::RawFrame {
                    width: frame.width,
                    height: frame.height,
                    bgra,
                    capture_us: *captured_us,
                }));
            }
        }
        let wants_key = monitor.keyframe.load(Ordering::Relaxed);
        // Nothing new, or too soon for it: only a keyframe request is answered now.
        if !wants_key && (!unsent || Instant::now() < next_due) {
            continue;
        }
        monitor.keyframe.store(false, Ordering::Relaxed);

        let size = (frame.width, frame.height);
        let wanted_fps = fps.min(VIEWER_FPS.load(Ordering::Relaxed)).max(1);
        let mut force = wants_key;
        if encoder.as_ref().is_none_or(|e| e.source_size() != size || e.wanted_fps != wanted_fps) {
            applied_bitrate = monitor.bitrate.load(Ordering::Relaxed);
            encoder = None;
            match VideoEncoder::new(frame.width, frame.height, wanted_fps, applied_bitrate, scale) {
                Ok(e) => {
                    let id = monitor.info.lock().unwrap().id;
                    info!("screen {id}: {}, {}x{} at up to {} fps", e.describe(), size.0, size.1, e.fps());
                    encoder = Some(e);
                }
                Err(e) => {
                    warn!("encoder: {e:#}");
                    continue;
                }
            }
            force = true;
            let mut info = monitor.info.lock().unwrap();
            (info.width, info.height) = size;
        }
        let enc = encoder.as_mut().unwrap();
        let target = monitor.bitrate.load(Ordering::Relaxed);
        if target != applied_bitrate {
            enc.set_bitrate(target);
            applied_bitrate = target;
        }
        let capture_us = if unsent { *captured_us } else { now_us() };
        unsent = false;
        // A little early is on time: a 60 Hz display's frames shouldn't wait for a 60 fps
        // encoder because of jitter.
        next_due = Instant::now() + Duration::from_secs_f64(0.9 / enc.fps() as f64);
        match enc.encode(frame, force) {
            Ok(Some((data, keyframe))) => {
                let _ = frames.send(Arc::new(EncodedFrame {
                    stream: slot as u8,
                    data,
                    keyframe,
                    capture_us,
                    hevc: false,
                    refresh: false,
                }));
            }
            Ok(None) => {}
            Err(e) => {
                warn!("encode: {e:#}");
                if enc.is_hardware() {
                    // Drop it; the picture goes again through OpenH264, as a keyframe.
                    warn!("hardware encoding failed; using OpenH264 from now on");
                    HARDWARE_FAILED.store(true, Ordering::Relaxed);
                    encoder = None;
                    monitor.keyframe.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Set once the hardware encoder has failed (or there's none): the host stays on OpenH264
/// after that.
static HARDWARE_FAILED: AtomicBool = AtomicBool::new(false);

/// The hardware encoder through Media Foundation where there is one, else OpenH264.
/// `GLASS_ENCODER=software` asks for OpenH264.
struct VideoEncoder {
    kind: Kind,
    /// The frame rate asked for when it was made (viewers' rate within --fps).
    wanted_fps: u32,
}

enum Kind {
    /// With the frame rate it runs at (at most SOFTWARE_MAX_FPS).
    Soft(Encoder, u32),
    Hard(mf::MfEncoder),
}

impl VideoEncoder {
    fn new(width: usize, height: usize, fps: u32, bitrate: u32, scale: f64) -> Result<Self> {
        let software = std::env::var("GLASS_ENCODER").is_ok_and(|e| e.eq_ignore_ascii_case("software"));
        if software && !HARDWARE_FAILED.swap(true, Ordering::Relaxed) {
            info!("GLASS_ENCODER=software: encoding with OpenH264");
        }
        if !HARDWARE_FAILED.load(Ordering::Relaxed) && crate::win_capture::hardware_fits((width, height), scale) {
            if !mf::available() {
                info!("no hardware H.264 encoder; encoding with OpenH264");
                HARDWARE_FAILED.store(true, Ordering::Relaxed);
            } else {
                match mf::MfEncoder::new(width, height, fps, bitrate, scale) {
                    Ok(e) => return Ok(Self { kind: Kind::Hard(e), wanted_fps: fps }),
                    Err(e) => {
                        warn!("hardware encoder unavailable, using OpenH264: {e:#}");
                        HARDWARE_FAILED.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        let soft_fps = fps.min(SOFTWARE_MAX_FPS);
        let e = Encoder::new(width, height, soft_fps, bitrate, scale)?;
        Ok(Self { kind: Kind::Soft(e, soft_fps), wanted_fps: fps })
    }

    fn is_hardware(&self) -> bool {
        matches!(self.kind, Kind::Hard(_))
    }

    /// Which encoder, for the log.
    fn describe(&self) -> String {
        match &self.kind {
            Kind::Soft(..) => "OpenH264 (software)".into(),
            Kind::Hard(e) => format!("{} (Media Foundation, hardware)", e.name),
        }
    }

    fn fps(&self) -> u32 {
        match &self.kind {
            Kind::Soft(_, fps) => *fps,
            Kind::Hard(e) => e.fps(),
        }
    }

    fn source_size(&self) -> (usize, usize) {
        match &self.kind {
            Kind::Soft(e, _) => e.source_size(),
            Kind::Hard(e) => e.source_size(),
        }
    }

    fn set_bitrate(&mut self, bps: u32) {
        match &mut self.kind {
            Kind::Soft(e, _) => e.set_bitrate(bps),
            Kind::Hard(e) => e.set_bitrate(bps),
        }
    }

    fn encode(&mut self, frame: &Frame, force: bool) -> Result<Option<(Vec<u8>, bool)>> {
        match &mut self.kind {
            Kind::Soft(e, _) => e.encode(frame, force),
            Kind::Hard(e) => e.encode(frame, force),
        }
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
    fn supports_lossless(&self) -> bool {
        true
    }
    fn set_full_colour(&self, on: bool) {
        FULL_COLOUR.store(on, Ordering::Relaxed);
        if !on && let Some(monitors) = self.monitors.lock().unwrap().as_ref() {
            for m in monitors {
                *m.latest.lock().unwrap() = None;
            }
        }
    }
    fn picture(&self, id: u32, max: [u32; 2]) -> Option<crate::lossless::RawFrame> {
        *THUMBS_UNTIL.lock().unwrap() = Some(Instant::now() + Duration::from_secs(10));
        let frame = {
            let monitors = self.monitors.lock().unwrap();
            let m = monitors.as_ref()?.get(id as usize)?;
            let thumb = m.thumb.lock().unwrap();
            thumb.as_ref()?.1.clone()
        };
        crate::thumbs::downscale(&frame.bgra, frame.width, frame.height, frame.width * 4, false, max)
    }
    fn frame_generation(&self, stream: u8) -> Option<u64> {
        let monitors = self.monitors.lock().unwrap();
        let m = monitors.as_ref()?.iter().find(|m| m.slot.load(Ordering::Relaxed) == stream as i32)?;
        Some(m.generation.load(Ordering::Relaxed))
    }
    fn raw_frame(&self, stream: u8) -> Option<Arc<crate::lossless::RawFrame>> {
        let monitors = self.monitors.lock().unwrap();
        let m = monitors.as_ref()?.iter().find(|m| m.slot.load(Ordering::Relaxed) == stream as i32)?;
        m.latest.lock().unwrap().clone()
    }
    /// Encoders follow on their next frame (a new encoder, so a keyframe).
    fn set_max_fps(&self, fps: u32) {
        VIEWER_FPS.store(fps, Ordering::Relaxed);
    }
    fn set_bitrate(&self, bps: u32) {
        if let Some(monitors) = self.monitors.lock().unwrap().as_ref() {
            for m in monitors {
                m.bitrate.store(bps, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(w: usize, h: usize, fill: u8) -> Vec<u8> {
        vec![fill; w * h * 4]
    }

    #[test]
    fn colour_pointers_blend_by_alpha() {
        let mut pixels = picture(4, 4, 0);
        // 2×1: opaque white, then half-transparent white.
        let shape = PointerShape {
            kind: SHAPE_COLOR,
            width: 2,
            height: 1,
            pitch: 8,
            data: vec![255, 255, 255, 255, 255, 255, 255, 128],
        };
        draw_pointer(&mut pixels, 4, 4, 16, &shape, 1, 2);
        let at = |x: usize, y: usize| pixels[y * 16 + x * 4];
        assert_eq!(at(1, 2), 255);
        assert_eq!(at(2, 2), 128);
        assert_eq!(at(0, 2), 0);
        assert_eq!(at(1, 1), 0);
    }

    #[test]
    fn monochrome_pointers_and_then_xor() {
        let mut pixels = picture(2, 1, 0x40);
        // 2×1 pointer: AND row then XOR row. Pixel 0: AND 0, XOR 1 → white. Pixel 1:
        // AND 1, XOR 1 → inverted.
        let shape = PointerShape {
            kind: SHAPE_MONOCHROME,
            width: 2,
            height: 2,
            pitch: 1,
            data: vec![0b0100_0000, 0b1100_0000],
        };
        draw_pointer(&mut pixels, 2, 1, 8, &shape, 0, 0);
        assert_eq!(&pixels[0..3], &[0xFF; 3]);
        assert_eq!(&pixels[4..7], &[0xBF; 3]);
    }

    #[test]
    fn pointers_are_clipped_at_the_edges() {
        let mut pixels = picture(2, 2, 0);
        let shape = PointerShape {
            kind: SHAPE_MASKED_COLOR,
            width: 2,
            height: 2,
            pitch: 8,
            data: vec![9; 16].into_iter().enumerate().map(|(i, v)| if i % 4 == 3 { 0 } else { v }).collect(),
        };
        draw_pointer(&mut pixels, 2, 2, 8, &shape, -1, 1);
        assert_eq!(pixels[8], 9, "the visible part is drawn");
        assert_eq!(pixels[0], 0);
    }

    /// Runs on a real desktop only (CI runners may have none to duplicate).
    #[test]
    #[ignore]
    fn captures_a_display() {
        let (device, _) = outputs().unwrap().into_iter().next().expect("a display");
        let mut dup = Duplicator::new(&device).unwrap();
        let mailbox = Arc::new(Mailbox::default());
        let tx = FrameSender(mailbox.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            dup.next(&tx).unwrap();
            if let Ok((frame, _)) = mailbox.recv_timeout(Duration::ZERO) {
                assert_eq!((frame.width, frame.height), (dup.width, dup.height));
                return;
            }
        }
        panic!("no frame");
    }
}
