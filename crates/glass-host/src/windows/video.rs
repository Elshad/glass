//! Desktop capture and encoding.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::broadcast;

use crate::media::VideoSource;
pub use crate::media::{AudioPackets, DisplayInfo, EncodedFrame, Layout};

pub struct VideoConfig {
    pub fps: i32,
    pub bitrate: i32,
    pub scale: f64,
}

pub struct Video;

impl Video {
    pub fn new(
        config: VideoConfig,
        frames: broadcast::Sender<Arc<EncodedFrame>>,
        audio: AudioPackets,
    ) -> Self {
        let _ = (config, frames, audio);
        Self
    }
}

impl VideoSource for Video {
    fn streams(&self) -> Vec<DisplayInfo> {
        Vec::new()
    }
    fn request_keyframe(&self, _stream: Option<u8>) {}
    fn set_layout(&self, _layout: Layout) -> Result<Vec<DisplayInfo>> {
        anyhow::bail!("screen capture isn't available on Windows yet")
    }
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(Vec::new())
    }
    fn set_bitrate(&self, _bps: u32) {}
}
