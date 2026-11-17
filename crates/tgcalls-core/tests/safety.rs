use std::time::{Duration, Instant};
use tgcalls_core::{
    PCM_BYTES,
    codec::{Decoder, Encoder},
    jitter::JitterBuffer,
    packet::{parse_rtp, validate_sdp},
    telegram,
};

#[test]
fn hostile_rtp_is_bounded_and_panic_free() {
    let mut rng = 1u64;
    for length in 0..2048 {
        let mut packet = vec![0; length];
        for byte in &mut packet {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *byte = rng as u8;
        }
        let _ = parse_rtp(&packet);
    }
    assert!(parse_rtp(&[0x80; 11]).is_err());
    let mut packet = vec![0x80, 111, 0, 1, 0, 0, 3, 192, 0, 0, 0, 1, 0xaa];
    assert_eq!(
        parse_rtp(&packet)
            .map(|p| (p.sequence, p.timestamp, p.payload.len()))
            .ok(),
        Some((1, 960, 1))
    );
    packet[0] |= 0x10;
    assert!(parse_rtp(&packet).is_err());
    packet[0] = 0xa0;
    packet[12] = 0;
    assert!(parse_rtp(&packet).is_err());
}

#[test]
fn jitter_handles_wrap_reordering_duplicates_and_capacity() {
    let now = Instant::now();
    let mut buffer = JitterBuffer::default();
    buffer.push(65535, 0, now, 1);
    buffer.push(1, 1920, now + Duration::from_millis(40), 3);
    buffer.push(0, 960, now + Duration::from_millis(45), 2);
    buffer.push(0, 960, now + Duration::from_millis(46), 99);
    buffer.push(5000, 960, now, 99);
    let ready = now + Duration::from_millis(250);
    assert_eq!(buffer.pop(ready), Some(Some(1)));
    assert_eq!(buffer.pop(ready), Some(Some(2)));
    assert_eq!(buffer.pop(ready), Some(Some(3)));
    assert_eq!(buffer.dropped, 2);
    assert_eq!(buffer.pop(ready), Some(None));
}

#[test]
fn rusty_opus_codes_and_conceals() -> Result<(), Box<dyn std::error::Error>> {
    let mut encoder = Encoder::new()?;
    let mut decoder = Decoder::new()?;
    assert!(encoder.encode(&[0; 10]).is_err());
    let pcm = vec![0u8; PCM_BYTES];
    let packet = encoder.encode(&pcm)?;
    assert!(!packet.is_empty());
    assert_eq!(decoder.decode(&packet)?.len(), PCM_BYTES);
    assert_eq!(decoder.decode(&[])?.len(), PCM_BYTES);
    assert!(decoder.decode(&[0; 1501]).is_err());
    Ok(())
}

#[test]
fn opus_plc_tracks_remote_packet_duration() -> Result<(), Box<dyn std::error::Error>> {
    let mut encoder = rusty_opus::OpusEncoder::new(48000, 2, rusty_opus::Application::Voip)?;
    let mut packet = [0; 1500];
    let size = encoder.encode(&vec![0.0; 2880 * 2], 2880, &mut packet)?;
    let mut decoder = Decoder::new()?;
    assert_eq!(decoder.decode(&packet[..size])?.len(), 2880 * 2 * 2);
    assert_eq!(decoder.decode(&[])?.len(), 2880 * 2 * 2);
    Ok(())
}

#[test]
fn jitter_increases_playout_delay_after_a_late_packet() {
    let begin = Instant::now();
    let mut buffer = JitterBuffer::default();
    buffer.push(0, 0, begin, 0);
    buffer.push(2, 1920, begin + Duration::from_millis(20), 2);
    buffer.push(1, 960, begin + Duration::from_millis(40), 1);
    assert_eq!(buffer.pop(begin + Duration::from_millis(40)), Some(Some(0)));
    buffer.push(5, 4800, begin + Duration::from_millis(60), 5);
    assert!(buffer.target_delay_ms() > 40);
    assert_eq!(buffer.pop(begin + Duration::from_millis(60)), None);
    assert_eq!(buffer.pop(begin + Duration::from_millis(80)), Some(Some(1)));
}

fn offer() -> String {
    format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\na=fingerprint:sha-256 {}\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=ice-ufrag:abcd\r\na=ice-pwd:abcdefghijklmnopqrstuv\r\na=rtcp-mux\r\na=rtpmap:111 opus/48000/2\r\na=sendrecv\r\na=ssrc:4294967295 cname:test\r\n",
        ["AB"; 32].join(":")
    )
}
fn response() -> serde_json::Value {
    serde_json::json!({"transport": {"ufrag":"efgh", "pwd":"0123456789012345678901",
        "fingerprints":[{"hash":"sha-256", "setup":"active", "fingerprint":(["CD";32].join(":"))}],
        "candidates":[{"foundation":"1", "component":"1", "protocol":"udp", "priority":"2130706431", "ip":"149.154.167.50", "port":"32000", "type":"host"}]}})
}

#[test]
fn telegram_json_matches_original_wire_fields_and_checks_injection()
-> Result<(), Box<dyn std::error::Error>> {
    let join: serde_json::Value =
        serde_json::from_str(&telegram::join_payload(&offer(), u32::MAX)?)?;
    assert_eq!(join["ssrc"], -1);
    assert_eq!(join["fingerprints"][0]["setup"], "passive");
    let answer = telegram::answer_sdp(&offer(), &response().to_string(), &[])?;
    assert!(answer.contains("a=ice-lite\r\n"));
    assert!(answer.contains("a=setup:active\r\n"));
    assert!(answer.contains("a=recvonly\r\n"));
    assert!(!answer.contains("a=ssrc:4294967295"));
    rtc::peer_connection::sdp::RTCSessionDescription::answer(answer)?;
    let mut malicious = response();
    malicious["transport"]["ufrag"] = "efgh\r\na=setup:actpass".into();
    assert!(telegram::answer_sdp(&offer(), &malicious.to_string(), &[]).is_err());
    malicious = response();
    malicious["transport"]["fingerprints"] = serde_json::json!([]);
    assert!(telegram::answer_sdp(&offer(), &malicious.to_string(), &[]).is_err());
    assert!(validate_sdp(&"v=0\r\n".repeat(20_000)).is_err());
    Ok(())
}
