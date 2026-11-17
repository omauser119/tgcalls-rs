use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tgcalls_core::{
    PCM_BYTES,
    auth::{AuthKey, KeyExchange, TELEGRAM_PRIME_HEX},
    personal::PersonalCall,
    signaling::Signaling,
    video::{H264Assembler, VideoConfig, VideoDecoder, VideoEncoder},
};

fn key() -> Result<AuthKey, Box<dyn std::error::Error>> {
    Ok(AuthKey::from_bytes(&[42; 256])?)
}

#[test]
fn dh_commitment_fingerprint_and_duplicate_protection() -> Result<(), Box<dyn std::error::Error>> {
    use crypto_bigint::U2048;
    let prime = U2048::from_be_hex(TELEGRAM_PRIME_HEX).to_be_bytes();
    let mut caller = KeyExchange::new(3, &prime, &[1; 256], None)?;
    let mut callee = KeyExchange::new(3, &prime, &[2; 256], Some(&caller.first_message()))?;
    let (a, ha) = caller.complete(callee.public(), None)?;
    let (b, hb) = callee.complete(caller.public(), Some(a.fingerprint()))?;
    assert_eq!(a.fingerprint(), b.fingerprint());
    assert_eq!(ha, hb);
    assert!(caller.complete(callee.public(), None).is_err());
    assert!(KeyExchange::new(3, &[0; 256], &[0; 256], None).is_err());
    Ok(())
}

#[test]
fn signaling_authentication_replay_and_resend() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = Signaling::new(key()?, true);
    let mut b = Signaling::new(key()?, false);
    let now = Instant::now();
    let packet = a.send(br#"{"@type":"InitialSetup"}"#, now)?;
    let mut forged = packet.clone();
    forged[20] ^= 1;
    assert!(b.receive(&forged).is_err());
    assert_eq!(b.receive(&packet)?.len(), 1);
    assert!(b.receive(&packet)?.is_empty());
    let ack = b.service(now)?.ok_or("missing ACK")?;
    assert!(a.receive(&ack)?.is_empty());
    assert!(a.service(now + Duration::from_secs(10))?.is_none());
    let dropped = a.send(b"second", now)?;
    drop(dropped);
    let retry = a
        .service(now + Duration::from_secs(4))?
        .ok_or("missing resend")?;
    assert_eq!(b.receive(&retry)?, vec![b"second".to_vec()]);
    Ok(())
}

fn video_config() -> VideoConfig {
    VideoConfig {
        width: 32,
        height: 32,
        fps: 10,
        bitrate: 64_000,
    }
}
fn black() -> Vec<u8> {
    let mut frame = vec![16; 32 * 32];
    frame.extend(vec![128; 32 * 32 / 2]);
    frame
}

#[test]
fn remade_h264_roundtrip_and_rtp_fragmentation() -> Result<(), Box<dyn std::error::Error>> {
    let mut encoder = VideoEncoder::new(video_config())?;
    let au = encoder.encode(&black())?;
    assert!(!au.is_empty());
    let frame = VideoDecoder::default()
        .decode(7, 9000, &au)?
        .ok_or("missing decoded frame")?;
    assert_eq!((frame.width, frame.height), (32, 32));
    assert_eq!(frame.i420, black());
    let mut assembler = H264Assembler::default();
    assert!(
        assembler
            .push(1, 1, false, &[0x7c, 0x85, 1, 2], Instant::now())?
            .is_none()
    );
    assert!(
        assembler
            .push(3, 1, true, &[0x7c, 0x45, 3], Instant::now())
            .is_err()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn personal_call_encrypted_signaling_and_bidirectional_media()
-> Result<(), Box<dyn std::error::Error>> {
    let _ = env_logger::try_init();
    let a = Arc::new(
        PersonalCall::new(
            key()?,
            true,
            Some(video_config()),
            vec![],
            "127.0.0.1",
            None,
            1100,
        )
        .await?,
    );
    let b = Arc::new(
        PersonalCall::new(
            key()?,
            false,
            Some(video_config()),
            vec![],
            "127.0.0.1",
            None,
            2200,
        )
        .await?,
    );
    let begin = Instant::now();
    while begin.elapsed() < Duration::from_secs(5) {
        for packet in a.poll_signaling().await? {
            b.receive_signaling(&packet).await?;
        }
        for packet in b.poll_signaling().await? {
            a.receive_signaling(&packet).await?;
        }
        if a.ready().await && b.ready().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(a.media.stats().connection_state, "Connected");
    assert_eq!(b.media.stats().connection_state, "Connected");
    a.media.send_pcm(&vec![0; PCM_BYTES]).await?;
    assert_eq!(
        b.media
            .receive(Duration::from_secs(3))
            .await
            .map_err(|e| format!("audio: {e}"))?
            .pcm
            .len(),
        PCM_BYTES
    );
    a.media.send_video(&black()).await?;
    let frame = b
        .media
        .receive_video(Duration::from_secs(3))
        .await
        .map_err(|e| format!("video a->b: {e}"))?;
    assert_eq!(frame.i420, black());
    b.media.send_video(&black()).await?;
    assert_eq!(
        a.media
            .receive_video(Duration::from_secs(3))
            .await
            .map_err(|e| format!("video b->a: {e}"))?
            .i420,
        black()
    );
    a.media.close().await?;
    b.media.close().await?;
    Ok(())
}
