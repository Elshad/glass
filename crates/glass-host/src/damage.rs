//! When each part of each display last changed, so an agent's `wait_for_change` needs no
//! screenshots and no polling, and misses nothing small (a caret, a spinner, one field).
//!
//! Capture hears about every change anyway: ScreenCaptureKit's dirty rectangles on macOS,
//! Desktop Duplication's dirty and move rectangles on Windows, PipeWire's damage on Linux.
//! It notes when each 16×16 block last changed, under one sequence number for every
//! display. Where none of those is to hand, frames are compared block by block instead.
//!
//! A display nothing captures (on a Mac, one viewers aren't watching) is followed while an
//! agent uses it: `follow` starts what's needed, and it stops a minute after the agent's
//! last look.
//!
//! Adapted from voxl (MIT, github.com/tembo/voxl): src/wait.rs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::media::VideoSource;

const BLOCK: usize = 16;
/// Quiet this long and the screen has settled (unless the agent says otherwise).
pub const DEFAULT_IDLE: Duration = Duration::from_millis(300);
/// A display is followed for this long after an agent last looked at it or waited on it.
pub const WANTED_FOR: Duration = Duration::from_secs(60);
/// How often a display nothing reports changes for is compared with how it was.
const POLL_EVERY: Duration = Duration::from_millis(200);
/// The size compared then (big enough that a caret still shows).
const POLL_MAX: [u32; 2] = [1600, 1600];

/// A change: its sequence number and when it happened.
type Mark = (u64, Instant);

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// One display: when each block last changed.
struct Map {
    width: usize,
    height: usize,
    cols: usize,
    /// Row-major.
    blocks: Vec<Mark>,
    /// The latest change anywhere on it.
    last: Mark,
}

impl Map {
    /// A display first heard of: no change known yet.
    fn new(width: usize, height: usize) -> Self {
        let cols = width.div_ceil(BLOCK);
        let none = (0, Instant::now());
        Map { width, height, cols, blocks: vec![none; cols * height.div_ceil(BLOCK)], last: none }
    }

