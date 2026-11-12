//! The pointer, drawn by each viewer's browser instead of into the video. Baked into the
//! picture it trails the viewer's own mouse by the whole stream latency (130 ms or more on
//! LTE), and every move costs an encoded frame; drawn by the browser as a CSS cursor it
//! moves at once.
//!
//! - The platform looks at the pointer about 30 times a second: where it is, whether it's
//!   showing, and its image when that changes.
//! - The image goes on the control stream as `ServerMsg::Cursor` (up to 256×256 RGBA, so
//!   it needs a reliable channel): WebTransport's control stream, or for a viewer over
//!   WebRTC the same stream carried by the bridge's reliable "bi" data channel (remote.rs).
//! - Where it is goes as a small `KIND_CURSOR` datagram on change, and once a second
//!   regardless, so a lost one is soon made good. Over WebRTC it rides the "d" channel like
//!   pings and audio.
//!
//! Each display stream is one encode shared by every viewer, so the pointer is in the
//! picture for everyone or for no one. It stays in while anything needs it: a viewer that
//! doesn't draw its own (an older page, trackpad mode, which has its own), a recording, and
//! on Windows an agent (its screenshots are taken from the stream). Viewers that draw their
//! own are told (`CURSOR_BAKED` in every datagram) and stand down meanwhile. A second
//! encode just for the viewers that could draw their own would double the encoder's work
//! for the time someone else happens to watch; a pointer that lags as it did before, for
//! that while, is the better trade.
//!
//! The pointer moving by itself (another viewer, an agent, the person at the computer)
//! still shows: each datagram says whether this viewer moved it last (`CURSOR_YOURS`); when
//! it didn't, the viewer draws the pointer where the host says it is.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use glass_proto::{CURSOR_BAKED, CURSOR_OFF_STREAM, CURSOR_VISIBLE, CURSOR_YOURS, CursorPos};
use tokio::sync::watch;
use tracing::info;

use crate::control::ServerMsg;
use crate::media::DisplayInfo;
use crate::server::Shared;

/// The platform can show viewers the pointer apart from the picture. Linux under Wayland
/// keeps it in the picture: the portal fixes the cursor mode when screen sharing is
/// approved, so it can't follow who's watching without asking again (and its metadata mode
/// isn't wired up yet). X11 sessions can (see `active`).
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", windows));

const POLL: Duration = Duration::from_millis(33);
/// Where it is goes out at least this often, so a lost datagram is soon made good.
const HEARTBEAT: Duration = Duration::from_secs(1);
/// The pointer moving this soon after someone's input moved it is their doing.
const DRIVEN: Duration = Duration::from_millis(250);
/// Agents' screenshots on Windows come from the stream: it keeps the pointer this long
/// after an agent last looked or acted.
const AGENT_BAKES: bool = cfg!(windows);
const AGENT_HOLD: Duration = Duration::from_secs(60);
/// Bigger images are skipped (the viewer keeps the last one).
pub const MAX_SIDE: usize = 256;

/// A pointer image: straight (not premultiplied) RGBA, `scale` pixels to a point, the
/// hotspot in pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub hot_x: usize,
    pub hot_y: usize,
    pub scale: f32,
    pub rgba: Vec<u8>,
}

/// One look at the pointer.
pub struct Sample {
    /// In global points (the coordinates of `DisplayInfo::x`, `y`).
    pub at: (f64, f64),
    pub visible: bool,
    /// Its image, when it changed since the last look.
    pub image: Option<Image>,
}

pub trait Poll: Send {
    fn poll(&mut self, shared: &Shared) -> Option<Sample>;
}

/// Who moved the pointer: the person at the computer (or anything else not through Glass),
/// an agent, or a viewer's session (`new_driver`).
pub const LOCAL: u64 = 0;
pub const AGENT: u64 = 1;
static NEXT_DRIVER: AtomicU64 = AtomicU64::new(2);

pub fn new_driver() -> u64 {
    NEXT_DRIVER.fetch_add(1, Ordering::Relaxed)
}

static MOVER: AtomicU64 = AtomicU64::new(LOCAL);
static MOVED_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    // Never 0, which reads as "never".
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// `driver` just sent the pointer somewhere.
pub fn moved_by(driver: u64) {
    MOVER.store(driver, Ordering::Relaxed);
    MOVED_MS.store(now_ms(), Ordering::Relaxed);
}

/// Where the pointer is now, for each session to send on (`Pos::datagram`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pos {
    pub stream: u8,
    pub x: f32,
    pub y: f32,
    pub visible: bool,
    pub baked: bool,
    pub shape: u32,
    pub driver: u64,
}

