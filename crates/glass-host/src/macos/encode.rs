//! Hardware H.264 or HEVC encoding with VideoToolbox in low-latency rate-control mode.
//!
//! Output is Annex B (start-code delimited) with the parameter sets (SPS/PPS, plus VPS
//! for HEVC) prepended to every keyframe, which WebCodecs decodes without an
//! out-of-band `description`.

use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use anyhow::{Result, bail};
use block2::RcBlock;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMTimeInvalid, kCMVideoCodecType_H264,
    kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVImageBuffer, kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferColorPrimaries_P3_D65,
    kCVImageBufferTransferFunction_sRGB, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxAllowedFrameQP, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_HEVC_Main_AutoLevel, kVTProfileLevel_HEVC_Main42210_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const MAX_GOP_FRAMES: i32 = 900;

/// Frames the encoder skipped to stay within its rate limits, logged every few seconds.
fn note_skipped_frame() {
    use std::sync::Mutex;
    static SKIPPED: Mutex<(u32, Option<std::time::Instant>)> = Mutex::new((0, None));
    let mut s = SKIPPED.lock().unwrap();
    s.0 += 1;
    let since = *s.1.get_or_insert_with(std::time::Instant::now);
    if since.elapsed() >= std::time::Duration::from_secs(5) {
        tracing::info!(
            "encoder skipped {} frames in {:.0}s to stay within the bitrate",
            s.0,
            since.elapsed().as_secs_f64()
        );
        *s = (0, Some(std::time::Instant::now()));
    }
}

/// The average target, plus two ceilings: twice that over any second, so a runaway burst
/// can't swamp a slow link, and about two frames' worth over two frame intervals
/// (`rate::short_window`), so one frame can't take most of a second's bits and queue
/// everything behind it. The encoder meets that by coarsening the frame, which the loose
/// QP cap at low rates allows (a keyframe it can't fit is asked for again).
fn set_rate(session: &VTCompressionSession, bps: u32, fps: u32) {
    let bytes_per_sec = bps as f64 / 8.0;
    let (short_bytes, short_secs) = crate::rate::short_window(bps, fps);
    let limits = [
        CFNumber::new_f64(bytes_per_sec * 2.0),
        CFNumber::new_f64(1.0),
        CFNumber::new_f64(short_bytes),
        CFNumber::new_f64(short_secs),
    ];
    let limits = CFArray::from_retained_objects(&limits);
    let average = CFNumber::new_i32(bps as i32);
    for (key, value, what) in unsafe {
        [
            (
                kVTCompressionPropertyKey_AverageBitRate,
                &*average as &CFType,
                "bitrate",
            ),
            (
                kVTCompressionPropertyKey_DataRateLimits,
                &*limits as &CFType,
                "data rate limits",
            ),
        ]
    } {
        let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
        if status != 0 {
            tracing::warn!("set {what} ({bps} bps): {status}");
        }
    }
}

pub use crate::media::EncodedFrame;

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    hevc: bool,
    /// HEVC 4:2:2 (sharp text): fed BGRA, so colour isn't subsampled before encoding.
    rext: bool,
    force_keyframe: Arc<AtomicBool>,
    on_frame: Arc<dyn Fn(EncodedFrame) + Send + Sync>,
    /// The sharpness cap in force (MaxAllowedFrameQP; 0 = none), from `apply_policy`.
    qp_cap: AtomicU32,
    /// The cap is lifted for the keyframe just submitted; restore it on the next frame.
    cap_lifted: AtomicBool,
    /// Bytes produced, for sharing the bitrate between displays by activity.
    bytes: Arc<AtomicU64>,
    /// Size of the latest output frame (an idle refresh stops once these get tiny).
    last_size: Arc<AtomicU64>,
}

// VTCompressionSession is thread-safe; frames are submitted from the capture queue only.
unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