    /// Blocks covering [x0, x1) × [y0, y1), clipped: (col0, col1, row0, row1), empty when
    /// it's off the picture.
    fn span(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> (usize, usize, usize, usize) {
        let (x1, y1) = (x1.min(self.width), y1.min(self.height));
        if x1 <= x0 || y1 <= y0 {
            return (0, 0, 0, 0);
        }
        (x0 / BLOCK, x1.div_ceil(BLOCK), y0 / BLOCK, y1.div_ceil(BLOCK))
    }

    /// `region` (0..1 of the display) as blocks.
    fn region(&self, r: Region) -> (usize, usize, usize, usize) {
        let (w, h) = (self.width as f64, self.height as f64);
        let x0 = (r.x * w).floor().max(0.0) as usize;
        let y0 = (r.y * h).floor().max(0.0) as usize;
        let x1 = ((r.x + r.w) * w).ceil().max(0.0) as usize;
        let y1 = ((r.y + r.h) * h).ceil().max(0.0) as usize;
        self.span(x0, y0, x1, y1)
    }
}

#[derive(Default)]
struct Tracker {
    displays: HashMap<u32, Map>,
    seq: u64,
    /// When an agent last looked at each display.
    wanted: HashMap<u32, Instant>,
    /// Displays compared frame by frame (`poll`).
    polling: HashSet<u32>,
}

static TRACKER: LazyLock<Mutex<Tracker>> = LazyLock::new(Default::default);
/// The latest sequence number; wakes waits.
static SEQ: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

/// Part of a display: x, y, width, height as 0..1 of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Areas of display `display` that changed, as (x0, y0, x1, y1) in the pixels of a picture
/// of it `size` big (any size: sources capture at different ones).
pub fn note(display: u32, size: (usize, usize), rects: &[(u32, u32, u32, u32)]) {
    if size.0 == 0 || size.1 == 0 {
        return;
    }
    let mut t = TRACKER.lock().unwrap();
    let now = (t.seq + 1, Instant::now());
    let map = t.displays.entry(display).or_insert_with(|| Map::new(size.0, size.1));
    // Into the map's pixels, rounding outwards.
    let (sx, sy) = (map.width as f64 / size.0 as f64, map.height as f64 / size.1 as f64);
    let mut any = false;
    for &(x0, y0, x1, y1) in rects {
        let (x0, x1) = ((x0 as f64 * sx).floor() as usize, (x1 as f64 * sx).ceil() as usize);
        let (y0, y1) = ((y0 as f64 * sy).floor() as usize, (y1 as f64 * sy).ceil() as usize);
        let (c0, c1, r0, r1) = map.span(x0, y0, x1, y1);
        for row in r0..r1 {
            map.blocks[row * map.cols + c0..row * map.cols + c1].fill(now);
            any = true;
        }
    }
    if any {
        map.last = now;
        t.seq = now.0;
        drop(t);
        SEQ.send_replace(now.0);
    }
}

/// All of display `display` changed (a source that can't say where).
pub fn note_all(display: u32, size: (usize, usize)) {
    note(display, size, &[(0, 0, size.0 as u32, size.1 as u32)]);
}

/// The latest sequence number: pass it to a wait as `since` to count only what changes
/// after now.
pub fn seq() -> u64 {
    TRACKER.lock().unwrap().seq
}

/// The latest change on `display` (in `region`, or anywhere on it) and the latest sequence
/// number, read together: a `seq` handed out never covers a change the answer didn't see.
fn latest(display: u32, region: Option<Region>) -> (Mark, u64) {
    let t = TRACKER.lock().unwrap();
    let Some(m) = t.displays.get(&display) else {
        return ((0, *START), t.seq);
    };
    let Some(r) = region else {
        return (m.last, t.seq);
    };
    let (c0, c1, r0, r1) = m.region(r);
    let mark = (r0..r1)
        .flat_map(|row| &m.blocks[row * m.cols + c0..row * m.cols + c1])
        .fold((0, *START), |a, &b| if b.0 > a.0 { b } else { a });
    (mark, t.seq)
}

/// An agent is looking at `display`: follow its changes for a while.
fn want(display: u32) {
    TRACKER.lock().unwrap().wanted.insert(display, Instant::now());
}

/// Whether an agent looked at `display` lately (sources that compare frames themselves
/// only do it then).
pub fn wanted(display: u32) -> bool {
    TRACKER.lock().unwrap().wanted.get(&display).is_some_and(|t| t.elapsed() < WANTED_FOR)
}

/// An agent is looking at `display`: make sure its changes are noted, by the source's own
/// capture (`VideoSource::track_damage`, which may start one) or, failing that, by
/// comparing pictures of it. May block.
pub fn follow(video: &Arc<dyn VideoSource>, display: u32) {
    want(display);
    if video.track_damage(display) {
        return;
    }
    if !TRACKER.lock().unwrap().polling.insert(display) {
        return;
    }
    let video = video.clone();
    let started = std::thread::Builder::new().name(format!("glass-damage-{display}")).spawn(move || poll(video, display));
    if started.is_err() {
        TRACKER.lock().unwrap().polling.remove(&display);
    }
}

/// Compares pictures of `display` until no agent has looked at it for a while.
fn poll(video: Arc<dyn VideoSource>, display: u32) {
    let id = display;
    tracing::debug!("display {id}: comparing pictures for agents");
    let mut differ = Differ::default();
    loop {
        {
            let mut t = TRACKER.lock().unwrap();
            if !t.wanted.get(&display).is_some_and(|at| at.elapsed() < WANTED_FOR) {
                t.polling.remove(&display);
                break;
            }
        }
        if let Some(f) = video.picture(display, POLL_MAX)
            && let Some(rects) = differ.diff(f.width, f.height, f.width * 4, &f.bgra)
        {
            note(display, (f.width, f.height), &rects);
        }
        std::thread::sleep(POLL_EVERY);
    }
    tracing::debug!("display {id}: stopped comparing pictures");
}

/// Finds the 16×16 blocks that changed between one picture and the next.
#[derive(Default)]
pub struct Differ {
    size: (usize, usize),
    /// The last picture, rows packed.
    prev: Vec<u8>,
}

impl Differ {
    /// What changed since the last picture, as rects (`note`'s): `None` for the first one,
    /// or a new size, which there's nothing to compare with. `data` is 4-byte pixels, rows
    /// `stride` bytes apart.
    pub fn diff(&mut self, width: usize, height: usize, stride: usize, data: &[u8]) -> Option<Vec<(u32, u32, u32, u32)>> {
        let row = width * 4;
        if width == 0 || height == 0 || stride < row || data.len() < stride * (height - 1) + row {
            return None;
        }
        if self.size != (width, height) {
            self.size = (width, height);
            self.prev.clear();
            for y in 0..height {
                self.prev.extend_from_slice(&data[y * stride..][..row]);
            }
            return None;
        }
        let cols = width.div_ceil(BLOCK);
        let mut dirty = vec![false; cols];
        let mut rects = Vec::new();
        for by in 0..height.div_ceil(BLOCK) {
            dirty.fill(false);
            let (y0, y1) = (by * BLOCK, ((by + 1) * BLOCK).min(height));
            for y in y0..y1 {
                let (now, was) = (&data[y * stride..][..row], &mut self.prev[y * row..][..row]);
                if now == was {
                    continue;
                }
                for (c, d) in dirty.iter_mut().enumerate() {
                    let (a, b) = (c * BLOCK * 4, ((c + 1) * BLOCK * 4).min(row));
                    if !*d && now[a..b] != was[a..b] {
                        *d = true;
                    }
                }
                was.copy_from_slice(now);
            }
            // Runs of changed blocks in this row of blocks, one rect each.
            let mut c = 0;
            while c < cols {
                if !dirty[c] {
                    c += 1;
                    continue;
                }
                let c0 = c;
                while c < cols && dirty[c] {
                    c += 1;
                }
                rects.push(((c0 * BLOCK) as u32, y0 as u32, (c * BLOCK).min(width) as u32, y1 as u32));
            }
        }
        Some(rects)
    }
}

/// What to wait for.
#[derive(Debug, Clone, Copy)]
pub struct Wait {
    /// Wait for a change after `since` first.
    pub change: bool,
    /// Then until nothing has changed for this long (counting from the call at the
    /// earliest, so what an action just did has time to show). Zero: don't.
    pub idle: Duration,
    /// Only changes here count.
    pub region: Option<Region>,
    /// Count changes after this sequence number (from a screenshot or an earlier wait),
    /// instead of after the call: a change that came first isn't missed.
    pub since: Option<u64>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Waited {
    /// Something changed (in the region) after `since`.
    pub changed: bool,
    pub timed_out: bool,
    /// Pass it as `since` next time.
    pub seq: u64,
    pub waited: Duration,
}

/// Waits on `display` as `w` says.
pub async fn wait(display: u32, w: Wait) -> Waited {
    let start = Instant::now();
    let since = w.since.unwrap_or_else(seq);
    let deadline = start + w.timeout;
    let mut rx = SEQ.subscribe();
    let timed_out = loop {
        // Mark the current change seen before looking, so a later one wakes us.
        rx.borrow_and_update();
        let ((seq, at), _) = latest(display, w.region);
        let now = Instant::now();
        let done_at = (!w.change || seq > since).then(|| at.max(start) + w.idle);
        if done_at.is_some_and(|t| t <= now) {
            break false;
        }
        if now >= deadline {
            break true;
        }
        let wake = done_at.map_or(deadline, |t| t.min(deadline));
        tokio::select! {
            _ = rx.changed() => {}
            _ = tokio::time::sleep_until(wake.into()) => {}
        }
    };
    let ((latest, _), seq) = latest(display, w.region);
    Waited { changed: latest > since, timed_out, seq, waited: start.elapsed() }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test has its own display ids: the tracker is shared.

    fn region(x: f64, y: f64, w: f64, h: f64) -> Option<Region> {
        Some(Region { x, y, w, h })
    }

    /// The latest change's seq on display `d` (in `r`).
    fn at(d: u32, r: Option<Region>) -> u64 {
        latest(d, r).0.0
    }

    #[test]
    fn changes_mark_the_blocks_they_touch() {
        let d = 1001;
        note_all(d, (160, 100));
        let first = at(d, None);
        // A caret: two pixels wide, in the block at column 3, row 2.
        note(d, (160, 100), &[(50, 40, 52, 47)]);
        let caret = at(d, None);
        assert!(caret > first);
        // Its block, as 0..1 of the display.
        assert_eq!(at(d, region(48.0 / 160.0, 32.0 / 100.0, 0.1, 0.16)), caret);
        // Elsewhere: only the first change, of all of it.
        assert_eq!(at(d, region(0.0, 0.0, 0.25, 0.3)), first);
        assert_eq!(at(d, region(0.5, 0.5, 0.5, 0.5)), first);
    }

    #[test]
    fn pictures_of_other_sizes_are_scaled() {
        let d = 1002;
        note(d, (64, 64), &[]);
        // Nothing known yet.
        assert_eq!(at(d, None), 0);
        // Half the size: its block at 16, 16 is the map's at 32, 32.
        note(d, (32, 32), &[(16, 16, 17, 17)]);
        assert!(at(d, region(0.5, 0.5, 0.25, 0.25)) > 0);
        assert_eq!(at(d, region(0.0, 0.0, 0.5, 0.5)), 0);
        assert_eq!(at(d, region(0.75, 0.75, 0.25, 0.25)), 0);
    }

    #[test]
    fn rects_off_the_picture_change_nothing() {
        let d = 1003;
        note_all(d, (64, 64));
        let first = at(d, None);
        note(d, (64, 64), &[(70, 0, 80, 10), (10, 10, 10, 20)]);
        assert_eq!(at(d, None), first);
        // A display nothing was heard of has no changes.
        assert_eq!(at(4040, None), 0);
    }

    #[test]
    fn the_differ_finds_small_changes() {
        let (w, h, stride) = (40, 20, 40 * 4 + 8);
        let mut a = vec![0u8; stride * h];
        let mut d = Differ::default();
        assert_eq!(d.diff(w, h, stride, &a), None);
        assert_eq!(d.diff(w, h, stride, &a), Some(vec![]));
        // One pixel, then two neighbouring blocks, then the padding (which doesn't count).
        a[5 * stride + 3 * 4] = 1;
        assert_eq!(d.diff(w, h, stride, &a), Some(vec![(0, 0, 16, 16)]));
        a[17 * stride + 20 * 4] = 1;
        a[18 * stride + 39 * 4 + 2] = 1;
        assert_eq!(d.diff(w, h, stride, &a), Some(vec![(16, 16, 40, 20)]));
        a[3 * stride + w * 4 + 1] = 9;
        assert_eq!(d.diff(w, h, stride, &a), Some(vec![]));
        // A new size starts over.
        assert_eq!(d.diff(20, 10, 80, &a[..800]), None);
    }

    #[tokio::test]
    async fn waits_for_quiet_counting_from_the_call() {
        let d = 1004;
        note_all(d, (32, 32));
        let w = Wait { change: false, idle: Duration::from_millis(60), region: None, since: None, timeout: Duration::from_secs(2) };
        let waited = wait(d, w).await;
        assert!(!waited.changed && !waited.timed_out);
        assert!(waited.waited >= Duration::from_millis(60), "{waited:?}");
    }

    #[tokio::test]
    async fn a_change_wakes_a_wait_for_one() {
        let d = 1005;
        note_all(d, (32, 32));
        let since = seq();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            note(d, (32, 32), &[(0, 0, 1, 1)]);
        });
        let w = Wait { change: true, idle: Duration::ZERO, region: None, since: Some(since), timeout: Duration::from_secs(5) };
        let waited = wait(d, w).await;
        assert!(waited.changed && !waited.timed_out, "{waited:?}");
        assert!(waited.seq > since && waited.waited < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn since_catches_a_change_before_the_call() {
        let d = 1006;
        note_all(d, (32, 32));
        let since = seq();
        note(d, (32, 32), &[(20, 20, 21, 21)]);
        let w = Wait { change: true, idle: Duration::ZERO, region: None, since: Some(since), timeout: Duration::from_millis(200) };
        let waited = wait(d, w).await;
        assert!(waited.changed && !waited.timed_out && waited.waited < Duration::from_millis(100), "{waited:?}");
        // Without it, only what comes after the call counts.
        let w = Wait { since: None, ..w };
        let waited = wait(d, w).await;
        assert!(!waited.changed && waited.timed_out, "{waited:?}");
    }

    #[tokio::test]
    async fn changes_outside_the_region_dont_count() {
        let d = 1007;
        note_all(d, (64, 64));
        let since = seq();
        note(d, (64, 64), &[(0, 0, 8, 8)]);
        let w = Wait { change: true, idle: Duration::ZERO, region: region(0.5, 0.5, 0.5, 0.5), since: Some(since), timeout: Duration::from_millis(100) };
        let waited = wait(d, w).await;
        assert!(!waited.changed && waited.timed_out, "{waited:?}");
        let waited = wait(d, Wait { region: region(0.0, 0.0, 0.2, 0.2), ..w }).await;
        assert!(waited.changed && !waited.timed_out, "{waited:?}");
    }

    #[tokio::test]
    async fn a_change_then_quiet() {
        let d = 1008;
        note_all(d, (32, 32));
        let since = seq();
        tokio::spawn(async move {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                note(d, (32, 32), &[(0, 0, 4, 4)]);
            }
        });
        let w = Wait { change: true, idle: Duration::from_millis(80), region: None, since: Some(since), timeout: Duration::from_secs(5) };
        let waited = wait(d, w).await;
        assert!(waited.changed && !waited.timed_out, "{waited:?}");
        // At least the three changes and the quiet after the last.
        assert!(waited.waited >= Duration::from_millis(140), "{waited:?}");
    }
}