impl Pos {
    /// The datagram for the session that moves the pointer as `driver`.
    pub fn datagram(&self, driver: u64) -> [u8; glass_proto::CURSOR_LEN] {
        let flags = if self.visible { CURSOR_VISIBLE } else { 0 }
            | if self.baked { CURSOR_BAKED } else { 0 }
            | if self.driver == driver { CURSOR_YOURS } else { 0 };
        CursorPos { stream: self.stream, flags, shape: self.shape, x: self.x, y: self.y }.write()
    }
}

fn positions() -> &'static watch::Sender<Pos> {
    static TX: OnceLock<watch::Sender<Pos>> = OnceLock::new();
    TX.get_or_init(|| watch::channel(Pos { baked: true, stream: CURSOR_OFF_STREAM, ..Default::default() }).0)
}

pub fn subscribe() -> watch::Receiver<Pos> {
    positions().subscribe()
}

/// The image now, for a viewer that starts drawing the pointer.
static SHAPE: Mutex<Option<ServerMsg>> = Mutex::new(None);

pub fn current_shape() -> Option<ServerMsg> {
    SHAPE.lock().unwrap().clone()
}

fn shape_msg(id: u32, image: &Image) -> ServerMsg {
    ServerMsg::Cursor {
        id,
        w: image.width as u32,
        h: image.height as u32,
        hx: image.hot_x as u32,
        hy: image.hot_y as u32,
        scale: image.scale,
        rgba: base64::engine::general_purpose::STANDARD.encode(&image.rgba),
    }
}

// ---- Who needs the pointer in the picture ----

static BAKES: AtomicUsize = AtomicUsize::new(0);
/// Sessions drawing the pointer themselves: only while there are any is it looked at.
static LISTENERS: AtomicUsize = AtomicUsize::new(0);
static AGENT_MS: AtomicU64 = AtomicU64::new(0);
/// What the capture was last told: `None` before the first time.
static APPLIED: Mutex<Option<bool>> = Mutex::new(None);

/// Viewers can draw the pointer on this host (the platform, or the test pattern).
pub fn active() -> bool {
    SUPPORTED || x11() || crate::testmode::enabled()
}

/// An X11 session on Linux: no portal in the way, so the capture can leave the pointer out
/// whenever viewers draw it (`crate::platform::x11_cursor` looks at it).
fn x11() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::platform::is_x11()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The pointer is in the picture now (as far as viewers should assume).
pub fn baked() -> bool {
    APPLIED.lock().unwrap().unwrap_or(true)
}

fn wanted() -> bool {
    let agent = AGENT_MS.load(Ordering::Relaxed);
    let agent_recent = agent != 0 && now_ms().saturating_sub(agent) < AGENT_HOLD.as_millis() as u64;
    !active() || BAKES.load(Ordering::Relaxed) > 0 || (AGENT_BAKES && agent_recent)
}

/// Tells the capture to put the pointer in, or leave it out, when that changed.
fn apply() {
    let want = wanted();
    let mut applied = APPLIED.lock().unwrap();
    if *applied == Some(want) {
        return;
    }
    let Some(shared) = crate::server::shared() else { return };
    shared.video.set_cursor_baked(want);
    *applied = Some(want);
    drop(applied);
    info!("pointer: {}", if want { "in the video" } else { "drawn by viewers" });
    // Viewers learn at once rather than on the next heartbeat.
    positions().send_modify(|p| p.baked = want);
}

/// While held, the pointer stays in the picture.
pub struct Bake(());

pub fn bake() -> Bake {
    BAKES.fetch_add(1, Ordering::Relaxed);
    apply();
    Bake(())
}

impl Drop for Bake {
    fn drop(&mut self) {
        BAKES.fetch_sub(1, Ordering::Relaxed);
        apply();
    }
}

/// An agent looked or acted (Windows keeps the pointer in its screenshots).
pub fn agent_acted() {
    if AGENT_BAKES {
        AGENT_MS.store(now_ms(), Ordering::Relaxed);
        apply();
    }
}

/// One session's part: it draws the pointer itself, or needs it in the picture.
pub struct Viewer {
    on: Arc<AtomicBool>,
    bake: Option<Bake>,
}

impl Viewer {
    /// `on`: the viewer said it draws the pointer. `flag` mirrors it for the session's
    /// datagrams and control messages.
    pub fn new(on: bool, flag: Arc<AtomicBool>) -> Self {
        let on = on && active();
        flag.store(on, Ordering::Relaxed);
        if on {
            LISTENERS.fetch_add(1, Ordering::Relaxed);
        }
        Self { on: flag, bake: (!on).then(bake) }
    }

