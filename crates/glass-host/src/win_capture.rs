//! The parts of Windows capture and hardware encoding that don't call Windows: turning
//! duplicated pictures upright, NV12, and fixing up encoder output to Annex B with
//! parameter sets. Kept apart so their tests run on every OS.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::soft_encode::{Frame, to_i420};

/// How a display's scan-out is turned relative to the desktop (DXGI_MODE_ROTATION).
/// Desktop Duplication hands over the picture as scanned out, so a portrait display's
/// picture arrives on its side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    None,
    Cw90,
    Cw180,
    Cw270,
}

impl Rotation {
    /// From DXGI_MODE_ROTATION (0 unspecified, 1 identity, 2/3/4 a quarter turn each).
    pub fn from_dxgi(value: i32) -> Self {
        match value {
            2 => Self::Cw90,
            3 => Self::Cw180,
            4 => Self::Cw270,
            _ => Self::None,
        }
    }

    /// The upright size of a picture that's `size` as scanned out.
    pub fn upright(self, (w, h): (usize, usize)) -> (usize, usize) {
        match self {
            Self::Cw90 | Self::Cw270 => (h, w),
            Self::None | Self::Cw180 => (w, h),
        }
    }

    /// Where upright pixel (`x`, `y`) of a `w`×`h` upright picture is in the scanned-out
    /// one. Matches the Desktop Duplication sample's rotation handling.
    fn source(self, (w, h): (usize, usize), x: usize, y: usize) -> (usize, usize) {
        match self {
            Self::None => (x, y),
            Self::Cw90 => (y, w - 1 - x),
            Self::Cw180 => (w - 1 - x, h - 1 - y),
            Self::Cw270 => (h - 1 - y, x),
        }
    }
}

/// A rectangle of a picture, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// Square blocks rotated at a time, so reads down a column stay in cache.
const TILE: usize = 32;

/// Copies `crop` of the upright picture into `dst` (rows `crop.w * 4` bytes, no padding),
/// from `src`: 4-byte pixels as scanned out, `size` pixels, rows `stride` bytes apart.
pub fn blit(src: &[u8], size: (usize, usize), stride: usize, rotation: Rotation, crop: Crop, dst: &mut Vec<u8>) {
    let up = rotation.upright(size);
    assert!(crop.x + crop.w <= up.0 && crop.y + crop.h <= up.1, "crop outside the picture");
    let row = crop.w * 4;
    dst.clear();
    dst.resize(row * crop.h, 0);
    if crop.w == 0 || crop.h == 0 {
        return;
    }
    if rotation == Rotation::None {
        for (y, out) in dst.chunks_exact_mut(row).enumerate() {
            let at = (crop.y + y) * stride + crop.x * 4;
            out.copy_from_slice(&src[at..at + row]);
        }
        return;
    }
    // Bands of rows rotate in parallel: at 4K a single thread took about a frame's time.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 4);
    let band = crop.h.div_ceil(threads).div_ceil(TILE) * TILE;
    std::thread::scope(|s| {
        for (i, out) in dst.chunks_mut(band * row).enumerate() {
            s.spawn(move || {
                let rows = out.len() / row;
                rotate_band(src, up, stride, rotation, crop, i * band, rows, out);
            });
        }
    });
}

/// Rotates upright rows `y0..y0 + rows` of `crop` into `out`, a tile at a time.
#[allow(clippy::too_many_arguments)]
fn rotate_band(src: &[u8], up: (usize, usize), stride: usize, rotation: Rotation, crop: Crop, y0: usize, rows: usize, out: &mut [u8]) {
    // Bytes between the source pixels of neighbouring upright pixels in a row.
    let step: isize = match rotation {
        Rotation::None => 4,
        Rotation::Cw90 => -(stride as isize),
        Rotation::Cw180 => -4,
        Rotation::Cw270 => stride as isize,
    };
    let row = crop.w * 4;
    for ty in (0..rows).step_by(TILE) {
        for tx in (0..crop.w).step_by(TILE) {
            let tw = TILE.min(crop.w - tx);
            for y in ty..(ty + TILE).min(rows) {
                let (sx, sy) = rotation.source(up, crop.x + tx, crop.y + y0 + y);
                let mut at = (sy * stride + sx * 4) as isize;
                let dst = &mut out[y * row + tx * 4..][..tw * 4];
                for px in dst.chunks_exact_mut(4) {
                    let a = at as usize;
                    px.copy_from_slice(&src[a..a + 4]);
                    at += step;
                }
            }
        }
    }
}

