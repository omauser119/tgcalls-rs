use crate::video::{H264_FMTP, H264Assembler, VideoConfig, VideoDecoder, VideoEncoder, VideoFrame};
use crate::{
    AudioFrame, Error, Impairment, Result, SessionStats, codec, jitter::JitterBuffer,
    packet::validate_sdp, telegram,
};
use async_trait::async_trait;
use bytes::Bytes;
use rtc::ice::mdns::MulticastDnsMode;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp::{Packet, header::Header};
use rtc::rtp_transceiver::rtp_sender::RTCRtpHeaderExtensionCapability;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, watch};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, static_rtp::TrackLocalStaticRTP};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    received: AtomicU64,
    concealed: AtomicU64,
    dropped: AtomicU64,
    active_tracks: AtomicU64,
    sent_video: AtomicU64,
    received_video: AtomicU64,
}

struct TrackBudget(Arc<Counters>);
impl Drop for TrackBudget {
    fn drop(&mut self) {
        self.0.active_tracks.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Handler {
    state: watch::Sender<RTCPeerConnectionState>,
    gathered: watch::Sender<bool>,
    frames: mpsc::Sender<Result<AudioFrame>>,
    echo: Option<Arc<TrackLocalStaticRTP>>,
    counters: Arc<Counters>,
    impairment: Impairment,
    video_frames: mpsc::Sender<Result<VideoFrame>>,
    echo_video: Option<Arc<TrackLocalStaticSample>>,
}

#[async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        self.state.send_replace(state);
    }
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.send_replace(true);
        }
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if self.counters.active_tracks.fetch_add(1, Ordering::AcqRel) >= 64 {
            self.counters.active_tracks.fetch_sub(1, Ordering::AcqRel);
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let budget = TrackBudget(self.counters.clone());
        let frames = self.frames.clone();
        let counters = self.counters.clone();
        let echo = self.echo.clone();
        let impairment = self.impairment.clone();
        let is_video = track.kind().await == RtpCodecKind::Video;
        let video_frames = self.video_frames.clone();
        let echo_video = self.echo_video.clone();
        // Handler must return immediately: the connection driver delivers media itself.
        tokio::spawn(async move {
            let _budget = budget;
            use futures::FutureExt;
            if is_video {
                let result = std::panic::AssertUnwindSafe(receive_video_track(
                    track,
                    echo_video,
                    video_frames.clone(),
                    counters,
                ))
                .catch_unwind()
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        let _ = video_frames.try_send(Err(e));
                    }
                    Err(_) => {
                        let _ = video_frames.try_send(Err(Error::Panic));
                    }
                }
                return;
            }
            let result = std::panic::AssertUnwindSafe(receive_track(
                track,
                echo,
                frames.clone(),
                counters,
                impairment,
            ))
            .catch_unwind()
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = frames.try_send(Err(e));
                }
                Err(_) => {
                    let _ = frames.try_send(Err(Error::Panic));
                }
            }
        });
    }
}

