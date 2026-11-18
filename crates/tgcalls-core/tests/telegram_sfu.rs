//! A real controlling ICE/DTLS/SRTP peer exercising Telegram's JSON signaling shape.
//! This is an SFU conformance fixture, not a live Telegram compatibility claim.
use async_trait::async_trait;
use rtc::ice::mdns::MulticastDnsMode;
use rtc::peer_connection::configuration::{
    media_engine::MediaEngine, setting_engine::SettingEngineBuilder,
};
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tgcalls_core::{PCM_BYTES, codec::Decoder, session::Session};
use tokio::sync::{mpsc, watch};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCSessionDescription,
};

struct SfuHandler {
    gathered: watch::Sender<bool>,
    audio: mpsc::Sender<rtc::rtp::Packet>,
}
#[async_trait]
impl PeerConnectionEventHandler for SfuHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.send_replace(true);
        }
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let tx = self.audio.clone();
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                    let _ = tx.send(packet).await;
                }
            }
        });
    }
}

fn attr<'a>(sdp: &'a str, prefix: &str) -> Result<&'a str, Box<dyn std::error::Error>> {
    sdp.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .ok_or_else(|| format!("missing {prefix}").into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telegram_json_bridge_answers_a_controlling_sfu() -> Result<(), Box<dyn std::error::Error>>
{
    let call = Session::group("127.0.0.1", None, 98765).await?;
    let template = call.local_offer().await?;
    let payload: serde_json::Value = serde_json::from_str(&call.join_payload().await?)?;
    assert_eq!(payload["ssrc"], 98765);
    let (audio, mut incoming) = mpsc::channel(8);
    let (gathered, mut gather_rx) = watch::channel(false);
    let mut media = MediaEngine::default();
    media.register_codec(
        RTCRtpCodecParameters {
            payload_type: 111,
            rtp_codec: RTCRtpCodec {
                mime_type: "audio/opus".into(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
                rtcp_feedback: vec![],
            },
        },
        RtpCodecKind::Audio,
    )?;
    let server = PeerConnectionBuilder::new()
        .with_media_engine(media)
        .with_setting_engine(
            SettingEngineBuilder::default()
                .with_prflx_acceptance_min_wait(Some(Duration::ZERO))
                .with_multicast_dns_mode(MulticastDnsMode::Disabled)
                .build(),
        )
        .with_handler(Arc::new(SfuHandler { gathered, audio }))
        .with_udp_addrs(vec!["127.0.0.1:0"])
        .build()
        .await?;
    server
        .add_transceiver_from_kind(
            RtpCodecKind::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await?;
    server.create_data_channel("data", None).await?;
    let offer = server.create_offer(None).await?;
    server.set_local_description(offer).await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !*gather_rx.borrow_and_update() {
            gather_rx.changed().await?;
        }
        Ok::<_, tokio::sync::watch::error::RecvError>(())
    })
    .await??;
    let offer = server
        .local_description()
        .await
        .ok_or("missing server offer")?
        .sdp;
    let (hash, fp) = attr(&offer, "a=fingerprint:")?
        .split_once(' ')
        .ok_or("invalid fingerprint")?;
    let candidates: Vec<_> = offer.lines().filter_map(|l| l.strip_prefix("a=candidate:"))
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 8 { return Err("invalid candidate"); }
            Ok(json!({"foundation":fields[0], "component":fields[1], "protocol":fields[2], "priority":fields[3],
                "ip":fields[4], "port":fields[5], "type":fields[7]}))
        }).collect::<Result<_, _>>()?;
    let response = json!({"transport":{
        "ufrag":attr(&offer,"a=ice-ufrag:")?, "pwd":attr(&offer,"a=ice-pwd:")?,
        "fingerprints":[{"hash":hash,"setup":"active","fingerprint":fp}], "candidates":candidates,
    }})
    .to_string();
    let answer = template.replace("a=setup:actpass", "a=setup:passive");
    server
        .set_remote_description(RTCSessionDescription::answer(answer)?)
        .await?;
    call.connect_telegram(&response).await?;
    call.set_sources(vec![11_111]).await?;
    call.send_pcm(&[0; PCM_BYTES]).await?;
    let received = tokio::time::timeout(Duration::from_secs(3), incoming.recv())
        .await?
        .ok_or("missing SRTP audio")?;
    assert_eq!(received.header.ssrc, 98765);
    assert_eq!(Decoder::new()?.decode(&received.payload)?.len(), PCM_BYTES);
    call.close().await?;
    server.close().await?;
    Ok(())
}
