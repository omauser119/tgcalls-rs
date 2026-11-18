use tgcalls_core::audio_processing::{VoiceOptions, VoiceProcessor};

#[test]
fn capture_and_render_support_device_rates_and_preserve_mono_balance()
-> Result<(), Box<dyn std::error::Error>> {
    let options = VoiceOptions {
        echo_cancellation: true,
        noise_suppression: true,
        automatic_gain: true,
    };
    let mut voice = VoiceProcessor::new(44_100, 1, 48_000, 2, options)?;
    let capture: Vec<u8> = (0..441)
        .flat_map(|i| {
            ((1000. * (i as f32 * 0.11).sin() + f32::from((i * 239 % 121) as i16) - 60.).round()
                as i16)
                .to_le_bytes()
        })
        .collect();
    let mut nonzero = false;
    for _ in 0..20 {
        voice.render(&[0; 1920])?;
        let output = voice.capture(&capture, 25)?;
        assert_eq!(output.len(), 1920);
        assert!(output.chunks_exact(4).all(|frame| frame[..2] == frame[2..]));
        nonzero |= output.iter().any(|value| *value != 0);
    }
    assert!(nonzero);
    assert!(voice.capture(&[0; 12], 25).is_err());
    Ok(())
}
