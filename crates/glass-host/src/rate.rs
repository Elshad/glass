//! Adaptive bitrate, AIMD like TCP: back off hard when any client sees loss or a latency
//! climb, then creep back up while the network is clean — but only while the stream
//! actually uses what it has (a still screen proves nothing about the link), and only
//! cautiously near the rate where the link last choked.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Fraction kept on congestion.
const DECREASE: f64 = 0.7;
/// After a cautious start: each clean report, until the link shows congestion.
const RAMP: f64 = 1.5;
/// Increase per clean report, as a fraction of the maximum.
const INCREASE: f64 = 0.1;
/// Near (and past) the rate where the link last choked, probe in steps this size instead.
const PROBE: f64 = 0.02;
/// "Near" the last choke point: this fraction of it and above.
const NEAR_CEILING: f64 = 0.85;
/// Raise only when the stream used at least this share of its allowance last second.
const USED_FOR_INCREASE: f64 = 0.4;
/// The remembered choke point is forgotten after this long without congestion.
const CEILING_MEMORY: Duration = Duration::from_secs(60);
/// Don't increase this soon after a decrease (let queues drain).
const HOLD_AFTER_DECREASE: Duration = Duration::from_secs(2);
/// Latency this far above the recent best counts as congestion (queues building up).
const LATENCY_SLACK_MS: f64 = 40.0;
/// Recent latency samples used to estimate the uncongested baseline.
const BASELINE_WINDOW: usize = 30;
/// Below this the picture becomes unusable; hold here instead. (These reports can't tell a
/// queue on the way from a bad second, so they stop well short of what a viewer that
/// acknowledges frames can go down to: see `AckRate`.)
const MIN_FRACTION: f64 = 0.08;
/// The short-window limit: a frame interval's worth of the target, this many intervals...
const SHORT_WINDOW_FRAMES: f64 = 2.0;
/// ...with this much slack (so a frame may be about three average frames, no more).
const SHORT_WINDOW_SLACK: f64 = 1.5;

#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct Report {
    /// Frames the client couldn't decode in the last second (lost or incomplete).
    pub dropped: u32,
    /// Frames decoded in the last second.
    pub frames: u32,
    /// Average capture-to-display latency in the last second, if measured.
    pub latency_ms: Option<f64>,
    /// Video bits received in the last second (older clients don't send it).
    #[serde(default)]
    pub received_bps: Option<f64>,
}

pub struct RateControl {
    max: u32,
    target: u32,
    last_decrease: Option<Instant>,
    latencies: VecDeque<f64>,
    /// The target when congestion was last seen: where the link chokes.
    ceiling: Option<u32>,
    /// Started low for an unknown link: climbing fast until it first shows congestion.
    ramping: bool,
}

impl RateControl {
    pub fn new(max: u32) -> Self {
        Self {
            max,
            target: max,
            last_decrease: None,
            latencies: VecDeque::new(),
            ceiling: None,
            ramping: false,
        }
    }

    pub fn target(&self) -> u32 {
        self.target
    }

    pub fn max(&self) -> u32 {
        self.max
    }

    /// Starts low for a link of unknown size (a new viewer over the internet): the first
    /// keyframe is sized for it, then the rate climbs as the link proves it can take more.
    pub fn start_cautiously(&mut self, bps: u32, now: Instant) -> Option<u32> {
        if bps >= self.target {
            return None;
        }
        self.target = bps;
        self.ceiling = None;
        self.last_decrease = Some(now);
        self.ramping = true;
        Some(bps)
    }

