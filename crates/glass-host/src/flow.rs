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
//! A viewer that acknowledges frames once drawn (KIND_FRAME_ACK) is tracked by those
//! instead: a frame is gone when the viewer has it on screen, so every queue on the way
//! counts (a relay's, the network's, a slow decoder), and how long acknowledgements take
//! feeds its bitrate (rate::AckRate).
//!
//! Adapted from voxl (MIT, github.com/tembo/voxl): `flow_ok` in its video.rs, and the
//! delay and delivery estimates in its hub.rs.

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
/// A frame unconfirmed this long no longer keeps the link from counting as idle: a refresh
/// is what would show its loss (or get a decoder to give up its last frame).
const IDLE_AFTER: Duration = Duration::from_secs(1);
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
    bytes: usize,
    /// Bytes of frames still in flight when it was sent: it waits behind those.
    ahead: u64,
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
    /// The viewer acknowledges frames: they're gone when it says so, not when they leave
    /// the send buffer.
    acks: bool,
    /// Bytes of frames acknowledged, ever.
    acked: u64,
    /// How much longer than the quickest lately acknowledgements take (smoothed).
    queue_ms: f64,
    /// Bytes a second acknowledged, smoothed, and the window being counted (start, bytes).
    delivered: f64,
    delivered_window: Option<(Instant, u64)>,
    /// When `delivered` was last measured.
    delivered_at: Option<Instant>,
    /// When a frame was last acknowledged.
    last_ack: Option<Instant>,
    /// The bitrate frames are being made at, to tell how long one takes just to send.
    pace_bps: f64,
}

/// Nothing on its way for this long: the stream had nothing to send.
const REST: Duration = Duration::from_millis(250);
/// No acknowledgement for this long: what they said no longer describes the link.
const NO_NEWS: Duration = Duration::from_secs(1);

/// Wrapping frame-id order: `a` is `b` or before it.
fn id_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < u32::MAX / 2
}

