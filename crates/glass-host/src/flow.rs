//! Video flow control: while a viewer's link is still busy with the frames already sent,
//! don't capture and encode a new one. The change stays pending (the capture keeps the
//! newest picture) and the next frame carries it, so nothing is dropped and no keyframe
//! has to be waited for. Dropping frames that were already encoded, as the WebRTC bridge
//! did, broke the stream until a keyframe, and a keyframe is the biggest frame there is
//! on exactly the link that can't take it.
//!
//! Each send path (a WebTransport session, the WebRTC bridge) keeps a [`Link`] for its
//! viewer: the frames sent and when, and how many bytes have left its send buffer, so it
//! knows which frames have gone. It publishes whether that link is full; the encoders are
//! shared, so a display is held while any viewer it goes to is full. A viewer stuck for
//! seconds (a dead socket, a suspended tab) stops holding everyone else up.
//!
//! Adapted from voxl (MIT, github.com/tembo/voxl): `flow_ok` in its video.rs.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use glass_proto::MAX_STREAMS;

/// How often a held capture looks again (and a send path re-checks a busy link).
pub const POLL: Duration = Duration::from_millis(8);
/// Queue allowed beyond a round trip before the link counts as full.
const SLACK_MS: f64 = 120.0;
/// Round trip assumed before any is measured.
const DEFAULT_RTT_MS: f64 = 40.0;
/// A frame unconfirmed this long: the viewer is stuck, and stops holding the others.
const STUCK: Duration = Duration::from_secs(4);
/// A hold never lasts longer than this: one frame goes anyway. The newest picture reaches a
/// viewer even if what's "in flight" can't be confirmed (an acknowledgement or a last frame
/// lost with nothing after it to show it, a decoder sitting on its last frame).
const MAX_HOLD: Duration = Duration::from_millis(750);
/// The least round trip seen over this long is the link's own (the rest is queue).
const BASE_WINDOW: Duration = Duration::from_secs(10);
/// Frame interval the frame-count limit assumes (the fastest common rate).
const FRAME_MS: f64 = 1000.0 / 60.0;
/// Frames remembered at most (a link that never confirms anything mustn't grow this).
const MAX_TRACKED: usize = 256;
/// Delivery is measured from one delivery to another at least this much later...
const RATE_WINDOW: Duration = Duration::from_millis(100);
/// ...and the best of the last this long is what the link carries. Kept short: a link
/// that slows down (LTE) is believed within a couple of seconds, and until then the age
/// of the oldest frame holds capture anyway.
const RATE_MEMORY: Duration = Duration::from_secs(2);

struct Sent {
    stream: u8,
    id: u32,
    at: Instant,
    /// `Link::total` once this frame's last byte was handed over.
    end: u64,
}

/// One viewer's frames in flight.
#[derive(Default)]
pub struct Link {
    inflight: VecDeque<Sent>,
    /// Bytes handed to the transport, ever (video and everything else sharing its buffer).
    total: u64,
    /// The least round trip the transport has measured lately, if it measures (QUIC), and
    /// when. The least, since its smoothed figure includes any queue on the way.
    rtt: Option<(f64, Instant)>,
    /// The least time a frame took to go, recently, and when it was seen.
    base: Option<(f64, Instant)>,
    /// Stuck: frames still go, but it holds no one up until it has caught up.
    stalled: bool,
    /// When a frame last went.
    last_sent: Option<Instant>,
    /// Bytes out of the buffer (`total` less what's still queued), at the last look.
    out: u64,
    /// The delivery sample open now: the delivery it started at (when, and `out` then).
    window: Option<(Instant, u64)>,
    /// Bytes a second delivered in recent windows, oldest first.
    rates: VecDeque<(Instant, f64)>,
}

impl Link {
    /// A video fragment (or a whole frame) of `bytes` was handed to the transport.
    pub fn sent(&mut self, stream: u8, id: u32, bytes: usize, now: Instant) {
        self.last_sent = Some(now);
        self.total += bytes as u64;
        match self.inflight.back_mut() {
            Some(last) if last.stream == stream && last.id == id => last.end = self.total,
            _ => {
                if self.inflight.len() >= MAX_TRACKED {
                    self.inflight.pop_front();
                }
                self.inflight.push_back(Sent { stream, id, at: now, end: self.total });
            }
        }
    }

