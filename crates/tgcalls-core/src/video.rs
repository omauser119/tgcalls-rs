//! H.264 encode/decode uses Remade-With-Rust/rusty_h264 exclusively.
//! Bounded RTP reassembly and SPS inspection precede decoder allocation.
use crate::{Error, Result};
use rusty_h264::{Decoder, Encoder, EncoderConfig, YuvPlanes};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const MAX_VIDEO_BYTES: usize = 1280 * 720 * 3 / 2;
pub const MAX_ACCESS_UNIT: usize = 1024 * 1024;
pub const H264_FMTP: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}
impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            fps: 30,
            bitrate: 800_000,
        }
    }
}
impl VideoConfig {
    pub fn validate(&self) -> Result<()> {
        if self.width < 16
            || self.height < 16
            || self.width > 1280
            || self.height > 720
            || !self.width.is_multiple_of(16)
            || !self.height.is_multiple_of(16)
            || !(1..=60).contains(&self.fps)
            || u64::from(self.width / 16) * u64::from(self.height / 16) * u64::from(self.fps)
                > 108_000
            || !(16_000..=10_000_000).contains(&self.bitrate)
        {
            return Err(Error::Invalid(
                "video requires 16-aligned dimensions <=1280x720, 1..60 fps within H264 level 3.1, bounded bitrate"
                    .into(),
            ));
        }
        Ok(())
    }
    pub fn bytes(&self) -> usize {
        self.width as usize * self.height as usize * 3 / 2
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoFrame {
    pub ssrc: u32,
    pub timestamp: u32,
    pub width: u32,
    pub height: u32,
    pub i420: Vec<u8>,
}

pub struct VideoEncoder {
    encoder: Encoder,
    config: VideoConfig,
}
impl VideoEncoder {
    pub fn new(config: VideoConfig) -> Result<Self> {
        config.validate()?;
        // Baseline disables CABAC, B-frames and lookahead: every input produces an
        // access unit immediately and matches the negotiated Constrained Baseline profile.
        let mut options = EncoderConfig::baseline(config.width as usize, config.height as usize);
        options.framerate = config.fps as f32;
        options.bitrate = config.bitrate;
        options.gop_size = config.fps;
        options.level_idc = 31;
        let encoder = Encoder::new(options).map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self { encoder, config })
    }
    pub fn encode(&mut self, i420: &[u8]) -> Result<Vec<u8>> {
        if i420.len() != self.config.bytes() {
            return Err(Error::Invalid("I420 frame size mismatch".into()));
        }
        let y = self.config.width as usize * self.config.height as usize;
        let frame = YuvPlanes::new(
            self.config.width as usize,
            self.config.height as usize,
            &i420[..y],
            &i420[y..y + y / 4],
            &i420[y + y / 4..],
            self.config.width as usize,
            self.config.width as usize / 2,
        )
        .ok_or_else(|| Error::Invalid("invalid I420 planes".into()))?;
        let bytes = self
            .encoder
            .encode_planes(&frame)
            .map_err(|e| Error::Codec(e.to_string()))?;
        if bytes.len() > MAX_ACCESS_UNIT {
            return Err(Error::Invalid(
                "encoded video exceeds access-unit budget".into(),
            ));
        }
        Ok(bytes)
    }
    pub fn request_keyframe(&mut self) {
        self.encoder.request_keyframe();
    }
    pub fn config(&self) -> &VideoConfig {
        &self.config
    }
}

pub struct VideoDecoder {
    decoder: Decoder,
    has_sps: bool,
}
impl Default for VideoDecoder {
    fn default() -> Self {
        Self {
            decoder: Decoder::new(),
            has_sps: false,
        }
    }
}
impl VideoDecoder {
    pub fn decode(
        &mut self,
        ssrc: u32,
        timestamp: u32,
        access_unit: &[u8],
    ) -> Result<Option<VideoFrame>> {
        if access_unit.len() > MAX_ACCESS_UNIT {
            return Err(Error::Invalid("oversized H264 access unit".into()));
        }
        for nal in annex_b_nals(access_unit)? {
            if nal.first().is_some_and(|b| b & 31 == 7) {
                validate_sps(&nal[1..])?;
                self.has_sps = true;
            }
        }
        if !self.has_sps {
            return Ok(None);
        }
        let decoded = self
            .decoder
            .decode(access_unit)
            .map_err(|e| Error::Codec(e.to_string()))?;
        let Some(frame) = decoded else {
            return Ok(None);
        };
        if frame.width > 1280 || frame.height > 720 || frame.width == 0 || frame.height == 0 {
            return Err(Error::Invalid("decoded dimensions exceed budget".into()));
        }
        let mut i420 = Vec::with_capacity(frame.width * frame.height * 3 / 2);
        i420.extend_from_slice(&frame.y);
        i420.extend_from_slice(&frame.u);
        i420.extend_from_slice(&frame.v);
        if i420.len() > MAX_VIDEO_BYTES {
            return Err(Error::Invalid("decoded frame exceeds budget".into()));
        }
        Ok(Some(VideoFrame {
            ssrc,
            timestamp,
            width: frame.width as u32,
            height: frame.height as u32,
            i420,
        }))
    }
}

pub fn annex_b_nals(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    if bytes.len() > MAX_ACCESS_UNIT {
        return Err(Error::Invalid("oversized Annex-B".into()));
    }
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let length = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        starts.push((index, index + length));
        index += length;
        if starts.len() > 256 {
            return Err(Error::Invalid("NAL count exceeds budget".into()));
        }
    }
    let mut result = Vec::new();
    for (i, (_, start)) in starts.iter().enumerate() {
        let end = starts
            .get(i + 1)
            .map_or(bytes.len(), |(position, _)| *position);
        if *start < end {
            result.push(&bytes[*start..end]);
        }
    }
    Ok(result)
}

