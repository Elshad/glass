//! System audio as Opus, for viewers that can decode it (WebCodecs `AudioDecoder`).
//!
//! Every platform's audio source sends the same 5 ms s16 PCM packets (`KIND_AUDIO`) into
//! one broadcast channel. One encoder here turns them into 10 ms Opus packets
//! (`KIND_AUDIO_OPUS`) on the same channel, so it runs once for the whole host, not once per
//! viewer; each session forwards whichever kind its viewer asked for in hello. Raw PCM is
//! ~1.5 Mbps; on a relayed WebRTC link of about 1 Mbps (a car's browser) that starves video.
//!
//! Choices (CELT-only "restricted low delay", 10 ms frames, 96 kbps stereo):
//! - Frame size: each frame is buffered whole before it's encoded, so it adds its length to
//!   the delay. 10 ms keeps the added delay to ~12.5 ms (with CELT's 2.5 ms look-ahead), a
//!   lost datagram costs 10 ms of sound rather than 20, and packets arrive steadily enough
//!   for the viewer's 40 ms jitter buffer. 20 ms would save ~30 kbps of per-datagram
//!   overhead (headers, DTLS/SCTP on WebRTC) and code slightly better, but adds 10 ms of
//!   delay to every sound and doubles what each loss costs; for a remote desktop
//!   (clicks, games, calls) the lower latency is worth more.
//! - Bitrate: 96 kbps stereo is near-transparent for music with CELT at 10 ms and leaves
//!   most of a 1 Mbps link for video (~130 kbps on the wire with overhead, against 1.5
//!   Mbps of PCM). 128 kbps buys little audible quality for a third more bandwidth.
//! - Restricted low delay: system audio is mostly music, games and video, which CELT codes
//!   well, and it has the shortest look-ahead (2.5 ms against 6.5 ms for the SILK/hybrid
//!   modes).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use glass_proto::{AudioHeader, KIND_AUDIO, KIND_AUDIO_OPUS, OPUS_FRAMES_PER_PACKET, opus_packet};
use tokio::sync::broadcast;

use crate::media::AudioPackets;

pub const BITRATE: i32 = 96_000;
const CHANNELS: usize = 2;
/// Opus's largest packet; at 96 kbps a 10 ms packet is ~120 bytes.
const MAX_PACKET: usize = 1275;
/// Longer than this without PCM is a gap (nothing playing): a half-filled frame from before
/// it is dropped rather than glued to whatever plays next.
const GAP: Duration = Duration::from_millis(50);

/// Sessions whose viewer takes Opus with audio on. The encoder idles while there are none.
static LISTENERS: AtomicUsize = AtomicUsize::new(0);