    /// Video stopped (a hidden tab pauses it, and its decoder is given up): what's in flight
    /// will never be confirmed, and says nothing about the link.
    pub fn forget(&mut self) {
        self.inflight.clear();
        self.stalled = false;
        self.window = None;
    }

    /// Something other than video went through the same buffer.
    pub fn other(&mut self, bytes: usize) {
        self.total += bytes as u64;
    }

    /// The transport's own round-trip estimate.
    pub fn set_rtt(&mut self, rtt: Duration, now: Instant) {
        let ms = rtt.as_secs_f64() * 1000.0;
        if self.rtt.is_none_or(|(b, at)| ms < b || now.duration_since(at) >= BASE_WINDOW) {
            self.rtt = Some((ms, now));
        }
    }

    /// `queued`: bytes still in the send buffer. Every frame whose last byte is out of it
    /// has gone; how long that took feeds the round-trip baseline.
    pub fn drained(&mut self, queued: usize, now: Instant) {
        let out = self.total.saturating_sub(queued as u64).max(self.out);
        let delivered = out > self.out;
        self.out = out;
        while let Some(f) = self.inflight.front() {
            if f.end > out {
                break;
            }
            self.sample(now.duration_since(f.at), now);
            self.inflight.pop_front();
        }
        if self.inflight.is_empty() {
            self.stalled = false;
        }
        // Delivery rate: bytes delivered between two deliveries, so a sample never counts
        // a burst of acknowledgements without the wait before it. Only while there's
        // more to deliver (an idle link says nothing about how much it could carry).
        if delivered {
            match self.window {
                Some((start, from)) if now.duration_since(start) >= RATE_WINDOW => {
                    let rate = (out - from) as f64 / now.duration_since(start).as_secs_f64();
                    self.rates.push_back((now, rate));
                    while self.rates.front().is_some_and(|&(at, _)| now.duration_since(at) > RATE_MEMORY) {
                        self.rates.pop_front();
                    }
                    self.window = Some((now, out));
                }
                Some(_) => {}
                None => self.window = Some((now, out)),
            }
        }
        if !self.busy() {
            self.window = None;
        }
    }

    /// Bytes a second the link has carried (the best recent window), once measured.
    pub fn rate(&self) -> Option<f64> {
        self.rates.iter().map(|&(_, r)| r).reduce(f64::max).filter(|&r| r > 0.0)
    }

    fn sample(&mut self, took: Duration, now: Instant) {
        let ms = took.as_secs_f64() * 1000.0;
        if self
            .base
            .is_none_or(|(b, at)| ms < b || now.duration_since(at) >= BASE_WINDOW)
        {
            self.base = Some((ms, now));
        }
    }

    /// The round trip: the transport's, else the quickest a frame has gone lately.
    pub fn rtt_ms(&self) -> f64 {
        self.rtt
            .or(self.base)
            .map(|(b, _)| b)
            .unwrap_or(DEFAULT_RTT_MS)
    }

    /// Frames sent that haven't gone yet.
    pub fn busy(&self) -> bool {
        !self.inflight.is_empty()
    }