async fn receive_video_track(
    track: Arc<dyn TrackRemote>,
    echo: Option<Arc<TrackLocalStaticSample>>,
    frames: mpsc::Sender<Result<VideoFrame>>,
    counters: Arc<Counters>,
) -> Result<()> {
    use webrtc::media_stream::Track;
    let mut assembler = H264Assembler::default();
    let mut decoder = VideoDecoder::default();
    let mut last_pli = Instant::now() - Duration::from_secs(2);
    while let Some(event) = track.poll().await {
        let TrackRemoteEvent::OnRtpPacket(mut packet) = event else {
            continue;
        };
        if let Some(output) = &echo {
            packet.header.ssrc = *output
                .ssrcs()
                .await
                .first()
                .ok_or_else(|| Error::Invalid("missing video echo SSRC".into()))?;
            output.write_rtp(packet).await?;
            continue;
        }
        let result = assembler
            .push(
                packet.header.sequence_number,
                packet.header.timestamp,
                packet.header.marker,
                &packet.payload,
                Instant::now(),
            )
            .and_then(|au| {
                au.map(|au| decoder.decode(packet.header.ssrc, packet.header.timestamp, &au))
                    .transpose()
                    .map(Option::flatten)
            });
        match result {
            Ok(Some(frame)) => {
                counters.received_video.fetch_add(1, Ordering::Relaxed);
                if frames.try_send(Ok(frame)).is_err() {
                    counters.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(None) => {}
            Err(_) => {
                counters.dropped.fetch_add(1, Ordering::Relaxed);
                assembler = H264Assembler::default();
                decoder = VideoDecoder::default();
                if last_pli.elapsed() >= Duration::from_secs(1) {
                    last_pli = Instant::now();
                    let _=track.write_rtcp(vec![Box::new(rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication {
                        sender_ssrc:0,media_ssrc:packet.header.ssrc,
                    })]).await;
                }
            }
        }
    }
    Ok(())
}

async fn receive_track(
    track: Arc<dyn TrackRemote>,
    echo: Option<Arc<TrackLocalStaticRTP>>,
    frames: mpsc::Sender<Result<AudioFrame>>,
    counters: Arc<Counters>,
    impairment: Impairment,
) -> Result<()> {
    let mut decoder = codec::Decoder::new()?;
    let mut jitter = JitterBuffer::<Packet>::default();
    let mut next_playout = Instant::now();
    let mut last: Option<(u32, u16, u32, u32)> = None;
    let mut delayed: Vec<(Instant, Packet)> = Vec::new();
    let mut rng = impairment.seed.max(1);
    loop {
        let deadline = if echo.is_some() {
            delayed.iter().map(|(at, _)| *at).min()
        } else {
            jitter.ready_at().map(|ready| ready.max(next_playout))
        };
        tokio::select! {
            event = track.poll() => match event {
                Some(TrackRemoteEvent::OnRtpPacket(mut packet)) => {
                    if packet.payload.len() > crate::MAX_PACKET { counters.dropped.fetch_add(1, Ordering::Relaxed); continue; }
                    if let Some(output) = &echo {
                        use webrtc::media_stream::Track;
                        packet.header.ssrc = *output.ssrcs().await.first().ok_or_else(|| Error::Invalid("echo SSRC missing".into()))?;
                        // Seeded impairment acts on the media return path, not on ICE/DTLS.
                        rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
                        if ((rng % 1_000_000) as f64) < impairment.loss_percent * 10_000.0 {
                            counters.dropped.fetch_add(1, Ordering::Relaxed); continue;
                        }
                        let delay = impairment.delay_ms + if impairment.jitter_ms == 0 { 0 } else { rng % (impairment.jitter_ms + 1) };
                        if delay == 0 { output.write_rtp(packet).await?; }
                        else if delayed.len() < 128 { delayed.push((Instant::now() + Duration::from_millis(delay), packet)); }
                        else { counters.dropped.fetch_add(1, Ordering::Relaxed); }
                    } else {
                        jitter.push(packet.header.sequence_number, packet.header.timestamp, Instant::now(), packet);
                    }
                },
                Some(_) => {},
                None => break,
            },
            Some(scheduled) = async {
                let Some(at) = deadline else { return std::future::pending().await; };
                tokio::time::sleep_until(at.into()).await;
                Some(at)
            } => {
                let now = Instant::now();
                if let Some(output) = &echo {
                    let mut index = 0;
                    while index < delayed.len() {
                        if delayed[index].0 <= now {
                            let (_, packet) = delayed.swap_remove(index);
                            output.write_rtp(packet).await?;
                        } else { index += 1; }
                    }
                    continue;
                }
                if let Some(packet) = jitter.pop(now) {
                    let (ssrc, sequence, timestamp, concealed, pcm) = if let Some(packet) = packet {
                        let pcm = decoder.decode(&packet.payload)?;
                        (packet.header.ssrc, packet.header.sequence_number, packet.header.timestamp, false, pcm)
                    } else if let Some((ssrc, seq, ts, samples)) = last {
                        (ssrc, seq.wrapping_add(1), ts.wrapping_add(samples), true, decoder.decode(&[])?)
                    } else { continue; };
                    let duration = Duration::from_secs_f64(pcm.len() as f64 / (crate::SAMPLE_RATE * crate::CHANNELS * 2) as f64);
                    next_playout = scheduled + duration;
                    if next_playout <= now {
                        next_playout = now + duration;
                    }
                    let samples = pcm.len() / (crate::CHANNELS * 2);
                    jitter.set_frame_samples(samples);
                    last = Some((ssrc, sequence, timestamp, samples as u32));
                    if concealed { counters.concealed.fetch_add(1, Ordering::Relaxed); }
                    counters.received.fetch_add(1, Ordering::Relaxed);
                    if frames.try_send(Ok(AudioFrame { ssrc, sequence, timestamp, concealed, target_delay_ms: jitter.target_delay_ms(), pcm })).is_err() {
                        counters.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
    Ok(())
}

struct Peer {
    pc: Arc<dyn PeerConnection>,
    output: Arc<TrackLocalStaticRTP>,
    state: watch::Receiver<RTCPeerConnectionState>,
    gathered: watch::Receiver<bool>,
    ssrc: u32,
    video_output: Option<Arc<TrackLocalStaticSample>>,
}

impl Peer {
    #[allow(clippy::too_many_arguments)]
    async fn new(
        bind_ip: &str,
        socket: Option<std::net::UdpSocket>,
        frames: mpsc::Sender<Result<AudioFrame>>,
        counters: Arc<Counters>,
        echo: bool,
        impairment: Impairment,
        ssrc: u32,
        video: Option<VideoConfig>,
        video_frames: mpsc::Sender<Result<VideoFrame>>,
        one_way: bool,
        ice_servers: Vec<webrtc::peer_connection::RTCIceServer>,
    ) -> Result<Self> {
        let codec = RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.into(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
            rtcp_feedback: vec![],
        };
        let mut media = MediaEngine::default();
        media.register_codec(
            RTCRtpCodecParameters {
                rtp_codec: codec.clone(),
                payload_type: 111,
            },
            RtpCodecKind::Audio,
        )?;
        media.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: "urn:ietf:params:rtp-hdrext:ssrc-audio-level".into(),
            },
            RtpCodecKind::Audio,
            None,
        )?;
        let video_codec = RTCRtpCodec {
            mime_type: "video/H264".into(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: H264_FMTP.into(),
            rtcp_feedback: vec![rtc::rtp_transceiver::rtp_sender::RTCPFeedback {
                typ: "nack".into(),
                parameter: "pli".into(),
            }],
        };
        if let Some(config) = &video {
            config.validate()?;
            media.register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: video_codec.clone(),
                    payload_type: 100,
                },
                RtpCodecKind::Video,
            )?;
        }
        let registry = register_default_interceptors(Registry::new(), &mut media)?;
        let output = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            "tgcalls-rs".into(),
            format!("audio-{ssrc}"),
            "audio".into(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec,
                ..Default::default()
            }],
        )));
        let (state_tx, state) = watch::channel(RTCPeerConnectionState::New);
        let (gather_tx, gathered) = watch::channel(false);
        let video_output = video
            .as_ref()
            .map(|_| {
                TrackLocalStaticSample::new(
                    Instant::now(),
                    MediaStreamTrack::new(
                        "tgcalls-rs".into(),
                        format!("video-{}", ssrc.wrapping_add(1)),
                        "camera".into(),
                        RtpCodecKind::Video,
                        vec![RTCRtpEncodingParameters {
                            rtp_coding_parameters: RTCRtpCodingParameters {
                                ssrc: Some(ssrc.wrapping_add(1)),
                                ..Default::default()
                            },
                            codec: video_codec,
                            ..Default::default()
                        }],
                    ),
                )
            })
            .transpose()?
            .map(Arc::new);
        let handler = Arc::new(Handler {
            state: state_tx,
            gathered: gather_tx,
            frames,
            echo: if echo { Some(output.clone()) } else { None },
            counters,
            impairment,
            video_frames,
            echo_video: if echo { video_output.clone() } else { None },
        });
        let mut builder = PeerConnectionBuilder::new()
            .with_media_engine(media)
            .with_configuration(
                webrtc::peer_connection::RTCConfigurationBuilder::default()
                    .with_ice_servers(ice_servers)
                    .build(),
            )
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_data_channel_send_buffer_limit(64 * 1024)
            .with_setting_engine(
                SettingEngineBuilder::default()
                    .with_multicast_dns_mode(MulticastDnsMode::Disabled)
                    .build(),
            )
            .with_udp_addrs(vec![std::net::SocketAddr::new(
                bind_ip
                    .parse()
                    .map_err(|_| Error::Invalid("invalid bind IP".into()))?,
                0,
            )]);
        if let Some(socket) = socket {
            builder = builder
                .with_udp_addrs(vec![])
                .with_prebound_udp_sockets(vec![socket]);
        }
        let pc: Arc<dyn PeerConnection> = Arc::new(builder.build().await?);
        if one_way {
            pc.add_transceiver_from_track(
                output.clone(),
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Sendonly,
                    ..Default::default()
                }),
            )
            .await?;
        } else {
            pc.add_track(output.clone()).await?;
        }
        if let Some(track) = &video_output {
            if one_way {
                pc.add_transceiver_from_track(
                    track.clone(),
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Sendonly,
                        ..Default::default()
                    }),
                )
                .await?;
            } else {
                pc.add_track(track.clone()).await?;
            }
        }
        Ok(Self {
            pc,
            output,
            state,
            gathered,
            ssrc,
            video_output,
        })
    }

    async fn local_sdp(&self, offer: bool) -> Result<String> {
        let desc = if offer {
            self.pc.create_offer(None).await?
        } else {
            self.pc.create_answer(None).await?
        };
        self.pc.set_local_description(desc).await?;
        let mut gathered = self.gathered.clone();
        tokio::time::timeout(TIMEOUT, async {
            while !*gathered.borrow_and_update() {
                gathered.changed().await.map_err(|_| Error::Closed)?;
            }
            Ok::<(), Error>(())
        })
        .await
        .map_err(|_| Error::Timeout)??;
        self.pc
            .local_description()
            .await
            .map(|s| s.sdp)
            .ok_or(Error::Closed)
    }

    async fn wait_connected(&self) -> Result<()> {
        let mut state = self.state.clone();
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let current = *state.borrow_and_update();
                match current {
                    RTCPeerConnectionState::Connected => return Ok(()),
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        return Err(Error::Closed);
                    }
                    _ => state.changed().await.map_err(|_| Error::Closed)?,
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout)?
    }
}

