//! System audio: not captured on Windows yet (WASAPI loopback to come).

pub use crate::media::AudioPackets;

pub const SUPPORTED: bool = false;

pub fn start(packets: AudioPackets) {
    let _ = packets;
}
