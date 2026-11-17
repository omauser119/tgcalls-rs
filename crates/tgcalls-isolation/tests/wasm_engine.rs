//! Functional media test, not a performance benchmark. Build scripts/build-wasm.sh
//! first and set TGCALLS_WORKER_WASM to the resulting trusted artifact.
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tgcalls_isolation::wasm::{Limits, WasmSandbox};

fn execute(worker: &mut WasmSandbox, request: Value) -> Result<Value, Box<dyn std::error::Error>> {
    let result: Value = serde_json::from_slice(&worker.request(&serde_json::to_vec(&request)?)?)?;
    if result["ok"] != true {
        return Err(format!("worker error: {}", result["error"]).into());
    }
    Ok(result)
}
fn collect(
    response: Value,
    source: usize,
    queue: &mut VecDeque<(usize, Value)>,
    audio: &mut usize,
    video: &mut usize,
) {
    if let Some(datagrams) = response["datagrams"].as_array() {
        for d in datagrams {
            queue.push_back((1 - source, d["bytes"].clone()));
        }
    }
    *audio += response["audio"].as_array().map_or(0, Vec::len);
    if let Some(frames) = response["video"].as_array() {
        for frame in frames {
            assert_eq!(frame["width"], 32);
            assert_eq!(frame["height"], 32);
            *video += 1;
        }
    }
}
#[test]
fn complete_webrtc_audio_video_inside_no_jit_interpreter() -> Result<(), Box<dyn std::error::Error>>
{
    let path = std::env::var("TGCALLS_WORKER_WASM")?;
    let bytes = std::fs::read(path)?;
    let mut workers = [
        WasmSandbox::new(&bytes, Limits::default())?,
        WasmSandbox::new(&bytes, Limits::default())?,
    ];
    let addresses = ["127.0.0.1:40001", "127.0.0.1:40002"];
    for i in 0..2 {
        execute(
            &mut workers[i],
            json!({"op":"new","local":addresses[i],"ssrc":1000+i*1000,"video":{"width":32,"height":32,"fps":10,"bitrate":64000}}),
        )?;
    }
    let offer = execute(&mut workers[0], json!({"op":"offer"}))?["payload"].clone();
    let answered = execute(&mut workers[1], json!({"op":"answer","sdp":offer}))?;
    let initial = execute(
        &mut workers[0],
        json!({"op":"accept_answer","sdp":answered["payload"]}),
    )?;
    let mut queue = VecDeque::new();
    let (mut audio, mut video) = (0, 0);
    collect(answered, 1, &mut queue, &mut audio, &mut video);
    collect(initial, 0, &mut queue, &mut audio, &mut video);
    let start = Instant::now();
    let mut sent = false;
    let mut states = [String::new(), String::new()];
    let mut total_packets = 0;
    while start.elapsed() < Duration::from_secs(45) && (audio == 0 || video == 0) {
        let mut connected = true;
        for (i, worker) in workers.iter_mut().enumerate() {
            let response = execute(worker, json!({"op":"tick"}))?;
            let state = response["payload"]["connection_state"]
                .as_str()
                .unwrap_or_default();
            if states[i] != state {
                eprintln!("peer {i}: {state}");
                states[i] = state.into();
            }
            connected &= state.eq_ignore_ascii_case("connected");
            collect(response, i, &mut queue, &mut audio, &mut video);
        }
        let mut packets = 0;
        while let Some((target, bytes)) = queue.pop_front() {
            let response = execute(
                &mut workers[target],
                json!({"op":"packet","peer":addresses[1-target],"bytes":bytes}),
            )?;
            collect(response, target, &mut queue, &mut audio, &mut video);
            packets += 1;
            total_packets += 1;
            assert!(packets < 1024, "network loop budget");
        }
        if !sent && connected {
            collect(
                execute(
                    &mut workers[0],
                    json!({"op":"send_audio","pcm":vec![0;3840]}),
                )?,
                0,
                &mut queue,
                &mut audio,
                &mut video,
            );
            let mut i420 = vec![16; 1024];
            i420.extend(vec![128; 512]);
            collect(
                execute(&mut workers[0], json!({"op":"send_video","i420":i420}))?,
                0,
                &mut queue,
                &mut audio,
                &mut video,
            );
            sent = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        audio > 0,
        "no decoded RustyOpus frame; states={states:?}, packets={total_packets}, sent={sent}"
    );
    assert!(video > 0, "no decoded Remade H264 frame");
    assert_eq!(workers[0].generation(), 0);
    assert_eq!(workers[1].generation(), 0);
    Ok(())
}
