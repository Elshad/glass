//! Session recording: the encoded video, exactly as sent, saved to an MP4 on the host.
//!
//! Only stream 0 is recorded (the main display, or the first one when showing them all):
//! one display per file keeps the files playable everywhere. Nothing is re-encoded: H.264
//! access units are rewritten from Annex B to length-prefixed samples and put in a plain
//! (not fragmented) MP4. Samples go straight to disk in one `mdat`; the index (`moov`) is
//! written when the recording ends, so a file cut short by a crash is unplayable, the
//! price of staying simple and compatible.
//!
//! The file starts at the first keyframe (one is requested). When the picture size
//! changes (dynamic resolution, a new display), the encoder starts over with a new SPS;
//! that file is finished and the recording continues in a new one ("… part 2.mp4").
//! HEVC frames aren't recorded (H.264 only); they're skipped with a warning.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::sync::broadcast;

use crate::media::EncodedFrame;

pub fn supported() -> bool {
    true
}

/// A recording of the host's streams that ends (and is finalized) when dropped.
pub struct Recording {
    /// The first file; later parts sit next to it.
    pub path: PathBuf,
    state: Arc<Mutex<Option<Recorder>>>,
}

impl Drop for Recording {
    fn drop(&mut self) {
        // The frame thread sees `None` and stops; the index is written here, at once.
        let recorder = self.state.lock().unwrap().take();
        if let Some(r) = recorder {
            r.finish();
        }
    }
}

/// Starts recording what's streamed, for `who`.
pub fn start(who: &str) -> Result<Recording> {
    let shared = crate::server::shared().context("the host isn't ready yet")?;
    let settings = crate::settings::load();
    let dir = crate::settings::recordings_dir(&settings);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let base = format!("Glass {} {}", local_stamp(), sanitize(who));
    let path = crate::control::unique_path(&dir, &format!("{base}.mp4"));
    let recorder = Recorder {
        dir,
        base,
        first: path.clone(),
        part: 1,
        file: None,
        warned_hevc: false,
    };
    let state = Arc::new(Mutex::new(Some(recorder)));
    let mut frames = shared.frames.subscribe();
    shared.video.request_keyframe(Some(0));
    let thread_state = state.clone();
    std::thread::Builder::new()
        .name("recording".into())
        .spawn(move || {
            loop {
                let frame = match frames.blocking_recv() {
                    Ok(f) => f,
                    // Missed frames: wait for the next keyframe rather than record a gap
                    // that decodes as garbage.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(r) = thread_state.lock().unwrap().as_mut() {
                            r.need_keyframe();
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                let mut guard = thread_state.lock().unwrap();
                let Some(r) = guard.as_mut() else { return };
                if let Err(e) = r.frame(&frame) {
                    tracing::warn!("recording stopped: {e:#}");
                    if let Some(r) = guard.take() {
                        r.finish();
                    }
                    return;
                }
            }
        })?;
    tracing::info!("recording to {}", path.display());
    Ok(Recording { path, state })
}

struct Recorder {
    dir: PathBuf,
    base: String,
    first: PathBuf,
    part: u32,
    file: Option<Mp4Writer>,
    warned_hevc: bool,
}

impl Recorder {
    fn frame(&mut self, frame: &EncodedFrame) -> Result<()> {
        if frame.stream != 0 {
            return Ok(());
        }
        if frame.hevc {
            if !self.warned_hevc {
                self.warned_hevc = true;
                tracing::warn!("recording: HEVC frames aren't recorded (H.264 only)");
            }
            return Ok(());
        }
        let au = AccessUnit::parse(&frame.data);
        // A new picture size: finish this file, carry on in the next part.
        if let (Some(w), Some(sps)) = (&self.file, &au.sps)
            && w.sps != *sps
        {
            self.file.take().unwrap().finish()?;
            self.part += 1;
        }
        if self.file.is_none() {
            let (Some(sps), Some(pps)) = (au.sps.clone(), au.pps.clone()) else {
                return Ok(()); // waiting for a keyframe with its parameter sets
            };
            if !frame.keyframe {
                return Ok(());
            }
            let path = if self.part == 1 {
                self.first.clone()
            } else {
                crate::control::unique_path(&self.dir, &format!("{} part {}.mp4", self.base, self.part))
            };
            self.file = Some(Mp4Writer::create(&path, sps, pps)?);
        }
        let w = self.file.as_mut().unwrap();
        if w.waiting_for_keyframe && !frame.keyframe {
            return Ok(());
        }
        w.waiting_for_keyframe = false;
        w.write_sample(&au.sample, frame.keyframe, frame.capture_us)
    }

    fn need_keyframe(&mut self) {
        if let Some(w) = &mut self.file {
            w.waiting_for_keyframe = true;
        }
        if let Some(shared) = crate::server::shared() {
            shared.video.request_keyframe(Some(0));
        }
    }

    fn finish(mut self) {
        if let Some(w) = self.file.take() {
            let path = w.path.clone();
            match w.finish() {
                Ok(()) => tracing::info!("recording saved: {}", path.display()),
                Err(e) => tracing::warn!("recording {}: {e:#}", path.display()),
            }
        }
    }
}

/// One frame split into its parameter sets and the sample to store (length-prefixed NALs).
struct AccessUnit {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    sample: Vec<u8>,
}

impl AccessUnit {
    fn parse(annex_b: &[u8]) -> Self {
        let mut au = Self {
            sps: None,
            pps: None,
            sample: Vec::with_capacity(annex_b.len()),
        };
        for nal in nal_units(annex_b) {
            match nal[0] & 0x1f {
                7 => au.sps = Some(nal.to_vec()),
                8 => au.pps = Some(nal.to_vec()),
                9 => {} // access unit delimiter
                _ => {
                    au.sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    au.sample.extend_from_slice(nal);
                }
            }
        }
        au
    }
}

/// The NAL units in an Annex B stream (3- or 4-byte start codes).
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
        // A 4-byte start code's leading zero, and trailing zeros, belong to no NAL.
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&data[s..end]);
        }
    }
    out
}

