//! 10 ms PCM processing at the audio device boundary.
use crate::{Error, Result};
use nnnoiseless::DenoiseState;
use serde::{Deserialize, Serialize};
use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{EchoCanceller, GainController2},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct VoiceOptions {
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub automatic_gain: bool,
}

pub struct VoiceProcessor {
    processor: AudioProcessing,
    denoise: Vec<Box<DenoiseState<'static>>>,
    capture: StreamConfig,
    render: StreamConfig,
    output: StreamConfig,
    options: VoiceOptions,
}

impl VoiceProcessor {
    pub fn new(
        capture_rate: u32,
        capture_channels: u16,
        render_rate: u32,
        render_channels: u16,
        options: VoiceOptions,
    ) -> Result<Self> {
        let capture = stream(capture_rate, capture_channels)?;
        let render = stream(render_rate, render_channels)?;
        let output = StreamConfig::new(48_000, capture_channels);
        let mut config = Config::default();
        config.pipeline.multi_channel_capture = capture_channels == 2;
        config.pipeline.multi_channel_render = render_channels == 2;
        config.echo_canceller = options.echo_cancellation.then(EchoCanceller::default);
        config.gain_controller2 = options.automatic_gain.then(|| GainController2 {
            adaptive_digital: Some(Default::default()),
            ..Default::default()
        });
        let processor = AudioProcessing::builder()
            .config(config)
            .capture_config(capture)
            .render_config(render)
            .build();
        let denoise = if options.noise_suppression {
            (0..capture_channels).map(|_| DenoiseState::new()).collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            processor,
            denoise,
            capture,
            render,
            output,
            options,
        })
    }

    /// Submit actual far-end PCM queued for playback, after the application's mix.
    pub fn render(&mut self, pcm: &[u8]) -> Result<()> {
        let samples = samples(pcm, self.render)?;
        if self.options.echo_cancellation {
            let mut output = vec![0; samples.len()];
            self.processor
                .process_render_i16(&samples, &mut output)
                .map_err(|e| Error::Codec(e.to_string()))?;
        }
        Ok(())
    }

    /// Process one capture block and return 10 ms of stereo s16le at 48 kHz.
    pub fn capture(&mut self, pcm: &[u8], delay_ms: u16) -> Result<Vec<u8>> {
        let samples = samples(pcm, self.capture)?;
        if delay_ms > 500 {
            return Err(Error::Invalid("audio device delay exceeds 500 ms".into()));
        }
        if self.options.echo_cancellation {
            self.processor
                .set_stream_delay_ms(i32::from(delay_ms))
                .map_err(|e| Error::Codec(e.to_string()))?;
        }
        let mut converted = vec![0; self.output.num_samples()];
        self.processor
            .process_capture_i16_with_config(&samples, &self.capture, &self.output, &mut converted)
            .map_err(|e| Error::Codec(e.to_string()))?;

        let channels = usize::from(self.output.num_channels());
        for (channel, denoise) in self.denoise.iter_mut().enumerate() {
            let mut input = [0.0; DenoiseState::FRAME_SIZE];
            for (destination, &source) in input
                .iter_mut()
                .zip(converted[channel..].iter().step_by(channels))
            {
                *destination = f32::from(source);
            }
            let mut output = [0.0; DenoiseState::FRAME_SIZE];
            denoise.process_frame(&mut output, &input);
            for (destination, sample) in converted[channel..]
                .iter_mut()
                .step_by(channels)
                .zip(output)
            {
                *destination = sample.round() as i16;
            }
        }

        let mut output = Vec::with_capacity(1920);
        for frame in converted.chunks_exact(channels) {
            output.extend_from_slice(&frame[0].to_le_bytes());
            output.extend_from_slice(&frame[channels - 1].to_le_bytes());
        }
        Ok(output)
    }
}

fn stream(rate: u32, channels: u16) -> Result<StreamConfig> {
    if !matches!(rate, 8_000 | 16_000 | 24_000 | 32_000 | 44_100 | 48_000)
        || !(1..=2).contains(&channels)
    {
        return Err(Error::Invalid("unsupported PCM device format".into()));
    }
    Ok(StreamConfig::new(rate, channels))
}

fn samples(pcm: &[u8], config: StreamConfig) -> Result<Vec<i16>> {
    if pcm.len() != config.num_samples() * 2 {
        return Err(Error::Invalid(
            "expected exactly 10 ms of device PCM".into(),
        ));
    }
    Ok(pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&s| i16::from_le_bytes(s))
        .collect())
}