impl Encoder {
    pub fn new(
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        hevc: bool,
        rext: bool,
        // The picture is Display P3 (a wide-colour display), not sRGB.
        p3: bool,
        force_keyframe: Arc<AtomicBool>,
        on_frame: Arc<dyn Fn(EncodedFrame) + Send + Sync>,
    ) -> Result<Self> {
        // Sharp text: HEVC Main 4:2:2 10 (Range Extensions) keeps full horizontal colour
        // detail, which 4:2:0 halves. VideoToolbox has no public 4:4:4 profile.
        let rext = rext && hevc;
        let yes = CFBoolean::new(true);
        let spec_keys = unsafe {
            [
                kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
            ]
        };
        let spec = CFDictionary::<CFString, CFType>::from_slices(&spec_keys, &[yes, yes]);

        let mut raw: *mut VTCompressionSession = ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width,
                height,
                if hevc {
                    kCMVideoCodecType_HEVC
                } else {
                    kCMVideoCodecType_H264
                },
                Some(spec.as_opaque()),
                None,
                None,
                None,
                ptr::null_mut(),
                NonNull::from(&mut raw),
            )
        };
        if status != 0 || raw.is_null() {
            bail!("VTCompressionSessionCreate failed: {status}");
        }
        let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };

        let set = |key: &CFString, value: &CFType| {
            let status = unsafe { VTSessionSetProperty(&session, key, Some(value)) };
            if status != 0 {
                tracing::warn!("VTSessionSetProperty({key}) failed: {status}");
            }
            status
        };
        unsafe {
            set(kVTCompressionPropertyKey_RealTime, yes);
            set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                CFBoolean::new(false),
            );
            let profile_status = set(
                kVTCompressionPropertyKey_ProfileLevel,
                if rext {
                    kVTProfileLevel_HEVC_Main42210_AutoLevel
                } else if hevc {
                    kVTProfileLevel_HEVC_Main_AutoLevel
                } else {
                    kVTProfileLevel_H264_High_AutoLevel
                },
            );
            if rext && profile_status != 0 {
                session.invalidate();
                bail!("this encoder has no HEVC 4:2:2 profile ({profile_status})");
            }

            set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_i32(fps),
            );
            // Colour, written into the stream (VUI) so the browser shows it as captured:
            // Display P3 on a wide-colour display (not clipped to sRGB), else sRGB.
            set(
                kVTCompressionPropertyKey_ColorPrimaries,
                if p3 {
                    kCVImageBufferColorPrimaries_P3_D65
                } else {
                    kCVImageBufferColorPrimaries_ITU_R_709_2
                },
            );
            set(kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_sRGB);
            set(kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2);
            // Keyframes on demand (new client or reported loss), plus one before H.264's
            // frame_num wraps at 1024: Safari's hardware decoder fails on the wrap.
            set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                &CFNumber::new_i32(MAX_GOP_FRAMES),
            );
            set_rate(&session, bitrate as u32, fps.max(1) as u32);
            session.prepare_to_encode_frames();
        }

        Ok(Self {
            session,
            hevc,
            rext,
            force_keyframe,
            on_frame,
            qp_cap: AtomicU32::new(0),
            cap_lifted: AtomicBool::new(false),
            bytes: Arc::new(AtomicU64::new(0)),
            last_size: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Applies the sharpness policy for `bps` (see `rate::sharp_policy`): the bitrate, the
    /// expected frame rate and a cap on how coarse frames may get. Returns the frame-rate
    /// ceiling for capture (at most `max_fps`: what the display and viewers can show).
    pub fn apply_policy(&self, bps: u32, max_fps: u32) -> u32 {
        let (fps, qp) = crate::rate::sharp_policy(bps, max_fps);
        set_rate(&self.session, bps, fps);
        self.set(
            unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
            fps as i32,
            "frame rate",
        );
        if self.qp_cap.swap(qp, Ordering::Relaxed) != qp && !self.cap_lifted.load(Ordering::Relaxed)
        {
            self.set(
                unsafe { kVTCompressionPropertyKey_MaxAllowedFrameQP },
                qp as i32,
                "QP cap",
            );
        }
        fps
    }

    /// Capture should deliver BGRA (full colour) rather than 4:2:0, for HEVC 4:2:2.
    pub fn wants_bgra(&self) -> bool {
        self.rext
    }

    fn set(&self, key: &CFString, value: i32, what: &str) {
        let n = CFNumber::new_i32(value);
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(&n)) };
        if status != 0 {
            tracing::warn!("set {what} ({value}): {status}");
        }
    }

    /// Bytes produced since the last call.
    pub fn take_bytes(&self) -> u64 {
        self.bytes.swap(0, Ordering::Relaxed)
    }

    pub fn last_frame_size(&self) -> u64 {
        self.last_size.load(Ordering::Relaxed)
    }

    pub fn keyframe_pending(&self) -> bool {
        self.force_keyframe.load(Ordering::Relaxed)
    }

    /// `refresh`: the picture is unchanged since the last encode (see FLAG_REFRESH).
    pub fn encode(&self, image: &CVImageBuffer, pts: CMTime, capture_us: u64, refresh: bool) {
        let keyframe = self.force_keyframe.swap(false, Ordering::Relaxed);
        // Keyframes go uncapped: capping them makes them bigger, not sharper (measured
        // 661 → 981 KB), which stalls a slow link. The cap returns on the next frame.
        let cap = self.qp_cap.load(Ordering::Relaxed);
        if keyframe && cap != 0 {
            self.set(
                unsafe { kVTCompressionPropertyKey_MaxAllowedFrameQP },
                51,
                "QP cap",
            );
            self.cap_lifted.store(true, Ordering::Relaxed);
        } else if !keyframe && self.cap_lifted.swap(false, Ordering::Relaxed) && cap != 0 {
            self.set(
                unsafe { kVTCompressionPropertyKey_MaxAllowedFrameQP },
                cap as i32,
                "QP cap",
            );
        }
        let props = keyframe.then(|| {
            let keys = unsafe { [kVTEncodeFrameOptionKey_ForceKeyFrame] };
            CFDictionary::<CFString, CFType>::from_slices(&keys, &[CFBoolean::new(true)])
        });
        let on_frame = self.on_frame.clone();
        let force_keyframe = self.force_keyframe.clone();
        let hevc = self.hevc;
        let (bytes, last_size) = (self.bytes.clone(), self.last_size.clone());
        let handler = RcBlock::new(
            move |status: i32, _flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer| {
                if status != 0 {
                    tracing::warn!("encode failed: {status}");
                    return;
                }
                if sample.is_null() {
                    // Dropped by rate control to stay within the limits: count, don't shout.
                    // A keyframe someone is waiting for is asked for again.
                    note_skipped_frame();
                    if keyframe {
                        force_keyframe.store(true, Ordering::Relaxed);
                    }
                    return;
                }
                if let Some(frame) = unsafe { annex_b(&*sample, hevc) } {
                    bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                    last_size.store(frame.data.len() as u64, Ordering::Relaxed);
                    on_frame(EncodedFrame {
                        capture_us,
                        refresh,
                        ..frame
                    });
                }
            },
        );
        let status = unsafe {
            self.session.encode_frame_with_output_handler(
                image,
                pts,
                CMTime::new(0, 0),
                props.as_deref().map(|d| d.as_opaque()),
                ptr::null_mut(),
                RcBlock::as_ptr(&handler),
            )
        };
        if status != 0 {
            tracing::warn!("VTCompressionSessionEncodeFrame failed: {status}");
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Flush and stop so no late frames from this session follow the next encoder's
        // keyframe (the client would decode them against the wrong reference).
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
        }
    }
}

