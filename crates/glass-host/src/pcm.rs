//! System audio from a device's own mix format (Windows' WASAPI hands over whatever the
//! output mixes in: usually 32-bit float, sometimes 44.1 kHz, 16/24-bit or 5.1) → 48 kHz
//! stereo → the 5 ms s16 packets every host sends.

#![cfg_attr(not(any(windows, test)), allow(dead_code))]

use glass_proto::{AUDIO_FRAMES_PER_PACKET, AUDIO_SAMPLE_RATE, audio_packet};

/// How each sample is stored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sample {
    Float32,
    /// Signed little-endian integers this many bytes wide (2, 3 or 4), MSB-aligned.
    Int(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Format {
    pub rate: u32,
    pub channels: usize,
    pub sample: Sample,
    /// Which speaker each channel is, in order (WAVEFORMATEXTENSIBLE's dwChannelMask; 0 =
    /// unknown).
    pub mask: u32,
}

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

impl Format {
    /// From a WAVEFORMATEX's fields. `sub_format` is an extensible format's SubFormat GUID's
    /// first field, which is the plain format tag for PCM and float. `None` for formats we
    /// can't read (compressed ones).
    pub fn from_wave(tag: u16, sub_format: Option<u32>, rate: u32, channels: u16, bits: u16, block_align: u16, mask: u32) -> Option<Self> {
        let tag = if tag == WAVE_FORMAT_EXTENSIBLE { u16::try_from(sub_format?).ok()? } else { tag };
        let channels = channels as usize;
        if channels == 0 || rate == 0 {
            return None;
        }
        let container = block_align as usize / channels;
        let sample = match (tag, bits) {
            (WAVE_FORMAT_IEEE_FLOAT, 32) if container == 4 => Sample::Float32,
            (WAVE_FORMAT_PCM, 16 | 24 | 32) if (2..=4).contains(&container) && container * 8 >= bits as usize => Sample::Int(container),
            _ => return None,
        };
        Some(Self { rate, channels, sample, mask })
    }

    pub fn frame_bytes(&self) -> usize {
        self.channels
            * match self.sample {
                Sample::Float32 => 4,
                Sample::Int(n) => n,
            }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sample = match self.sample {
            Sample::Float32 => "float".to_string(),
            Sample::Int(n) => format!("{}-bit", n * 8),
        };
        write!(f, "{} Hz, {} ch, {sample}", self.rate, self.channels)
    }
}

/// How much of each speaker goes to the left and right, by WAVEFORMATEXTENSIBLE speaker bit.
/// Centre and rear channels go to both sides at -3 dB; the subwoofer is left out.
fn speaker_gains(bit: u32) -> (f32, f32) {
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match bit {
        0x1 => (1.0, 0.0),                                // front left
        0x2 => (0.0, 1.0),                                // front right
        0x4 | 0x100 | 0x800 | 0x2000 | 0x10000 => (H, H), // centres (front, back, top…)
        0x8 => (0.0, 0.0),                                // LFE
        0x10 | 0x40 | 0x200 | 0x1000 | 0x8000 => (H, 0.0), // left surrounds
        0x20 | 0x80 | 0x400 | 0x4000 | 0x20000 => (0.0, H), // right surrounds
        _ => (0.0, 0.0),
    }
}

/// Each channel's (left, right) gain for a downmix to stereo.
fn channel_gains(channels: usize, mask: u32) -> Vec<(f32, f32)> {
    if channels == 1 {
        return vec![(1.0, 1.0)];
    }
    let speakers: Vec<u32> = (0..32).map(|b| 1u32 << b).filter(|b| mask & b != 0).collect();
    if speakers.len() < channels {
        // Unknown layout: the first two are left and right, the rest left out.
        return (0..channels).map(|c| [(1.0, 0.0), (0.0, 1.0)].get(c).copied().unwrap_or((0.0, 0.0))).collect();
    }
    speakers[..channels].iter().map(|&b| speaker_gains(b)).collect()
}

/// Linear interpolation to 48 kHz, carried across buffers.
struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// The last frame of the previous buffer, and how far past it the next output is.
    last: Option<[f32; 2]>,
    pos: f64,
}

