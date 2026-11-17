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
        if self
            .counters
            .active_tracks
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n < 64 { Some(n + 1) } else { None }
            })
            .is_err()
        {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let budget = TrackBudget(self.counters.clone());
        let frames = self.frames.clone();
        let counters = self.counters.clone();
        let echo = self.echo.clone();
        let impairment = self.impairment.clone();
        // Handler must return immediately: the connection driver delivers media itself.
        tokio::spawn(async move {
            let _budget = budget;
            use futures::FutureExt;
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

async fn receive_track(
    track: Arc<dyn TrackRemote>,
    echo: Option<Arc<TrackLocalStaticRTP>>,
    frames: mpsc::Sender<Result<AudioFrame>>,
    counters: Arc<Counters>,
    impairment: Impairment,
) -> Result<()> {
    let mut decoder = codec::Decoder::new()?;
    let mut jitter = JitterBuffer::<Packet>::default();
    let mut timer = tokio::time::interval(Duration::from_millis(20));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_playout = Instant::now();
    let mut last: Option<(u32, u16, u32, u32)> = None;
    let mut delayed: Vec<(Instant, Packet)> = Vec::new();
    let mut rng = impairment.seed.max(1);
    loop {
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
            _ = timer.tick() => {
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
                if now < next_playout { continue; }
                if let Some(packet) = jitter.pop(now) {
                    let (ssrc, sequence, timestamp, concealed, pcm) = if let Some(packet) = packet {
                        let pcm = decoder.decode(&packet.payload)?;
                        (packet.header.ssrc, packet.header.sequence_number, packet.header.timestamp, false, pcm)
                    } else if let Some((ssrc, seq, ts, samples)) = last {
                        (ssrc, seq.wrapping_add(1), ts.wrapping_add(samples), true, decoder.decode(&[])?)
                    } else { continue; };
                    let duration = Duration::from_secs_f64(pcm.len() as f64 / (crate::SAMPLE_RATE * crate::CHANNELS * 2) as f64);
                    next_playout = now + duration.saturating_sub(Duration::from_millis(2));
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
}

impl Peer {
    async fn new(
        bind_ip: &str,
        socket: Option<std::net::UdpSocket>,
        frames: mpsc::Sender<Result<AudioFrame>>,
        counters: Arc<Counters>,
        echo: bool,
        impairment: Impairment,
        ssrc: u32,
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
        let handler = Arc::new(Handler {
            state: state_tx,
            gathered: gather_tx,
            frames,
            echo: if echo { Some(output.clone()) } else { None },
            counters,
            impairment,
        });
        let mut builder = PeerConnectionBuilder::new()
            .with_media_engine(media)
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
        pc.add_track(output.clone()).await?;
        Ok(Self {
            pc,
            output,
            state,
            gathered,
            ssrc,
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
}

impl Session {
    pub async fn loopback(
        impairment: Impairment,
        bind_ip: &str,
        sockets: Option<Vec<std::net::UdpSocket>>,
    ) -> Result<Self> {
        impairment.validate()?;
        let (tx, rx) = mpsc::channel(128);
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
        })
    }

    pub async fn group(
        bind_ip: &str,
        sockets: Option<Vec<std::net::UdpSocket>>,
        ssrc: u32,
    ) -> Result<Self> {
        if ssrc == 0 {
            return Err(Error::Invalid("SSRC must be nonzero".into()));
        }
        let (tx, rx) = mpsc::channel(128);
        let counters = Arc::new(Counters::default());
        let primary = Peer::new(
            bind_ip,
            sockets.and_then(|s| s.into_iter().next()),
            tx,
            counters.clone(),
            false,
            Impairment::default(),
            ssrc,
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
        let packet = self.encoder.lock().await.encode(pcm)?;
        let (sequence_number, timestamp) = *state;
        let rms = (pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| {
                let x = f64::from(i16::from_le_bytes(*s)) / 32768.0;
                x * x
            })
            .sum::<f64>()
            / (pcm.len() / 2) as f64)
            .sqrt();
        let level = if rms <= 0.0 {
            127
        } else {
            (-20.0 * rms.log10()).clamp(0.0, 127.0) as u8
        };
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
