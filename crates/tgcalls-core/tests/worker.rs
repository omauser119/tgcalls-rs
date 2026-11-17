use crypto_bigint::U2048;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tgcalls_core::{auth::TELEGRAM_PRIME_HEX, worker::Worker};

fn execute(worker: &mut Worker, request: Value) -> Result<Value, Box<dyn std::error::Error>> {
    let response: Value = serde_json::from_slice(&worker.execute(&serde_json::to_vec(&request)?))?;
    if response["ok"] != true {
        return Err(response["error"].to_string().into());
    }
    Ok(response)
}
fn collect(
    response: Value,
    source: usize,
    packets: &mut VecDeque<(usize, Value)>,
    audio: &mut usize,
    video: &mut usize,
) {
    if let Some(out) = response["datagrams"].as_array() {
        for packet in out {
            packets.push_back((1 - source, packet["bytes"].clone()));
        }
    }
    *audio += response["audio"].as_array().map_or(0, Vec::len);
    *video += response["video"].as_array().map_or(0, Vec::len);
}
#[test]
fn mobile_worker_keeps_dh_keys_internal_and_negotiates_personal_audio_video()
-> Result<(), Box<dyn std::error::Error>> {
    let mut peers = [Worker::default(), Worker::default()];
    let addresses = ["127.0.0.1:41001", "127.0.0.1:41002"];
    for i in 0..2 {
        execute(
            &mut peers[i],
            json!({"op":"new","local":addresses[i],"ssrc":1000+i*1000,"video":{"width":32,"height":32,"fps":10,"bitrate":64000}}),
        )?;
    }
    let prime = U2048::from_be_hex(TELEGRAM_PRIME_HEX)
        .to_be_bytes()
        .to_vec();
    let commitment = execute(
        &mut peers[0],
        json!({"op":"init_dh","g":3,"prime":prime,"random":vec![1;256]}),
    )?["payload"]
        .clone();
    let gb = execute(
        &mut peers[1],
        json!({"op":"init_dh","g":3,"prime":prime,"random":vec![2;256],"commitment":commitment}),
    )?["payload"]
        .clone();
    let a = execute(&mut peers[0], json!({"op":"complete_dh","remote":gb}))?["payload"].clone();
    let b = execute(
        &mut peers[1],
        json!({"op":"complete_dh","remote":a["public"],"fingerprint":a["fingerprint"]}),
    )?["payload"]
        .clone();
    assert_eq!(a["verification_hash"], b["verification_hash"]);
    execute(
        &mut peers[0],
        json!({"op":"start_personal","outgoing":true}),
    )?;
    execute(
        &mut peers[1],
        json!({"op":"start_personal","outgoing":false}),
    )?;
    let start = Instant::now();
    let mut queue = VecDeque::new();
    let (mut audio, mut video) = (0, 0);
    let mut sent = false;
    while start.elapsed() < Duration::from_secs(4) && (audio == 0 || video == 0) {
        for i in 0..2 {
            let response = execute(&mut peers[i], json!({"op":"poll_signaling"}))?;
            if let Some(signals) = response["signaling"].as_array() {
                // Deliberately deliver NegotiateChannels before InitialSetup.
                for signal in signals.iter().rev() {
                    let received =
                        execute(&mut peers[1 - i], json!({"op":"signaling","packet":signal}))?;
                    collect(received, 1 - i, &mut queue, &mut audio, &mut video);
                }
            }
            collect(response, i, &mut queue, &mut audio, &mut video);
        }
        let mut connected = true;
        for (i, peer) in peers.iter_mut().enumerate() {
            let response = execute(peer, json!({"op":"tick"}))?;
            connected &= response["payload"]["connection_state"]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case("connected"));
            collect(response, i, &mut queue, &mut audio, &mut video);
        }
        let mut count = 0;
        while let Some((target, bytes)) = queue.pop_front() {
            let response = execute(
                &mut peers[target],
                json!({"op":"packet","peer":addresses[1-target],"bytes":bytes}),
            )?;
            collect(response, target, &mut queue, &mut audio, &mut video);
            count += 1;
            assert!(count < 1024);
        }
        if connected && !sent {
            collect(
                execute(&mut peers[0], json!({"op":"send_audio","pcm":vec![0;3840]}))?,
                0,
                &mut queue,
                &mut audio,
                &mut video,
            );
            let mut frame = vec![16; 1024];
            frame.extend(vec![128; 512]);
            collect(
                execute(&mut peers[1], json!({"op":"send_video","i420":frame}))?,
                1,
                &mut queue,
                &mut audio,
                &mut video,
            );
            sent = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(audio > 0, "missing decoded audio");
    assert!(video > 0, "missing decoded video");
    Ok(())
}
