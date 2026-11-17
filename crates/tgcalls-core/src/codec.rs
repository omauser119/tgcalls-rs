use crate::{CHANNELS, Error, FRAME_SAMPLES, MAX_PACKET, PCM_BYTES, Result, SAMPLE_RATE};
use rusty_opus::{Application, OpusDecoder, OpusEncoder};

pub struct Encoder(OpusEncoder);
pub struct Decoder {
    opus: OpusDecoder,
    last_samples: usize,
}

impl Encoder {
    pub fn new() -> Result<Self> {
        let mut encoder = OpusEncoder::new(SAMPLE_RATE as i32, CHANNELS, Application::Voip)
            .map_err(|e| Error::Codec(e.to_string()))?;
        encoder.bitrate_bps = 64_000;
        encoder.use_inband_fec = true;
        Ok(Self(encoder))
    }

    pub fn encode(&mut self, pcm: &[u8]) -> Result<Vec<u8>> {
        if pcm.len() != PCM_BYTES {
            return Err(Error::Invalid(format!(
                "expected {PCM_BYTES} PCM s16le bytes"
            )));
        }
        let samples: Vec<f32> = pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0)
            .collect();
        let mut packet = vec![0; MAX_PACKET];
        let len = self
            .0
            .encode(&samples, FRAME_SAMPLES, &mut packet)
            .map_err(|e| Error::Codec(e.to_string()))?;
        packet.truncate(len);
        Ok(packet)
    }
}

impl Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            opus: OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS)
                .map_err(|e| Error::Codec(e.to_string()))?,
            last_samples: FRAME_SAMPLES,
        })
    }

    /// Empty payload requests Opus packet loss concealment.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        if packet.len() > MAX_PACKET {
            return Err(Error::Invalid("oversized Opus packet".into()));
        }
        // Telegram peers can send up to 120 ms per Opus packet.
        let max_samples = 5760;
        let mut pcm = vec![0.0f32; max_samples * CHANNELS];
        let count = self
            .opus
            .decode(
                packet,
                if packet.is_empty() {
                    self.last_samples
                } else {
                    max_samples
                },
                &mut pcm,
            )
            .map_err(|e| Error::Codec(e.to_string()))?;
        if count == 0 || count > max_samples {
            return Err(Error::Codec("invalid decoded frame length".into()));
        }
        self.last_samples = count;
        Ok(pcm[..count * CHANNELS]
            .iter()
            .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
            .collect())
    }
}
