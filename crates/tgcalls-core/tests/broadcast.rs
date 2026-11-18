use rff_core::{CodecId, Packet};
use rff_format::{FormatRegistry, Stream};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use tgcalls_core::{PCM_BYTES, broadcast::decode_ogg_part, codec::Encoder};

#[derive(Clone, Default)]
struct MemoryOutput(Arc<Mutex<Vec<u8>>>);

impl Write for MemoryOutput {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("poisoned Ogg output"))?
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn stream_part_decodes_remade_ogg_and_rusty_opus_to_stereo_pcm()
-> Result<(), Box<dyn std::error::Error>> {
    let mut formats = FormatRegistry::new();
    rff_format_ogg::register(&mut formats);
    let output = MemoryOutput::default();
    let mut mux = formats.open_muxer("ogg", Box::new(output.clone()))?;
    let mut stream = Stream::new(0, CodecId::Opus);
    stream.channels = 2;
    stream.sample_rate = 48_000;
    mux.write_header(&[stream])?;
    let mut encoder = Encoder::new()?;
    let pcm: Vec<u8> = (0..PCM_BYTES / 2)
        .flat_map(|sample| {
            let signal = ((sample as f32 * 0.07).sin() * 4_000.0) as i16;
            signal.to_le_bytes()
        })
        .collect();
    for _ in 0..10 {
        mux.write_packet(&Packet::from_data(0, encoder.encode(&pcm)?))?;
    }
    mux.write_trailer()?;
    let data = output
        .0
        .lock()
        .map_err(|_| "failed to collect Ogg output")?;
    let decoded = decode_ogg_part(&data)?;
    assert_eq!(decoded.len(), 10);
    assert!(decoded.iter().all(|frame| frame.pcm.len() == PCM_BYTES));
    assert!(
        decoded
            .iter()
            .flat_map(|frame| &frame.pcm)
            .any(|&byte| byte != 0)
    );
    assert!(decode_ogg_part(b"OggS").is_err());
    Ok(())
}