    /// Too much in flight to send another frame: more frames than a round trip (plus
    /// decoding) needs, the oldest waiting longer than a round trip plus a little, or more
    /// bytes than the link delivers in that time. (The bytes matter on a slow link: right
    /// after a backlog clears, nothing in flight is old, and a burst of frames would fill
    /// the queue again before the oldest got old.)
    pub fn full(&mut self, now: Instant) -> bool {
        let Some(oldest) = self.inflight.front() else {
            return false;
        };
        let age = now.duration_since(oldest.at);
        if age > STUCK && !self.stalled {
            tracing::info!(
                "video link stuck ({} frames, oldest {} ms): not holding capture for it",
                self.inflight.len(),
                age.as_millis()
            );
            self.stalled = true;
        }
        if self.stalled || self.last_sent.is_none_or(|at| now.saturating_duration_since(at) >= MAX_HOLD) {
            return false;
        }
        let rtt = self.rtt_ms();
        // Until the link's rate is known, only a few: a burst into a slow link is a queue.
        let max_frames = match self.rate() {
            Some(_) => (((rtt + 40.0) / FRAME_MS).ceil() as usize + 2).clamp(3, 40),
            None => 3,
        };
        let too_many_bytes = self.rate().is_some_and(|rate| {
            let in_flight = self.total.saturating_sub(self.out) as f64;
            in_flight / rate * 1000.0 > rtt + SLACK_MS
        });
        self.inflight.len() >= max_frames || age.as_secs_f64() * 1000.0 > rtt + SLACK_MS || too_many_bytes
    }

    /// The streams with frames in flight, as a bit mask.
    fn streams(&self) -> u8 {
        self.inflight.iter().fold(0, |m, f| m | 1 << (f.stream as usize).min(MAX_STREAMS - 1))
    }
}

/// Per viewer: the streams it holds (a bit each; 0 when its link isn't full).
static VIEWERS: Mutex<Option<HashMap<u64, u8>>> = Mutex::new(None);
/// The union of those, read by capture on every frame.
static HELD: AtomicU8 = AtomicU8::new(0);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// A send path's place in the registry; its hold is lifted when it drops.
pub struct Viewer {
    id: u64,
    held: u8,
}

impl Viewer {
    pub fn new() -> Self {
        Self { id: NEXT.fetch_add(1, Ordering::Relaxed), held: 0 }
    }

    /// Re-evaluates `link` and publishes the result if it changed. Returns whether it's full.
    pub fn update(&mut self, link: &mut Link, now: Instant) -> bool {
        let full = link.full(now);
        let held = if full { link.streams() } else { 0 };
        if held != self.held {
            self.held = held;
            publish(self.id, held);
        }
        full
    }
}

impl Default for Viewer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        if self.held != 0 {
            publish(self.id, 0);
        }
    }
}

fn publish(id: u64, held: u8) {
    let mut viewers = VIEWERS.lock().unwrap();
    let viewers = viewers.get_or_insert_with(Default::default);
    if held == 0 {
        viewers.remove(&id);
    } else {
        viewers.insert(id, held);
    }
    HELD.store(viewers.values().fold(0, |m, h| m | h), Ordering::Relaxed);
}