impl Resampler {
    fn new(rate: u32) -> Self {
        Self { step: rate as f64 / AUDIO_SAMPLE_RATE as f64, last: None, pos: 0.0 }
    }

    fn run(&mut self, frames: &[[f32; 2]], out: &mut Vec<[f32; 2]>) {
        if self.step == 1.0 {
            out.extend_from_slice(frames);
            return;
        }
        let Some(&end) = frames.last() else { return };
        // Index 0 is the previous buffer's last frame (or this one's first, at the start).
        let at = |i: usize| -> [f32; 2] {
            match self.last {
                Some(l) if i == 0 => l,
                Some(_) => frames[i - 1],
                None => frames[i],
            }
        };
        let len = frames.len() + usize::from(self.last.is_some());
        let mut pos = self.pos;
        // Stops short of the last frame: the next buffer interpolates from it.
        while pos < (len - 1) as f64 {
            let i = pos as usize;
            let t = (pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            pos += self.step;
        }
        self.pos = pos - (len - 1) as f64;
        self.last = Some(end);
    }
}

/// Turns buffers in a device's format into interleaved 48 kHz stereo s16.
pub struct Converter {
    format: Format,
    gains: Vec<(f32, f32)>,
    resampler: Resampler,
    stereo: Vec<[f32; 2]>,
    resampled: Vec<[f32; 2]>,
}

impl Converter {
    pub fn new(format: Format) -> Self {
        Self {
            format,
            gains: channel_gains(format.channels, format.mask),
            resampler: Resampler::new(format.rate),
            stereo: Vec::new(),
            resampled: Vec::new(),
        }
    }

    fn sample(&self, b: &[u8]) -> f32 {
        match self.format.sample {
            Sample::Float32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Sample::Int(n) => {
                // Put the bytes at the top of an i32, so every width scales the same.
                let mut v = [0u8; 4];
                v[4 - n..].copy_from_slice(&b[..n]);
                i32::from_le_bytes(v) as f32 / 2_147_483_648.0
            }
        }
    }

    /// Appends `bytes` (whole frames, interleaved) to `out` as 48 kHz stereo s16.
    pub fn convert(&mut self, bytes: &[u8], out: &mut Vec<i16>) {
        let frame = self.format.frame_bytes();
        let width = frame / self.format.channels;
        self.stereo.clear();
        for f in bytes.chunks_exact(frame) {
            let mut lr = [0.0f32; 2];
            for (s, (gl, gr)) in f.chunks_exact(width).zip(&self.gains) {
                let v = self.sample(s);
                lr[0] += v * gl;
                lr[1] += v * gr;
            }
            self.stereo.push(lr);
        }
        self.resampled.clear();
        self.resampler.run(&self.stereo, &mut self.resampled);
        out.extend(self.resampled.iter().flatten().map(|v| (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16));
    }
}

/// Writes mono samples (-1..1) as frames in `format`, every channel the same: a viewer's
/// voice, into a device that wouldn't take our own format.
pub fn encode_mono(samples: &[f32], format: &Format, out: &mut [u8]) {
    let frame = format.frame_bytes();
    let width = frame / format.channels;
    for (f, &v) in out.chunks_exact_mut(frame).zip(samples) {
        let bytes = match format.sample {
            Sample::Float32 => v.to_le_bytes(),
            // The top bytes of an i32, whatever the width.
            Sample::Int(_) => ((v.clamp(-1.0, 1.0) as f64 * 2_147_483_648.0) as i32).to_le_bytes(),
        };
        for ch in f.chunks_exact_mut(width) {
            ch.copy_from_slice(&bytes[4 - width..]);
        }
    }
}

/// Cuts interleaved stereo s16 into AUDIO_FRAMES_PER_PACKET-frame packets, numbered and
/// stamped with the host clock when they're complete (as the Linux host does).
#[derive(Default)]
pub struct Packetizer {
    pending: Vec<i16>,
    seq: u32,
}

impl Packetizer {
    pub const CHANNELS: usize = 2;