pub struct Session {
    primary: Peer,
    echo: Option<Peer>,
    encoder: Mutex<codec::Encoder>,
    send_state: Mutex<(u16, u32)>,
    incoming: Mutex<mpsc::Receiver<Result<AudioFrame>>>,
    counters: Arc<Counters>,
    telegram: Mutex<Option<(String, String, Vec<u32>)>>,
    video_encoder: Mutex<Option<VideoEncoder>>,
    video_incoming: Mutex<mpsc::Receiver<Result<VideoFrame>>>,
}

impl Session {
    pub(crate) async fn personal_media(
        bind_ip: &str,
        sockets: Option<Vec<std::net::UdpSocket>>,
        ssrc: u32,
        video: Option<VideoConfig>,
        servers: Vec<webrtc::peer_connection::RTCIceServer>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel(128);
        let (vtx, vrx) = mpsc::channel(4);
        let counters = Arc::new(Counters::default());
        let primary = Peer::new(
            bind_ip,
            sockets.and_then(|s| s.into_iter().next()),
            tx,
            counters.clone(),
            false,
            Impairment::default(),
            ssrc,
            video.clone(),
            vtx,
            true,
            servers,
        )
        .await?;
        Ok(Self {
            primary,
            echo: None,
            encoder: Mutex::new(codec::Encoder::new()?),
            send_state: Mutex::new((0, 0)),
            incoming: Mutex::new(rx),
            counters,
            telegram: Mutex::new(None),
            video_encoder: Mutex::new(video.map(VideoEncoder::new).transpose()?),
            video_incoming: Mutex::new(vrx),
        })
    }
    pub async fn loopback(
        impairment: Impairment,
        bind_ip: &str,
        sockets: Option<Vec<std::net::UdpSocket>>,
    ) -> Result<Self> {
        Self::loopback_video(impairment, bind_ip, sockets, None).await
    }