/// True when HEVC 4:2:2 (Range Extensions) hardware encoding works here (probed once).
pub fn supports_rext() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let ok = supports_hevc()
            && Encoder::new(
                640,
                360,
                30,
                1_000_000,
                true,
                true,
                false,
                Arc::new(AtomicBool::new(false)),
                Arc::new(|_| {}),
            )
            .is_ok();
        tracing::info!(
            "HEVC 4:2:2 hardware encoding: {}",
            if ok { "available" } else { "unavailable" }
        );
        ok
    })
}

/// True when HEVC hardware encoding works here (probed once, with a tiny session).
pub fn supports_hevc() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let ok = Encoder::new(
            640,
            360,
            30,
            1_000_000,
            true,
            false,
            false,
            Arc::new(AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
        .is_ok();
        tracing::info!(
            "HEVC hardware encoding: {}",
            if ok { "available" } else { "unavailable" }
        );
        ok
    })
}

/// Whether a NAL unit (header first) starts a keyframe: an IDR slice in H.264, any
/// random-access (IRAP, types 16–23) picture in HEVC.
fn is_keyframe_nal(nal: &[u8], hevc: bool) -> bool {
    match nal.first() {
        Some(h) if hevc => (16..=23).contains(&((h >> 1) & 0x3f)),
        Some(h) => h & 0x1f == 5,
        None => false,
    }
}