const TIMESCALE: u32 = 90_000;

struct Sample {
    size: u32,
    us: u64,
    key: bool,
}

struct Mp4Writer {
    path: PathBuf,
    out: BufWriter<File>,
    /// Offset of the mdat box (its 64-bit size is patched at the end).
    mdat: u64,
    written: u64,
    sps: Vec<u8>,
    pps: Vec<u8>,
    samples: Vec<Sample>,
    waiting_for_keyframe: bool,
}

impl Mp4Writer {
    fn create(path: &Path, sps: Vec<u8>, pps: Vec<u8>) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
        let mut out = BufWriter::new(file);
        let ftyp = mp4_box(b"ftyp", &[b"isom".as_slice(), &512u32.to_be_bytes(), b"isomiso2avc1mp41"].concat());
        out.write_all(&ftyp)?;
        let mdat = ftyp.len() as u64;
        // size 1 = a 64-bit size follows; long sessions pass 4 GB.
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&0u64.to_be_bytes())?;
        Ok(Self {
            path: path.to_path_buf(),
            out,
            mdat,
            written: 0,
            sps,
            pps,
            samples: Vec::new(),
            waiting_for_keyframe: false,
        })
    }

    fn write_sample(&mut self, sample: &[u8], key: bool, us: u64) -> Result<()> {
        if sample.is_empty() {
            return Ok(());
        }
        self.out.write_all(sample)?;
        self.written += sample.len() as u64;
        self.samples.push(Sample {
            size: sample.len() as u32,
            us,
            key,
        });
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        let moov = self.moov();
        self.out.write_all(&moov)?;
        let mut file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(self.mdat + 8))?;
        file.write_all(&(16 + self.written).to_be_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// Sample durations in TIMESCALE units, from the capture times (variable frame rate:
    /// a still screen sends nothing, so the last frame simply lasts longer).
    fn durations(&self) -> Vec<u32> {
        let mut d: Vec<u32> = self
            .samples
            .windows(2)
            .map(|w| {
                let us = w[1].us.saturating_sub(w[0].us);
                ((us * TIMESCALE as u64 / 1_000_000) as u32).max(1)
            })
            .collect();
        if !self.samples.is_empty() {
            d.push(d.last().copied().unwrap_or(TIMESCALE / 30));
        }
        d
    }

    fn moov(&self) -> Vec<u8> {
        let (width, height) = sps_size(&self.sps).unwrap_or((0, 0));
        let durations = self.durations();
        let total: u64 = durations.iter().map(|&d| d as u64).sum();
        let movie_ms = total * 1000 / TIMESCALE as u64;
        let n = self.samples.len() as u32;

        let mvhd = full_box(b"mvhd", 0, &cat(&[
            &0u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            &1000u32.to_be_bytes(),
            &(movie_ms as u32).to_be_bytes(),
            &0x0001_0000u32.to_be_bytes(), // rate 1.0
            &0x0100u16.to_be_bytes(),      // volume 1.0
            &[0; 10],
            &MATRIX,
            &[0; 24],
            &2u32.to_be_bytes(), // next track id
        ]));
        let tkhd = full_box(b"tkhd", 3, &cat(&[
            &0u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            &1u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            &(movie_ms as u32).to_be_bytes(),
            &[0; 8],
            &0u16.to_be_bytes(), // layer
            &0u16.to_be_bytes(), // alternate group
            &0u16.to_be_bytes(), // volume (video: 0)
            &0u16.to_be_bytes(),
            &MATRIX,
            &(width << 16).to_be_bytes(),
            &(height << 16).to_be_bytes(),
        ]));
        let mdhd = full_box(b"mdhd", 0, &cat(&[
            &0u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            &TIMESCALE.to_be_bytes(),
            &(total as u32).to_be_bytes(),
            &0x55c4u16.to_be_bytes(), // language: und
            &0u16.to_be_bytes(),
        ]));
        let hdlr = full_box(b"hdlr", 0, &cat(&[&[0; 4], b"vide", &[0; 12], b"Glass\0"]));
        let vmhd = full_box(b"vmhd", 1, &[0; 8]);
        let dref = full_box(b"dref", 0, &cat(&[&1u32.to_be_bytes(), &full_box(b"url ", 1, &[])]));
        let dinf = mp4_box(b"dinf", &dref);

        let mut avcc = vec![1, self.sps[1], self.sps[2], self.sps[3], 0xff, 0xe1];
        avcc.extend_from_slice(&(self.sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&self.sps);
        avcc.push(1);
        avcc.extend_from_slice(&(self.pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&self.pps);
        let avc1 = mp4_box(b"avc1", &cat(&[
            &[0; 6],
            &1u16.to_be_bytes(), // data reference index
            &[0; 16],
            &(width as u16).to_be_bytes(),
            &(height as u16).to_be_bytes(),
            &0x0048_0000u32.to_be_bytes(), // 72 dpi
            &0x0048_0000u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            &1u16.to_be_bytes(), // frames per sample
            &[0; 32],            // compressor name
            &0x0018u16.to_be_bytes(),
            &(-1i16).to_be_bytes(),
            &mp4_box(b"avcC", &avcc),
        ]));
        let stsd = full_box(b"stsd", 0, &cat(&[&1u32.to_be_bytes(), &avc1]));

        // Runs of equal durations.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &d in &durations {
            match runs.last_mut() {
                Some((count, delta)) if *delta == d => *count += 1,
                _ => runs.push((1, d)),
            }
        }
        let mut stts = (runs.len() as u32).to_be_bytes().to_vec();
        for (count, delta) in runs {
            stts.extend_from_slice(&count.to_be_bytes());
            stts.extend_from_slice(&delta.to_be_bytes());
        }
        let keys: Vec<u32> = (0..n).filter(|&i| self.samples[i as usize].key).map(|i| i + 1).collect();
        let mut stss = (keys.len() as u32).to_be_bytes().to_vec();
        for k in keys {
            stss.extend_from_slice(&k.to_be_bytes());
        }
        // Every sample in one chunk, which starts right after the mdat header.
        let stsc = cat(&[&1u32.to_be_bytes(), &1u32.to_be_bytes(), &n.to_be_bytes(), &1u32.to_be_bytes()]);
        let mut stsz = cat(&[&0u32.to_be_bytes(), &n.to_be_bytes()]);
        for s in &self.samples {
            stsz.extend_from_slice(&s.size.to_be_bytes());
        }
        let co64 = cat(&[&1u32.to_be_bytes(), &(self.mdat + 16).to_be_bytes()]);
        let stbl = mp4_box(b"stbl", &cat(&[
            &stsd,
            &full_box(b"stts", 0, &stts),
            &full_box(b"stss", 0, &stss),
            &full_box(b"stsc", 0, &stsc),
            &full_box(b"stsz", 0, &stsz),
            &full_box(b"co64", 0, &co64),
        ]));
        let minf = mp4_box(b"minf", &cat(&[&vmhd, &dinf, &stbl]));
        let mdia = mp4_box(b"mdia", &cat(&[&mdhd, &hdlr, &minf]));
        let trak = mp4_box(b"trak", &cat(&[&tkhd, &mdia]));
        mp4_box(b"moov", &cat(&[&mvhd, &trak]))
    }
}

const MATRIX: [u8; 36] = {
    let mut m = [0u8; 36];
    // 1.0 (16.16), 1.0, 1.0 (2.30) on the diagonal.
    m[2] = 1;
    m[18] = 1;
    m[32] = 0x40;
    m
};

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + body.len());
    b.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