    pub async fn loopback_video(
        impairment: Impairment,
        bind_ip: &str,
        sockets: Option<Vec<std::net::UdpSocket>>,
        video: Option<VideoConfig>,
    ) -> Result<Self> {
        impairment.validate()?;
        let (tx, rx) = mpsc::channel(128);
        let (vtx, vrx) = mpsc::channel(4);
        let counters = Arc::new(Counters::default());
        let mut sockets = sockets.unwrap_or_default().into_iter();
        let primary = Peer::new(
            bind_ip,
            sockets.next(),
            tx.clone(),
            counters.clone(),
            false,
            Impairment::default(),
            1001,
            video.clone(),
            vtx.clone(),
            false,
            vec![],
        )
        .await?;
        let echo = Peer::new(
            bind_ip,
            sockets.next(),
            tx,
            counters.clone(),
            true,
            impairment,
            1002,
            video.clone(),
            vtx,
            false,
            vec![],
        )
        .await?;
        let result = async {
            let offer = primary.local_sdp(true).await?;
            validate_sdp(&offer)?;
            echo.pc
                .set_remote_description(RTCSessionDescription::offer(offer)?)
                .await?;
            let answer = echo.local_sdp(false).await?;
            validate_sdp(&answer)?;
            primary
                .pc
                .set_remote_description(RTCSessionDescription::answer(answer)?)
                .await?;
            primary.wait_connected().await?;
            echo.wait_connected().await
        }
        .await;
        if let Err(e) = result {
            let _ = primary.pc.close().await;
            let _ = echo.pc.close().await;
            return Err(e);
        }
        Ok(Self {
            primary,
            echo: Some(echo),
            encoder: Mutex::new(codec::Encoder::new()?),
            send_state: Mutex::new((0, 0)),
            incoming: Mutex::new(rx),
            counters,
            telegram: Mutex::new(None),
            video_encoder: Mutex::new(video.map(VideoEncoder::new).transpose()?),
            video_incoming: Mutex::new(vrx),
        })
    }

