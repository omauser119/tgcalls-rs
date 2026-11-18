use crate::{CHANNELS, Error, FRAME_SAMPLES, MAX_PACKET, PCM_BYTES, Result, SAMPLE_RATE};
use rusty_opus::{Application, OpusDecoder, OpusEncoder};

pub struct Encoder(OpusEncoder);
pub struct Decoder {
    opus: OpusDecoder,
    last_samples: usize,
    pcm: Vec<f32>,
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
        self.encode_pcm::<false>(pcm).map(|(packet, _)| packet)
    }

    #[cfg(feature = "async")]
    pub(crate) fn encode_with_level(&mut self, pcm: &[u8]) -> Result<(Vec<u8>, u8)> {
        self.encode_pcm::<true>(pcm)
    }

    fn encode_pcm<const LEVEL: bool>(&mut self, pcm: &[u8]) -> Result<(Vec<u8>, u8)> {
        if pcm.len() != PCM_BYTES {
            return Err(Error::Invalid(format!(
                "expected {PCM_BYTES} PCM s16le bytes"
            )));
        }
        let mut samples = [0.; FRAME_SAMPLES * CHANNELS];
        let mut energy = 0u64;
        for (slot, bytes) in samples.iter_mut().zip(pcm.as_chunks::<2>().0) {
            let sample = i16::from_le_bytes(*bytes);
            *slot = f32::from(sample) * (1. / 32768.);
            if LEVEL {
                let sample = i64::from(sample);
                energy += (sample * sample) as u64;
            }
        }
        let mut packet = [0; MAX_PACKET];
        let len = self
            .0
            .encode(&samples, FRAME_SAMPLES, &mut packet)
            .map_err(|e| Error::Codec(e.to_string()))?;
        let level = if energy == 0 {
            127
        } else {
            let normalized = energy as f64 / (samples.len() as f64 * 32768. * 32768.);
            (-10. * normalized.log10()).clamp(0., 127.) as u8
        };
        Ok((packet[..len].to_vec(), level))
    }
}

impl Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            opus: OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS)
                .map_err(|e| Error::Codec(e.to_string()))?,
            last_samples: FRAME_SAMPLES,
            pcm: vec![0.; 5760 * CHANNELS],
        })
    }

    /// Empty payload requests Opus packet loss concealment.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        if packet.len() > MAX_PACKET {
            return Err(Error::Invalid("oversized Opus packet".into()));
        }
        // Telegram peers can send up to 120 ms per Opus packet.
        let max_samples = 5760;
        let count = self
            .opus
            .decode(
                packet,
                if packet.is_empty() {
                    self.last_samples
                } else {
                    max_samples
                },
                &mut self.pcm,
            )
            .map_err(|e| Error::Codec(e.to_string()))?;
        if count == 0 || count > max_samples {
            return Err(Error::Codec("invalid decoded frame length".into()));
        }
        self.last_samples = count;
        Ok(self.pcm[..count * CHANNELS]
            .iter()
            .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
            .collect())
    }
}