/// Capture of `stream` should wait: a viewer's link is full. (A keyframe someone is
/// waiting for goes anyway; the caller checks that.)
pub fn held(stream: u8) -> bool {
    HELD.load(Ordering::Relaxed) & 1 << (stream as usize).min(MAX_STREAMS - 1) != 0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A link that carries `bps` after a one-way delay of `rtt / 2`, with a send buffer in
    /// front of it: what's handed over waits its turn there. "Gone" here means acknowledged,
    /// as the WebRTC data channel counts it: half a round trip after the last byte is sent.
    pub(crate) struct SimLink {
        pub bps: f64,
        pub rtt: Duration,
        /// When the link will have sent everything handed to it so far.
        busy_until: Instant,
        /// (when acknowledged, bytes) for everything handed over.
        acks: VecDeque<(Instant, usize)>,
    }

    impl SimLink {
        pub(crate) fn new(bps: f64, rtt: Duration, now: Instant) -> Self {
            Self { bps, rtt, busy_until: now, acks: VecDeque::new() }
        }

        /// Hands over `bytes`; returns when they'll have arrived.
        pub(crate) fn send(&mut self, bytes: usize, now: Instant) -> Instant {
            let start = self.busy_until.max(now);
            self.busy_until = start + Duration::from_secs_f64(bytes as f64 * 8.0 / self.bps);
            self.acks.push_back((self.busy_until + self.rtt, bytes));
            self.busy_until + self.rtt / 2
        }

        /// Bytes not yet acknowledged.
        pub(crate) fn outstanding(&mut self, now: Instant) -> usize {
            while self.acks.front().is_some_and(|&(at, _)| at <= now) {
                self.acks.pop_front();
            }
            self.acks.iter().map(|&(_, b)| b).sum()
        }
    }

    /// Runs a screen in constant motion (a frame every 1/60 s wanted, `frame` bytes each)
    /// over `link` for `secs`; returns (frames sent, worst capture-to-arrival delay).
    fn run(link: &mut SimLink, frame: usize, secs: f64, hold: bool) -> (u32, Duration) {
        let t0 = Instant::now();
        let mut l = Link::default();
        let mut sent = 0;
        let mut worst = Duration::ZERO;
        // A change happened at `pending` and hasn't been sent yet.
        let mut pending: Option<Instant> = None;
        let step = Duration::from_millis(2);
        let mut next_capture = t0;
        let mut now = t0;
        while now < t0 + Duration::from_secs_f64(secs) {
            if now >= next_capture {
                pending.get_or_insert(now);
                next_capture += Duration::from_micros(16_667);
            }
            let out = link.outstanding(now);
            l.drained(out, now);
            if let Some(changed) = pending
                && !(hold && l.full(now))
            {
                let arrives = link.send(frame, now);
                l.sent(0, sent, frame, now);
                worst = worst.max(arrives - changed);
                sent += 1;
                pending = None;
            }
            now += step;
        }
        (sent, worst)
    }

    #[test]
    fn a_slow_link_gets_fewer_frames_not_a_growing_queue() {
        // The Tesla case: ~1 Mbps through a TURN relay with a 160 ms round trip, and a
        // moving screen making 6 KB frames (2.9 Mbps at 60 fps).
        let rtt = Duration::from_millis(160);
        let mut held = SimLink::new(1_000_000.0, rtt, Instant::now());
        let (frames, worst) = run(&mut held, 6_000, 10.0, true);
        // About what fits (1 Mbps / 48 kbit ≈ 20 fps), each shown soon after it changed.
        assert!((180..=215).contains(&frames), "{frames} frames");
        assert!(worst < Duration::from_millis(450), "worst delay {worst:?}");

        // Sending every frame instead: the queue grows the whole time.
        let mut flooded = SimLink::new(1_000_000.0, rtt, Instant::now());
        let (_, worst) = run(&mut flooded, 6_000, 10.0, false);
        assert!(worst > Duration::from_secs(5), "worst delay {worst:?}");
    }

    #[test]
    fn a_deep_queue_is_never_filled() {
        // A fast link behind a deep buffer (bufferbloat): 5 Mbps, 40 ms, 50 KB frames
        // (20 Mbps offered), 80 ms each on the wire. Holding keeps the queue to about a
        // round trip and 120 ms: the delay stays under half a second, and every frame the
        // link can carry goes (12.5 a second).
        let mut link = SimLink::new(5_000_000.0, Duration::from_millis(40), Instant::now());
        let (frames, worst) = run(&mut link, 50_000, 5.0, true);
        assert!(frames >= 58, "{frames} frames");
        assert!(worst < Duration::from_millis(450), "worst delay {worst:?}");
        let mut flooded = SimLink::new(5_000_000.0, Duration::from_millis(40), Instant::now());
        let (_, worst) = run(&mut flooded, 50_000, 5.0, false);
        assert!(worst > Duration::from_secs(3), "worst delay {worst:?}");
    }

    #[test]
    fn a_frame_is_gone_once_its_last_byte_has_left() {
        let t0 = Instant::now();
        let mut l = Link::default();
        l.sent(0, 1, 1_000, t0);
        l.sent(0, 1, 1_000, t0);
        l.other(100);
        l.sent(1, 7, 500, t0);
        assert_eq!(l.streams(), 0b11);
        // 2.6 KB handed over; 1 KB still queued: frame 1 hasn't gone.
        l.drained(1_000, t0 + Duration::from_millis(30));
        assert_eq!(l.inflight.len(), 2);
        l.drained(500, t0 + Duration::from_millis(50));
        assert_eq!(l.inflight.len(), 1);
        assert_eq!(l.rtt_ms(), 50.0, "the quickest a frame went");
        l.set_rtt(Duration::from_millis(20), t0);
        l.set_rtt(Duration::from_millis(900), t0 + Duration::from_millis(60));
        assert_eq!(l.rtt_ms(), 20.0, "the transport's own (least) wins");
        l.drained(0, t0 + Duration::from_millis(60));
        assert!(!l.busy());
    }

    #[test]
    fn full_on_too_many_frames_or_an_old_one() {
        let t0 = Instant::now();
        let mut l = Link::default();
        l.set_rtt(Duration::from_millis(40), t0);
        l.sent(0, 0, 1_000, t0);
        assert!(!l.full(t0));
        // Older than a round trip and 120 ms.
        assert!(l.full(t0 + Duration::from_millis(161)));
        // Before the link's rate is known, three frames at most.
        let mut l = Link::default();
        l.set_rtt(Duration::from_millis(40), t0);
        for id in 0..2 {
            l.sent(0, id, 100, t0);
        }
        assert!(!l.full(t0));
        l.sent(0, 2, 100, t0);
        assert!(l.full(t0));
        // Measured (1 MB delivered in 100 ms), more frames than 40 ms + 40 ms of decoding
        // needs at 60 fps (5 + 2).
        let t1 = t0 + Duration::from_millis(100);
        l.drained(200, t0);
        l.sent(0, 3, 1_000_000, t0);
        l.drained(0, t1);
        assert!(l.rate().is_some_and(|r| (r - 10_000_000.0).abs() < 5_000.0), "{:?}", l.rate());
        for id in 4..10 {
            l.sent(0, id, 100, t1);
        }
        assert!(!l.full(t1));
        l.sent(0, 10, 100, t1);
        assert!(l.full(t1));
        // Or more bytes than it delivers in a round trip and 120 ms (1.6 MB).
        l.drained(0, t1);
        l.sent(0, 11, 1_700_000, t1);
        assert!(l.full(t1));
    }

    #[test]
    fn a_hold_never_stops_frames_for_long() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut l = Link::default();
        l.set_rtt(ms(40), t0);
        l.sent(0, 0, 1_000, t0);
        // Never confirmed: full once old, but one frame goes after a while...
        assert!(l.full(t0 + ms(700)));
        assert!(!l.full(t0 + ms(750)));
        // ...and then it's held again.
        l.sent(0, 1, 1_000, t0 + ms(750));
        assert!(l.full(t0 + ms(1000)));
    }

    #[test]
    fn pausing_video_forgets_what_was_in_flight() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut l = Link::default();
        let mut v = Viewer::new();
        for id in 0..3 {
            l.sent(3, id, 1_000, t0);
        }
        assert!(v.update(&mut l, t0 + ms(10)));
        assert!(held(3));
        // A hidden tab: these will never be confirmed, and it isn't the link's doing.
        l.forget();
        assert!(!l.busy());
        assert!(!v.update(&mut l, t0 + ms(20)));
        assert!(!held(3));
    }

    #[test]
    fn a_stuck_viewer_stops_holding_the_others() {
        let t0 = Instant::now();
        let mut l = Link::default();
        let mut v = Viewer::new();
        l.sent(2, 0, 1_000, t0);
        assert!(v.update(&mut l, t0 + Duration::from_millis(500)));
        assert!(held(2), "full: its stream is held");
        assert!(!v.update(&mut l, t0 + Duration::from_secs(5)));
        assert!(!held(2), "stuck: released");
        // Still stalled while frames pile up; caught up, it counts again.
        l.sent(2, 1, 1_000, t0 + Duration::from_secs(5));
        assert!(!v.update(&mut l, t0 + Duration::from_secs(6)));
        l.drained(0, t0 + Duration::from_secs(6));
        l.sent(2, 2, 1_000, t0 + Duration::from_secs(6));
        l.drained(0, t0 + Duration::from_secs(6));
        l.sent(2, 3, 1_000, t0 + Duration::from_secs(7));
        assert!(v.update(&mut l, t0 + Duration::from_millis(7700)));
        drop(v);
        assert!(!held(2), "gone: released");
    }
}
