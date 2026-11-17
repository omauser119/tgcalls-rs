use std::{sync::Arc, time::Duration};
use tgcalls_core::{Impairment, PCM_BYTES, session::Session};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_ice_dtls_srtp_audio_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    let session = Arc::new(Session::loopback(Impairment::default(), "127.0.0.1", None).await?);
    let writer = session.clone();
    let task = tokio::spawn(async move {
        for _ in 0..8 {
            writer.send_pcm(&vec![0; PCM_BYTES]).await?;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<(), tgcalls_core::Error>(())
    });
    let frame = session.receive(Duration::from_secs(3)).await?;
    assert!(!frame.concealed);
    assert_eq!(frame.pcm.len(), PCM_BYTES);
    task.await??;
    assert_eq!(session.stats().sent_frames, 8);
    session.close().await?;
    Ok(())
}