impl Link {
    /// A video fragment (or a whole frame) of `bytes` was handed to the transport.
    pub fn sent(&mut self, stream: u8, id: u32, bytes: usize, now: Instant) {
        self.last_sent = Some(now);
        self.total += bytes as u64;
        match self.inflight.back_mut() {
            Some(last) if last.stream == stream && last.id == id => {
                last.end = self.total;
                last.bytes += bytes;
            }
            _ => {
                if self.inflight.len() >= MAX_TRACKED {
                    self.inflight.pop_front();
                }
                let ahead = self.inflight.iter().map(|f| f.bytes as u64).sum();
                // Delivery is measured from here after a rest: time spent idle says nothing.
                if ahead == 0 && self.last_ack.is_none_or(|at| now.saturating_duration_since(at) > REST) {
                    self.delivered_window = Some((now, 0));
                }
                self.inflight.push_back(Sent { stream, id, at: now, end: self.total, bytes, ahead });
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
        // Acknowledged frames are gone only when the viewer says so.
        if self.acks {
            return;
        }
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
        if delivered {
            self.delivery(out, now);
        }
        if !self.busy() {
            self.window = None;
        }
    }

    /// Delivery rate: bytes delivered between two deliveries (`so_far`: delivered ever),
    /// so a sample never counts a burst of acknowledgements without the wait before it.
    /// Only while there's more to deliver (an idle link says nothing about how much it
    /// could carry).
    fn delivery(&mut self, so_far: u64, now: Instant) {
        match self.window {
            Some((start, from)) if now.duration_since(start) >= RATE_WINDOW => {
                let rate = so_far.saturating_sub(from) as f64 / now.duration_since(start).as_secs_f64();
                self.rates.push_back((now, rate));
                while self.rates.front().is_some_and(|&(at, _)| now.duration_since(at) > RATE_MEMORY) {
                    self.rates.pop_front();
                }
                self.window = Some((now, so_far));
            }
            Some(_) => {}
            None => self.window = Some((now, so_far)),
        }
    }

    /// The viewer drew frame `id` of `stream`: it and every earlier one of that stream are
    /// gone (an earlier one it never drew was lost or skipped). From then on frames are
    /// gone only when acknowledged.
    pub fn acked(&mut self, stream: u8, id: u32, now: Instant) {
        self.expect_acks();
        self.last_ack = Some(now);
        let pace = self.pace();
        let mut bytes = 0;
        let mut took = None;
        self.inflight.retain(|f| {
            let gone = f.stream == stream && id_le(f.id, id);
            if gone {
                bytes += f.bytes;
                if f.id == id {
                    let sending = (f.ahead + f.bytes as u64) as f64 / pace;
                    took = Some((now.duration_since(f.at), Duration::from_secs_f64(sending)));
                }
            }
            !gone
        });
        if let Some((took, sending)) = took {
            self.sample(took, now);
            let base = self.base.map_or(0.0, |(b, _)| b);
            // How long it took beyond the quickest, less how long it (and what was ahead of
            // it) takes just to send at the rate it was made for. A keyframe is many times
            // the size of a frame: its own length isn't a queue on the link.
            let queued = (took.saturating_sub(sending).as_secs_f64() * 1000.0 - base).max(0.0);
            self.queue_ms = self.queue_ms * 0.7 + queued * 0.3;
        }
        if bytes > 0 {
            self.acked += bytes as u64;
            self.delivery(self.acked, now);
            let (start, counted) = *self.delivered_window.get_or_insert((now, 0));
            let counted = counted + bytes as u64;
            let took = now.duration_since(start).as_secs_f64();
            if took >= 0.5 {
                let rate = counted as f64 / took;
                let recent = self.delivered > 0.0 && self.delivered_at.is_some_and(|at| now.saturating_duration_since(at) < NO_NEWS);
                self.delivered = if recent { self.delivered * 0.6 + rate * 0.4 } else { rate };
                self.delivered_at = Some(now);
                self.delivered_window = Some((now, 0));
            } else {
                self.delivered_window = Some((start, counted));
            }
        }
        if self.inflight.is_empty() {
            self.stalled = false;
            self.window = None;
        }
    }

    /// It said so (or just did): frames are gone only when it says, from now on.
    pub fn expect_acks(&mut self) {
        if !self.acks {
            // Its own baseline: acknowledgements take a decode longer than the buffer did.
            self.acks = true;
            self.base = None;
            self.window = None;
        }
    }

    /// The bitrate frames are being made at (see [`Link::acked`]).
    pub fn set_pace(&mut self, bps: u32) {
        self.pace_bps = bps as f64;
    }

    /// Bytes a second frames are expected to take to send.
    fn pace(&self) -> f64 {
        let bps = if self.pace_bps > 0.0 { self.pace_bps } else { crate::rate::ACK_START_BPS as f64 };
        bps / 8.0
    }

    /// What acknowledgements say about the link now, for its bitrate.
    pub fn ack_sample(&mut self, now: Instant) -> crate::rate::AckSample {
        let heard = self.last_ack.is_some_and(|at| now.saturating_duration_since(at) < NO_NEWS);
        let pace = self.pace();
        crate::rate::AckSample {
            // Nothing on its way: whatever queue there was, the last frames came through it.
            queue_ms: if self.busy() && heard { self.queue_ms } else { 0.0 },
            oldest_ms: self.inflight.front().map_or(0.0, |f| {
                let sending = (f.ahead + f.bytes as u64) as f64 / pace;
                (now.duration_since(f.at).as_secs_f64() - sending).max(0.0) * 1000.0
            }),
            rtt_ms: self.rtt_ms(),
            // None heard lately means no evidence either way: it would "use" the rate, and
            // raise it, or count as a link that delivers nothing, on a silence that can be a
            // hidden tab, a lost last frame or a decoder sitting on one.
            delivered_bps: if heard { self.delivered * 8.0 } else { 0.0 },
            held: self.full(now),
            waited_ms: self.inflight.front().map_or(0.0, |f| {
                (now.duration_since(f.at).as_secs_f64() - f.bytes as f64 / pace).max(0.0) * 1000.0
            }),
            in_flight_bits: self.inflight.iter().map(|f| f.bytes as f64 * 8.0).sum(),
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

    /// The round trip: the transport's, else the quickest a frame has gone lately (with
    /// acknowledgements, always that: it includes decoding, which the transport's doesn't).
    pub fn rtt_ms(&self) -> f64 {
        if self.acks { self.base } else { self.rtt.or(self.base) }
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
            let in_flight = if self.acks {
                self.inflight.iter().map(|f| f.bytes).sum::<usize>() as f64
            } else {
                self.total.saturating_sub(self.out) as f64
            };
            in_flight / rate * 1000.0 > rtt + SLACK_MS
        });
        self.inflight.len() >= max_frames || age.as_secs_f64() * 1000.0 > rtt + SLACK_MS || too_many_bytes
    }

    /// The streams with a frame on its way lately, as a bit mask.
    fn recent_streams(&self, now: Instant) -> u8 {
        self.inflight
            .iter()
            .filter(|f| now.saturating_duration_since(f.at) < IDLE_AFTER)
            .fold(0, |m, f| m | 1 << (f.stream as usize).min(MAX_STREAMS - 1))
    }

    /// The streams with frames in flight, as a bit mask.
    fn streams(&self) -> u8 {
        self.inflight.iter().fold(0, |m, f| m | 1 << (f.stream as usize).min(MAX_STREAMS - 1))
    }
}

/// Per viewer: the streams it holds (a bit each; 0 when its link isn't full), and the
/// streams it has frames in flight for.
static VIEWERS: Mutex<Option<HashMap<u64, (u8, u8)>>> = Mutex::new(None);
/// The unions of those, read by capture on every frame.
static HELD: AtomicU8 = AtomicU8::new(0);
static BUSY: AtomicU8 = AtomicU8::new(0);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// A send path's place in the registry; its hold is lifted when it drops.
pub struct Viewer {
    id: u64,
    /// What it last published: (held, busy).
    state: (u8, u8),
}

impl Viewer {
    pub fn new() -> Self {
        Self { id: NEXT.fetch_add(1, Ordering::Relaxed), state: (0, 0) }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Re-evaluates `link` and publishes the result if it changed. Returns whether it's full.
    pub fn update(&mut self, link: &mut Link, now: Instant) -> bool {
        let full = link.full(now);
        let state = (if full { link.streams() } else { 0 }, link.recent_streams(now));
        if state != self.state {
            self.state = state;
            publish(self.id, state);
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
        if self.state != (0, 0) {
            publish(self.id, (0, 0));
        }
    }
}

fn publish(id: u64, state: (u8, u8)) {
    let mut viewers = VIEWERS.lock().unwrap();
    let viewers = viewers.get_or_insert_with(Default::default);
    if state == (0, 0) {
        viewers.remove(&id);
    } else {
        viewers.insert(id, state);
    }
    HELD.store(viewers.values().fold(0, |m, s| m | s.0), Ordering::Relaxed);
    BUSY.store(viewers.values().fold(0, |m, s| m | s.1), Ordering::Relaxed);
}

/// No viewer has a frame of `stream` still on its way: the link is idle, so a frame that
/// only refines a still picture won't queue ahead of the next real change.
// Only the Mac's capture refines a still screen.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn idle(stream: u8) -> bool {
    BUSY.load(Ordering::Relaxed) & 1 << (stream as usize).min(MAX_STREAMS - 1) == 0
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

    /// A still screen refined with 20 KB re-encodes every 83 ms over 1 Mbps / 160 ms, then a
    /// change at 2 s: how long until that change arrives.
    fn change_after_refreshes(only_when_idle: bool) -> Duration {
        let t0 = Instant::now();
        let mut link = SimLink::new(1_000_000.0, Duration::from_millis(160), t0);
        let mut l = Link::default();
        let mut id = 0;
        let mut now = t0;
        while now < t0 + Duration::from_secs(2) {
            l.drained(link.outstanding(now), now);
            if !only_when_idle || !l.busy() {
                link.send(20_000, now);
                l.sent(0, id, 20_000, now);
                id += 1;
            }
            now += Duration::from_millis(83);
        }
        link.send(6_000, now) - now
    }

    #[test]
    fn refreshes_wait_for_an_idle_link() {
        // Back to back, refreshes queue (1.9 Mbps into 1 Mbps) and the change waits behind
        // them; one at a time, it waits for one at most.
        assert!(change_after_refreshes(false) > Duration::from_millis(1500));
        assert!(change_after_refreshes(true) < Duration::from_millis(350));
    }

    /// A moving screen (30 fps wanted) whose frames are sized by the ack-driven bitrate
    /// (rate::AckRate), held by acknowledgements, over `link` for `secs`. Returns the
    /// final bitrate and the worst change-to-screen delay over the last 5 seconds.
    fn paced_by_acks(link: &mut SimLink, secs: u64) -> (u32, Duration) {
        let t0 = Instant::now();
        let mut l = Link::default();
        let mut rate = crate::rate::AckRate::new(30_000_000);
        let mut acks: VecDeque<(Instant, u32)> = VecDeque::new();
        let (mut id, mut worst) = (0u32, Duration::ZERO);
        let mut pending: Option<Instant> = None;
        let (mut next_capture, mut next_tick) = (t0, t0);
        let mut now = t0;
        let end = t0 + Duration::from_secs(secs);
        while now < end {
            while acks.front().is_some_and(|&(at, _)| at <= now) {
                let (_, acked) = acks.pop_front().unwrap();
                l.acked(0, acked, now);
            }
            if now >= next_tick {
                let bps = rate.update(l.ack_sample(now), now);
                l.set_pace(bps);
                next_tick += crate::rate::ACK_TICK;
            }
            if now >= next_capture {
                pending.get_or_insert(now);
                next_capture += Duration::from_micros(33_333);
            }
            if let Some(changed) = pending
                && !l.full(now)
            {
                // A keyframe first; then as much as the bitrate allows.
                let bytes = if id == 0 { 400_000 } else { (rate.target() / 8 / 30) as usize };
                // Drawn 5 ms after it arrives, and acknowledged half a round trip later.
                let shown = link.send(bytes, now) + Duration::from_millis(5);
                acks.push_back((shown + link.rtt / 2, id));
                l.sent(0, id, bytes, now);
                if now + Duration::from_secs(5) >= end {
                    worst = worst.max(shown - changed);
                }
                id += 1;
                pending = None;
            }
            now += Duration::from_millis(2);
        }
        (rate.target(), worst)
    }

    #[test]
    fn acks_bring_the_rate_down_to_a_slow_link_and_keep_the_delay_short() {
        // The bench case: 1 Mbps, 150 ms. From 4 Mbps it settles around the link, and what
        // changes is on screen within half a second.
        let mut link = SimLink::new(1_000_000.0, Duration::from_millis(150), Instant::now());
        let (bps, worst) = paced_by_acks(&mut link, 30);
        // (Around the link's rate: it keeps probing gently, with capture holding the queue.)
        assert!((250_000..1_250_000).contains(&bps), "{bps} bps");
        assert!(worst < Duration::from_millis(500), "worst delay {worst:?}");
    }

    #[test]
    fn acks_find_a_fast_link_quickly() {
        // 50 Mbps, 20 ms: slow start climbs well past where it began.
        let mut link = SimLink::new(50_000_000.0, Duration::from_millis(20), Instant::now());
        let (bps, worst) = paced_by_acks(&mut link, 5);
        assert!(bps >= 20_000_000, "{bps} bps");
        assert!(worst < Duration::from_millis(150), "worst delay {worst:?}");
    }

    #[test]
    fn acks_decide_when_frames_are_gone() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut l = Link::default();
        for id in 0..3 {
            l.sent(0, id, 1_000, t0);
        }
        l.sent(1, 0, 1_000, t0);
        // Out of the buffer isn't gone, once the viewer acknowledges.
        l.acked(0, 0, ms(40));
        l.drained(0, ms(50));
        assert_eq!(l.inflight.len(), 3);
        // An ack covers the frames before it on its stream (2 implies 1), not other streams.
        l.acked(0, 2, ms(60));
        assert_eq!(l.inflight.len(), 1);
        assert_eq!(l.rtt_ms(), 40.0);
        // 20 ms more than the quickest: queueing, smoothed. (Set far above their size here,
        // so that takes no time to send.)
        l.set_pace(1_000_000_000);
        l.queue_ms = 6.0;
        l.sent(0, 3, 10, ms(60));
        assert!((l.ack_sample(ms(60)).queue_ms - 6.0).abs() < 1e-9);
        assert!(id_le(u32::MAX, 1) && !id_le(1, u32::MAX), "ids wrap");
    }

    #[test]
    fn a_keyframes_own_length_isnt_a_queue() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut l = Link::default();
        l.set_pace(4_000_000);
        // A small frame sets the baseline: 150 ms.
        l.sent(0, 0, 1_000, t0);
        l.acked(0, 0, t0 + ms(152));
        assert!(l.base.is_some_and(|(b, _)| (b - 152.0).abs() < 1.0), "{:?}", l.base);
        // 400 KB at 4 Mbps is 800 ms just to send: arriving in 950 ms is no queue, and nor
        // is the small frame right behind it.
        let t1 = t0 + ms(1000);
        l.sent(0, 1, 400_000, t1);
        l.sent(0, 2, 1_000, t1 + ms(20));
        assert!(l.ack_sample(t1 + ms(900)).oldest_ms < 100.0);
        l.acked(0, 1, t1 + ms(950));
        l.acked(0, 2, t1 + ms(960));
        assert!(l.queue_ms < 5.0, "{}", l.queue_ms);
        // Taking twice that (a link at half the rate) is.
        let t2 = t1 + ms(1000);
        l.sent(0, 3, 400_000, t2);
        l.acked(0, 3, t2 + ms(1750));
        assert!(l.queue_ms > 100.0, "{}", l.queue_ms);
    }

    #[test]
    fn silence_is_no_evidence_about_the_link() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut l = Link::default();
        l.expect_acks();
        l.sent(0, 0, 100_000, t0);
        l.acked(0, 0, t0 + ms(600));
        l.sent(0, 1, 1_000, t0 + ms(600));
        l.acked(0, 1, t0 + ms(1400));
        let heard = l.ack_sample(t0 + ms(1400));
        assert!(heard.delivered_bps > 0.0);
        // Its last frame is never confirmed (lost, or a decoder sitting on it): no queue
        // and no delivery to judge by, however old the frame gets.
        l.sent(0, 2, 1_000, t0 + ms(1500));
        let later = l.ack_sample(t0 + ms(3000));
        assert_eq!((later.queue_ms, later.delivered_bps), (0.0, 0.0));
        // And it doesn't change the rate.
        let mut rate = crate::rate::AckRate::new(30_000_000);
        for i in 0..50 {
            let at = t0 + ms(3000 + i * 100);
            assert_eq!(rate.update(l.ack_sample(at), at), crate::rate::ACK_START_BPS);
        }
    }