    pub fn push(&mut self, pcm: &[i16], mut send: impl FnMut(Vec<u8>)) {
        self.pending.extend_from_slice(pcm);
        let per_packet = AUDIO_FRAMES_PER_PACKET * Self::CHANNELS;
        let mut offset = 0;
        while self.pending.len() - offset >= per_packet {
            send(audio_packet(self.seq, crate::clock::now_us(), Self::CHANNELS as u8, &self.pending[offset..offset + per_packet]));
            self.seq = self.seq.wrapping_add(1);
            offset += per_packet;
        }
        self.pending.drain(..offset);
    }

    /// Drops a partial packet left over from before a gap (nothing played for a while).
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_proto::AUDIO_HEADER_LEN;

    fn float_bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn reads_wave_formats() {
        // The usual shared-mode mix format: extensible, float, stereo.
        let f = Format::from_wave(0xFFFE, Some(3), 48_000, 2, 32, 8, 0x3).unwrap();
        assert_eq!((f.sample, f.frame_bytes()), (Sample::Float32, 8));
        // 24-bit samples in 32-bit containers, and packed 24-bit.
        assert_eq!(Format::from_wave(0xFFFE, Some(1), 44_100, 2, 24, 8, 0x3).unwrap().sample, Sample::Int(4));
        assert_eq!(Format::from_wave(1, None, 44_100, 2, 24, 6, 0).unwrap().sample, Sample::Int(3));
        assert_eq!(Format::from_wave(1, None, 48_000, 1, 16, 2, 0).unwrap().sample, Sample::Int(2));
        // Compressed, or nonsense.
        assert!(Format::from_wave(0x55, None, 48_000, 2, 16, 4, 0).is_none());
        assert!(Format::from_wave(0xFFFE, None, 48_000, 2, 32, 8, 0).is_none());
        assert!(Format::from_wave(3, None, 48_000, 0, 32, 0, 0).is_none());
    }

    #[test]
    fn float_stereo_48k_passes_through() {
        let f = Format::from_wave(3, None, 48_000, 2, 32, 8, 0).unwrap();
        let mut c = Converter::new(f);
        let mut out = Vec::new();
        c.convert(&float_bytes(&[0.5, -0.5, 1.5, -2.0]), &mut out);
        assert_eq!(out, [16383, -16383, i16::MAX, -i16::MAX]);
    }

    #[test]
    fn integer_samples_scale_to_s16() {
        let mut out = Vec::new();
        let s16 = Format::from_wave(1, None, 48_000, 2, 16, 4, 0).unwrap();
        let bytes: Vec<u8> = [16384i16, -32768].iter().flat_map(|s| s.to_le_bytes()).collect();
        Converter::new(s16).convert(&bytes, &mut out);
        assert_eq!(out, [16383, -i16::MAX]);
        out.clear();
        // Packed 24-bit: 0x400000 is half scale.
        let s24 = Format::from_wave(1, None, 48_000, 2, 24, 6, 0).unwrap();
        Converter::new(s24).convert(&[0x00, 0x00, 0x40, 0x00, 0x00, 0xC0], &mut out);
        assert_eq!(out, [16383, -16383]);
    }

    #[test]
    fn mono_goes_to_both_sides() {
        let f = Format::from_wave(3, None, 48_000, 1, 32, 4, 0x4).unwrap();
        let mut out = Vec::new();
        Converter::new(f).convert(&float_bytes(&[0.5]), &mut out);
        assert_eq!(out, [16383, 16383]);
    }

    #[test]
    fn surround_folds_into_stereo() {
        // 5.1: FL FR FC LFE BL BR.
        let f = Format::from_wave(0xFFFE, Some(3), 48_000, 6, 32, 24, 0x3F).unwrap();
        let mut out = Vec::new();
        Converter::new(f).convert(&float_bytes(&[0.2, 0.0, 0.0, 1.0, 0.0, 0.0]), &mut out);
        assert_eq!(out, [(0.2 * i16::MAX as f32) as i16, 0], "front left stays left; LFE is dropped");
        out.clear();
        Converter::new(f).convert(&float_bytes(&[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]), &mut out);
        assert_eq!(out[0], out[1], "centre goes to both");
        assert!((out[0] as f32 - 0.5 * std::f32::consts::FRAC_1_SQRT_2 * i16::MAX as f32).abs() < 2.0);
        // An unknown layout keeps the first two channels.
        let unknown = Format::from_wave(3, None, 48_000, 4, 32, 16, 0).unwrap();
        out.clear();
        Converter::new(unknown).convert(&float_bytes(&[0.5, -0.5, 1.0, 1.0]), &mut out);
        assert_eq!(out, [16383, -16383]);
    }