    pub async fn group(
        bind_ip: &str,
        sockets: Option<Vec<std::net::UdpSocket>>,
        ssrc: u32,
    ) -> Result<Self> {
        Self::group_video(bind_ip, sockets, ssrc, None).await
    }

    pub async fn group_video(
        bind_ip: &str,
        sockets: Option<Vec<std::net::UdpSocket>>,
        ssrc: u32,
        video: Option<VideoConfig>,
    ) -> Result<Self> {
        if ssrc == 0 {
            return Err(Error::Invalid("SSRC must be nonzero".into()));
        }
        let (tx, rx) = mpsc::channel(128);
        let (vtx, vrx) = mpsc::channel(4);
        let counters = Arc::new(Counters::default());
        let primary = Peer::new(
            bind_ip,
            sockets.and_then(|s| s.into_iter().next()),
            tx,
            counters.clone(),
            false,
            Impairment::default(),
            ssrc,
            video.clone(),
            vtx,
            true,
            vec![],
        )
        .await?;
        primary.pc.create_data_channel("data", None).await?;
        let offer = primary.local_sdp(true).await?;
        Ok(Self {
            primary,
            echo: None,
            encoder: Mutex::new(codec::Encoder::new()?),
            send_state: Mutex::new((0, 0)),
            incoming: Mutex::new(rx),
            counters,
            telegram: Mutex::new(Some((offer, String::new(), vec![]))),
            video_encoder: Mutex::new(video.map(VideoEncoder::new).transpose()?),
            video_incoming: Mutex::new(vrx),
        })
    }

    pub async fn join_payload(&self) -> Result<String> {
        let info = self.telegram.lock().await;
        let (offer, _, _) = info
            .as_ref()
            .ok_or_else(|| Error::Invalid("not a Telegram session".into()))?;
        telegram::join_payload(offer, self.primary.ssrc)
    }

    /// Exposes local SDP for diagnostics and SFU conformance tests; it contains ICE secrets.
    pub async fn local_offer(&self) -> Result<String> {
        self.telegram
            .lock()
            .await
            .as_ref()
            .map(|(offer, _, _)| offer.clone())
            .ok_or_else(|| Error::Invalid("not a Telegram session".into()))
    }

    pub async fn connect_telegram(&self, params: &str) -> Result<()> {
        let mut info = self.telegram.lock().await;
        let (offer, response, sources) = info
            .as_mut()
            .ok_or_else(|| Error::Invalid("not a Telegram session".into()))?;
        let answer = telegram::answer_sdp(offer, params, sources)?;
        self.primary
            .pc
            .set_remote_description(RTCSessionDescription::answer(answer)?)
            .await?;
        *response = params.into();
        drop(info);
        self.primary.wait_connected().await
    }

