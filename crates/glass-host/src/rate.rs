//! Adaptive bitrate, AIMD like TCP: back off hard when any client sees loss or a latency
//! climb, then creep back up while the network is clean — but only while the stream
//! actually uses what it has (a still screen proves nothing about the link), and only
//! cautiously near the rate where the link last choked.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Fraction kept on congestion.
const DECREASE: f64 = 0.7;
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
/// The least the rate goes to: still a picture, if a slow one. An absolute floor, not a
/// share of the maximum: 8% of a 30 Mbps maximum (2.4 Mbps) was more than a 1 Mbps link
/// carries, so the queue grew without end and nothing decoded.
pub const MIN_BPS: u32 = 250_000;
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
}

impl RateControl {
    pub fn new(max: u32) -> Self {
        Self {
            max,
            target: max,
            last_decrease: None,
            latencies: VecDeque::new(),
            ceiling: None,
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
        self.ceiling = Some(bps);
        self.last_decrease = Some(now);
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

        let min = MIN_BPS.min(self.max);
        let next = if congested {
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
/// at the target, with some slack. Without it, a limit over a whole second let one frame
/// take most of the second's bits; on a slow link that one frame is a queue everything
/// after it waits behind. Encoders meet it by coarsening the frame (see [`motion_policy`]:
/// the QP cap is loose at low rates so they can).
pub fn short_window(bps: u32, fps: u32) -> (f64, f64) {
    let secs = SHORT_WINDOW_FRAMES / fps.max(1) as f64;
    (bps as f64 / 8.0 * secs * SHORT_WINDOW_SLACK, secs)
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
    fn a_cautious_start_climbs_gently() {
        let mut rc = RateControl::new(MAX);
        let t0 = Instant::now();
        assert_eq!(rc.start_cautiously(6_000_000, t0), Some(6_000_000));
        assert_eq!(rc.start_cautiously(9_000_000, t0), None, "never raises");
        rc.report(clean(), t0 + Duration::from_secs(2));
        assert_eq!(rc.target(), 6_600_000, "probes up from there");
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
        assert_eq!(rc.target(), MIN_BPS);
    }

    #[test]
    fn a_slow_link_can_take_the_rate_down_to_a_quarter_megabit() {
        // WebTransport over a 1 Mbps link with a 30 Mbps maximum: congestion every second
        // brings it under the link within a dozen seconds, and it can go lower still.
        let mut rc = RateControl::new(MAX);
        let t0 = Instant::now();
        let under = (0..20u64)
            .position(|i| {
                rc.report(lossy(), t0 + Duration::from_secs(i));
                rc.target() < 1_000_000
            })
            .expect("gets under the link");
        assert!(under < 12, "{under} s");
        for i in 20..40 {
            rc.report(lossy(), t0 + Duration::from_secs(i));
        }
        assert_eq!(rc.target(), 250_000);
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