/// Converts VideoToolbox's AVCC/HVCC (4-byte length-prefixed) output to Annex B.
unsafe fn annex_b(sample: &CMSampleBuffer, hevc: bool) -> Option<EncodedFrame> {
    let block = unsafe { sample.data_buffer() }?;
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    let status =
        unsafe { block.copy_data_bytes(0, len, NonNull::new_unchecked(avcc.as_mut_ptr().cast())) };
    if status != 0 {
        return None;
    }

    let mut out = Vec::with_capacity(len + 64);
    let mut keyframe = false;
    let mut i = 0;
    while i + 4 <= avcc.len() {
        let n = u32::from_be_bytes(avcc[i..i + 4].try_into().unwrap()) as usize;
        let nal = avcc.get(i + 4..i + 4 + n)?;
        if is_keyframe_nal(nal, hevc) {
            keyframe = true;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
        i += 4 + n;
    }

    if keyframe {
        let format = unsafe { sample.format_description() }?;
        let mut params = Vec::new();
        // H.264: SPS, PPS. HEVC: VPS, SPS, PPS.
        for idx in 0..if hevc { 3 } else { 2 } {
            params.extend_from_slice(&START_CODE);
            params.extend_from_slice(unsafe { parameter_set(&format, idx, hevc) }?);
        }
        out.splice(0..0, params);
    }

    Some(EncodedFrame {
        stream: 0,
        data: out,
        keyframe,
        capture_us: 0,
        hevc,
        refresh: false,
    })
}

/// H.264: index 0 is the SPS, 1 the PPS. HEVC: 0 VPS, 1 SPS, 2 PPS.
unsafe fn parameter_set(format: &CMFormatDescription, idx: usize, hevc: bool) -> Option<&[u8]> {
    let mut ptr: *const u8 = ptr::null();
    let mut size = 0usize;
    let get = if hevc {
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex
    } else {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex
    };
    let status = unsafe {
        get(
            format,
            idx,
            &mut ptr,
            &mut size,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    (status == 0 && !ptr.is_null()).then(|| unsafe { std::slice::from_raw_parts(ptr, size) })
}

#[cfg(test)]
mod tests {
    #[test]
    fn keyframe_nals() {
        // H.264: IDR (5) yes, non-IDR slice (1) no.
        assert!(super::is_keyframe_nal(&[0x65], false));
        assert!(!super::is_keyframe_nal(&[0x41], false));
        // HEVC: IDR_W_RADL (19) and CRA (21) yes, TRAIL_R (1) and SPS (33) no.
        assert!(super::is_keyframe_nal(&[19 << 1, 1], true));
        assert!(super::is_keyframe_nal(&[21 << 1, 1], true));
        assert!(!super::is_keyframe_nal(&[1 << 1, 1], true));
        assert!(!super::is_keyframe_nal(&[33 << 1, 1], true));
    }
}