/// Largest picture side H.264 hardware encoders take (level 5.1); bigger screens are halved.
const MAX_SIDE: usize = 4096;
/// Smallest picture side hardware encoders reliably take.
const MIN_SIDE: usize = 64;

/// Whether the hardware encoder halves the picture: asked to (`scale` of 0.5 or less), or
/// too big for H.264. Unlike OpenH264 it encodes 4K whole.
pub fn hardware_divisor((w, h): (usize, usize), scale: f64) -> usize {
    if scale <= 0.5 || w > MAX_SIDE || h > MAX_SIDE { 2 } else { 1 }
}

/// What the hardware encoder encodes a `w`×`h` picture at (even sides).
pub fn hardware_size((w, h): (usize, usize), scale: f64) -> (usize, usize) {
    let div = hardware_divisor((w, h), scale);
    ((w / div) & !1, (h / div) & !1)
}

/// Whether a picture this size can go to the hardware encoder (otherwise it's OpenH264's,
/// without giving up on hardware for other pictures).
pub fn hardware_fits(size: (usize, usize), scale: f64) -> bool {
    let (w, h) = hardware_size(size, scale);
    (MIN_SIDE..=MAX_SIDE).contains(&w) && (MIN_SIDE..=MAX_SIDE).contains(&h)
}

/// Converts `frame` to NV12 at `out` size (`div` box-filters it down): `out_buf` gets the
/// Y plane then interleaved U/V, `w * h * 3 / 2` bytes. The same BT.601 limited-range
/// conversion as OpenH264's input; `u` and `v` are scratch.
pub fn to_nv12(frame: &Frame, div: usize, (w, h): (usize, usize), out_buf: &mut [u8], u: &mut Vec<u8>, v: &mut Vec<u8>) {
    let chroma = (w / 2) * (h / 2);
    u.resize(chroma, 0);
    v.resize(chroma, 0);
    let (y, uv) = out_buf[..w * h + chroma * 2].split_at_mut(w * h);
    to_i420(frame, div, (w, h), y, u, v);
    for ((pair, &cu), &cv) in uv.chunks_exact_mut(2).zip(u.iter()).zip(v.iter()) {
        pair[0] = cu;
        pair[1] = cv;
    }
}

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// The NAL units of Annex B `data` (each without its start code).
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
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
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
            // A 4-byte start code's leading zero belongs to it, not to this NAL.
            while end > s && data[end - 1] == 0 && starts.get(n + 1).is_some() {
                end -= 1;
            }
            &data[s..end]
        })
        .filter(|n| !n.is_empty())
        .collect()
}

