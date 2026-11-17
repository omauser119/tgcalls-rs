//! Telegram V2 reliable encrypted signaling (call versions 8.0.0 / 9.0.0).
//! Authentication is checked before parsing, replay state or acknowledgements change.
use crate::{Error, Result, auth::AuthKey};
use aes::cipher::{KeyIvInit, StreamCipher};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

const ACK: u8 = 255;
const EMPTY: u8 = 254;
const CUSTOM: u8 = 127;
const RELIABLE: u32 = 1 << 30;
const SINGLE: u32 = 1 << 31;
const MASK: u32 = RELIABLE - 1;
pub const MAX_SIGNAL_PACKET: usize = 16_384;
const MAX_PENDING: usize = 64;

#[derive(Default, Clone)]
struct Replay {
    largest: u32,
    bitmap: u64,
}
impl Replay {
    fn accept(&mut self, counter: u32) -> bool {
        if counter == 0 || counter > MASK {
            return false;
        }
        if counter > self.largest {
            let shift = counter - self.largest;
            self.bitmap = if shift >= 64 {
                1
            } else {
                (self.bitmap << shift) | 1
            };
            self.largest = counter;
            true
        } else {
            let delta = self.largest - counter;
            if delta >= 64 || self.bitmap & (1 << delta) != 0 {
                return false;
            }
            self.bitmap |= 1 << delta;
            true
        }
    }
}

struct Pending {
    bytes: Vec<u8>,
    sent: Instant,
}
pub struct Signaling {
    key: AuthKey,
    outgoing: bool,
    counter: u32,
    packets: Replay,
    reliable: Replay,
    pending: BTreeMap<u32, Pending>,
    acks: BTreeSet<u32>,
}

