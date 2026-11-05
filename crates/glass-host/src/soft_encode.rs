//! Software H.264 (OpenH264, screen-content real-time mode), used by the Linux host and
//! by the test pattern on every platform. Output is Annex B with
//! SPS/PPS in every IDR, which the browser decodes without an out-of-band description.
// Off Linux only the test pattern uses this, so the capture-facing parts go unused.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use anyhow::{Result, anyhow};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder as H264, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::YUVSlices;

/// Above this many pixels, frames are halved before encoding to keep software encode
/// within the frame budget (4K → 1080p, 5K → 1440p).
pub(crate) const MAX_PIXELS: usize = 2560 * 1600;
/// Matches the macOS encoder: a periodic IDR before frame_num wraps.
const MAX_GOP_FRAMES: u32 = 900;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Bytes B, G, R, X (BGRx / BGRA).
    Bgrx,
    /// Bytes R, G, B, X (RGBx / RGBA).
    Rgbx,
}

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

pub struct Encoder {
    h264: H264,
    src: (usize, usize),
    div: usize,
    out: (usize, usize),
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    threads: usize,
    /// One slice per thread is set once OpenH264 has initialised (on the first frame).
    sliced: bool,
}

/// Cores for colour conversion and OpenH264's slices.
fn threads() -> usize {
    // available_parallelism() can read cgroup files, so ask once.
    static THREADS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *THREADS.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8))
}

impl Encoder {
    /// `scale` (from --scale) of 0.5 or less also halves the frame.
    pub fn new(width: usize, height: usize, fps: u32, bitrate: u32, scale: f64) -> Result<Self> {
        let div = if width * height > MAX_PIXELS || scale <= 0.5 {
            2
        } else {
            1
        };
        let out = ((width / div) & !1, (height / div) & !1);
        let threads = threads();
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(MAX_GOP_FRAMES))
            .skip_frames(false)
            .num_threads(threads.min(4) as u16);
        let h264 = H264::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| anyhow!("openh264: {e}"))?;
        let (w, h) = out;
        Ok(Self {
            h264,
            src: (width, height),
            div,
            out,
            y: vec![0; w * h],
            u: vec![0; (w / 2) * (h / 2)],
            v: vec![0; (w / 2) * (h / 2)],
            threads,
            sliced: false,
        })
    }

    /// Changes the target bitrate on the fly (no new keyframe).
    pub fn set_bitrate(&mut self, bps: u32) {
        let mut info = openh264_sys2::TagBitrateInfo {
            iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
            iBitrate: bps as i32,
        };
        // SAFETY: SetOption(ENCODER_OPTION_BITRATE) takes a pointer to SBitrateInfo, which
        // lives for the duration of the call.
        unsafe {
            self.h264.raw_api().set_option(
                openh264_sys2::ENCODER_OPTION_BITRATE,
                (&raw mut info).cast(),
            );
        }
    }

    /// Size the encoder was built for; a different frame size needs a new encoder.
    pub fn source_size(&self) -> (usize, usize) {
        self.src
    }

    /// Returns Annex B data and whether it's a keyframe, or None if nothing was produced.
    pub fn encode(
        &mut self,
        frame: &Frame,
        force_keyframe: bool,
    ) -> Result<Option<(Vec<u8>, bool)>> {
        to_i420(
            frame,
            self.div,
            self.out,
            &mut self.y,
            &mut self.u,
            &mut self.v,
        );
        if force_keyframe {
            self.h264.force_intra_frame();
        }
        let (w, h) = self.out;
        let yuv = YUVSlices::new((&self.y, &self.u, &self.v), (w, h), (w, w / 2, w / 2));
        let bits = self.h264.encode(&yuv).map_err(|e| anyhow!("encode: {e}"))?;
        let keyframe = matches!(bits.frame_type(), FrameType::IDR | FrameType::I);
        let data = bits.to_vec();
        if !self.sliced {
            self.sliced = true;
            self.split_slices();
        }
        Ok((!data.is_empty()).then_some((data, keyframe)))
    }

    /// OpenH264 encodes slices in parallel, but the crate always asks for one slice, so
    /// `num_threads` alone does nothing. Re-applies the parameters with one slice per
    /// thread (the crate initialises lazily, so this runs after the first frame).
    fn split_slices(&mut self) {
        if self.threads < 2 {
            return;
        }
        // SAFETY: Get/SetOption(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT) read and write an
        // SEncParamExt, which lives for the duration of the calls.
        unsafe {
            let api = self.h264.raw_api();
            let mut params: openh264_sys2::SEncParamExt = std::mem::zeroed();
            let option = openh264_sys2::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT;
            if api.get_option(option, (&raw mut params).cast()) != 0 {
                return;
            }
            let slices = &mut params.sSpatialLayers[0].sSliceArgument;
            slices.uiSliceMode = openh264_sys2::SM_FIXEDSLCNUM_SLICE;
            // OpenH264 runs at most 4 slice threads; it drops to one when it starts with
            // a single slice.
            let slice_threads = self.threads.min(4);
            slices.uiSliceNum = slice_threads as u32;
            params.iMultipleThreadIdc = slice_threads as u16;
            if api.set_option(option, (&raw mut params).cast()) != 0 {
                tracing::warn!("openh264: couldn't split into {slice_threads} slices");
            }
        }
    }
}