fn full_box(kind: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    mp4_box(kind, &cat(&[&(flags & 0x00ff_ffff).to_be_bytes(), body]))
}

/// Picture size from an H.264 SPS (with cropping).
fn sps_size(sps: &[u8]) -> Option<(u32, u32)> {
    // Drop emulation-prevention bytes (00 00 03).
    let mut rbsp = Vec::with_capacity(sps.len());
    let mut zeros = 0;
    for &b in sps.get(1..)? {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = Bits { data: &rbsp, pos: 0 };
    let profile = r.bits(8)?;
    r.bits(16)?; // constraints, level
    r.ue()?; // sps id
    let mut chroma = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma = r.ue()?;
        if chroma == 3 {
            r.bits(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.bits(1)?;
        if r.bits(1)? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bits(1)? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2 max frame num
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bits(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max ref frames
    r.bits(1)?;
    let mbs_w = r.ue()? + 1;
    let map_h = r.ue()? + 1;
    let frame_mbs_only = r.bits(1)?;
    if frame_mbs_only == 0 {
        r.bits(1)?;
    }
    r.bits(1)?;
    let mut w = mbs_w * 16;
    let mut h = (2 - frame_mbs_only) * map_h * 16;
    if r.bits(1)? == 1 {
        let (l, rt, t, b) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (cx, cy) = match chroma {
            0 => (1, 2 - frame_mbs_only),
            1 => (2, 2 * (2 - frame_mbs_only)),
            2 => (2, 2 - frame_mbs_only),
            _ => (1, 2 - frame_mbs_only),
        };
        w = w.checked_sub((l + rt) * cx)?;
        h = h.checked_sub((t + b) * cy)?;
    }
    Some((w, h))
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | ((byte >> (7 - self.pos % 8)) & 1) as u32;
            self.pos += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    fn se(&mut self) -> Option<i64> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

/// "2026-10-22 14-03" in local time (`date` knows the time zone).
fn local_stamp() -> String {
    #[cfg(windows)]
    return crate::platform::clock::local_stamp();
    #[cfg(not(windows))]
    std::process::Command::new("date")
        .arg("+%Y-%m-%d %H-%M")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::access::now().to_string())
}

/// A file-name-safe version of a device label.
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || " -_".contains(c) { c } else { ' ' })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let joined = words.join(" ");
    if joined.is_empty() {
        "viewer".into()
    } else {
        joined.chars().take(48).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soft_encode::{Encoder, Frame, PixelFormat};

    fn frames(n: usize) -> Vec<(Vec<u8>, bool)> {
        let (w, h) = (320, 180);
        let mut enc = Encoder::new(w, h, 30, 2_000_000, 1.0).unwrap();
        let mut out = Vec::new();
        for i in 0..n {
            let mut data = vec![40u8; w * h * 4];
            for y in 0..h {
                for x in (i * 6 % w)..((i * 6 % w) + 20).min(w) {
                    data[(y * w + x) * 4..][..3].copy_from_slice(&[255, 255, 255]);
                }
            }
            let frame = Frame { width: w, height: h, stride: w * 4, format: PixelFormat::Bgrx, data };
            if let Some(f) = enc.encode(&frame, i == 0).unwrap() {
                out.push(f);
            }
        }
        out
    }

    /// Top-level box kinds and their sizes.
    fn boxes(data: &[u8]) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= data.len() {
            let mut size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as u64;
            let kind = String::from_utf8_lossy(&data[i + 4..i + 8]).into_owned();
            if size == 1 {
                size = u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap());
            }
            out.push((kind, size));
            i += size as usize;
        }
        out
    }

    fn mux(path: &Path) -> usize {
        let mut w = None::<Mp4Writer>;
        let mut count = 0;
        for (n, (data, key)) in frames(20).into_iter().enumerate() {
            let au = AccessUnit::parse(&data);
            if w.is_none() {
                w = Some(Mp4Writer::create(path, au.sps.clone().unwrap(), au.pps.clone().unwrap()).unwrap());
            }
            w.as_mut().unwrap().write_sample(&au.sample, key, n as u64 * 33_333).unwrap();
            count += 1;
        }
        w.unwrap().finish().unwrap();
        count
    }

    #[test]
    fn muxes_a_playable_mp4() {
        let dir = std::env::temp_dir().join(format!("glass-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.mp4");
        let samples = mux(&path);
        assert!(samples > 10);
        let data = std::fs::read(&path).unwrap();
        let top = boxes(&data);
        let kinds: Vec<&str> = top.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["ftyp", "mdat", "moov"]);
        assert_eq!(top.iter().map(|(_, s)| s).sum::<u64>(), data.len() as u64, "sizes add up");
        let moov = &data[data.len() - top[2].1 as usize..];
        for inner in [b"mvhd", b"tkhd", b"avcC", b"stss", b"co64"] {
            assert!(moov.windows(4).any(|w| w == inner), "{}", String::from_utf8_lossy(inner));
        }
        // The track says 320×180, read back from the SPS.
        let tkhd = moov.windows(4).position(|w| w == b"tkhd").unwrap();
        let wh = &moov[tkhd + 4 + 4 + 72..][..8];
        assert_eq!(u32::from_be_bytes(wh[..4].try_into().unwrap()) >> 16, 320);
        assert_eq!(u32::from_be_bytes(wh[4..].try_into().unwrap()) >> 16, 180);

        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height,nb_read_frames", "-of", "csv=p=0"])
            .arg(&path)
            .output();
        match probe {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                assert_eq!(text.trim().trim_end_matches(','), format!("h264,320,180,{samples}"), "ffprobe: {text}");
            }
            _ => eprintln!("ffprobe not found; skipped the decode check"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn splits_annex_b() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let au = AccessUnit::parse(&data);
        assert_eq!(au.sps.as_deref(), Some(&[0x67, 1, 2][..]));
        assert_eq!(au.pps.as_deref(), Some(&[0x68, 3][..]));
        assert_eq!(au.sample, [0, 0, 0, 3, 0x65, 9, 9]);
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(sanitize("Safari on iPad"), "Safari on iPad");
        assert_eq!(sanitize("../../etc (x)"), "etc x");
        assert_eq!(sanitize("///"), "viewer");
    }
}