impl Signaling {
    pub fn new(key: AuthKey, outgoing: bool) -> Self {
        Self {
            key,
            outgoing,
            counter: 0,
            packets: Replay::default(),
            reliable: Replay::default(),
            pending: BTreeMap::new(),
            acks: BTreeSet::new(),
        }
    }
    fn next(&mut self) -> Result<u32> {
        self.counter = self
            .counter
            .checked_add(1)
            .filter(|n| *n <= MASK)
            .ok_or_else(|| Error::Invalid("signaling counter exhausted; terminate call".into()))?;
        Ok(self.counter)
    }
    fn seal(&self, plain: &[u8]) -> Vec<u8> {
        let x = if self.outgoing { 128 } else { 136 };
        let mut hasher = Sha256::new();
        hasher.update(&self.key.0[88 + x..120 + x]);
        hasher.update(plain);
        let hash = hasher.finalize();
        let msg_key = &hash[8..24];
        let (key, iv) = derive(&self.key, msg_key, x);
        let mut result = Vec::with_capacity(16 + plain.len());
        result.extend_from_slice(msg_key);
        result.extend_from_slice(plain);
        ctr::Ctr128BE::<aes::Aes256>::new(key.as_ref().into(), (&iv).into())
            .apply_keystream(&mut result[16..]);
        result
    }
    fn open(&self, packet: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        if !(21..=MAX_SIGNAL_PACKET).contains(&packet.len()) {
            return Err(Error::Invalid("invalid signaling packet size".into()));
        }
        let x = if self.outgoing { 136 } else { 128 };
        let msg_key = &packet[..16];
        let (key, iv) = derive(&self.key, msg_key, x);
        let mut plain = Zeroizing::new(packet[16..].to_vec());
        ctr::Ctr128BE::<aes::Aes256>::new(key.as_ref().into(), (&iv).into())
            .apply_keystream(&mut plain);
        let mut hasher = Sha256::new();
        hasher.update(&self.key.0[88 + x..120 + x]);
        hasher.update(plain.as_slice());
        if !bool::from(hasher.finalize()[8..24].ct_eq(msg_key)) {
            return Err(Error::Invalid("signaling authentication failed".into()));
        }
        Ok(plain)
    }
    pub fn send(&mut self, message: &[u8], now: Instant) -> Result<Vec<u8>> {
        if message.is_empty()
            || message.len() + 25 > MAX_SIGNAL_PACKET
            || self.pending.len() >= MAX_PENDING
        {
            return Err(Error::Invalid(
                "signaling message/queue exceeds budget".into(),
            ));
        }
        let seq = self.next()? | RELIABLE;
        let mut plain = Vec::with_capacity(message.len() + 9);
        plain.extend_from_slice(&seq.to_be_bytes());
        plain.push(CUSTOM);
        plain.extend_from_slice(&(message.len() as u32).to_be_bytes());
        plain.extend_from_slice(message);
        self.pending.insert(
            seq,
            Pending {
                bytes: plain.clone(),
                sent: now,
            },
        );
        Ok(self.seal(&plain))
    }
    pub fn receive(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>> {
        let plain = self.open(packet)?;
        let mut offset = 0;
        let mut parsed = Vec::new();
        while offset < plain.len() {
            let header = plain
                .get(offset..offset + 5)
                .ok_or_else(|| Error::Invalid("truncated signaling header".into()))?;
            let seq = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
            let kind = header[4];
            let first = offset == 0;
            offset += 5;
            if seq & MASK == 0 || (kind == ACK && first) || (kind == EMPTY && !first) {
                return Err(Error::Invalid("invalid signaling sequence/type".into()));
            }
            let data = if kind == CUSTOM {
                let length = plain
                    .get(offset..offset + 4)
                    .ok_or_else(|| Error::Invalid("missing signal length".into()))?;
                let length =
                    u32::from_be_bytes([length[0], length[1], length[2], length[3]]) as usize;
                offset += 4;
                if length > 16_000 {
                    return Err(Error::Invalid("signaling message exceeds budget".into()));
                }
                let data = plain
                    .get(offset..offset + length)
                    .ok_or_else(|| Error::Invalid("truncated signal message".into()))?;
                offset += length;
                data.to_vec()
            } else if kind == EMPTY || kind == ACK {
                vec![]
            } else {
                return Err(Error::Invalid("unknown signaling message type".into()));
            };
            if seq & SINGLE != 0 && (!first || offset != plain.len()) {
                return Err(Error::Invalid("invalid single-message packet".into()));
            }
            parsed.push((seq, kind, data));
            if parsed.len() > 128 {
                return Err(Error::Invalid(
                    "too many messages per signaling packet".into(),
                ));
            }
        }
        let Some((first_seq, _, _)) = parsed.first() else {
            return Err(Error::Invalid("empty packet".into()));
        };
        if !self.packets.accept(first_seq & MASK) {
            return Ok(vec![]);
        }
        let mut result = Vec::new();
        for (index, (seq, kind, data)) in parsed.into_iter().enumerate() {
            match kind {
                ACK => {
                    self.pending.remove(&seq);
                }
                CUSTOM if seq & RELIABLE != 0 => {
                    if self.acks.len() >= 128 {
                        return Err(Error::Invalid("ack queue budget exceeded".into()));
                    }
                    self.acks.insert(seq);
                    if self.reliable.accept(seq & MASK) {
                        result.push(data);
                    }
                }
                CUSTOM if (index == 0 || self.packets.accept(seq & MASK)) => {
                    result.push(data);
                }
                _ => {}
            }
        }
        Ok(result)
    }
    /// Called by the signaling pump. Service packets get fresh packet counters.
    pub fn service(&mut self, now: Instant) -> Result<Option<Vec<u8>>> {
        if self.acks.is_empty()
            && !self
                .pending
                .values()
                .any(|p| now.saturating_duration_since(p.sent) >= Duration::from_secs(3))
        {
            return Ok(None);
        }
        let mut plain = self.next()?.to_be_bytes().to_vec();
        plain.push(EMPTY);
        let acks: Vec<_> = self.acks.iter().copied().collect();
        for seq in acks {
            if plain.len() + 21 > MAX_SIGNAL_PACKET {
                break;
            }
            plain.extend_from_slice(&seq.to_be_bytes());
            plain.push(ACK);
            self.acks.remove(&seq);
        }
        for pending in self.pending.values_mut() {
            if now.saturating_duration_since(pending.sent) < Duration::from_secs(3) {
                continue;
            }
            if plain.len() + pending.bytes.len() + 16 > MAX_SIGNAL_PACKET {
                break;
            }
            plain.extend_from_slice(&pending.bytes);
            pending.sent = now;
        }
        Ok(Some(self.seal(&plain)))
    }
}

fn derive(auth: &AuthKey, message: &[u8], x: usize) -> (Zeroizing<[u8; 32]>, [u8; 16]) {
    let mut a = Sha256::new();
    a.update(message);
    a.update(&auth.0[x..x + 36]);
    let a = a.finalize();
    let mut b = Sha256::new();
    b.update(&auth.0[40 + x..76 + x]);
    b.update(message);
    let b = b.finalize();
    let mut key = Zeroizing::new([0u8; 32]);
    let mut iv = [0u8; 16];
    key[..8].copy_from_slice(&a[..8]);
    key[8..24].copy_from_slice(&b[8..24]);
    key[24..].copy_from_slice(&a[24..]);
    iv[..4].copy_from_slice(&b[..4]);
    iv[4..12].copy_from_slice(&a[8..16]);
    iv[12..].copy_from_slice(&b[24..28]);
    (key, iv)
}