struct Bits {
    data: Vec<u8>,
    position: usize,
}
impl Bits {
    fn bit(&mut self) -> Result<u32> {
        let byte = self
            .data
            .get(self.position / 8)
            .ok_or_else(|| Error::Invalid("truncated SPS".into()))?;
        let result = u32::from((byte >> (7 - self.position % 8)) & 1);
        self.position += 1;
        Ok(result)
    }
    fn read(&mut self, count: u32) -> Result<u32> {
        let mut x = 0;
        for _ in 0..count {
            x = (x << 1) | self.bit()?;
        }
        Ok(x)
    }
    fn ue(&mut self) -> Result<u32> {
        let mut zeroes = 0;
        while self.bit()? == 0 {
            zeroes += 1;
            if zeroes > 24 {
                return Err(Error::Invalid("SPS integer exceeds budget".into()));
            }
        }
        Ok((1u32 << zeroes) - 1 + self.read(zeroes)?)
    }
}

fn validate_sps(escaped: &[u8]) -> Result<()> {
    if escaped.len() > 4096 {
        return Err(Error::Invalid("oversized SPS".into()));
    }
    let mut data = Vec::with_capacity(escaped.len());
    let mut zeros = 0;
    for byte in escaped {
        if zeros >= 2 && *byte == 3 {
            zeros = 0;
            continue;
        }
        data.push(*byte);
        zeros = if *byte == 0 { zeros + 1 } else { 0 };
    }
    let mut b = Bits { data, position: 0 };
    if b.read(8)? != 66 {
        return Err(Error::Invalid(
            "only negotiated H264 Baseline profile is accepted".into(),
        ));
    }
    b.read(8)?;
    if b.read(8)? > 31 || b.ue()? > 31 || b.ue()? > 12 {
        return Err(Error::Invalid("unsupported SPS level/id".into()));
    }
    match b.ue()? {
        0 => {
            if b.ue()? > 12 {
                return Err(Error::Invalid("invalid POC bits".into()));
            }
        }
        1 => {
            b.bit()?;
            b.ue()?;
            b.ue()?;
            let count = b.ue()?;
            if count > 255 {
                return Err(Error::Invalid("SPS POC budget".into()));
            }
            for _ in 0..count {
                b.ue()?;
            }
        }
        2 => {}
        _ => return Err(Error::Invalid("invalid SPS POC type".into())),
    }
    if b.ue()? > 4 {
        return Err(Error::Invalid(
            "H264 reference-frame budget exceeded".into(),
        ));
    }
    b.bit()?;
    let width = b
        .ue()?
        .checked_add(1)
        .and_then(|v| v.checked_mul(16))
        .ok_or_else(|| Error::Invalid("SPS width overflow".into()))?;
    let height = b
        .ue()?
        .checked_add(1)
        .and_then(|v| v.checked_mul(16))
        .ok_or_else(|| Error::Invalid("SPS height overflow".into()))?;
    if b.bit()? != 1 || width > 1280 || height > 720 {
        return Err(Error::Invalid(
            "H264 dimensions/interlace exceed budget".into(),
        ));
    }
    Ok(())
}