    pub fn set(&mut self, on: bool) {
        let on = on && active();
        self.on.store(on, Ordering::Relaxed);
        // Holding a bake exactly while not drawing its own.
        match (on, self.bake.is_some()) {
            (true, true) => {
                LISTENERS.fetch_add(1, Ordering::Relaxed);
                self.bake = None;
            }
            (false, false) => {
                LISTENERS.fetch_sub(1, Ordering::Relaxed);
                self.bake = Some(bake());
            }
            _ => {}
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        if self.bake.is_none() {
            LISTENERS.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

// ---- Looking at the pointer ----

pub fn start(shared: Arc<Shared>) {
    let poller: Box<dyn Poll> = if crate::testmode::enabled() {
        Box::new(crate::testmode::CursorPoller::default())
    } else {
        #[cfg(any(target_os = "macos", windows))]
        {
            Box::new(crate::platform::cursor::Poller::default())
        }
        #[cfg(target_os = "linux")]
        {
            if !x11() {
                // Nothing to look at: the pointer stays in the picture.
                apply();
                return;
            }
            Box::new(crate::platform::x11_cursor::Poller::default())
        }
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        {
            apply();
            return;
        }
    };
    let _ = std::thread::Builder::new()
        .name("glass-cursor".into())
        .spawn(move || run(shared, poller));
}

fn run(shared: Arc<Shared>, mut poller: Box<dyn Poll>) {
    let tx = positions();
    let mut shape = 0u32;
    let mut last_at = None;
    let mut driver = LOCAL;
    let mut sent = Instant::now();
    loop {
        std::thread::sleep(POLL);
        // Agents' hold runs out here.
        apply();
        if LISTENERS.load(Ordering::Relaxed) == 0 {
            continue;
        }
        let Some(sample) = poller.poll(&shared) else { continue };
        if let Some(image) = &sample.image {
            shape = shape.wrapping_add(1).max(1);
            let msg = shape_msg(shape, image);
            *SHAPE.lock().unwrap() = Some(msg.clone());
            let _ = shared.events.send(msg);
        }
        if last_at != Some(sample.at) {
            last_at = Some(sample.at);
            let recent = now_ms().saturating_sub(MOVED_MS.load(Ordering::Relaxed)) < DRIVEN.as_millis() as u64;
            driver = if recent { MOVER.load(Ordering::Relaxed) } else { LOCAL };
        }
        let (stream, x, y) = locate(&shared.video.streams(), sample.at);
        let pos = Pos { stream, x, y, visible: sample.visible, baked: baked(), shape, driver };
        if pos != *tx.borrow() || sent.elapsed() >= HEARTBEAT {
            tx.send_replace(pos);
            sent = Instant::now();
        }
    }
}

/// The stream a global point is on, and where on it (0..1).
pub fn locate(streams: &[DisplayInfo], (x, y): (f64, f64)) -> (u8, f32, f32) {
    streams
        .iter()
        .enumerate()
        .find(|(_, d)| x >= d.x && y >= d.y && x < d.x + d.points_width && y < d.y + d.points_height)
        .map(|(i, d)| {
            (i as u8, ((x - d.x) / d.points_width.max(1.0)) as f32, ((y - d.y) / d.points_height.max(1.0)) as f32)
        })
        .unwrap_or((CURSOR_OFF_STREAM, 0.0, 0.0))
}

// ---- Images ----

/// Premultiplied RGBA (what Core Graphics draws) to straight.
pub fn unpremultiply(rgba: &mut [u8]) {
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

// Adapted from voxl (MIT, github.com/tembo/voxl): its Windows cursor_image.

/// A colour cursor (BGRA rows) as straight RGBA: premultiplied alpha, or (old style, all
/// alpha 0) opaque where the AND mask (`and`, true = transparent) is clear.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn colour_to_rgba(width: usize, height: usize, bgra: &[u8], and: Option<&[bool]>) -> Vec<u8> {
    let n = width * height;
    let has_alpha = bgra.as_chunks::<4>().0.iter().take(n).any(|p| p[3] != 0);
    let mut rgba = vec![0u8; n * 4];
    for i in 0..n {
        let Some(p) = bgra.get(i * 4..i * 4 + 4) else { break };
        let a = if has_alpha {
            p[3]
        } else {
            match and {
                Some(m) if m.get(i).copied().unwrap_or(true) => 0,
                _ => 255,
            }
        };
        let un = |c: u8| {
            if a == 0 {
                0
            } else if has_alpha {
                ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
            } else {
                c
            }
        };
        rgba[i * 4..i * 4 + 4].copy_from_slice(&[un(p[2]), un(p[1]), un(p[0]), a]);
    }
    rgba
}

/// A monochrome cursor (AND and XOR masks, true = set) as RGBA. Pixels that invert the
/// screen (the text cursor) become black with a white outline, to show on any background.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn mono_to_rgba(width: usize, height: usize, and: &[bool], xor: &[bool]) -> Vec<u8> {
    let (w, h) = (width, height);
    let at = |m: &[bool], x: usize, y: usize| m.get(y * w + x).copied().unwrap_or(false);
    let mut rgba = vec![0u8; w * h * 4];
    let mut invert = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let px = &mut rgba[(y * w + x) * 4..][..4];
            match (at(and, x, y), at(xor, x, y)) {
                (false, false) => px.copy_from_slice(&[0, 0, 0, 255]),
                (false, true) => px.copy_from_slice(&[255, 255, 255, 255]),
                (true, true) => {
                    px.copy_from_slice(&[0, 0, 0, 255]);
                    invert[y * w + x] = true;
                }
                (true, false) => {}
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            if rgba[(y * w + x) * 4 + 3] != 0 {
                continue;
            }
            let near = (y.saturating_sub(1)..(y + 2).min(h))
                .any(|ny| (x.saturating_sub(1)..(x + 2).min(w)).any(|nx| invert[ny * w + nx]));
            if near {
                rgba[(y * w + x) * 4..][..4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(x: f64, w: f64) -> DisplayInfo {
        DisplayInfo {
            id: 0,
            width: (w * 2.0) as usize,
            height: 800,
            x,
            y: 0.0,
            points_width: w,
            points_height: 400.0,
            main: false,
            builtin: false,
        }
    }

    #[test]
    fn locates_the_stream_a_point_is_on() {
        let streams = [display(0.0, 1000.0), display(1000.0, 500.0)];
        assert_eq!(locate(&streams, (250.0, 100.0)), (0, 0.25, 0.25));
        assert_eq!(locate(&streams, (1250.0, 200.0)), (1, 0.5, 0.5));
        assert_eq!(locate(&streams, (2000.0, 0.0)).0, CURSOR_OFF_STREAM);
        assert_eq!(locate(&streams, (10.0, -1.0)).0, CURSOR_OFF_STREAM);
    }

    #[test]
    fn the_datagram_says_whose_it_is() {
        let pos = Pos { stream: 1, x: 0.5, y: 0.25, visible: true, baked: false, shape: 3, driver: 7 };
        let mine = CursorPos::read(&pos.datagram(7)).unwrap();
        assert_eq!(mine.flags, CURSOR_VISIBLE | CURSOR_YOURS);
        assert_eq!((mine.stream, mine.shape, mine.x, mine.y), (1, 3, 0.5, 0.25));
        let theirs = CursorPos::read(&Pos { baked: true, visible: false, ..pos }.datagram(8)).unwrap();
        assert_eq!(theirs.flags, CURSOR_BAKED);
    }

    #[test]
    fn premultiplied_becomes_straight() {
        let mut px = [64, 32, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0];
        unpremultiply(&mut px);
        assert_eq!(px, [128, 64, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn colour_cursors_use_alpha_or_the_mask() {
        // BGRA, premultiplied: half-transparent red.
        assert_eq!(colour_to_rgba(1, 1, &[0, 0, 100, 128], None), [199, 0, 0, 128]);
        // No alpha at all: the AND mask decides.
        let px = [10, 20, 30, 0, 40, 50, 60, 0];
        assert_eq!(colour_to_rgba(2, 1, &px, Some(&[false, true])), [30, 20, 10, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn monochrome_inverting_pixels_get_an_outline() {
        // 3×1: transparent, invert, transparent → white, black, white.
        let rgba = mono_to_rgba(3, 1, &[true, true, true], &[false, true, false]);
        assert_eq!(rgba, [255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255]);
        // Black and white stay as they are; a lone transparent pixel stays clear.
        let rgba = mono_to_rgba(3, 1, &[false, false, true], &[false, true, false]);
        assert_eq!(rgba, [0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn a_viewer_drawing_its_own_lets_the_pointer_out() {
        // (Counts only: the capture isn't there to tell in a unit test.)
        let base = BAKES.load(Ordering::Relaxed);
        let flag = Arc::new(AtomicBool::new(false));
        let mut v = Viewer::new(false, flag.clone());
        assert_eq!(BAKES.load(Ordering::Relaxed), base + 1);
        v.set(SUPPORTED);
        assert_eq!(flag.load(Ordering::Relaxed), SUPPORTED);
        assert_eq!(BAKES.load(Ordering::Relaxed), base + usize::from(!SUPPORTED));
        v.set(false);
        assert_eq!(BAKES.load(Ordering::Relaxed), base + 1);
        drop(v);
        assert_eq!(BAKES.load(Ordering::Relaxed), base);
    }
}