    #[test]
    fn resamples_to_48k_across_buffers() {
        let f = Format::from_wave(3, None, 44_100, 2, 32, 8, 0x3).unwrap();
        let mut c = Converter::new(f);
        let mut out = Vec::new();
        // One second in uneven buffers comes out as one second at 48 kHz (± a frame).
        let mut sent = 0;
        for len in [441usize, 100, 7, 1000, 42_552] {
            let frames: Vec<f32> = (sent..sent + len).flat_map(|i| [i as f32 / 44_100.0, 0.0]).collect();
            c.convert(&float_bytes(&frames), &mut out);
            sent += len;
        }
        assert_eq!(sent, 44_100);
        let frames = out.len() / 2;
        assert!((47_999..=48_001).contains(&frames), "{frames} frames");
        // A ramp stays a smooth ramp: no jumps at buffer edges.
        let left: Vec<i32> = out.iter().step_by(2).map(|&v| v as i32).collect();
        assert!(left.windows(2).all(|w| (0..=2).contains(&(w[1] - w[0]))), "monotonic and smooth");
    }

    #[test]
    fn upsamples_from_lower_rates_too() {
        let f = Format::from_wave(3, None, 24_000, 2, 32, 8, 0x3).unwrap();
        let mut c = Converter::new(f);
        let mut out = Vec::new();
        c.convert(&float_bytes(&[0.0, 0.0, 0.5, 0.5, 1.0, 1.0]), &mut out);
        let left: Vec<i16> = out.iter().step_by(2).copied().collect();
        assert_eq!(left, [0, 8191, 16383, 24575]);
    }

    #[test]
    fn encodes_mono_for_any_device() {
        let s16 = Format::from_wave(1, None, 48_000, 1, 16, 2, 0).unwrap();
        let mut out = [0u8; 4];
        encode_mono(&[0.5, -1.0], &s16, &mut out);
        assert_eq!(out, [16384i16.to_le_bytes(), i16::MIN.to_le_bytes()].concat()[..]);
        // Float stereo: both channels carry the sample.
        let f32 = Format::from_wave(3, None, 48_000, 2, 32, 8, 0x3).unwrap();
        let mut out = [0u8; 8];
        encode_mono(&[0.25], &f32, &mut out);
        assert_eq!(out, [0.25f32.to_le_bytes(), 0.25f32.to_le_bytes()].concat()[..]);
        // Packed 24-bit, and full scale doesn't wrap.
        let s24 = Format::from_wave(1, None, 48_000, 1, 24, 3, 0).unwrap();
        let mut out = [0u8; 3];
        encode_mono(&[1.0], &s24, &mut out);
        assert_eq!(out, [0xFF, 0xFF, 0x7F]);
    }

    #[test]
    fn packets_are_5ms_numbered_stereo() {
        let mut p = Packetizer::default();
        let mut sent = Vec::new();
        let samples: Vec<i16> = (0..(AUDIO_FRAMES_PER_PACKET * 2 * 2 + 10) as i32).map(|i| i as i16).collect();
        p.push(&samples[..100], |b| sent.push(b));
        assert!(sent.is_empty());
        p.push(&samples[100..], |b| sent.push(b));
        assert_eq!(sent.len(), 2);
        for (i, packet) in sent.iter().enumerate() {
            assert_eq!(packet.len(), AUDIO_HEADER_LEN + AUDIO_FRAMES_PER_PACKET * 2 * 2);
            assert_eq!((packet[0], packet[1]), (glass_proto::KIND_AUDIO, 2));
            assert_eq!(u32::from_le_bytes(packet[4..8].try_into().unwrap()), i as u32);
        }
        // The first sample of the second packet follows on from the first.
        let first = i16::from_le_bytes([sent[1][AUDIO_HEADER_LEN], sent[1][AUDIO_HEADER_LEN + 1]]);
        assert_eq!(first as usize, AUDIO_FRAMES_PER_PACKET * 2);
        assert_eq!(p.pending.len(), 10);
        p.clear();
        assert!(p.pending.is_empty());
    }
}
