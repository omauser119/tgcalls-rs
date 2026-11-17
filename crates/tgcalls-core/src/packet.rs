//! Small deterministic RTP validator used before allocating or forwarding packets.
use crate::{Error, Result};

#[derive(Debug, PartialEq, Eq)]
pub struct RtpView<'a> {
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub payload: &'a [u8],
}

pub fn parse_rtp(data: &[u8]) -> Result<RtpView<'_>> {
    let bad = || Error::Invalid("malformed RTP".into());
    if data.len() < 12 || data.len() > 65_535 || data[0] >> 6 != 2 {
        return Err(bad());
    }
    let mut offset = 12 + usize::from(data[0] & 15) * 4;
    if offset > data.len() {
        return Err(bad());
    }
    if data[0] & 0x10 != 0 {
        let ext = data.get(offset..offset + 4).ok_or_else(bad)?;
        offset += 4 + usize::from(u16::from_be_bytes([ext[2], ext[3]])) * 4;
        if offset > data.len() {
            return Err(bad());
        }
    }
    let mut end = data.len();
    if data[0] & 0x20 != 0 {
        let padding = usize::from(*data.last().ok_or_else(bad)?);
        if padding == 0 || padding > end - offset {
            return Err(bad());
        }
        end -= padding;
    }
    Ok(RtpView {
        sequence: u16::from_be_bytes([data[2], data[3]]),
        timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        ssrc: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
        payload_type: data[1] & 0x7f,
        payload: &data[offset..end],
    })
}

pub fn validate_sdp(sdp: &str) -> Result<()> {
    if sdp.len() > 65_536
        || sdp.as_bytes().contains(&0)
        || !sdp.starts_with("v=0")
        || sdp.lines().count() > 512
        || sdp.lines().any(|l| l.len() > 2048)
    {
        return Err(Error::Invalid("invalid or oversized SDP".into()));
    }
    Ok(())
}