/// BT.601 limited range, 2x2-averaged chroma; `div` box-filters the source down.
/// Bands of rows convert in parallel: at 4K this was most of the time per frame.
pub(crate) fn to_i420(
    f: &Frame,
    div: usize,
    (w, h): (usize, usize),
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let (cw, ch) = (w / 2, h / 2);
    if cw == 0 || ch == 0 {
        return;
    }
    // Chroma rows per band (each is two luma rows).
    let rows = ch.div_ceil(threads().min(ch));
    let mut bands = y[..w * h]
        .chunks_mut(rows * 2 * w)
        .zip(u[..cw * ch].chunks_mut(rows * cw))
        .zip(v[..cw * ch].chunks_mut(rows * cw))
        .enumerate();
    // The last band runs here, the others on their own threads.
    let Some((last, ((ly, lu), lv))) = bands.next_back() else {
        return;
    };
    std::thread::scope(|s| {
        for (i, ((yb, ub), vb)) in bands {
            s.spawn(move || convert_band(f, div, w, i * rows, yb, ub, vb));
        }
        convert_band(f, div, w, last * rows, ly, lu, lv);
    });
}

/// Converts chroma rows from `cy0` on; `y`, `u` and `v` hold just this band.
fn convert_band(f: &Frame, div: usize, w: usize, cy0: usize, y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    match div {
        1 => convert_band_div::<1>(f, w, cy0, y, u, v),
        2 => convert_band_div::<2>(f, w, cy0, y, u, v),
        _ => unreachable!("frames are encoded at full or half size"),
    }
}

