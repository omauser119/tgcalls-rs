//! Bounded encoded media handoff to the authenticated Telegram conference context.
use crate::{Error, Result, video::MAX_ACCESS_UNIT};
use rtc::rtp::Packet;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 16;
const TTL: Duration = Duration::from_millis(500);
pub(crate) const MAX_AUDIO_FRAME: usize = 2048;

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    Audio,
    H264,
}

#[derive(Serialize, Deserialize)]
pub struct ConferenceFrame {
    pub id: u32,
    pub encrypt: bool,
    pub kind: FrameKind,
    pub ssrc: u32,
    pub clear_bytes: usize,
    pub bytes: Vec<u8>,
}

pub(crate) enum Pending {
    SendAudio {
        timestamp: u32,
    },
    ReceiveAudio {
        arrival: Instant,
        packet: Packet,
    },
    SendH264 {
        ssrc: u32,
        timestamp: u32,
        clear_bytes: usize,
    },
    ReceiveH264 {
        ssrc: u32,
        timestamp: u32,
    },
}

#[derive(Default)]
pub(crate) struct FrameCrypto {
    next_id: u32,
    pending: BTreeMap<u32, (Instant, Pending)>,
    requests: VecDeque<ConferenceFrame>,
}

impl FrameCrypto {
    pub(crate) fn stage(
        &mut self,
        ssrc: u32,
        metadata: Pending,
        bytes: Vec<u8>,
        now: Instant,
    ) -> Result<()> {
        self.pending
            .retain(|_, (creation, _)| now.saturating_duration_since(*creation) <= TTL);
        let (encrypt, kind, clear_bytes, max) = match metadata {
            Pending::SendAudio { .. } => (true, FrameKind::Audio, 0, MAX_AUDIO_FRAME),
            Pending::ReceiveAudio { .. } => (false, FrameKind::Audio, 0, MAX_AUDIO_FRAME),
            Pending::SendH264 { clear_bytes, .. } => {
                (true, FrameKind::H264, clear_bytes, MAX_ACCESS_UNIT)
            }
            Pending::ReceiveH264 { .. } => (false, FrameKind::H264, 0, MAX_ACCESS_UNIT),
        };
        if bytes.is_empty() || bytes.len() > max || bytes.len() <= clear_bytes {
            return Err(Error::Invalid("invalid conference frame span".into()));
        }
        if self.pending.len() >= MAX_PENDING || self.requests.len() >= MAX_PENDING {
            return Err(Error::Invalid("conference transform queue full".into()));
        }
        let id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| Error::Invalid("conference frame counter exhausted".into()))?;
        self.next_id = id;
        self.pending.insert(id, (now, metadata));
        self.requests.push_back(ConferenceFrame {
            id,
            encrypt,
            kind,
            ssrc,
            clear_bytes,
            bytes,
        });
        Ok(())
    }

    pub(crate) fn take(&mut self, id: u32, now: Instant) -> Result<Pending> {
        let (created, frame) = self
            .pending
            .remove(&id)
            .ok_or_else(|| Error::Invalid("unknown conference frame".into()))?;
        if now.saturating_duration_since(created) > TTL {
            return Err(Error::Invalid("expired conference frame".into()));
        }
        Ok(frame)
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = ConferenceFrame> + '_ {
        self.requests.drain(..)
    }
}

pub(crate) fn strip_audio_trailer(frame: &[u8]) -> Result<&[u8]> {
    if frame.len() < 2 {
        return Err(Error::Invalid("missing conference audio trailer".into()));
    }
    let tail = if frame[frame.len() - 2] & 1 != 0 {
        2
    } else {
        1
    };
    let size = frame.len() - tail;
    if size == 0 || size > 1275 {
        return Err(Error::Invalid("invalid authenticated Opus payload".into()));
    }
    Ok(&frame[..size])
}
