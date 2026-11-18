use std::{path::PathBuf, time::Duration};
use tgcalls_core::{AudioFrame, Impairment, PCM_BYTES};
use tgcalls_sandbox::{Config, Sandbox, ring::SharedAudio};

#[test]
fn sealed_ring_roundtrip_and_overflow_are_bounded() -> anyhow::Result<()> {
    use std::os::fd::AsFd;
    let producer = SharedAudio::create()?;
    let consumer = SharedAudio::attach(producer.fd().try_clone_to_owned()?)?;
    let frame = AudioFrame {
        ssrc: 7,
        sequence: 8,
        timestamp: 9,
        concealed: false,
        target_delay_ms: 40,
        pcm: vec![5; PCM_BYTES],
    };
    producer.push(false, &frame)?;
    let received = consumer
        .pop(false)?
        .ok_or_else(|| anyhow::anyhow!("missing frame"))?;
    assert_eq!(received.pcm, frame.pcm);
    assert_eq!(received.timestamp, 9);
    for _ in 0..32 {
        producer.push(false, &frame)?;
    }
    assert!(producer.push(false, &frame).is_err());
    assert!(nix::unistd::ftruncate(producer.fd().as_fd(), 0).is_err());
    Ok(())
}

#[test]
fn seccomp_blocks_filesystem_in_a_separate_process() -> anyhow::Result<()> {
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_tgcalls-sandbox"))
        .arg("--self-test-seccomp")
        .status()?;
    assert!(status.success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_transmits_audio_and_recovers_from_sigkill() -> anyhow::Result<()> {
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_tgcalls-sandbox"));
    let sandbox = Sandbox::start(
        helper,
        Config::Loopback {
            impairment: Impairment::default(),
        },
        "127.0.0.1".parse()?,
    )
    .await?;
    let pid = sandbox
        .worker_pid()
        .await
        .ok_or_else(|| anyhow::anyhow!("missing worker PID"))?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    assert!(status.lines().any(|l| l == "NoNewPrivs:\t1"));
    assert!(status.lines().any(|l| l == "Seccomp:\t2"));
    assert!(status.lines().any(|l| l == "CapEff:\t0000000000000000"));
    let writer = sandbox.clone();
    let send = tokio::spawn(async move {
        for _ in 0..8 {
            writer.send_pcm(vec![0; PCM_BYTES]).await?;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    });
    let frame = sandbox.receive(Duration::from_secs(3)).await?;
    assert_eq!(frame.pcm.len(), PCM_BYTES);
    assert!(!frame.concealed);
    send.await??;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while sandbox.generation() == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await?;
    assert_ne!(sandbox.worker_pid().await, Some(pid));
    assert!(!sandbox.needs_rejoin());
    let reader = sandbox.clone();
    let waiting = tokio::spawn(async move { reader.receive(Duration::from_secs(30)).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    sandbox.close().await;
    assert!(tokio::time::timeout(Duration::from_secs(1), waiting).await??.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telegram_worker_restart_requires_fresh_join_credentials() -> anyhow::Result<()> {
    let sandbox = Sandbox::start(
        PathBuf::from(env!("CARGO_BIN_EXE_tgcalls-sandbox")),
        Config::Telegram { ssrc: 123456 },
        "127.0.0.1".parse()?,
    )
    .await?;
    let before: serde_json::Value = serde_json::from_str(&sandbox.join_payload().await?)?;
    assert!(sandbox.needs_rejoin());
    assert!(sandbox.send_pcm(vec![0; PCM_BYTES]).await.is_err());
    let pid = sandbox
        .worker_pid()
        .await
        .ok_or_else(|| anyhow::anyhow!("missing worker PID"))?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while sandbox.generation() == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await?;
    let after: serde_json::Value = serde_json::from_str(&sandbox.join_payload().await?)?;
    assert_ne!(before["ufrag"], after["ufrag"]);
    assert_ne!(before["fingerprints"], after["fingerprints"]);
    assert!(sandbox.needs_rejoin());
    assert!(sandbox.send_pcm(vec![0; PCM_BYTES]).await.is_err());
    sandbox.close().await;
    Ok(())
}