fn convert_band_div<const D: usize>(f: &Frame, w: usize, cy0: usize, y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    let (ri, gi, bi) = match f.format {
        PixelFormat::Bgrx => (2, 1, 0),
        PixelFormat::Rgbx => (0, 1, 2),
    };
    let cw = w / 2;
    let n = (D * D) as i32;
    // Per chroma column: the r, g, b sums of its four luma pixels.
    let mut acc = vec![[0i32; 3]; cw];
    for (i, (urow, vrow)) in u.chunks_exact_mut(cw).zip(v.chunks_exact_mut(cw)).enumerate() {
        for dy in 0..2 {
            let py = (cy0 + i) * 2 + dy;
            let src: [&[u8]; D] = std::array::from_fn(|k| {
                let start = (py * D + k) * f.stride;
                &f.data[start..start + w * D * 4]
            });
            let yrow = &mut y[(i * 2 + dy) * w..][..w];
            for (px, out) in yrow.iter_mut().enumerate() {
                // Average a D×D block of source pixels.
                let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
                for row in &src {
                    for p in row[px * D * 4..(px + 1) * D * 4].chunks_exact(4) {
                        r += p[ri] as i32;
                        g += p[gi] as i32;
                        b += p[bi] as i32;
                    }
                }
                let (r, g, b) = (r / n, g / n, b / n);
                *out = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                let a = &mut acc[px / 2];
                a[0] += r;
                a[1] += g;
                a[2] += b;
            }
        }
        for ((a, cu), cv) in acc.iter_mut().zip(urow.iter_mut()).zip(vrow.iter_mut()) {
            let (r, g, b) = (a[0] / 4, a[1] / 4, a[2] / 4);
            *cu = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            *cv = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            *a = [0; 3];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original single-threaded conversion, which the parallel one must match exactly.
    fn reference_i420(
        f: &Frame,
        div: usize,
        (w, h): (usize, usize),
        y: &mut [u8],
        u: &mut [u8],
        v: &mut [u8],
    ) {
        let (ri, gi, bi) = match f.format {
            PixelFormat::Bgrx => (2, 1, 0),
            PixelFormat::Rgbx => (0, 1, 2),
        };
        // Average a div×div block of source pixels.
        let sample = |sx: usize, sy: usize| -> (i32, i32, i32) {
            let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
            for dy in 0..div {
                let row = &f.data[(sy * div + dy) * f.stride..];
                for dx in 0..div {
                    let p = &row[(sx * div + dx) * 4..];
                    r += p[ri] as i32;
                    g += p[gi] as i32;
                    b += p[bi] as i32;
                }
            }
            let n = (div * div) as i32;
            (r / n, g / n, b / n)
        };
        for cy in 0..h / 2 {
            for cx in 0..w / 2 {
                let (mut rs, mut gs, mut bs) = (0, 0, 0);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (px, py) = (cx * 2 + dx, cy * 2 + dy);
                    let (r, g, b) = sample(px, py);
                    y[py * w + px] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                    rs += r;
                    gs += g;
                    bs += b;
                }
                let (r, g, b) = (rs / 4, gs / 4, bs / 4);
                u[cy * (w / 2) + cx] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                v[cy * (w / 2) + cx] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        }
    }

    #[test]
    fn white_and_black_to_i420() {
        let frame = |px: [u8; 4]| Frame {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgrx,
            data: px.repeat(4),
        };
        let (mut y, mut u, mut v) = (vec![0; 4], vec![0; 1], vec![0; 1]);
        to_i420(
            &frame([255, 255, 255, 0]),
            1,
            (2, 2),
            &mut y,
            &mut u,
            &mut v,
        );
        assert_eq!((y[0], u[0], v[0]), (235, 128, 128));
        to_i420(&frame([0, 0, 0, 0]), 1, (2, 2), &mut y, &mut u, &mut v);
        assert_eq!((y[0], u[0], v[0]), (16, 128, 128));
    }

    #[test]
    fn encodes_a_decodable_keyframe() {
        let frame = Frame {
            width: 64,
            height: 64,
            stride: 64 * 4,
            format: PixelFormat::Bgrx,
            data: (0..64 * 64)
                .flat_map(|i| [(i % 251) as u8, (i % 7 * 30) as u8, 200, 0])
                .collect(),
        };
        let mut enc = Encoder::new(64, 64, 30, 1_000_000, 1.0).unwrap();
        let (data, keyframe) = enc.encode(&frame, true).unwrap().expect("output");
        assert!(keyframe);
        assert_eq!(&data[..4], &[0, 0, 0, 1], "Annex B");
        let nal_types: Vec<u8> = data
            .windows(5)
            .filter(|w| w[..4] == [0, 0, 0, 1])
            .map(|w| w[4] & 0x1f)
            .collect();
        assert!(
            nal_types.contains(&7) && nal_types.contains(&8) && nal_types.contains(&5),
            "{nal_types:?}"
        );
        let mut dec = openh264::decoder::Decoder::new().unwrap();
        assert!(dec.decode(&data).unwrap().is_some(), "decodes");
    }

    #[test]
    fn red_differs_by_channel_order() {
        let bgrx = Frame {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgrx,
            data: [0, 0, 255, 0].repeat(4),
        };
        let rgbx = Frame {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Rgbx,
            data: [255, 0, 0, 0].repeat(4),
        };
        let (mut y1, mut u1, mut v1) = (vec![0; 4], vec![0; 1], vec![0; 1]);
        let (mut y2, mut u2, mut v2) = (vec![0; 4], vec![0; 1], vec![0; 1]);
        to_i420(&bgrx, 1, (2, 2), &mut y1, &mut u1, &mut v1);
        to_i420(&rgbx, 1, (2, 2), &mut y2, &mut u2, &mut v2);
        assert_eq!((y1, u1, v1.clone()), (y2, u2, v2));
        assert!(v1[0] > 200, "red has high V");
    }

    /// A frame with detail everywhere, padded rows and an odd size.
    fn busy_frame(width: usize, height: usize, format: PixelFormat) -> Frame {
        let stride = width * 4 + 12;
        let data = (0..stride * height)
            .map(|i| ((i * 7919) ^ (i / stride * 31)) as u8)
            .collect();
        Frame { width, height, stride, format, data }
    }

    #[test]
    fn parallel_conversion_matches_the_reference() {
        for (width, height) in [(2, 2), (6, 4), (643, 361), (1923, 1083)] {
            for format in [PixelFormat::Bgrx, PixelFormat::Rgbx] {
                for div in [1, 2] {
                    let frame = busy_frame(width, height, format);
                    let (w, h) = ((width / div) & !1, (height / div) & !1);
                    let planes = || (vec![0u8; w * h], vec![0u8; w * h / 4], vec![0u8; w * h / 4]);
                    let ((mut y1, mut u1, mut v1), (mut y2, mut u2, mut v2)) = (planes(), planes());
                    to_i420(&frame, div, (w, h), &mut y1, &mut u1, &mut v1);
                    reference_i420(&frame, div, (w, h), &mut y2, &mut u2, &mut v2);
                    assert!(y1 == y2 && u1 == u2 && v1 == v2, "{width}x{height} {format:?} div {div}");
                }
            }
        }
    }

    #[test]
    fn sliced_frames_decode() {
        let mut enc = Encoder::new(1280, 720, 30, 4_000_000, 1.0).unwrap();
        let mut dec = openh264::decoder::Decoder::new().unwrap();
        for i in 0..4u8 {
            let mut frame = busy_frame(1280, 720, PixelFormat::Bgrx);
            frame.data.iter_mut().step_by(97).for_each(|b| *b = b.wrapping_add(i * 40));
            let (data, _) = enc.encode(&frame, i == 0).unwrap().expect("output");
            assert!(dec.decode(&data).unwrap().is_some(), "frame {i} decodes");
        }
    }

    /// Time per 4K frame, split into colour conversion and encoding:
    /// `cargo test --release -p glass-host soft_encode_4k_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn soft_encode_4k_speed() {
        let (width, height) = (3840, 2160);
        // Flat areas, gradients and fine detail, like a desktop.
        let mut frame = busy_frame(width, height, PixelFormat::Bgrx);
        for (i, p) in frame.data.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % (frame.stride / 4), i / (frame.stride / 4));
            p[0] = (x ^ y) as u8;
            p[1] = (x * 3 / 7) as u8;
            p[2] = ((x / 40) * 37 + (y / 20) * 11) as u8;
        }
        let mut enc = Encoder::new(width, height, 60, 30_000_000, 1.0).unwrap();
        let (w, h) = enc.out;
        let (mut convert, mut encode) = (0.0, 0.0);
        let frames = 60;
        for i in 0..frames {
            // A block moves each frame, as when dragging a window.
            for row in 300..700 {
                let start = row * frame.stride + i * 160;
                frame.data[start..start + 2400].iter_mut().for_each(|b| *b = b.wrapping_add(90));
            }
            let t = std::time::Instant::now();
            to_i420(&frame, enc.div, enc.out, &mut enc.y, &mut enc.u, &mut enc.v);
            convert += t.elapsed().as_secs_f64();
            let t = std::time::Instant::now();
            let yuv = YUVSlices::new((&enc.y, &enc.u, &enc.v), (w, h), (w, w / 2, w / 2));
            enc.h264.encode(&yuv).unwrap();
            if !enc.sliced {
                enc.sliced = true;
                enc.split_slices();
            }
            encode += t.elapsed().as_secs_f64();
        }
        let ms = |s: f64| s / frames as f64 * 1e3;
        println!(
            "4K → {w}x{h}, {} threads: convert {:.1} ms, encode {:.1} ms per frame",
            enc.threads,
            ms(convert),
            ms(encode)
        );
    }
}
