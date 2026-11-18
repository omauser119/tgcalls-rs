//! Bounded audio engine. Telegram authentication remains with the caller's MTProto client.
#![forbid(unsafe_code)]

pub mod audio_processing;
pub mod auth;
pub mod codec;
mod ice_io;
pub mod jitter;
pub mod negotiation;
pub mod packet;
#[cfg(feature = "async")]
pub mod personal;
pub mod sansio;
#[cfg(feature = "async")]
pub mod session;
pub mod signaling;
pub mod telegram;
pub mod video;
pub mod worker;

use serde::{Deserialize, Serialize};

pub const SAMPLE_RATE: usize = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME_SAMPLES: usize = 960;
pub const PCM_BYTES: usize = FRAME_SAMPLES * CHANNELS * 2;
pub const MAX_PACKET: usize = 1500;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("WebRTC: {0}")]
    WebRtc(#[from] rtc::shared::error::Error),
    #[error("codec: {0}")]
    Codec(String),
    #[error("operation timed out")]
    Timeout,
    #[error("connection closed")]
    Closed,
    #[error("internal panic contained at engine boundary")]
    Panic,
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Impairment {
    pub loss_percent: f64,
    pub jitter_ms: u64,
    pub delay_ms: u64,
    pub seed: u64,
}
impl Impairment {
    pub fn validate(&self) -> Result<()> {
        if !self.loss_percent.is_finite()
            || !(0.0..=100.0).contains(&self.loss_percent)
            || self.jitter_ms > 500
            || self.delay_ms > 2000
        {
            return Err(Error::Invalid("impairment exceeds limits".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioFrame {
    pub ssrc: u32,
    pub sequence: u16,
    pub timestamp: u32,
    pub concealed: bool,
    pub target_delay_ms: u32,
    pub pcm: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionStats {
    pub sent_frames: u64,
    pub received_frames: u64,
    pub concealed_frames: u64,
    pub dropped_frames: u64,
    pub connection_state: String,
    pub sent_video_frames: u64,
    pub received_video_frames: u64,
}