    pub async fn set_sources(&self, ssrcs: Vec<u32>) -> Result<()> {
        if ssrcs.len() > 64 || ssrcs.contains(&0) {
            return Err(Error::Invalid("invalid remote sources".into()));
        }
        let mut info = self.telegram.lock().await;
        let (offer, response, sources) = info
            .as_mut()
            .ok_or_else(|| Error::Invalid("not a Telegram session".into()))?;
        if response.is_empty() {
            return Err(Error::Invalid(
                "connect before adding remote sources".into(),
            ));
        }
        if ssrcs.iter().all(|s| sources.contains(s)) {
            return Ok(());
        }
        for ssrc in ssrcs {
            if sources.contains(&ssrc) {
                continue;
            }
            if sources.len() == 64 {
                return Err(Error::Invalid(
                    "remote source budget exhausted; rejoin required".into(),
                ));
            }
            self.primary
                .pc
                .add_transceiver_from_kind(
                    RtpCodecKind::Audio,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        ..Default::default()
                    }),
                )
                .await?;
            sources.push(ssrc);
        }
        *offer = self.primary.local_sdp(true).await?;
        let answer = telegram::answer_sdp(offer, response, sources)?;
        self.primary
            .pc
            .set_remote_description(RTCSessionDescription::answer(answer)?)
            .await?;
        Ok(())
    }

    pub async fn send_pcm(&self, pcm: &[u8]) -> Result<u32> {
        let mut state = self.send_state.lock().await;
        let (packet, level) = self.encoder.lock().await.encode_with_level(pcm)?;
        let (sequence_number, timestamp) = *state;
        self.primary
            .output
            .write_rtp_with_extensions(
                Packet {
                    header: Header {
                        version: 2,
                        payload_type: 111,
                        sequence_number,
                        timestamp,
                        ssrc: self.primary.ssrc,
                        ..Default::default()
                    },
                    payload: Bytes::from(packet),
                },
                &[rtc::rtp::extension::HeaderExtension::AudioLevel(
                    rtc::rtp::extension::audio_level_extension::AudioLevelExtension {
                        level,
                        voice: level < 60,
                    },
                )],
            )
            .await?;
        *state = (
            sequence_number.wrapping_add(1),
            timestamp.wrapping_add(crate::FRAME_SAMPLES as u32),
        );
        self.counters.sent.fetch_add(1, Ordering::Relaxed);
        Ok(timestamp)
    }

    pub async fn send_video(&self, i420: &[u8]) -> Result<()> {
        let mut encoder = self.video_encoder.lock().await;
        let encoder = encoder
            .as_mut()
            .ok_or_else(|| Error::Invalid("video is not enabled".into()))?;
        let duration = Duration::from_secs_f64(1.0 / f64::from(encoder.config().fps));
        let data = encoder.encode(i420)?;
        self.primary
            .video_output
            .as_ref()
            .ok_or_else(|| Error::Invalid("video track missing".into()))?
            .write_sample(
                self.primary.ssrc.wrapping_add(1),
                100,
                &rtc::media::Sample {
                    data: Bytes::from(data),
                    duration,
                    timestamp: Instant::now(),
                    packet_timestamp: 0,
                    prev_dropped_packets: 0,
                    prev_padding_packets: 0,
                },
                &[],
            )
            .await?;
        self.counters.sent_video.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    pub async fn receive_video(&self, timeout: Duration) -> Result<VideoFrame> {
        tokio::time::timeout(timeout, self.video_incoming.lock().await.recv())
            .await
            .map_err(|_| Error::Timeout)?
            .ok_or(Error::Closed)?
    }

    pub(crate) fn rtc(&self) -> Arc<dyn PeerConnection> {
        self.primary.pc.clone()
    }
    pub(crate) async fn refresh_offer(&self) -> Result<String> {
        self.primary.local_sdp(true).await
    }
    pub async fn wait_connected(&self) -> Result<()> {
        self.primary.wait_connected().await
    }
    pub fn audio_ssrc(&self) -> u32 {
        self.primary.ssrc
    }

    pub async fn receive(&self, timeout: Duration) -> Result<AudioFrame> {
        tokio::time::timeout(timeout, self.incoming.lock().await.recv())
            .await
            .map_err(|_| Error::Timeout)?
            .ok_or(Error::Closed)?
    }

    pub fn stats(&self) -> SessionStats {
        SessionStats {
            sent_frames: self.counters.sent.load(Ordering::Relaxed),
            received_frames: self.counters.received.load(Ordering::Relaxed),
            concealed_frames: self.counters.concealed.load(Ordering::Relaxed),
            dropped_frames: self.counters.dropped.load(Ordering::Relaxed),
            connection_state: format!("{:?}", *self.primary.state.borrow()),
            sent_video_frames: self.counters.sent_video.load(Ordering::Relaxed),
            received_video_frames: self.counters.received_video.load(Ordering::Relaxed),
        }
    }

    pub async fn close(&self) -> Result<()> {
        self.primary.pc.close().await?;
        if let Some(echo) = &self.echo {
            echo.pc.close().await?;
        }
        Ok(())
    }
}