/// RFC 6184 single NAL, STAP-A and FU-A, with sequence continuity and timeout.
pub struct H264Assembler {
    timestamp: Option<u32>,
    next: u16,
    bytes: Vec<u8>,
    fragment: bool,
    started: Instant,
}
impl Default for H264Assembler {
    fn default() -> Self {
        Self {
            timestamp: None,
            next: 0,
            bytes: Vec::new(),
            fragment: false,
            started: Instant::now(),
        }
    }
}
impl H264Assembler {
    pub fn push(
        &mut self,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
        now: Instant,
    ) -> Result<Option<Vec<u8>>> {
        if payload.is_empty() || payload.len() > 1500 {
            return Err(Error::Invalid("invalid H264 RTP payload".into()));
        }
        if self.timestamp != Some(timestamp)
            || self.next != sequence
            || now.saturating_duration_since(self.started) > Duration::from_secs(1)
        {
            self.bytes.clear();
            self.fragment = false;
            self.started = now;
            self.timestamp = Some(timestamp);
        }
        self.next = sequence.wrapping_add(1);
        match payload[0] & 31 {
            1..=23 => {
                if self.fragment {
                    return Err(Error::Invalid("NAL interrupted FU-A".into()));
                }
                self.append(payload)?;
            }
            24 => {
                let mut index = 1;
                while index < payload.len() {
                    let length = payload
                        .get(index..index + 2)
                        .ok_or_else(|| Error::Invalid("truncated STAP-A".into()))?;
                    let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
                    index += 2;
                    let nal = payload
                        .get(index..index + length)
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| Error::Invalid("invalid STAP-A length".into()))?;
                    if nal[0] & 31 == 0 || nal[0] & 31 >= 24 {
                        return Err(Error::Invalid("invalid STAP-A NAL".into()));
                    }
                    self.append(nal)?;
                    index += length;
                }
            }
            28 => {
                let fu = *payload
                    .get(1)
                    .ok_or_else(|| Error::Invalid("truncated FU-A".into()))?;
                if payload.len() < 3 || fu & 0x20 != 0 || fu & 31 == 0 || fu & 31 >= 24 {
                    return Err(Error::Invalid("invalid FU-A header".into()));
                }
                if fu & 0x80 != 0 {
                    if self.fragment {
                        return Err(Error::Invalid("nested FU-A".into()));
                    }
                    self.append(&[(payload[0] & 0xe0) | (fu & 31)])?;
                    self.fragment = true;
                } else if !self.fragment {
                    return Err(Error::Invalid("missing FU-A start".into()));
                }
                self.extend(&payload[2..])?;
                if fu & 0x40 != 0 {
                    self.fragment = false;
                }
            }
            _ => return Err(Error::Invalid("unsupported H264 packetization".into())),
        }
        if marker {
            if self.fragment {
                return Err(Error::Invalid("unterminated H264 fragment".into()));
            }
            self.timestamp = None;
            return Ok(Some(std::mem::take(&mut self.bytes)));
        }
        Ok(None)
    }
    fn extend(&mut self, bytes: &[u8]) -> Result<()> {
        if self.bytes.len() + bytes.len() > MAX_ACCESS_UNIT {
            return Err(Error::Invalid("H264 reassembly budget exceeded".into()));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.extend(&[0, 0, 0, 1])?;
        self.extend(bytes)
    }
}
