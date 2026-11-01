//! System audio: a WASAPI loopback of the default output, turned into the same 5 ms
//! 48 kHz stereo s16 packets the other hosts send. Opens again when the default output
//! changes or goes away; plays nothing (sends nothing) while nothing plays, since loopback
//! only delivers what's mixed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToBSTR};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Owned;

use crate::pcm::{Converter, Format, Packetizer};
pub use crate::media::AudioPackets;

pub const SUPPORTED: bool = true;

/// What we ask the engine for (it converts with AUTOCONVERTPCM), so the usual path has
/// nothing to resample or downmix.
const WANTED: WAVEFORMATEX = WAVEFORMATEX {
    wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
    nChannels: 2,
    nSamplesPerSec: glass_proto::AUDIO_SAMPLE_RATE,
    nAvgBytesPerSec: glass_proto::AUDIO_SAMPLE_RATE * 8,
    nBlockAlign: 8,
    wBitsPerSample: 32,
    cbSize: 0,
};
/// The engine's buffer (100 ns units). Drained every 10 ms or so; this is only headroom.
const BUFFER_HNS: i64 = 200 * 10_000;

pub fn start(packets: AudioPackets) {
    let spawned = std::thread::Builder::new().name("glass-audio".into()).spawn(move || run(packets));
    if let Err(e) = spawned {
        tracing::warn!("audio thread: {e}");
    }
}

/// COM for this thread (WASAPI and WinRT need it). Already being initialised, in either
/// mode, is fine.
pub fn com_init() {
    // SAFETY: no preconditions; the thread stays initialised for its lifetime.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

pub fn enumerator() -> Result<IMMDeviceEnumerator> {
    com_init();
    // SAFETY: COM is initialised on this thread.
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?)
}

/// "Speakers (Realtek(R) Audio)", "CABLE Input (VB-Audio Virtual Cable)".
pub fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: the property store and the variant are released before returning.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = PropVariantToBSTR(&value).ok().map(|b| b.to_string());
        let _ = PropVariantClear(&mut value);
        name
    }
}

pub fn device_id(device: &IMMDevice) -> Option<String> {
    // SAFETY: GetId hands us a CoTaskMemAlloc'd string, freed after copying it.
    unsafe {
        let id = device.GetId().ok()?;
        let s = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        s
    }
}

/// The outputs that are plugged in and enabled, with their names.
pub fn outputs() -> Vec<(IMMDevice, String)> {
    let Ok(enumerator) = enumerator() else { return Vec::new() };
    // SAFETY: plain COM calls on live interfaces.
    let Ok(all) = (unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }) else { return Vec::new() };
    // SAFETY: as above.
    let count = unsafe { all.GetCount() }.unwrap_or(0);
    (0..count)
        // SAFETY: `i` is below the collection's count.
        .filter_map(|i| unsafe { all.Item(i) }.ok())
        .filter_map(|d| {
            let name = friendly_name(&d)?;
            Some((d, name))
        })
        .collect()
}

/// An auto-reset event the engine signals when it has a buffer for us.
pub fn event() -> Result<Owned<HANDLE>> {
    // SAFETY: a fresh unnamed event, owned (and closed) by the returned value.
    unsafe { Ok(Owned::new(CreateEventW(None, false, false, None)?)) }
}

/// The client's format, read from what it reports.
pub fn read_format(wave: *const WAVEFORMATEX) -> Option<Format> {
    // SAFETY: `wave` points at a WAVEFORMATEX, followed by the extensible part when its tag
    // says so (cbSize covers it). Both are packed, hence the unaligned reads.
    unsafe {
        let w = std::ptr::read_unaligned(wave);
        let (sub, mask) = if w.wFormatTag == 0xFFFE && w.cbSize >= 22 {
            let x = std::ptr::read_unaligned(wave.cast::<WAVEFORMATEXTENSIBLE>());
            (Some(x.SubFormat.data1), x.dwChannelMask)
        } else {
            (None, 0)
        };
        Format::from_wave(w.wFormatTag, sub, w.nSamplesPerSec, w.nChannels, w.wBitsPerSample, w.nBlockAlign, mask)
    }
}