    #[test]
    fn a_pause_says_nothing_about_the_link() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut l = Link::default();
        l.expect_acks();
        let mut rate = crate::rate::AckRate::new(30_000_000);
        for id in 0..3 {
            l.sent(0, id, 1_000, t0);
        }
        // The tab is hidden: frames in flight are never acknowledged.
        l.forget();
        let s = l.ack_sample(t0 + ms(5000));
        assert_eq!((s.queue_ms, s.oldest_ms, s.held), (0.0, 0.0, false));
        assert_eq!(rate.update(s, t0 + ms(5000)), crate::rate::ACK_START_BPS);
    }

    #[test]
    fn a_healthy_link_stays_near_its_capacity_from_a_cold_start() {
        // 8 Mbps, 288 ms: the keyframe (400 KB) at the very start must not read as congestion.
        let mut link = SimLink::new(8_000_000.0, Duration::from_millis(288), Instant::now());
        let (bps, _) = paced_by_acks(&mut link, 20);
        assert!((5_000_000..=9_000_000).contains(&bps), "{bps} bps");
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
    fn a_frame_never_confirmed_doesnt_block_refreshes_for_long() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut l = Link::default();
        l.sent(0, 0, 1_000, t0);
        assert_eq!(l.recent_streams(t0 + ms(999)), 1);
        assert_eq!(l.recent_streams(t0 + ms(1000)), 0);
        assert!(l.busy(), "still tracked, in case it's confirmed");
    }

    #[test]
    fn pausing_video_forgets_what_was_in_flight() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut l = Link::default();
        let mut v = Viewer::new();
        for id in 0..3 {
            l.sent(1, id, 1_000, t0);
        }
        assert!(v.update(&mut l, t0 + ms(10)));
        assert!(held(1));
        // A hidden tab: these will never be confirmed, and it isn't the link's doing.
        l.forget();
        assert!(!l.busy());
        assert!(!v.update(&mut l, t0 + ms(20)));
        assert!(!held(1));
    }

    #[test]
    fn a_stuck_viewer_stops_holding_the_others() {
        let t0 = Instant::now();
        let mut l = Link::default();
        let mut v = Viewer::new();
        l.sent(2, 0, 1_000, t0);
        assert!(v.update(&mut l, t0 + Duration::from_millis(500)));
        assert!(held(2), "full: its stream is held");
        assert!(!idle(2) && idle(3));
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
        assert!(idle(2));
    }
}