/// The latest SPS and PPS an encoder produced, to put before IDRs that come without them:
/// viewers decode each keyframe on its own (EncodedFrame promises parameter sets before
/// every keyframe), and Media Foundation encoders differ on whether they repeat them.
#[derive(Debug, Default)]
pub struct ParamSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSets {
    /// Remembers the parameter sets in Annex B `data` (an access unit, or the sequence
    /// header Media Foundation keeps on the output type).
    pub fn learn(&mut self, data: &[u8]) {
        for nal in nal_units(data) {
            match nal[0] & 0x1f {
                NAL_SPS => self.sps = Some(nal.to_vec()),
                NAL_PPS => self.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
    }

    pub fn known(&self) -> bool {
        self.sps.is_some() && self.pps.is_some()
    }

    /// One access unit as viewers want it, and whether it's a keyframe (has an IDR slice).
    pub fn fix(&mut self, data: Vec<u8>) -> (Vec<u8>, bool) {
        self.learn(&data);
        let nals = nal_units(&data);
        let has = |t: u8| nals.iter().any(|n| n[0] & 0x1f == t);
        let keyframe = has(NAL_IDR);
        let (missing_sps, missing_pps) = (!has(NAL_SPS), !has(NAL_PPS));
        if !keyframe || !(missing_sps || missing_pps) {
            return (data, keyframe);
        }
        let mut out = Vec::with_capacity(data.len() + 64);
        // An access unit delimiter has to stay first.
        let rest = match nals.first() {
            Some(n) if n[0] & 0x1f == NAL_AUD => {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(n);
                &nals[1..]
            }
            _ => &nals[..],
        };
        for (missing, set) in [(missing_sps, &self.sps), (missing_pps, &self.pps)] {
            if missing && let Some(set) = set {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(set);
            }
        }
        for nal in rest {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        (out, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soft_encode::PixelFormat;

    /// A `w`×`h` picture whose pixels hold their own coordinates, rows padded.
    fn coords(w: usize, h: usize, stride: usize) -> Vec<u8> {
        let mut data = vec![0xEE; stride * h];
        for y in 0..h {
            for x in 0..w {
                data[y * stride + x * 4..][..4].copy_from_slice(&[x as u8, y as u8, (x >> 8) as u8, (y >> 8) as u8]);
            }
        }
        data
    }

    fn at(pixels: &[u8], w: usize, x: usize, y: usize) -> (usize, usize) {
        let p = &pixels[(y * w + x) * 4..][..4];
        (p[0] as usize | (p[2] as usize) << 8, p[1] as usize | (p[3] as usize) << 8)
    }

    #[test]
    fn rotations_turn_the_picture_upright() {
        // Scanned out 3×2 (a portrait display's picture lies on its side).
        let src = coords(3, 2, 16);
        let all = |rotation: Rotation| {
            let (w, h) = rotation.upright((3, 2));
            let mut dst = Vec::new();
            blit(&src, (3, 2), 16, rotation, Crop { x: 0, y: 0, w, h }, &mut dst);
            (w, dst)
        };
        let (w, none) = all(Rotation::None);
        assert_eq!((at(&none, w, 0, 0), at(&none, w, 2, 1)), ((0, 0), (2, 1)));
        // A quarter turn clockwise: the source's left column becomes the top row, read
        // bottom to top.
        let (w, cw90) = all(Rotation::Cw90);
        assert_eq!(w, 2);
        assert_eq!([at(&cw90, w, 0, 0), at(&cw90, w, 1, 0), at(&cw90, w, 0, 2)], [(0, 1), (0, 0), (2, 1)]);
        let (w, cw180) = all(Rotation::Cw180);
        assert_eq!([at(&cw180, w, 0, 0), at(&cw180, w, 2, 1)], [(2, 1), (0, 0)]);
        let (w, cw270) = all(Rotation::Cw270);
        assert_eq!([at(&cw270, w, 0, 0), at(&cw270, w, 1, 0), at(&cw270, w, 0, 2)], [(2, 0), (2, 1), (0, 0)]);
    }

    #[test]
    fn rotating_four_quarter_turns_is_the_identity() {
        // Bigger than a tile, odd sizes, padded rows: exercises the tiled, banded path.
        let (w, h) = (75, 41);
        let src = coords(w, h, w * 4 + 8);
        let mut pic = src.clone();
        let (mut size, mut stride) = ((w, h), w * 4 + 8);
        for _ in 0..4 {
            let up = Rotation::Cw90.upright(size);
            let mut out = Vec::new();
            blit(&pic, size, stride, Rotation::Cw90, Crop { x: 0, y: 0, w: up.0, h: up.1 }, &mut out);
            (pic, size, stride) = (out, up, up.0 * 4);
        }
        let mut expect = Vec::new();
        blit(&src, (w, h), w * 4 + 8, Rotation::None, Crop { x: 0, y: 0, w, h }, &mut expect);
        assert_eq!(pic, expect);
    }

    #[test]
    fn crops_come_from_the_upright_picture() {
        let src = coords(50, 40, 200);
        let crop = Crop { x: 3, y: 5, w: 10, h: 7 };
        let mut dst = Vec::new();
        blit(&src, (50, 40), 200, Rotation::None, crop, &mut dst);
        assert_eq!((at(&dst, 10, 0, 0), at(&dst, 10, 9, 6)), ((3, 5), (12, 11)));
        // Upright 40×50 from a quarter turn: upright (x, y) is source (y, 39 - x).
        blit(&src, (50, 40), 200, Rotation::Cw90, crop, &mut dst);
        assert_eq!((at(&dst, 10, 0, 0), at(&dst, 10, 9, 6)), ((5, 36), (11, 27)));
    }

    #[test]
    fn hardware_encodes_4k_whole_and_halves_bigger() {
        assert_eq!(hardware_size((3840, 2160), 1.0), (3840, 2160));
        assert_eq!(hardware_size((5120, 2880), 1.0), (2560, 1440));
        assert_eq!(hardware_size((1921, 1081), 0.5), (960, 540));
        assert!(hardware_fits((1920, 1080), 1.0));
        assert!(!hardware_fits((60, 400), 1.0), "too narrow");
    }

    #[test]
    fn nv12_interleaves_the_i420_chroma() {
        let frame = Frame {
            width: 4,
            height: 2,
            stride: 16,
            format: PixelFormat::Bgrx,
            // Left half red, right half blue.
            data: [[0, 0, 255, 0], [0, 0, 255, 0], [255, 0, 0, 0], [255, 0, 0, 0]].concat().repeat(2),
        };
        let (mut y, mut u, mut v) = (vec![0; 8], vec![0; 2], vec![0; 2]);
        to_i420(&frame, 1, (4, 2), &mut y, &mut u, &mut v);
        let mut nv12 = vec![0; 12];
        let (mut su, mut sv) = (Vec::new(), Vec::new());
        to_nv12(&frame, 1, (4, 2), &mut nv12, &mut su, &mut sv);
        assert_eq!(&nv12[..8], &y[..]);
        assert_eq!(&nv12[8..], &[u[0], v[0], u[1], v[1]]);
        assert!(v[0] > 200 && u[1] > 200, "red has high V, blue high U");
    }

    #[test]
    fn nal_units_split_on_three_and_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 0];
        assert_eq!(nal_units(&data), vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 0][..]]);
    }

    #[test]
    fn idrs_get_the_parameter_sets_they_lack() {
        let mut sets = ParamSets::default();
        let first = vec![0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 1];
        let (out, key) = sets.fix(first.clone());
        assert!(key);
        assert_eq!(out, first, "already complete");
        let (out, key) = sets.fix(vec![0, 0, 1, 0x41, 2]);
        assert!(!key);
        assert_eq!(out, vec![0, 0, 1, 0x41, 2], "P frames pass through");
        // A later IDR without them, after an access unit delimiter.
        let (out, key) = sets.fix(vec![0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x65, 3]);
        assert!(key);
        let types: Vec<u8> = nal_units(&out).iter().map(|n| n[0] & 0x1f).collect();
        assert_eq!(types, [9, 7, 8, 5]);
        assert_eq!(nal_units(&out)[1], &[0x67, 0xAA]);
    }

    #[test]
    fn parameter_sets_can_come_from_the_sequence_header() {
        let mut sets = ParamSets::default();
        assert!(!sets.known());
        sets.learn(&[0, 0, 0, 1, 0x67, 7, 0, 0, 0, 1, 0x68, 8]);
        assert!(sets.known());
        let (out, _) = sets.fix(vec![0, 0, 0, 1, 0x65, 9]);
        let types: Vec<u8> = nal_units(&out).iter().map(|n| n[0] & 0x1f).collect();
        assert_eq!(types, [7, 8, 5]);
    }
}