/// A shared-mode client on `device` with `flags` and a buffer of `buffer_hns` (100 ns
/// units): in our format, converted by the engine, or (when the engine won't) in the
/// device's own mix format, which we convert.
pub fn open(device: &IMMDevice, flags: u32, wanted: &WAVEFORMATEX, buffer_hns: i64) -> Result<(IAudioClient, Format)> {
    let auto = flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    // SAFETY: `wanted` is a complete WAVEFORMATEX that outlives the call.
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        if client.Initialize(AUDCLNT_SHAREMODE_SHARED, auto, buffer_hns, 0, wanted, None).is_ok() {
            return Ok((client, read_format(wanted).context("our own format")?));
        }
    }
    // A client that failed to initialise can't be used again: a fresh one, in the mix
    // format.
    // SAFETY: the mix format is CoTaskMemAlloc'd; it's read, used and freed here.
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;
        let format = read_format(mix);
        let result = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer_hns, 0, mix, None);
        CoTaskMemFree(Some(mix as *const _));
        result?;
        Ok((client, format.context("the output's format isn't PCM")?))
    }
}

fn run(packets: AudioPackets) {
    com_init();
    let mut packetizer = Packetizer::default();
    let mut failures = 0u32;
    let mut last_error = String::new();
    loop {
        match capture(&packets, &mut packetizer, &mut failures) {
            // The default output changed: open the new one right away.
            Ok(()) => {}
            Err(e) => {
                // Say why once, not every retry (no output plugged in, say), unless it
                // worked in between.
                let msg = format!("{e:#}");
                if failures == 0 || msg != last_error {
                    tracing::warn!("audio: {msg}; trying again");
                    last_error = msg;
                }
                failures += 1;
                // An output just unplugged usually has a replacement a moment later.
                std::thread::sleep(if failures == 1 { Duration::from_millis(200) } else { Duration::from_secs(2) });
            }
        }
    }
}

/// Captures the current default output until it stops being the default (Ok) or fails.
/// `failures` goes back to 0 once it's recording.
fn capture(packets: &AudioPackets, packetizer: &mut Packetizer, failures: &mut u32) -> Result<()> {
    let enumerator = enumerator()?;
    // SAFETY: a plain COM call on a live interface.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.context("no sound output")?;
    let id = device_id(&device);
    let name = friendly_name(&device).unwrap_or_else(|| "the default output".into());
    let event = event()?;
    // Woken by the engine when it has audio; older Windows refuse that for loopback, and
    // then the wait below is just a 10 ms poll.
    let (client, format, woken) = match open(&device, AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, &WANTED, BUFFER_HNS) {
        Ok((client, format)) => (client, format, true),
        Err(_) => {
            let (client, format) = open(&device, AUDCLNT_STREAMFLAGS_LOOPBACK, &WANTED, BUFFER_HNS)
                .with_context(|| format!("couldn't record {name}"))?;
            (client, format, false)
        }
    };
    // SAFETY: the client is initialised; the event is declared before it, so it's closed
    // after the client is released.
    let reader: IAudioCaptureClient = unsafe {
        if woken {
            client.SetEventHandle(*event)?;
        }
        let reader = client.GetService()?;
        client.Start()?;
        reader
    };
    tracing::info!("audio: recording {name} ({format})");
    *failures = 0;

    let mut converter = Converter::new(format);
    let frame_bytes = format.frame_bytes();
    let mut pcm = Vec::new();
    let mut silence = Vec::new();
    let mut checked = Instant::now();
    let mut heard = Instant::now();
    loop {
        // Some Windows versions never signal loopback events either: the timeout keeps it
        // going.
        // SAFETY: the event is open.
        unsafe { WaitForSingleObject(*event, 10) };
        loop {
            // SAFETY: a plain COM call on a live interface.
            if unsafe { reader.GetNextPacketSize() }? == 0 {
                break;
            }
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            // SAFETY: out pointers to locals; `data` is then valid for `frames` frames until
            // ReleaseBuffer.
            unsafe { reader.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }?;
            let len = frames as usize * frame_bytes;
            let bytes = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                silence.resize(len, 0);
                &silence[..]
            } else {
                // SAFETY: see GetBuffer above.
                unsafe { std::slice::from_raw_parts(data, len) }
            };
            converter.convert(bytes, &mut pcm);
            // SAFETY: hands back the buffer from GetBuffer, no longer used.
            unsafe { reader.ReleaseBuffer(frames) }?;
            // A gap (nothing played): start afresh rather than glue on a stale bit.
            if heard.elapsed() > Duration::from_millis(100) {
                packetizer.clear();
            }
            heard = Instant::now();
            packetizer.push(&pcm, |p| {
                // No receivers (nobody listening) is fine.
                let _ = packets.send(Arc::new(p));
            });
            pcm.clear();
        }
        if checked.elapsed() > Duration::from_secs(1) {
            checked = Instant::now();
            // SAFETY: a plain COM call on a live interface.
            let now = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.ok();
            if now.as_ref().and_then(device_id) != id {
                tracing::info!("audio: the default output changed");
                // SAFETY: as above.
                unsafe { client.Stop() }?;
                return Ok(());
            }
        }
    }
}