    /// Feeds one client report; returns the new target if it changed meaningfully.
    pub fn report(&mut self, r: Report, now: Instant) -> Option<u32> {
        let baseline = self.latencies.iter().copied().fold(f64::INFINITY, f64::min);
        if let Some(l) = r.latency_ms.filter(|l| l.is_finite() && *l > 0.0) {
            self.latencies.push_back(l);
            if self.latencies.len() > BASELINE_WINDOW {
                self.latencies.pop_front();
            }
        }
        let latency_climbing = r
            .latency_ms
            .is_some_and(|l| baseline.is_finite() && l > baseline + LATENCY_SLACK_MS);
        let congested = r.dropped > 0 || latency_climbing;
        if self
            .last_decrease
            .is_some_and(|t| now.duration_since(t) > CEILING_MEMORY)
        {
            self.ceiling = None;
        }
        // Clients that don't report usage are taken to use it all (the old behaviour).
        let used = r
            .received_bps
            .is_none_or(|bps| bps >= self.target as f64 * USED_FOR_INCREASE);

        let min = (self.max as f64 * MIN_FRACTION) as u32;
        let next = if self.ramping && !congested && r.frames > 0 && used {
            ((self.target as f64 * RAMP) as u32).min(self.max)
        } else if self.ramping && !congested {
            self.target
        } else if congested {
            self.ramping = false;
            self.last_decrease = Some(now);
            self.ceiling = Some(self.target);
            ((self.target as f64 * DECREASE) as u32).max(min)
        } else if self
            .last_decrease
            .is_none_or(|t| now.duration_since(t) >= HOLD_AFTER_DECREASE)
            && r.frames > 0
            && used
        {
            let near = self.ceiling.map(|c| (c as f64 * NEAR_CEILING) as u32);
            match near {
                // Probe gently once close to where it choked.
                Some(near) if self.target >= near => self.target + (self.max as f64 * PROBE) as u32,
                // Recover quickly, but stop short of the choke point.
                Some(near) => (self.target + (self.max as f64 * INCREASE) as u32).min(near),
                None => self.target + (self.max as f64 * INCREASE) as u32,
            }
            .min(self.max)
        } else {
            self.target
        };
        // Back off on any congestion; ignore tiny increases so encoders aren't poked
        // every second near the cap.
        let changed = next < self.target
            || next - self.target >= (self.max as f64 * PROBE) as u32
            || (next == self.max && next != self.target);
        if changed {
            self.target = next;
            Some(next)
        } else {
            None
        }
    }
}

/// What a viewer's frame acknowledgements say about its link, every [`ACK_TICK`].
#[derive(Debug, Clone, Copy)]
pub struct AckSample {
    /// How much longer than the quickest lately frames take to be acknowledged: queueing
    /// somewhere on the way (network buffers, a relay, a slow decoder).
    pub queue_ms: f64,
    /// Age of the oldest frame not yet acknowledged (0: none).
    pub oldest_ms: f64,
    /// The quickest a frame has been acknowledged lately.
    pub rtt_ms: f64,
    /// Bits a second acknowledged (smoothed); 0 when none have been heard lately.
    pub delivered_bps: f64,
    /// The link is full: capture is waiting for it (crate::flow).
    pub held: bool,
    /// How long the oldest frame has been out, beyond what its own size takes to send.
    pub waited_ms: f64,
    /// Bits sent and not yet acknowledged.
    pub in_flight_bits: f64,
}

/// How often [`AckRate`] looks.
pub const ACK_TICK: Duration = Duration::from_millis(100);

/// A viewer's bitrate from its frame acknowledgements, ten times a second, instead of the
/// once-a-second reports [`RateControl`] gets (which cut to 70% a second after a queue
/// had already built). Frames queueing: back off to 85% of what was delivered. At the
/// start: ramp ×1.5 every 250 ms until the first sign of a queue, like TCP's slow start.
/// After: recover quickly to just under where the link last choked, then probe past it
/// gently. Only raised while the stream uses what it has (a still screen proves nothing).
///
/// Adapted from voxl (MIT, github.com/tembo/voxl): `RateCtl` in its video.rs.
pub struct AckRate {
    target: f64,
    max: f64,
    last_decrease: Option<Instant>,
    last_increase: Option<Instant>,
    slow_start: bool,
    /// What was delivered when the link last choked, and when.
    ceiling: Option<(f64, Instant)>,
    /// When acknowledgements first said something.
    first_heard: Option<Instant>,
}

/// How long after the first acknowledgement delay doesn't count against the link.
const ACK_GRACE: Duration = Duration::from_secs(3);
/// Where a viewer's rate starts before its link is known.
pub const ACK_START_BPS: u32 = 4_000_000;
/// The least it goes to: still a picture, if a slow one. An absolute floor, not a share of
/// the maximum: 8% of a 30 Mbps maximum is more than a 1 Mbps link carries.
const MIN_BPS: u32 = 250_000;

impl AckRate {
    pub fn new(max: u32) -> Self {
        Self {
            target: ACK_START_BPS.min(max) as f64,
            max: max as f64,
            last_decrease: None,
            last_increase: None,
            slow_start: true,
            ceiling: None,
            first_heard: None,
        }
    }

    #[cfg(test)]
    pub fn target(&self) -> u32 {
        self.target as u32
    }

