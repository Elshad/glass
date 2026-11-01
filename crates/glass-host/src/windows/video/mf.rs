//! Hardware H.264 through Media Foundation: the GPU vendor's encoder MFT (NVENC, Quick
//! Sync, AMF). The CPU converts each captured picture to NV12 (the converter OpenH264
//! uses) and the GPU encodes it, in low-latency CBR with no B-frames, so each frame comes
//! out before the next goes in. Hardware MFTs are mostly asynchronous: they say when they
//! want input and have output through their event queue, which is polled here so nothing
//! outlives the encoder. Output is Annex B with SPS/PPS before every IDR (put there when
//! the MFT leaves them out).

use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{VARIANT_BOOL, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFNominalRange_16_235, MFSTARTUP_LITE, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFVideoTransferMatrix_BT601, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_High, eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface, PWSTR};

use crate::soft_encode::Frame;
use crate::win_capture::{ParamSets, hardware_divisor, hardware_size, to_nv12};

/// Matches the macOS and OpenH264 encoders: a periodic IDR.
const MAX_GOP_FRAMES: u32 = 900;
/// How long to wait for a frame's output before giving up on the encoder. The first frame
/// gets longer: some encoders set up their sessions then.
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(500);
const FIRST_OUTPUT_TIMEOUT: Duration = Duration::from_secs(3);
/// Between looks at an asynchronous MFT's event queue.
const EVENT_POLL: Duration = Duration::from_micros(500);

/// Media Foundation, started once per process.
fn startup() -> Result<()> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| {
            // SAFETY: no preconditions; never shut down (the host uses it until it exits).
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| anyhow!("MFStartup: {e}"))
}

/// The hardware H.264 encoders, best first (MFT_ENUM_FLAG_SORTANDFILTER: the vendor's own
/// before others), with their names.
fn hardware_encoders() -> Result<Vec<(IMFActivate, String)>> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let (mut list, mut count): (*mut Option<IMFActivate>, u32) = (std::ptr::null_mut(), 0);
    // SAFETY: MFTEnumEx fills list with count activation objects in memory we free with
    // CoTaskMemFree once each has been moved out.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )
        .context("MFTEnumEx")?;
        if list.is_null() {
            return Ok(Vec::new());
        }
        let found = std::slice::from_raw_parts_mut(list, count as usize)
            .iter_mut()
            .filter_map(Option::take)
            .map(|a| {
                let name = friendly_name(&a).unwrap_or_else(|| "hardware H.264 encoder".into());
                (a, name)
            })
            .collect();
        CoTaskMemFree(Some(list as *const _));
        Ok(found)
    }
}

fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let (mut name, mut len) = (PWSTR::null(), 0u32);
    // SAFETY: GetAllocatedString hands over a CoTaskMemAlloc'd string, freed here.
    unsafe {
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        s
    }
}

/// Whether any hardware H.264 encoder is installed (asked once).
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        com_init();
        startup().is_ok() && hardware_encoders().is_ok_and(|l| !l.is_empty())
    })
}

/// Joins the thread to the multithreaded apartment (MFTs are created through COM).
fn com_init() {
    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    DONE.with(|done| {
        if !done.replace(true) {
            // SAFETY: no preconditions; an apartment the thread already has is kept.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        }
    });
}

fn variant_u32(value: u32) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: writing a VT_UI4 VARIANT's tag and value; it owns nothing to drop.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    v
}

fn variant_bool(value: bool) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: as above, for VT_BOOL.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if value { VARIANT_TRUE } else { VARIANT_BOOL(0) };
    }
    v
}

/// Sets a codec property, saying so when the encoder doesn't take it (most are hints).
fn set(codec: &ICodecAPI, api: &GUID, value: VARIANT, what: &str) -> bool {
    // SAFETY: both pointers are valid for the call.
    match unsafe { codec.SetValue(api, &value) } {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!("hardware encoder: {what} not set: {e}");
            false
        }
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    (hi as u64) << 32 | lo as u64
}

pub struct MfEncoder {
    activate: IMFActivate,
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    /// Asynchronous MFTs' event queue; `None` for synchronous ones.
    events: Option<IMFMediaEventGenerator>,
    /// Inputs the MFT has asked for and not yet been given (METransformNeedInput).
    wants_input: u32,
    /// The MFT allocates its output samples (else each output needs one of `output_size`).
    provides_samples: bool,
    output_size: u32,
    source: (usize, usize),
    div: usize,
    out: (usize, usize),
    fps: u32,
    u: Vec<u8>,
    v: Vec<u8>,
    sets: ParamSets,
    started: Instant,
    last_time: i64,
    frames: u64,
    pub name: String,
}

impl MfEncoder {
    pub fn new(width: usize, height: usize, fps: u32, bitrate: u32, scale: f64) -> Result<Self> {
        com_init();
        startup()?;
        let encoders = hardware_encoders()?;
        anyhow::ensure!(!encoders.is_empty(), "no hardware H.264 encoder");
        let mut errors = Vec::new();
        for (activate, name) in encoders {
            match Self::open(activate, &name, width, height, fps, bitrate, scale) {
                Ok(e) => return Ok(e),
                Err(e) => errors.push(format!("{name}: {e:#}")),
            }
        }
        bail!("{}", errors.join("; "))
    }

