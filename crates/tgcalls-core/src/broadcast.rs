//! Telegram's automatically scaled broadcast audio parts use Ogg/Opus.
use crate::{AudioFrame, Error, PCM_BYTES, Result};
use rff_core::CodecId;
use rff_format::FormatRegistry;
use rusty_opus::{OpusDecoder, repacketizer::parse_packet};
use std::io::Cursor;

const MAX_INPUT: usize = 1024 * 1024;
const MAX_SAMPLES: usize = 72_000;
const MAX_PACKET_SAMPLES: usize = 5760;

struct Layout {
    channels: usize,
    streams: usize,
    coupled: usize,
    map: Vec<u8>,
    skip: usize,
    gain: f32,
}

impl Layout {
    fn parse(head: &[u8]) -> Result<Self> {
        if head.len() < 19 || &head[..8] != b"OpusHead" || head[8] != 1 {
            return Err(invalid("invalid OpusHead"));
        }
        let channels = usize::from(head[9]);
        if !(1..=16).contains(&channels) {
            return Err(invalid("broadcast channel budget exceeded"));
        }
        let (streams, coupled, map) = match head[18] {
            0 if channels <= 2 => (1, usize::from(channels == 2), (0..channels as u8).collect()),
            1 | 255 if head.len() >= 21 + channels => (
                usize::from(head[19]),
                usize::from(head[20]),
                head[21..21 + channels].to_vec(),
            ),
            _ => return Err(invalid("unsupported broadcast Opus layout")),
        };
        if streams == 0
            || streams > 16
            || coupled > streams
            || map.iter().all(|&i| i == 255)
            || map
                .iter()
                .any(|&i| i != 255 && usize::from(i) >= streams + coupled)
        {
            return Err(invalid("invalid Opus multistream mapping"));
        }
        let skip = usize::from(u16::from_le_bytes([head[10], head[11]]));
        let gain = 10.0_f32.powf(f32::from(i16::from_le_bytes([head[16], head[17]])) / 5120.0);
        Ok(Self {
            channels,
            streams,
            coupled,
            map,
            skip,
            gain,
        })
    }
}

pub fn decode_ogg_part(bytes: &[u8]) -> Result<Vec<AudioFrame>> {
    if !bytes.starts_with(b"OggS") || bytes.len() > MAX_INPUT {
        return Err(invalid("invalid or oversized Ogg broadcast part"));
    }
    let mut formats = FormatRegistry::new();
    rff_format_ogg::register(&mut formats);
    let mut input = formats
        .open_demuxer("ogg", Box::new(Cursor::new(bytes.to_vec())))
        .map_err(codec)?;
    let streams = input.read_header().map_err(codec)?;
    let stream = streams
        .first()
        .filter(|stream| streams.len() == 1 && stream.codec_id == CodecId::Opus)
        .ok_or_else(|| invalid("expected one Ogg Opus stream"))?;
    let mut layout = Layout::parse(&stream.extradata)?;
    let mut decoders = (0..layout.streams)
        .map(|index| {
            OpusDecoder::new(48_000, 1 + usize::from(index < layout.coupled)).map_err(codec)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut output = Vec::new();
    let mut position = 0usize;
    let mut packet_count = 0;
    let mut decoded = vec![0.0; MAX_PACKET_SAMPLES * 2];
    let mut packet_buffer = Vec::with_capacity(1280);
    loop {
        let packet = match input.read_packet() {
            Ok(packet) => packet,
            Err(rff_core::Error::Eof) => break,
            Err(error) => return Err(codec(error)),
        };
        packet_count += 1;
        if packet_count > 640 || packet.data.len() > 32 * 1024 || packet.data.is_empty() {
            return Err(invalid("broadcast Opus packet budget exceeded"));
        }
        let mut samples_by_channel = Vec::with_capacity(layout.streams + layout.coupled);
        let mut data = packet.data.as_slice();
        for (index, decoder) in decoders.iter_mut().enumerate() {
            let channels = 1 + usize::from(index < layout.coupled);
            let mut samples = (0..channels).map(|_| Vec::new()).collect::<Vec<_>>();
            let (toc, ranges, consumed) =
                parse_packet(data, index + 1 < layout.streams).map_err(codec)?;
            for (offset, length) in ranges {
                let frame = data
                    .get(offset..offset + length)
                    .ok_or_else(|| invalid("invalid Opus frame range"))?;
                packet_buffer.clear();
                packet_buffer.push(toc & !3);
                packet_buffer.extend_from_slice(frame);
                let count = decoder
                    .decode(&packet_buffer, MAX_PACKET_SAMPLES, &mut decoded)
                    .map_err(codec)?;
                for (channel, pcm) in samples.iter_mut().enumerate() {
                    if pcm.len() + count > MAX_PACKET_SAMPLES {
                        return Err(invalid("Opus duration exceeds 120 ms"));
                    }
                    pcm.extend(
                        decoded[..count * channels]
                            .iter()
                            .skip(channel)
                            .step_by(channels)
                            .copied(),
                    );
                }
            }
            data = data
                .get(consumed..)
                .ok_or_else(|| invalid("invalid multistream Opus packet"))?;
            samples_by_channel.extend(samples);
        }
        let count = samples_by_channel.first().map_or(0, Vec::len);
        if !data.is_empty()
            || count == 0
            || position + count > MAX_SAMPLES
            || samples_by_channel.iter().any(|pcm| pcm.len() != count)
        {
            return Err(invalid("invalid broadcast duration or channel length"));
        }
        let start = count.min(layout.skip);
        layout.skip -= start;
        let end = if packet.flags.end_of_stream {
            packet
                .end_position
                .and_then(|end| usize::try_from(end).ok())
                .map_or(count, |end| count.min(end.saturating_sub(position)))
        } else {
            count
        };
        position += count;
        for index in start..end {
            let channel = |c: usize| {
                samples_by_channel
                    .get(usize::from(layout.map[c]))
                    .map_or(0.0, |pcm| pcm[index])
            };
            let stereo = match layout.channels {
                1 => [channel(0); 2],
                2 => [channel(0), channel(1)],
                count => {
                    let mixed = (0..count).map(channel).sum::<f32>() / (count as f32).sqrt();
                    [mixed; 2]
                }
            };
            for sample in stereo {
                let pcm = (sample * layout.gain * 32768.0).round() as i16;
                output.extend_from_slice(&pcm.to_le_bytes());
            }
        }
    }
    if output.is_empty() {
        return Err(invalid("empty decoded broadcast part"));
    }
    Ok(output
        .chunks(PCM_BYTES)
        .enumerate()
        .map(|(index, pcm)| {
            let mut pcm = pcm.to_vec();
            pcm.resize(PCM_BYTES, 0);
            AudioFrame {
                ssrc: 0,
                sequence: index as u16,
                timestamp: index as u32 * 960,
                concealed: false,
                target_delay_ms: 0,
                pcm,
            }
        })
        .collect())
}

fn invalid(reason: &str) -> Error {
    Error::Invalid(reason.into())
}
fn codec(error: impl std::fmt::Display) -> Error {
    Error::Codec(error.to_string())
}