    /// Every [`ACK_TICK`]; returns the target.
    pub fn update(&mut self, s: AckSample, now: Instant) -> u32 {
        let since = |t: Option<Instant>| t.map_or(Duration::MAX, |t| now.saturating_duration_since(t));
        // Nothing heard lately: no evidence of anything, so no change. (Frames unconfirmed
        // while a tab is hidden, a last frame lost, a decoder sitting on one.)
        if s.delivered_bps <= 0.0 {
            return self.target as u32;
        }
        // The first frames (keyframes, and a capture restart's) go out through QUIC's own slow
        // start, which stretches them by a few round trips whatever the link: not a queue.
        let heard_since = *self.first_heard.get_or_insert(now);
        let settled = now.saturating_duration_since(heard_since) >= ACK_GRACE;
        // Delay swings more on a long path (a cell's is hundreds of ms): what counts as a
        // queue scales with it.
        // Capture being held also keeps the delay down, so the queue alone can't show a target
        // above what the link carries. That shows as capture waiting with a round trip's worth
        // of the target out, the oldest of it out for longer than its size and a round trip
        // explain, and less than the target getting through. (Not a keyframe on a link that
        // can take it, or a few small frames on a stream with little to send.)
        let overrun = s.held
            && s.waited_ms > s.rtt_ms
            && s.in_flight_bits >= self.target * s.rtt_ms / 1000.0 * 0.5
            && s.delivered_bps < self.target * 0.9;
        let congested = overrun || s.queue_ms > 30f64.max(s.rtt_ms / 4.0) || s.oldest_ms > s.rtt_ms + 80.0;
        let clear = s.queue_ms <= 15f64.max(s.rtt_ms / 8.0);
        // Raising only means something when the stream presses against the target.
        let used = s.delivered_bps >= self.target * 0.5;
        // Nor does cutting, unless frames are really waiting: a few small ones on a still
        // screen, delayed by the path's own jitter, aren't overrunning anything, and a lower
        // target wouldn't help them. When they do wait, what got through is what it carries.
        //
        // Nor when the encoder is already well over its target (it can't make frames that
        // small: see `vt_at_low_bitrates`): a lower target wouldn't lower what's sent.
        let obeyed = s.delivered_bps <= self.target * 1.1;
        if congested && settled && (used || s.held) {
            if obeyed && since(self.last_decrease) >= Duration::from_millis(300) {
                self.ceiling = Some((s.delivered_bps, now));
                let mut next = self.target * 0.85;
                // What got through is the link's rate only when the stream was pressing against
                // the target; one big frame among few would pin it at a trickle.
                if used {
                    self.ceiling = Some((s.delivered_bps, now));
                    next = next.min(s.delivered_bps * 0.85);
                    self.slow_start = false;
                }
                self.target = next.max(MIN_BPS as f64);
                self.last_decrease = Some(now);
            }
        } else if self.slow_start && !congested && clear {
            if used && since(self.last_increase) >= Duration::from_millis(250) {
                self.target = (self.target * 1.5).min(self.max);
                self.last_increase = Some(now);
            }
        } else if !congested && clear && used && since(self.last_decrease) >= Duration::from_secs(1) {
            // Paths change: forget a choke point not seen lately.
            if self.ceiling.is_some_and(|(_, at)| now.saturating_duration_since(at) >= Duration::from_secs(15)) {
                self.ceiling = None;
            }
            let near = self.ceiling.map_or(f64::INFINITY, |(c, _)| c * 0.9);
            if self.target < near && since(self.last_increase) >= Duration::from_millis(200) {
                self.target = (self.target * 1.06 + 100_000.0).min(near).min(self.max);
                self.last_increase = Some(now);
            } else if self.target >= near && since(self.last_increase) >= Duration::from_secs(1) {
                self.target = (self.target * 1.02).min(self.max);
                self.last_increase = Some(now);
            }
        }
        self.target as u32
    }
}