/// Starts the encoder: reads PCM packets from `packets` and sends Opus packets back into it.
pub fn start(packets: AudioPackets) {
    let mut rx = packets.subscribe();
    let spawned = std::thread::Builder::new().name("glass-opus".into()).spawn(move || {
        let mut encoder = match Encoder::new() {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("opus encoder: {e}; viewers get PCM audio");
                return;
            }
        };
        loop {
            let pcm = match rx.blocking_recv() {
                Ok(p) => p,
                // Fell behind: what's pending no longer joins up with what comes next.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    encoder.gap();
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if LISTENERS.load(Ordering::Relaxed) == 0 {
                encoder.idle();
                continue;
            }
            encoder.push(&pcm, |p| {
                // No receivers (every session gone) is fine.
                let _ = packets.send(Arc::new(p));
            });
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("opus thread: {e}");
    }
}

/// 5 ms PCM packets in, 10 ms Opus packets out.
pub struct Encoder {
    opus: opus::Encoder,
    /// Interleaved stereo not yet encoded, and the capture time of its first frame.
    pending: Vec<i16>,
    pending_us: u64,
    /// The PCM sequence number expected next, to notice lost or skipped packets.
    next_pcm: Option<u32>,
    last_pcm: Option<Instant>,
    seq: u32,
    /// Nothing was encoded since the last listener left: start afresh.
    idle: bool,
    out: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Result<Self, opus::Error> {
        let mut opus = opus::Encoder::new(
            glass_proto::AUDIO_SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )?;
        opus.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        Ok(Self {
            opus,
            pending: Vec::with_capacity(OPUS_FRAMES_PER_PACKET * CHANNELS * 2),
            pending_us: 0,
            next_pcm: None,
            last_pcm: None,
            seq: 0,
            idle: false,
            out: vec![0; MAX_PACKET],
        })
    }

    /// Drops a partly filled frame.
    fn gap(&mut self) {
        self.pending.clear();
        self.next_pcm = None;
    }

    /// Nobody's listening: stop, and reset the encoder before it next runs.
    fn idle(&mut self) {
        self.gap();
        self.idle = true;
    }

    /// Takes one datagram from the audio channel. PCM (`KIND_AUDIO`) is encoded; anything
    /// else (our own Opus packets coming back) is ignored.
    pub fn push(&mut self, packet: &[u8], mut send: impl FnMut(Vec<u8>)) {
        let Some(h) = AudioHeader::read(packet).filter(|h| h.kind == KIND_AUDIO && h.channels > 0) else { return };
        let samples: Vec<i16> =
            packet[glass_proto::AUDIO_HEADER_LEN..].as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        if self.idle {
            self.idle = false;
            let _ = self.opus.reset_state();
        }
        let now = Instant::now();
        if self.next_pcm != Some(h.seq) || self.last_pcm.is_some_and(|t| now.duration_since(t) > GAP) {
            self.pending.clear();
        }
        self.next_pcm = Some(h.seq.wrapping_add(1));
        self.last_pcm = Some(now);
        if self.pending.is_empty() {
            self.pending_us = h.capture_us;
        }
        // Mono goes to both sides; past two channels, the first two (left, right).
        let channels = h.channels as usize;
        for frame in samples.chunks_exact(channels) {
            let left = frame[0];
            self.pending.extend_from_slice(&[left, *frame.get(1).unwrap_or(&left)]);
        }
        let per_packet = OPUS_FRAMES_PER_PACKET * CHANNELS;
        while self.pending.len() >= per_packet {
            match self.opus.encode(&self.pending[..per_packet], &mut self.out) {
                Ok(n) => {
                    send(opus_packet(self.seq, self.pending_us, CHANNELS as u8, &self.out[..n]));
                    self.seq = self.seq.wrapping_add(1);
                }
                Err(e) => tracing::debug!("opus encode: {e}"),
            }
            self.pending.drain(..per_packet);
            self.pending_us += OPUS_FRAMES_PER_PACKET as u64 * 1_000_000 / glass_proto::AUDIO_SAMPLE_RATE as u64;
        }
    }
}

/// Which system audio a session gets: none, PCM or Opus. Counts it in [`LISTENERS`] while
/// it takes Opus with audio on.
pub struct AudioTap {
    /// (audio on, viewer decodes Opus).
    state: std::sync::Mutex<(bool, bool)>,
    /// The datagram kind to forward (0: none), read for every packet.
    kind: AtomicU8,
    listeners: &'static AtomicUsize,
}

impl Default for AudioTap {
    fn default() -> Self {
        Self::with_counter(&LISTENERS)
    }
}

impl AudioTap {
    fn with_counter(listeners: &'static AtomicUsize) -> Self {
        Self { state: std::sync::Mutex::new((false, false)), kind: AtomicU8::new(0), listeners }
    }

    /// Start or stop sending audio.
    pub fn set_on(&self, on: bool) {
        self.update(|s| s.0 = on);
    }

    /// Whether the viewer decodes Opus (from hello's codecs).
    pub fn set_opus(&self, opus: bool) {
        self.update(|s| s.1 = opus);
    }

    fn update(&self, change: impl FnOnce(&mut (bool, bool))) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = *state;
        change(&mut state);
        match (before.0 && before.1, state.0 && state.1) {
            (false, true) => {
                self.listeners.fetch_add(1, Ordering::Relaxed);
            }
            (true, false) => {
                self.listeners.fetch_sub(1, Ordering::Relaxed);
            }
            _ => {}
        }
        let kind = match *state {
            (false, _) => 0,
            (true, false) => KIND_AUDIO,
            (true, true) => KIND_AUDIO_OPUS,
        };
        self.kind.store(kind, Ordering::Relaxed);
    }

    /// Should this audio datagram go to the session?
    pub fn wants(&self, packet: &[u8]) -> bool {
        let kind = self.kind.load(Ordering::Relaxed);
        kind != 0 && packet.first() == Some(&kind)
    }
}

impl Drop for AudioTap {
    fn drop(&mut self) {
        // The session is gone: it no longer listens.
        self.set_on(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_proto::{AUDIO_FRAMES_PER_PACKET, AUDIO_HEADER_LEN, AUDIO_SAMPLE_RATE, audio_packet};

    /// `packets` 5 ms PCM packets of a 1 kHz tone, numbered from `seq`.
    fn tone(seq: u32, packets: usize, channels: usize) -> Vec<Vec<u8>> {
        (0..packets)
            .map(|p| {
                let pcm: Vec<i16> = (0..AUDIO_FRAMES_PER_PACKET)
                    .flat_map(|f| {
                        let t = (p * AUDIO_FRAMES_PER_PACKET + f) as f64 / AUDIO_SAMPLE_RATE as f64;
                        let v = ((t * 1000.0 * std::f64::consts::TAU).sin() * 12_000.0) as i16;
                        std::iter::repeat_n(v, channels)
                    })
                    .collect();
                audio_packet(seq + p as u32, 5_000 * (seq as u64 + p as u64), channels as u8, &pcm)
            })
            .collect()
    }

    fn encode(enc: &mut Encoder, pcm: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for p in pcm {
            enc.push(p, |o| out.push(o));
        }
        out
    }

    #[test]
    fn encodes_and_decodes_back() {
        let mut enc = Encoder::new().unwrap();
        let pcm = tone(0, 200, 2); // 1 s
        let packets = encode(&mut enc, &pcm);
        assert_eq!(packets.len(), 100, "two 5 ms packets per 10 ms Opus packet");
        let mut bytes = 0;
        for (i, p) in packets.iter().enumerate() {
            let h = AudioHeader::read(p).unwrap();
            assert_eq!((h.kind, h.channels, h.seq), (KIND_AUDIO_OPUS, 2, i as u32));
            // Each takes the capture time of its first PCM packet.
            assert_eq!(h.capture_us, i as u64 * 10_000);
            assert!(p.len() <= 1200, "fits one datagram");
            bytes += p.len() - AUDIO_HEADER_LEN;
        }
        let kbps = bytes * 8 / 1000;
        assert!((40..=140).contains(&kbps), "{kbps} kbps");

        let mut dec = opus::Decoder::new(AUDIO_SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut decoded = Vec::new();
        let mut buf = vec![0i16; OPUS_FRAMES_PER_PACKET * 2];
        for p in &packets {
            let n = dec.decode(&p[AUDIO_HEADER_LEN..], &mut buf, false).unwrap();
            assert_eq!(n, OPUS_FRAMES_PER_PACKET);
            decoded.extend_from_slice(&buf[..n * 2]);
        }
        // The tone comes back: compare with the input, allowing for the codec's delay.
        let input: Vec<i16> = pcm.iter().flat_map(|p| p[AUDIO_HEADER_LEN..].as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b))).collect();
        let left = |v: &[i16]| v.iter().step_by(2).map(|&s| s as f64).collect::<Vec<_>>();
        let (a, b) = (left(&input), left(&decoded));
        let snr = (0..400)
            .map(|delay| {
                let (mut sig, mut err) = (0.0, 0.0);
                for i in 4800..a.len() - 400 {
                    sig += a[i] * a[i];
                    err += (a[i] - b[i + delay]).powi(2);
                }
                10.0 * (sig / err.max(1.0)).log10()
            })
            .fold(f64::MIN, f64::max);
        assert!(snr > 20.0, "SNR {snr:.1} dB");
        // Both channels carry it.
        let right: f64 = decoded.iter().skip(1).step_by(2).map(|&s| (s as f64).abs()).sum();
        assert!(right / (decoded.len() / 2) as f64 > 3000.0);
    }

    #[test]
    fn mono_and_gaps() {
        let mut enc = Encoder::new().unwrap();
        // Mono is encoded as stereo.
        let out = encode(&mut enc, &tone(0, 4, 1));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][1], 2);
        // A lost PCM packet drops the half-filled frame instead of joining across the gap.
        let mut out = encode(&mut enc, &tone(10, 1, 2));
        out.extend(encode(&mut enc, &tone(12, 2, 2)));
        assert_eq!(out.len(), 1);
        assert_eq!(AudioHeader::read(&out[0]).unwrap().capture_us, 12 * 5_000);
        // Its own Opus packets coming back round are ignored.
        let echo = out.clone();
        assert!(encode(&mut enc, &echo).is_empty());
    }

    #[test]
    fn taps_pick_a_kind_and_count_opus_listeners() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let pcm = audio_packet(0, 0, 2, &[0; 4]);
        let opus = opus_packet(0, 0, 2, &[0xFC]);
        let a = AudioTap::with_counter(&COUNT);
        assert!(!a.wants(&pcm) && !a.wants(&opus), "off until turned on");
        a.set_on(true);
        assert!(a.wants(&pcm) && !a.wants(&opus), "PCM by default");
        assert_eq!(COUNT.load(Ordering::Relaxed), 0);
        a.set_opus(true);
        assert!(!a.wants(&pcm) && a.wants(&opus));
        assert_eq!(COUNT.load(Ordering::Relaxed), 1);
        let b = AudioTap::with_counter(&COUNT);
        b.set_opus(true);
        assert_eq!(COUNT.load(Ordering::Relaxed), 1, "Opus but audio off: not listening");
        b.set_on(true);
        b.set_on(true);
        assert_eq!(COUNT.load(Ordering::Relaxed), 2);
        a.set_on(false);
        assert!(!a.wants(&opus));
        assert_eq!(COUNT.load(Ordering::Relaxed), 1);
        drop(b);
        assert_eq!(COUNT.load(Ordering::Relaxed), 0, "a session that ends stops listening");
    }
}
