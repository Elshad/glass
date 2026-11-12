//! Lost fragments, sent again. A keyframe is hundreds of datagrams and needs every one: on a
//! link losing 1% (a phone on cellular), almost none arrive whole. Asking for another
//! keyframe adds an equally large one to the queue, which is what makes such links take
//! seconds to show a first picture. Sending only what was lost costs a few datagrams.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use glass_proto::{FLAG_RETRANSMIT, MAX_STREAMS};

/// Long enough for a keyframe crawling through a slow link to be repaired.
const KEEP_FOR: Duration = Duration::from_secs(3);
/// About a second at 60 Mbps.
const KEEP_BYTES: usize = 8 * 1024 * 1024;
/// A fragment isn't sent again sooner than this after the last time: on a full queue the
/// viewer can't tell late from lost, and resending what's on its way only makes it later.
const RESEND_GAP: Duration = Duration::from_millis(250);
/// What repairs may take, per second: enough for a first keyframe, little enough that a
/// link losing a lot isn't flooded by them. Past it the viewer asks again, or gives up.
const RESEND_BYTES_PER_SEC: usize = 128 * 1024;

struct Sent {
    stream: usize,
    id: u32,
    dgrams: Vec<Vec<u8>>,
    bytes: usize,
    at: Instant,
    /// When each fragment was last sent again.
    resent: Vec<Option<Instant>>,
}

/// One session's recently sent frames.
#[derive(Default)]
pub struct SentFrames {
    frames: VecDeque<Sent>,
    bytes: usize,
    /// Repair budget: when it last filled, and what's left.
    budget_at: Option<Instant>,
    budget: usize,
    /// For the log: fragments sent again, and asked for but not sent.
    pub resent: u64,
    pub refused: u64,
}

impl SentFrames {
    pub fn remember(&mut self, stream: u8, id: u32, dgrams: Vec<Vec<u8>>) {
        self.remember_at(stream, id, dgrams, Instant::now());
    }

    fn remember_at(&mut self, stream: u8, id: u32, dgrams: Vec<Vec<u8>>, at: Instant) {
        let bytes = dgrams.iter().map(Vec::len).sum();
        let resent = vec![None; dgrams.len()];
        self.frames.push_back(Sent { stream: stream as usize, id, dgrams, bytes, at, resent });
        self.bytes += bytes;
        while let Some(old) = self.frames.front() {
            if self.bytes <= KEEP_BYTES && at.duration_since(old.at) <= KEEP_FOR {
                break;
            }
            self.bytes -= old.bytes;
            self.frames.pop_front();
        }
    }

    /// The datagrams to send again (no fragments: all of them), marked as such; none for
    /// now if they went recently or the budget's spent. `None`: not kept any more, or never
    /// sent, so the viewer should stop waiting for that frame.
    pub fn again(&mut self, stream: u8, id: u32, fragments: &[u16]) -> Option<Vec<Vec<u8>>> {
        self.again_at(stream, id, fragments, Instant::now())
    }