/// Shares a total bitrate between displays by how much each is changing (bytes it
/// produced recently): a still display needs a trickle, the one with motion needs most.
/// Floors keep a still display refreshable and a busy one usable.
pub fn split(total: u32, activity: &[u64]) -> Vec<u32> {
    const STILL_FLOOR: u32 = 300_000;
    const BUSY_FLOOR: u32 = 1_000_000;
    let n = activity.len();
    if n <= 1 {
        return vec![total; n];
    }
    let sum: u64 = activity.iter().sum();
    if sum == 0 {
        return vec![total / n as u32; n];
    }
    let busy = |a: u64| a * 20 >= sum; // at least 5% of the recent bytes
    let floors: Vec<u32> = activity
        .iter()
        .map(|&a| if busy(a) { BUSY_FLOOR } else { STILL_FLOOR })
        .collect();
    let floor_sum: u32 = floors.iter().sum();
    if floor_sum >= total {
        return floors
            .iter()
            .map(|&f| (f as u64 * total as u64 / floor_sum as u64) as u32)
            .collect();
    }
    let spare = (total - floor_sum) as u64;
    activity
        .iter()
        .zip(&floors)
        .map(|(&a, &f)| f + (spare * a / sum) as u32)
        .collect()
}

/// The most an encoder may spend over a short window, as (bytes, seconds): about two frames
/// at the target, with some slack, for encoders with a rate buffer to size (Media
/// Foundation, VA-API; VideoToolbox ignores such limits). Without it, one frame could take
/// most of a second's bits; on a slow link that one frame is a queue everything after it
/// waits behind.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn short_window(bps: u32, fps: u32) -> (f64, f64) {
    let secs = SHORT_WINDOW_FRAMES / fps.max(1) as f64;
    (bps as f64 / 8.0 * secs * SHORT_WINDOW_SLACK, secs)
}

/// How much a still screen may be refined after motion stops, at `bps` and `fps`: how
/// many re-encodes, and how many bytes they may take in all (each about two frames' worth
/// on average). On a slow link a burst of 36 full re-encodes queued ahead of the next real
/// change; a few, small enough, sharpen it without that.
// Only the Mac's capture refines a still screen.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn refresh_budget(bps: u32, fps: u32) -> (u32, u64) {
    let count = (bps / 1_000_000).clamp(3, 36);
    let frame = bps as u64 / 8 / fps.max(1) as u64;
    (count, frame * 2 * count as u64)
}

/// What a display's encoder does at a given bitrate to keep text sharp: a frame-rate
/// ceiling, and a cap on how coarse a frame may get (QP; lower is sharper). When bits run
/// short it sends fewer, sharp frames rather than many blurry ones; a still screen keeps
/// refining after motion stops. Measured on 2560×1440 text with VideoToolbox.
///
/// `max_fps`: what the display and its viewers can show (60, or up to 120 for a fast
/// screen; see crate::refresh).
pub fn sharp_policy(bps: u32, max_fps: u32) -> (u32, u32) {
    policy(bps, crate::sharp::active(), max_fps)
}

/// [`sharp_policy`] for a bitrate, with or without sharp text (a viewer reading code or
/// documents): then frames come at most half as often, each allowed to be finer (QP 4
/// lower), so the same bits buy detail instead of motion.
pub fn policy(bps: u32, text: bool, max_fps: u32) -> (u32, u32) {
    let (fps, qp) = motion_policy(bps, max_fps);
    if text {
        ((fps / 2).clamp(5, 30), qp.saturating_sub(4).max(20))
    } else {
        (fps, qp)
    }
}