    fn open(activate: IMFActivate, name: &str, width: usize, height: usize, fps: u32, bitrate: u32, scale: f64) -> Result<Self> {
        let div = hardware_divisor((width, height), scale);
        let (w, h) = hardware_size((width, height), scale);
        let fps = fps.max(1);
        // SAFETY: COM calls on interfaces we hold; the activation object is shut down when
        // the encoder drops (or here, on failure).
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().context("ActivateObject")?;
            let mut this = Self {
                activate,
                transform: transform.clone(),
                codec: transform.cast::<ICodecAPI>().ok(),
                events: None,
                wants_input: 0,
                provides_samples: false,
                output_size: 0,
                source: (width, height),
                div,
                out: (w, h),
                fps,
                u: Vec::new(),
                v: Vec::new(),
                sets: ParamSets::default(),
                started: Instant::now(),
                last_time: -1,
                frames: 0,
                name: name.to_string(),
            };
            if let Ok(attrs) = transform.GetAttributes() {
                // Asynchronous MFTs refuse every call until unlocked.
                if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("unlocking the async MFT")?;
                    this.events = Some(transform.cast().context("async MFT without an event queue")?);
                }
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            // Rate control has to be set before the media types on some encoders.
            if let Some(codec) = &this.codec {
                set(codec, &CODECAPI_AVLowLatencyMode, variant_bool(true), "low latency");
                set(codec, &CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32), "CBR");
                set(codec, &CODECAPI_AVEncCommonMeanBitRate, variant_u32(bitrate), "bitrate");
                set(codec, &CODECAPI_AVEncMPVGOPSize, variant_u32(MAX_GOP_FRAMES), "GOP size");
                set(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0), "no B-frames");
            }
            this.set_output_type(bitrate)?;
            this.set_input_type()?;
            let info = transform.GetOutputStreamInfo(0).context("GetOutputStreamInfo")?;
            let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
            this.provides_samples = info.dwFlags & provides != 0;
            this.output_size = info.cbSize.max((w * h * 3 / 2) as u32);
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).context("begin streaming")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).context("start of stream")?;
            Ok(this)
        }
    }

    /// H.264 out: High profile like the macOS encoder, else whatever the MFT offers.
    unsafe fn set_output_type(&self, bitrate: u32) -> Result<()> {
        let (w, h) = self.out;
        let mut last = None;
        for profile in [Some(eAVEncH264VProfile_High.0), Some(eAVEncH264VProfile_Main.0), None] {
            // SAFETY: plain attribute calls on a media type we just made.
            let result = unsafe {
                let t = MFCreateMediaType()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
                t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w as u32, h as u32))?;
                t.SetUINT64(&MF_MT_FRAME_RATE, pack(self.fps, 1))?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                if let Some(p) = profile {
                    t.SetUINT32(&MF_MT_MPEG2_PROFILE, p as u32)?;
                }
                self.transform.SetOutputType(0, &t, 0)
            };
            match result {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.expect("tried at least once")).context("SetOutputType (H.264)")
    }

    /// NV12 in, at the output size. Some MFTs only take a type built from their own list.
    unsafe fn set_input_type(&self) -> Result<()> {
        let (w, h) = self.out;
        let fill = |t: &IMFMediaType| -> windows::core::Result<()> {
            // SAFETY: plain attribute calls.
            unsafe {
                t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w as u32, h as u32))?;
                t.SetUINT64(&MF_MT_FRAME_RATE, pack(self.fps, 1))?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                // What to_nv12 produces.
                let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
                let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32);
                Ok(())
            }
        };
        // SAFETY: COM calls on the transform and media types we hold.
        unsafe {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            fill(&t)?;
            let Err(first) = self.transform.SetInputType(0, &t, 0) else {
                return Ok(());
            };
            for i in 0.. {
                let Ok(t) = self.transform.GetInputAvailableType(0, i) else { break };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    fill(&t)?;
                    return self.transform.SetInputType(0, &t, 0).context("SetInputType (offered NV12)");
                }
            }
            Err(first).context("SetInputType (NV12)")
        }
    }

    pub fn source_size(&self) -> (usize, usize) {
        self.source
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Changes the target bitrate on the fly (no new keyframe).
    pub fn set_bitrate(&mut self, bps: u32) {
        if let Some(codec) = &self.codec {
            set(codec, &CODECAPI_AVEncCommonMeanBitRate, variant_u32(bps), "bitrate");
        }
    }

    /// Returns Annex B data and whether it's a keyframe, or None if nothing came out.
    pub fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Option<(Vec<u8>, bool)>> {
        let sample = self.input_sample(frame)?;
        if force_keyframe
            && let Some(codec) = &self.codec
        {
            set(codec, &CODECAPI_AVEncVideoForceKeyFrame, variant_u32(1), "keyframe");
        }
        let timeout = if self.frames == 0 { FIRST_OUTPUT_TIMEOUT } else { OUTPUT_TIMEOUT };
        self.frames += 1;
        let data = match self.events.is_some() {
            true => self.encode_async(&sample, timeout)?,
            false => self.encode_sync(&sample)?,
        };
        if data.is_empty() {
            return Ok(None);
        }
        if !self.sets.known() {
            self.learn_sequence_header();
        }
        let (data, keyframe) = self.sets.fix(data);
        Ok(Some((data, keyframe)))
    }

    /// The frame in NV12, in a sample timed by the wall clock (rate control counts bits
    /// per second, and frames come irregularly).
    fn input_sample(&mut self, frame: &Frame) -> Result<IMFSample> {
        let (w, h) = self.out;
        let len = w * h * 3 / 2;
        let mut time = self.started.elapsed().as_nanos() as i64 / 100;
        if time <= self.last_time {
            time = self.last_time + 1;
        }
        self.last_time = time;
        // SAFETY: the buffer is locked for exactly len bytes while it's written.
        unsafe {
            let buffer = MFCreateMemoryBuffer(len as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            to_nv12(frame, self.div, (w, h), std::slice::from_raw_parts_mut(ptr, len), &mut self.u, &mut self.v);
            buffer.Unlock()?;
            buffer.SetCurrentLength(len as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(10_000_000 / self.fps as i64)?;
            Ok(sample)
        }
    }

    fn encode_sync(&mut self, sample: &IMFSample) -> Result<Vec<u8>> {
        // SAFETY: COM call with a sample we hold.
        unsafe { self.transform.ProcessInput(0, sample, 0) }.context("ProcessInput")?;
        let mut data = Vec::new();
        while let Some(out) = self.output()? {
            data.extend_from_slice(&out);
        }
        Ok(data)
    }

    /// Gives the MFT the frame when it asks for one, then takes what it makes of it.
    fn encode_async(&mut self, sample: &IMFSample, timeout: Duration) -> Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut data = Vec::new();
        let mut given = false;
        loop {
            if !given && self.wants_input > 0 {
                // SAFETY: COM call with a sample we hold.
                unsafe { self.transform.ProcessInput(0, sample, 0) }.context("ProcessInput")?;
                self.wants_input -= 1;
                given = true;
            }
            match self.next_event()? {
                Some(t) if t == METransformNeedInput.0 as u32 => self.wants_input += 1,
                Some(t) if t == METransformHaveOutput.0 as u32 => {
                    if let Some(out) = self.output()? {
                        data.extend_from_slice(&out);
                    }
                }
                Some(_) => {}
                None if given && !data.is_empty() => return Ok(data),
                None if Instant::now() >= deadline => {
                    bail!("no output within {} ms", timeout.as_millis())
                }
                None => std::thread::sleep(EVENT_POLL),
            }
        }
    }

    /// The next event type from an asynchronous MFT's queue, if there is one.
    fn next_event(&self) -> Result<Option<u32>> {
        let events = self.events.as_ref().expect("async MFT");
        // SAFETY: COM call; NO_WAIT returns at once.
        match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            // SAFETY: COM call on the event we just got.
            Ok(event) => Ok(Some(unsafe { event.GetType() }?)),
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => Ok(None),
            Err(e) => Err(e).context("GetEvent"),
        }
    }

    /// One output sample's bytes, or None when the MFT needs more input first.
    fn output(&mut self) -> Result<Option<Vec<u8>>> {
        for _ in 0..2 {
            // SAFETY: COM calls; the output buffer's sample and events are taken back out
            // of their ManuallyDrop wrappers after the call, so they're released once.
            unsafe {
                let mine = if self.provides_samples {
                    None
                } else {
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                    Some(sample)
                };
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(mine),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
                let sample = ManuallyDrop::take(&mut buffers[0].pSample);
                drop(ManuallyDrop::take(&mut buffers[0].pEvents));
                match result {
                    Ok(()) => return Ok(sample.map(|s| read_sample(&s)).transpose()?),
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                    // The MFT settled its output format (often on the first frame): take
                    // what it offers and ask again.
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        let t = self.transform.GetOutputAvailableType(0, 0).context("new output type")?;
                        self.transform.SetOutputType(0, &t, 0).context("SetOutputType after a stream change")?;
                    }
                    Err(e) => return Err(e).context("ProcessOutput"),
                }
            }
        }
        bail!("the output format kept changing")
    }

    /// Parameter sets from the output type (Annex B), for MFTs that send them only there.
    fn learn_sequence_header(&mut self) {
        // SAFETY: COM calls; the blob is copied into a buffer of its reported size.
        unsafe {
            let Ok(t) = self.transform.GetOutputCurrentType(0) else { return };
            let Ok(size) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return };
            let mut blob = vec![0; size as usize];
            if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None).is_ok() {
                self.sets.learn(&blob);
            }
        }
    }
}

/// A sample's bytes, all its buffers joined.
fn read_sample(sample: &IMFSample) -> Result<Vec<u8>> {
    // SAFETY: the buffer is locked while its current length is copied out.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock()?;
        Ok(data)
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // SAFETY: COM calls on interfaces we hold; failures don't matter on the way out.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}