    fn again_at(&mut self, stream: u8, id: u32, fragments: &[u16], now: Instant) -> Option<Vec<Vec<u8>>> {
        if stream as usize >= MAX_STREAMS {
            return None;
        }
        if self.budget_at.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
            self.budget_at = Some(now);
            self.budget = RESEND_BYTES_PER_SEC;
        }
        let sent = self.frames.iter_mut().rev().find(|s| s.stream == stream as usize && s.id == id)?;
        let all: Vec<u16>;
        let wanted = if fragments.is_empty() {
            all = (0..sent.dgrams.len() as u16).collect();
            &all
        } else {
            fragments
        };
        let mut out = Vec::new();
        for &f in wanted {
            let f = f as usize;
            let Some(d) = sent.dgrams.get(f) else { continue };
            if sent.resent[f].is_some_and(|t| now.duration_since(t) < RESEND_GAP) {
                continue;
            }
            if d.len() > self.budget {
                self.refused += 1;
                continue;
            }
            self.budget -= d.len();
            self.resent += 1;
            sent.resent[f] = Some(now);
            let mut d = d.clone();
            d[1] |= FLAG_RETRANSMIT;
            out.push(d);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_proto::VideoHeader;

    fn dgrams(id: u32, n: usize) -> Vec<Vec<u8>> {
        glass_proto::fragment(&vec![7u8; n * 1000], 1, id, true, 5, 1020)
    }

    #[test]
    fn sends_again_only_what_was_lost_marked_as_a_retransmit() {
        let mut sent = SentFrames::default();
        sent.remember(1, 9, dgrams(9, 5));
        let again = sent.again(1, 9, &[1, 3, 200]).unwrap();
        assert_eq!(again.len(), 2, "an index past the end is ignored");
        let h = VideoHeader::read(&again[1]).unwrap();
        assert_eq!((h.stream, h.frame_id, h.frag_idx), (1, 9, 3));
        assert_ne!(h.flags & FLAG_RETRANSMIT, 0);
        assert_ne!(h.flags & glass_proto::FLAG_KEYFRAME, 0, "still a keyframe's fragment");
        let later = Instant::now() + RESEND_GAP;
        assert_eq!(sent.again_at(1, 9, &[], later).unwrap().len(), 5, "nothing named: the whole frame");
    }

    #[test]
    fn asking_again_doesnt_flood_the_link() {
        let mut sent = SentFrames::default();
        let t = Instant::now();
        sent.remember_at(0, 1, dgrams(1, 400), t);
        // The same fragment, asked for over and over: sent once per gap.
        assert_eq!(sent.again_at(0, 1, &[5], t).unwrap().len(), 1);
        assert_eq!(sent.again_at(0, 1, &[5], t + Duration::from_millis(40)).unwrap().len(), 0);
        assert_eq!(sent.again_at(0, 1, &[5], t + RESEND_GAP).unwrap().len(), 1);
        // A whole large frame: only as much as the budget.
        let burst = sent.again_at(0, 1, &[], t + RESEND_GAP).unwrap();
        let bytes: usize = burst.iter().map(Vec::len).sum();
        assert!(bytes <= RESEND_BYTES_PER_SEC, "{bytes}");
        assert!(burst.len() > 50);
        assert!(sent.refused > 0);
        assert_eq!(sent.again_at(0, 1, &[300], t + RESEND_GAP).unwrap().len(), 0, "spent");
        assert!(!sent.again_at(0, 1, &[300], t + RESEND_GAP + Duration::from_secs(1)).unwrap().is_empty());
    }

    #[test]
    fn a_frame_never_sent_or_on_another_stream_is_gone() {
        let mut sent = SentFrames::default();
        sent.remember(1, 9, dgrams(9, 1));
        assert!(sent.again(1, 10, &[]).is_none());
        assert!(sent.again(0, 9, &[]).is_none());
        assert!(sent.again(200, 9, &[]).is_none());
    }

    #[test]
    fn keeps_a_bounded_recent_window() {
        let mut sent = SentFrames::default();
        let start = Instant::now();
        sent.remember_at(0, 1, dgrams(1, 1), start);
        sent.remember_at(0, 2, dgrams(2, 1), start + KEEP_FOR);
        assert!(sent.again(0, 1, &[]).is_some(), "just inside the time limit");
        sent.remember_at(0, 3, dgrams(3, 1), start + KEEP_FOR + Duration::from_millis(1));
        assert!(sent.again(0, 1, &[]).is_none(), "too old");
        assert!(sent.again(0, 2, &[]).is_some());
        for i in 4..30 {
            sent.remember_at(0, i, dgrams(i, 400), start + KEEP_FOR + Duration::from_millis(2));
        }
        assert!(sent.bytes <= KEEP_BYTES);
        assert!(sent.again(0, 29, &[]).is_some(), "the newest stays");
        assert!(sent.again(0, 2, &[]).is_none(), "the oldest went to stay under the size");
    }
}