/// A fast screen (over 60 Hz) gets its extra frames once the link carries a full 60 fps
/// comfortably: the adaptive rate settles near 10 Mbps for ordinary desktop work, and
/// frames of a moving picture are mostly small differences.
fn motion_policy(bps: u32, max_fps: u32) -> (u32, u32) {
    let (fps, qp) = match bps {
        b if b >= 10_000_000 && max_fps > 90 => (120, 28),
        b if b >= 8_000_000 && max_fps > 60 => (90, 30),
        b if b >= 12_000_000 => (60, 28),
        b if b >= 8_000_000 => (60, 30),
        b if b >= 5_000_000 => (45, 30),
        b if b >= 3_000_000 => (30, 30),
        // Under 3 Mbps a tight cap only forced frames bigger than the link (or skipped
        // ones, with the short-window limit): let them coarsen, and sharpen once still.
        b if b >= 1_500_000 => (15, 36),
        _ => (10, 42),
    };
    (fps.min(max_fps.max(1)), qp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cautious_start_climbs_fast_until_congestion() {
        let now = Instant::now();
        let mut rc = RateControl::new(30_000_000);
        assert_eq!(rc.start_cautiously(2_000_000, now), Some(2_000_000));
        let clean = || Report { dropped: 0, frames: 30, latency_ms: Some(80.0), received_bps: None };
        let mut t = now;
        for want in [3_000_000, 4_500_000, 6_750_000, 10_125_000] {
            t += Duration::from_secs(1);
            assert_eq!(rc.report(clean(), t), Some(want));
        }
        // Not using what it has: no reason to raise it (a still screen, or a link that only
        // looks clean because little was tried).
        t += Duration::from_secs(1);
        assert_eq!(rc.report(Report { received_bps: Some(100_000.0), ..clean() }, t), None);
        // A cut ends it: from here on it probes as usual.
        t += Duration::from_secs(1);
        rc.report(Report { dropped: 3, ..clean() }, t);
        assert_eq!(rc.target(), 7_087_500);
        t += Duration::from_secs(1);
        assert_eq!(rc.report(clean(), t), None, "holds after a decrease");
    }

    const MAX: u32 = 30_000_000;
    fn clean() -> Report {
        Report {
            dropped: 0,
            frames: 60,
            latency_ms: Some(20.0),
            received_bps: None,
        }
    }
    fn lossy() -> Report {
        Report {
            dropped: 3,
            ..clean()
        }
    }

    #[test]
    fn text_policy_trades_frames_for_detail() {
        for bps in [500_000, 2_000_000, 6_000_000, 20_000_000] {
            let (fps, qp) = policy(bps, false, 60);
            let (tfps, tqp) = policy(bps, true, 60);
            assert!(tfps < fps && tfps >= 5, "{bps}: {tfps} vs {fps}");
            assert!(tqp < qp && tqp >= 20, "{bps}: {tqp} vs {qp}");
        }
        assert_eq!(policy(20_000_000, true, 60), (30, 24));
    }

    #[test]
    fn backs_off_on_loss_and_recovers() {
        let mut rc = RateControl::new(MAX);
        let t0 = Instant::now();
        assert_eq!(rc.report(clean(), t0), None, "already at max");
        assert_eq!(rc.report(lossy(), t0), Some(21_000_000));
        // Held right after a decrease.
        assert_eq!(rc.report(clean(), t0 + Duration::from_secs(1)), None);
        assert_eq!(
            rc.report(clean(), t0 + Duration::from_secs(2)),
            Some(24_000_000)
        );
        for i in 3..40 {
            rc.report(clean(), t0 + Duration::from_secs(i));
        }
        assert_eq!(rc.target(), MAX);
    }

    #[test]
    fn approaches_the_last_choke_point_cautiously() {
        let mut rc = RateControl::new(MAX);
        let t0 = Instant::now();
        rc.report(lossy(), t0); // chokes at 30 → 21 Mbps
        let targets: Vec<u32> = (2..6)
            .map(|i| {
                rc.report(clean(), t0 + Duration::from_secs(i));
                rc.target()
            })
            .collect();
        // Quickly to 85% of 30 (25.5), then +0.6 Mbps steps.
        assert_eq!(targets, [24_000_000, 25_500_000, 26_100_000, 26_700_000]);
    }

    #[test]
    fn a_still_screen_does_not_raise_the_rate() {
        let mut rc = RateControl::new(MAX);
        let t0 = Instant::now();
        for i in 0..6 {
            rc.report(lossy(), t0 + Duration::from_secs(i));
        }
        let low = rc.target();
        // A calm screen sends little: no loss, but no evidence the link can take more.
        let idle = Report {
            received_bps: Some(100_000.0),
            ..clean()
        };
        for i in 10..40 {
            rc.report(idle, t0 + Duration::from_secs(i));
        }
        assert_eq!(rc.target(), low);
        // Using it is what earns more.
        let busy = Report {
            received_bps: Some(low as f64 * 0.9),
            ..clean()
        };
        rc.report(busy, t0 + Duration::from_secs(41));
        assert!(rc.target() > low);
    }

    #[test]
    fn a_cautious_start_never_raises_the_target() {
        let mut rc = RateControl::new(MAX);
        let t0 = Instant::now();
        assert_eq!(rc.start_cautiously(6_000_000, t0), Some(6_000_000));
        assert_eq!(rc.start_cautiously(9_000_000, t0), None, "never raises");
        rc.report(clean(), t0 + Duration::from_secs(2));
        assert_eq!(rc.target(), 9_000_000, "climbs from there");
    }

    #[test]
    fn rising_latency_counts_as_congestion() {
        let mut rc = RateControl::new(MAX);
        let t0 = Instant::now();
        for i in 0..5 {
            rc.report(clean(), t0 + Duration::from_secs(i));
        }
        let slow = Report {
            latency_ms: Some(90.0),
            ..clean()
        };
        assert_eq!(
            rc.report(slow, t0 + Duration::from_secs(6)),
            Some(21_000_000)
        );
    }

    #[test]
    fn never_drops_below_the_floor() {
        let mut rc = RateControl::new(MAX);
        let t0 = Instant::now();
        for _ in 0..50 {
            rc.report(lossy(), t0);
        }
        assert_eq!(rc.target(), (MAX as f64 * MIN_FRACTION) as u32);
    }

    /// A rate past its first seconds, as of `t0`.
    fn settled(t0: Instant) -> AckRate {
        let mut r = AckRate::new(MAX);
        r.update(sample(0.0, 1.0), t0 - ACK_GRACE);
        r
    }

    fn sample(queue_ms: f64, delivered_bps: f64) -> AckSample {
        AckSample { queue_ms, oldest_ms: 0.0, rtt_ms: 150.0, delivered_bps, held: false, waited_ms: 0.0, in_flight_bits: 0.0 }
    }

    #[test]
    fn acks_ramp_up_fast_at_the_start_and_cut_to_what_was_delivered() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut r = settled(t0);
        assert_eq!(r.target(), ACK_START_BPS);
        // Busy and clear: ×1.5 every 250 ms, not more often.
        assert_eq!(r.update(sample(0.0, 3_000_000.0), ms(0)), 6_000_000);
        assert_eq!(r.update(sample(0.0, 5_000_000.0), ms(100)), 6_000_000);
        assert_eq!(r.update(sample(0.0, 5_000_000.0), ms(250)), 9_000_000);
        // A still screen (little delivered) doesn't raise it.
        assert_eq!(r.update(sample(0.0, 100_000.0), ms(600)), 9_000_000);
        // Queueing: 85% of what got through.
        assert_eq!(r.update(sample(80.0, 5_000_000.0), ms(700)), 4_250_000);
        // Not again within 300 ms; then again.
        assert_eq!(r.update(sample(80.0, 4_000_000.0), ms(800)), 4_250_000);
        assert_eq!(r.update(sample(80.0, 4_000_000.0), ms(1000)), 3_400_000);
        // No more slow start: near the choke point (4 Mbps), +6% steps up to 90% of it,
        // a second after the last cut...
        assert_eq!(r.update(sample(0.0, 3_000_000.0), ms(1500)), 3_400_000);
        assert_eq!(r.update(sample(0.0, 3_000_000.0), ms(2000)), 3_600_000);
        assert_eq!(r.update(sample(0.0, 3_000_000.0), ms(2200)), 3_600_000, "capped at 90%");
        // ...then 2% a second past it.
        assert_eq!(r.update(sample(0.0, 3_000_000.0), ms(3200)), 3_672_000);
    }

    #[test]
    fn jitter_on_a_quiet_stream_is_not_congestion() {
        let t0 = Instant::now();
        let mut r = settled(t0);
        // 200 ms of "queue" on a still screen sending 100 kbps: no cut, however long.
        for i in 0..100 {
            let at = t0 + Duration::from_millis(i * 100);
            assert_eq!(r.update(sample(200.0, 100_000.0), at), ACK_START_BPS);
        }
        // The same delay with capture waiting for the link: that's the link's rate.
        let held = AckSample { held: true, ..sample(200.0, 800_000.0) };
        assert_eq!(r.update(held, t0 + Duration::from_secs(11)), 3_400_000);
    }

    #[test]
    fn a_target_the_encoder_is_already_over_isnt_cut_further() {
        let t0 = Instant::now();
        let mut r = settled(t0);
        // 5 Mbps coming out against a target of 4: queueing, but cutting changes nothing.
        for i in 0..50 {
            let at = t0 + Duration::from_millis(i * 100);
            assert_eq!(r.update(sample(200.0, 5_000_000.0), at), ACK_START_BPS);
        }
    }

    #[test]
    fn capture_waiting_with_the_target_out_is_an_overrun() {
        let t0 = Instant::now();
        let stuffed = AckSample { held: true, waited_ms: 200.0, in_flight_bits: 400_000.0, ..sample(0.0, 2_000_000.0) };
        // 4 Mbps, 150 ms: half a round trip's worth of the target is out, and 2 Mbps gets through.
        assert_eq!(settled(t0).update(stuffed, t0), 1_700_000);
        // A few small frames with capture momentarily waiting: not the same thing.
        let few = AckSample { in_flight_bits: 100_000.0, ..stuffed };
        assert_eq!(settled(t0).update(few, t0), 6_000_000);
    }

    #[test]
    fn an_old_unacknowledged_frame_is_congestion() {
        let t0 = Instant::now();
        let stuck = AckSample { oldest_ms: 400.0, ..sample(0.0, 3_000_000.0) };
        assert!(settled(t0).update(stuck, t0) < ACK_START_BPS);
        // Not in the first seconds: that's QUIC's slow start.
        assert_eq!(AckRate::new(MAX).update(stuck, t0), ACK_START_BPS);
    }

    #[test]
    fn a_slow_link_gets_a_short_refresh_burst() {
        // The Tesla case: 1 Mbps at 10 fps: three refreshes, 75 KB in all.
        assert_eq!(refresh_budget(1_000_000, 10), (3, 75_000));
        // A LAN: as many as before (3 s of them), each up to two frames' worth.
        assert_eq!(refresh_budget(50_000_000, 60).0, 36);
        assert_eq!(refresh_budget(12_000_000, 60), (12, 12 * 2 * 25_000));
    }

    #[test]
    fn no_frame_is_much_more_than_its_share() {
        // 1 Mbps at 10 fps: 12.5 KB a frame; 37.5 KB over two frame intervals at most.
        let (bytes, secs) = short_window(1_000_000, 10);
        assert!((bytes - 37_500.0).abs() < 1e-6 && (secs - 0.2).abs() < 1e-9, "{bytes} {secs}");
        let (bytes, secs) = short_window(20_000_000, 60);
        assert!((bytes / (20_000_000.0 / 8.0 / 60.0) - 3.0).abs() < 1e-9, "three frames' worth");
        assert!((secs - 2.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn split_conserves_the_total_and_favours_motion() {
        let parts = split(20_000_000, &[900_000, 10_000]);
        assert!(parts.iter().sum::<u32>() <= 20_000_000);
        assert!(parts.iter().sum::<u32>() >= 19_999_000);
        assert!(parts[0] as f64 >= 0.8 * 20_000_000.0, "{parts:?}");
        assert!(parts[1] >= 300_000, "still display keeps its floor");
        assert_eq!(split(5_000_000, &[0, 0]), vec![2_500_000, 2_500_000]);
        assert_eq!(split(7, &[1]), vec![7]);
        // Too little to go round: shares in proportion to the floors.
        let tight = split(1_000_000, &[100, 100, 100]);
        assert!(tight.iter().sum::<u32>() <= 1_000_000);
    }

    #[test]
    fn a_fast_screen_gets_more_frames_once_the_link_carries_60() {
        assert_eq!(policy(30_000_000, false, 60).0, 60, "60 Hz viewers stay at 60");
        assert_eq!(policy(30_000_000, false, 120).0, 120);
        assert_eq!(policy(10_000_000, false, 120).0, 120);
        assert_eq!(policy(9_000_000, false, 120).0, 90);
        assert_eq!(policy(6_000_000, false, 120).0, 45, "a slow link: as for anyone");
        assert_eq!(policy(30_000_000, false, 90).0, 90, "a 90 Hz screen gets 90");
        assert_eq!(policy(30_000_000, false, 30).0, 30, "a slow display caps it");
        assert_eq!(policy(30_000_000, true, 120).0, 30, "sharp text still trades frames for detail");
    }

    #[test]
    fn less_bitrate_means_fewer_frames_and_coarser_only_when_slow() {
        let mut last_fps = u32::MAX;
        for bps in [
            20_000_000, 10_000_000, 6_000_000, 4_000_000, 3_000_000, 2_000_000, 1_000_000,
        ] {
            let (fps, qp) = policy(bps, false, 60);
            assert!(fps <= last_fps);
            if bps >= 3_000_000 {
                assert!(qp <= 30, "{bps}: the QP cap holds at 30 from 3 Mbps up");
            }
            last_fps = fps;
        }
        // Under 3 Mbps frames may coarsen so they fit the link.
        assert!(policy(2_000_000, false, 60).1 > 30);
        assert!(policy(1_000_000, false, 60).1 > policy(2_000_000, false, 60).1);
    }
}
