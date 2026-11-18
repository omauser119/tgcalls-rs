use crypto_bigint::U2048;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tgcalls_core::{
    PCM_BYTES,
    auth::TELEGRAM_PRIME_HEX,
    sansio::Engine,
    worker::{BUFFER_MAGIC, Worker},
};

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

#[test]
fn worker_group_answers_controlling_sfu_and_sends_audio() -> Result<(), Box<dyn std::error::Error>>
{
    let local = "127.0.0.1:42001".parse()?;
    let remote = "127.0.0.1:42002".parse()?;
    let mut client = Worker::default();
    execute(
        &mut client,
        json!({"op":"new", "group":true, "local":local, "ssrc":101}),
    )?;
    let join = execute(&mut client, json!({"op":"group_payload"}))?;
    let join: Value =
        serde_json::from_str(join["payload"].as_str().ok_or("missing join payload")?)?;
    assert_eq!(join["ssrc"], 101);
    let mut sfu = Engine::new(remote, 202, None, Instant::now())?;
    sfu.personal_channel()?;
    let offer = sfu.offer()?;
    let params = json!({"transport": sfu_transport(&offer)?}).to_string();
    let result = execute(&mut client, json!({"op":"group_connect", "params":params}))?;
    let answer = result["payload"].as_str().ok_or("missing group answer")?;
    assert!(answer.contains("a=setup:passive"));
    sfu.accept_answer(answer.into())?;
    let now = Instant::now();
    let mut sent = false;
    while now.elapsed() < Duration::from_secs(5) {
        sfu.tick(Instant::now())?;
        let tick = execute(&mut client, json!({"op":"tick"}))?;
        let mut responses = vec![tick];
        for packet in sfu.outgoing()? {
            responses.push(execute(
                &mut client,
                json!({"op":"packet", "peer":remote, "bytes":packet.bytes}),
            )?);
        }
        if !sent && responses[0]["payload"]["connection_state"] == "connected" {
            execute(&mut client, json!({"op":"group_sources", "ssrcs":[202]}))?;
            responses.push(execute(
                &mut client,
                json!({"op":"send_audio", "pcm":vec![0; PCM_BYTES]}),
            )?);
            sent = true;
        }
        for response in responses {
            for packet in response["datagrams"].as_array().into_iter().flatten() {
                let bytes: Vec<u8> = serde_json::from_value(packet["bytes"].clone())?;
                sfu.receive_datagram(local, &bytes, Instant::now())?;
            }
        }
        if let Some(frame) = sfu.audio.pop_front() {
            assert_eq!(frame.ssrc, 101);
            assert_eq!(frame.pcm.len(), PCM_BYTES);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(format!(
        "no group SRTP audio; state={}, sent={sent}",
        sfu.connection_state
    )
    .into())
}

fn sfu_transport(offer: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let attribute = |prefix| {
        offer
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .ok_or("missing SDP attribute")
    };
    let (hash, fp) = attribute("a=fingerprint:")?
        .split_once(' ')
        .ok_or("invalid fingerprint")?;
    let candidates: Vec<_> = offer.lines().filter_map(|line| line.strip_prefix("a=candidate:"))
        .map(|c| {
            let fields: Vec<_> = c.split_whitespace().collect();
            if fields.len() < 8 { return Err("invalid candidate"); }
            Ok(json!({"foundation":fields[0], "component":fields[1], "protocol":fields[2], "priority":fields[3], "ip":fields[4], "port":fields[5], "type":fields[7]}))
        }).collect::<Result<_, _>>()?;
    Ok(json!({
        "ufrag":attribute("a=ice-ufrag:")?, "pwd":attribute("a=ice-pwd:")?,
        "fingerprints":[{"hash":hash, "setup":"active", "fingerprint":fp}], "candidates":candidates,
    }))
}

#[test]
fn group_worker_emits_simulcast_and_negotiates_sfu_h264_payload_types()
-> Result<(), Box<dyn std::error::Error>> {
    let mut client = Worker::default();
    execute(
        &mut client,
        json!({"op":"new", "group":true, "local":"127.0.0.1:42005", "ssrc":400,
        "video_layers":[{"width":32,"height":32,"fps":10,"bitrate":64_000},
                        {"width":64,"height":64,"fps":10,"bitrate":128_000}]}),
    )?;
    let join = execute(&mut client, json!({"op":"group_payload"}))?;
    let payload: Value =
        serde_json::from_str(join["payload"].as_str().ok_or("missing join payload")?)?;
    assert_eq!(
        payload["ssrc-groups"],
        json!([
            {"semantics":"SIM", "sources":[401, 403]},
            {"semantics":"FID", "sources":[401, 402]},
            {"semantics":"FID", "sources":[403, 404]},
        ])
    );
    let mut sfu = Engine::new("127.0.0.1:42006".parse()?, 501, None, Instant::now())?;
    let offer = sfu.offer()?;
    let params = json!({"transport": sfu_transport(&offer)?, "video": {"payload-types":[
        {"name":"H264", "id":108, "clockrate":90000, "parameters":{"packetization-mode":"1", "profile-level-id":"42e01f"}},
        {"name":"rtx", "id":109, "clockrate":90000, "parameters":{"apt":"108"}},
    ]}}).to_string();
    let result = execute(&mut client, json!({"op":"group_connect", "params":params}))?;
    let answer = result["payload"].as_str().ok_or("missing group answer")?;
    assert!(answer.contains("a=rtpmap:108 H264/90000"));
    assert!(answer.contains("a=rtpmap:109 rtx/90000"));
    execute(
        &mut client,
        json!({"op":"group_video","subscriptions":[{
            "endpoint":"other", "sources":[{"ssrc":502,"rtx":503}], "min_height":180,"max_height":360,
        }]}),
    )?;
    Ok(())
}

#[test]
fn host_buffer_requires_exact_verified_key_and_pcm_sizes() -> Result<(), Box<dyn std::error::Error>>
{
    let mut client = Worker::default();
    execute(
        &mut client,
        json!({"op":"new", "local":"127.0.0.1:42003", "ssrc":303}),
    )?;
    let mut key = [BUFFER_MAGIC, &[1], &[7; 255]].concat();
    let request = |worker: &mut Worker, body: &[u8]| -> Result<bool, serde_json::Error> {
        Ok(serde_json::from_slice::<Value>(&worker.execute(body))?["ok"] == true)
    };
    assert!(!request(&mut client, &key)?);
    key.push(7);
    assert!(request(&mut client, &key)?);
    assert!(!request(&mut client, &key)?);
    execute(&mut client, json!({"op":"start_personal", "outgoing":true}))?;
    execute(
        &mut client,
        json!({"op":"configure_audio", "capture_rate":48000, "capture_channels":1,
        "render_rate":48000, "render_channels":2,
        "options":{"echo_cancellation":true, "noise_suppression":true, "automatic_gain":true}}),
    )?;
    assert!(request(
        &mut client,
        &[BUFFER_MAGIC, &[2], &[0; 1920]].concat()
    )?);
    assert!(!request(
        &mut client,
        &[BUFFER_MAGIC, &[2], &[0; 1919]].concat()
    )?);
    assert!(request(
        &mut client,
        &[BUFFER_MAGIC, &[3, 30, 0], &[0; 960]].concat()
    )?);
    assert!(!request(
        &mut client,
        &[BUFFER_MAGIC, &[3, 30, 0], &[0; 959]].concat()
    )?);
    Ok(())
}
