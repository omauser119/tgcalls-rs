//! Synchronous host-driven transport, also compiled into the iOS WASI sandbox.
//! Only encrypted datagrams and an explicit clock cross the network capability boundary.
use crate::{
    AudioFrame, Error, Result, codec,
    group_crypto::{ConferenceFrame, FrameCrypto, MAX_AUDIO_FRAME, Pending, strip_audio_trailer},
    ice_io::{IceIo, datagram},
    negotiation::IceServer,
    video::{
        H264_FMTP, H264Assembler, VideoConfig, VideoDecoder, VideoEncoder, VideoFrame, resize_i420,
    },
};
use bytes::{Bytes, BytesMut};
use rtc::rtp_transceiver::RTCRtpSenderId;
use rtc::{
    media_stream::MediaStreamTrack,
    peer_connection::{
        RTCPeerConnection, RTCPeerConnectionBuilder,
        configuration::{
            interceptor_registry::{Registry, register_default_interceptors},
            media_engine::MediaEngine,
            setting_engine::SettingEngineBuilder,
        },
        message::RTCMessage,
        sdp::RTCSessionDescription,
        transport::RTCDtlsRole,
    },
    rtp::{Packet, header::Header},
    rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RTCRtpRtxParameters, RtpCodecKind,
    },
    sansio::Protocol,
    shared::TaggedBytesMut,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::SocketAddr,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
pub struct Datagram {
    pub peer: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoState {
    #[default]
    Inactive,
    Suspended,
    Active,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaState {
    pub muted: bool,
    pub low_battery: bool,
    pub video_state: VideoState,
    pub screencast_state: VideoState,
    pub video_rotation: u16,
}
struct AudioRx {
    decoder: codec::Decoder,
    jitter: crate::jitter::JitterBuffer<Packet>,
    next: Instant,
    sequence: u16,
    timestamp: u32,
}

struct VideoLayer {
    ssrc: u32,
    rtx: Option<u32>,
    sequence: u16,
    timestamp: u32,
    encoder: VideoEncoder,
}

pub struct Engine {
    pc: RTCPeerConnection,
    local: SocketAddr,
    audio_sender: RTCRtpSenderId,
    video_sender: Option<RTCRtpSenderId>,
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    audio_encoder: codec::Encoder,
    audio_decoders: BTreeMap<u32, AudioRx>,
    track_kinds: HashMap<rtc::media_stream::MediaStreamTrackId, RtpCodecKind>,
    video_layers: Vec<VideoLayer>,
    video_decoders: BTreeMap<u32, (H264Assembler, VideoDecoder)>,
    pub audio: VecDeque<AudioFrame>,
    pub video: VecDeque<VideoFrame>,
    pub connection_state: String,
    ice_io: IceIo,
    wire: VecDeque<TaggedBytesMut>,
    gathering_finished: bool,
    announce_reflexive: bool,
    trickle: VecDeque<String>,
    local_candidates: Vec<rtc::peer_connection::transport::RTCIceCandidateInit>,
    ice_servers: Vec<IceServer>,
    state_channel: Option<rtc::data_channel::RTCDataChannelId>,
    group_channel: Option<rtc::data_channel::RTCDataChannelId>,
    group_constraints: Option<String>,
    conference: Option<FrameCrypto>,
    pending_media_state: Option<MediaState>,
    pub remote_media_state: MediaState,
    local_media_state: MediaState,
}

impl Engine {
    pub fn personal_channel(&mut self) -> Result<()> {
        let channel = self.pc.create_data_channel(
            "data",
            Some(rtc::data_channel::RTCDataChannelInit {
                negotiated: Some(0),
                ..Default::default()
            }),
        )?;
        self.state_channel = Some(channel.id());
        self.pending_media_state = Some(self.local_media_state);
        Ok(())
    }

    pub fn set_media_state(&mut self, state: MediaState) -> Result<()> {
        if !matches!(state.video_rotation, 0 | 90 | 180 | 270) {
            return Err(Error::Invalid("invalid camera rotation".into()));
        }
        self.local_media_state = state;
        self.pending_media_state = Some(state);
        self.service_media_state(Instant::now())
    }

    fn service_media_state(&mut self, now: Instant) -> Result<()> {
        if let Some(id) = self.group_channel
            && let Some(constraints) = self.group_constraints.as_ref()
            && let Some(mut channel) = self.pc.data_channel(id)
            && channel.ready_state() == rtc::data_channel::RTCDataChannelState::Open
            && channel.outstanding_bytes() < 8192
        {
            channel.send_text(now, constraints.clone())?;
            self.group_constraints = None;
        }
        if let Some(id) = self.state_channel
            && let Some(state) = self.pending_media_state
            && let Some(mut channel) = self.pc.data_channel(id)
            && channel.ready_state() == rtc::data_channel::RTCDataChannelState::Open
            && channel.outstanding_bytes() < 4096
        {
            let mut value =
                serde_json::to_value(state).map_err(|e| Error::Invalid(e.to_string()))?;
            value["@type"] = serde_json::json!("MediaState");
            channel.send_text(now, value.to_string())?;
            self.pending_media_state = None;
        }
        while let Some(message) = self.pc.poll_data_read() {
            if let RTCMessage::DataChannelMessage(id, data) = message.message
                && self.state_channel == Some(id)
                && data.data.len() <= 1024
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data.data)
                && value["@type"] == "MediaState"
                && let Ok(state) = serde_json::from_value::<MediaState>(value)
                && matches!(state.video_rotation, 0 | 90 | 180 | 270)
            {
                self.remote_media_state = state;
            }
        }
        Ok(())
    }
    pub fn one_way(&mut self) -> Result<()> {
        let ids: Vec<_> = self.pc.get_transceivers().collect();
        for id in ids {
            self.pc
                .rtp_transceiver(id)
                .ok_or(Error::Closed)?
                .set_direction(rtc::rtp_transceiver::RTCRtpTransceiverDirection::Sendonly);
        }
        Ok(())
    }
    pub fn add_receiver(&mut self, video: bool) -> Result<()> {
        self.pc.add_transceiver_from_kind(
            if video {
                RtpCodecKind::Video
            } else {
                RtpCodecKind::Audio
            },
            Some(rtc::rtp_transceiver::RTCRtpTransceiverInit {
                direction: rtc::rtp_transceiver::RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )?;
        Ok(())
    }
    pub fn add_candidate(&mut self, value: String) -> Result<()> {
        if value.len() > 2048 || value.contains(['\r', '\n', '\0']) {
            return Err(Error::Invalid("invalid ICE candidate".into()));
        }
        self.pc
            .add_remote_candidate(rtc::peer_connection::transport::RTCIceCandidateInit {
                candidate: value,
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
                ..Default::default()
            })?;
        Ok(())
    }
    pub fn new(
        local: SocketAddr,
        ssrc: u32,
        video: Option<VideoConfig>,
        now: Instant,
    ) -> Result<Self> {
        Self::with_ice(local, ssrc, video, Vec::new(), false, now)
    }

    pub fn with_ice(
        local: SocketAddr,
        ssrc: u32,
        video: Option<VideoConfig>,
        servers: Vec<IceServer>,
        relay_only: bool,
        now: Instant,
    ) -> Result<Self> {
        Self::build(
            local,
            ssrc,
            video.into_iter().collect(),
            servers,
            relay_only,
            false,
            now,
        )
    }

    pub fn group(
        local: SocketAddr,
        ssrc: u32,
        video: Vec<VideoConfig>,
        now: Instant,
    ) -> Result<Self> {
        let mut engine = Self::build(local, ssrc, video, Vec::new(), false, true, now)?;
        engine.one_way()?;
        engine.group_channel = Some(engine.pc.create_data_channel("data", None)?.id());
        Ok(engine)
    }

    pub fn video_sources(&self) -> Vec<(u32, Option<u32>)> {
        self.video_layers
            .iter()
            .map(|layer| (layer.ssrc, layer.rtx))
            .collect()
    }

    pub fn set_video_constraints(&mut self, constraints: String) {
        self.group_constraints = Some(constraints);
    }

    pub(crate) fn require_conference_crypto(&mut self) {
        self.conference = Some(FrameCrypto::default());
    }

    pub(crate) fn conference_frames(&mut self) -> Vec<ConferenceFrame> {
        self.conference
            .as_mut()
            .map_or_else(Vec::new, |crypto| crypto.drain().collect())
    }

    fn build(
        local: SocketAddr,
        ssrc: u32,
        video: Vec<VideoConfig>,
        servers: Vec<IceServer>,
        relay_only: bool,
        group: bool,
        now: Instant,
    ) -> Result<Self> {
        if ssrc == 0 || ssrc >= u32::MAX - 6 || local.port() == 0 {
            return Err(Error::Invalid("invalid local transport or SSRC".into()));
        }
        if video.len() > 3
            || video.windows(2).any(|pair| {
                let (low, high) = (&pair[0], &pair[1]);
                low.width >= high.width
                    || low.height >= high.height
                    || low.fps != high.fps
                    || low.bitrate >= high.bitrate
            })
        {
            return Err(Error::Invalid(
                "video layers must increase in size and bitrate, with a common frame rate".into(),
            ));
        }
        let video_layers = video
            .into_iter()
            .enumerate()
            .map(|(index, config)| {
                let ssrc = ssrc + 2 * index as u32 + 1;
                Ok(VideoLayer {
                    ssrc,
                    rtx: group.then_some(ssrc + 1),
                    sequence: 0,
                    timestamp: 0,
                    encoder: VideoEncoder::new(config)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut media = MediaEngine::default();
        let opus = RTCRtpCodec {
            mime_type: "audio/opus".into(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
            rtcp_feedback: vec![],
        };
        media.register_codec(
            RTCRtpCodecParameters {
                rtp_codec: opus.clone(),
                payload_type: 111,
            },
            RtpCodecKind::Audio,
        )?;
        let h264 = RTCRtpCodec {
            mime_type: "video/H264".into(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: H264_FMTP.into(),
            rtcp_feedback: vec![],
        };
        if !video_layers.is_empty() {
            media.register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: h264.clone(),
                    payload_type: 100,
                },
                RtpCodecKind::Video,
            )?;
        }
        let interceptors = if group && !video_layers.is_empty() {
            media.register_codec(
                RTCRtpCodecParameters {
                    payload_type: 101,
                    rtp_codec: RTCRtpCodec {
                        mime_type: "video/rtx".into(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line: "apt=100".into(),
                        rtcp_feedback: vec![],
                    },
                },
                RtpCodecKind::Video,
            )?;
            register_default_interceptors(Registry::new(), &mut media)?
        } else {
            Registry::new()
        };
        let mut settings = SettingEngineBuilder::default()
            .with_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled)
            .with_discard_local_candidates_during_ice_restart(true);
        if group {
            settings = settings.with_answering_dtls_role(RTCDtlsRole::Server);
        }
        let mut pc = RTCPeerConnectionBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(interceptors)
            .with_setting_engine(settings.build())
            .build(now)?;
        let audio_sender = pc.add_track(make_track(ssrc, RtpCodecKind::Audio, opus))?;
        let video_sender = if !video_layers.is_empty() {
            let encodings = video_layers
                .iter()
                .enumerate()
                .map(|(index, layer)| RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(layer.ssrc),
                        rid: if video_layers.len() > 1 {
                            format!("v{index}")
                        } else {
                            String::new()
                        },
                        rtx: layer.rtx.map(|ssrc| RTCRtpRtxParameters { ssrc }),
                        ..Default::default()
                    },
                    codec: h264.clone(),
                    ..Default::default()
                })
                .collect();
            Some(pc.add_track(MediaStreamTrack::new(
                "tgcalls-rs".into(),
                format!("track-{}", ssrc + 1),
                "media".into(),
                RtpCodecKind::Video,
                encodings,
            ))?)
        } else {
            None
        };
        if relay_only
            && !servers.iter().any(|server| {
                server.turn
                    && server
                        .address()
                        .is_ok_and(|addr| addr.is_ipv4() == local.is_ipv4())
            })
        {
            return Err(Error::Invalid(
                "relay-only ICE requires a TURN server for the socket family".into(),
            ));
        }
        let local_candidates: Vec<_> = (!relay_only)
            .then(|| host_candidate(local))
            .into_iter()
            .collect();
        for candidate in &local_candidates {
            pc.add_local_candidate(candidate.clone())?;
        }
        let mut ice_io = IceIo::new(local, servers.clone(), now)?;
        let wire = ice_io.poll(local, now)?.into();
        let gathering_finished = ice_io.gathering_complete();
        if gathering_finished {
            pc.add_local_candidate(Default::default())?;
        }
        Ok(Self {
            pc,
            local,
            audio_sender,
            video_sender,
            ssrc,
            sequence: 0,
            timestamp: 0,
            audio_encoder: codec::Encoder::new()?,
            audio_decoders: BTreeMap::new(),
            track_kinds: HashMap::new(),
            video_layers,
            video_decoders: BTreeMap::new(),
            audio: VecDeque::new(),
            video: VecDeque::new(),
            connection_state: "New".into(),
            ice_io,
            wire,
            gathering_finished,
            announce_reflexive: !relay_only,
            trickle: VecDeque::new(),
            local_candidates,
            ice_servers: servers,
            state_channel: None,
            group_channel: None,
            group_constraints: None,
            conference: None,
            pending_media_state: None,
            remote_media_state: MediaState::default(),
            local_media_state: MediaState::default(),
        })
    }
    pub fn offer_template(&mut self) -> Result<String> {
        Ok(self.pc.create_offer(None)?.sdp)
    }

    pub fn offer(&mut self) -> Result<String> {
        let offer = self.pc.create_offer(None)?;
        self.pc.set_local_description(Instant::now(), offer)?;
        self.pc
            .local_description()
            .map(|s| s.sdp.clone())
            .ok_or(Error::Closed)
    }
    pub fn restart_offer(&mut self, local: SocketAddr, now: Instant) -> Result<String> {
        if local.ip().is_unspecified() || local.port() == 0 {
            return Err(Error::Invalid("invalid socket for ICE restart".into()));
        }
        if local != self.local {
            let mut io = IceIo::new(local, self.ice_servers.clone(), now)?;
            self.wire = io.poll(local, now)?.into();
            self.gathering_finished = io.gathering_complete();
            self.ice_io = io;
            self.local = local;
            self.local_candidates = self
                .announce_reflexive
                .then(|| host_candidate(local))
                .into_iter()
                .collect();
        }
        self.trickle.clear();
        self.pc.restart_ice();
        // The staged restart commits when the offer is installed, discarding
        // the old socket's candidates. Publish only the new generation below.
        self.offer()?;
        for candidate in &self.local_candidates {
            self.pc.add_local_candidate(candidate.clone())?;
            self.trickle.push_back(candidate.candidate.clone());
        }
        if self.gathering_finished {
            self.pc.add_local_candidate(Default::default())?;
        }
        self.pc
            .local_description()
            .map(|s| s.sdp.clone())
            .ok_or(Error::Closed)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }
    pub fn answer(&mut self, offer: String) -> Result<String> {
        crate::packet::validate_sdp(&offer)?;
        self.pc
            .set_remote_description(Instant::now(), RTCSessionDescription::offer(offer)?)?;
        let answer = self.pc.create_answer(None)?;
        self.pc.set_local_description(Instant::now(), answer)?;
        self.pc
            .local_description()
            .map(|s| s.sdp.clone())
            .ok_or(Error::Closed)
    }
    pub fn accept_answer(&mut self, answer: String) -> Result<()> {
        crate::packet::validate_sdp(&answer)?;
        Ok(self
            .pc
            .set_remote_description(Instant::now(), RTCSessionDescription::answer(answer)?)?)
    }
    pub fn tick(&mut self, now: Instant) -> Result<()> {
        self.pc.handle_timeout(now)?;
        self.service_ice(now)?;
        self.drain_media()
    }
    pub fn receive_datagram(&mut self, peer: SocketAddr, bytes: &[u8], now: Instant) -> Result<()> {
        if bytes.len() > 65535 {
            return Err(Error::Invalid("datagram too large".into()));
        }
        let input = datagram(self.local, peer, BytesMut::from(bytes), now);
        let (direct, candidate) = self.ice_io.receive(input, self.announce_reflexive)?;
        if let Some(input) = direct {
            self.pc.handle_read(input)?;
        }
        if let Some(candidate) = candidate {
            self.add_gathered(candidate)?;
        }
        self.service_ice(now)?;
        self.drain_media()
    }
    fn add_gathered(
        &mut self,
        candidate: rtc::peer_connection::transport::RTCIceCandidateInit,
    ) -> Result<()> {
        if !self
            .local_candidates
            .iter()
            .any(|current| current.candidate == candidate.candidate)
        {
            self.pc.add_local_candidate(candidate.clone())?;
            self.trickle.push_back(candidate.candidate.clone());
            self.local_candidates.push(candidate);
        }
        Ok(())
    }

    fn service_ice(&mut self, now: Instant) -> Result<()> {
        let (candidates, incoming) = self.ice_io.turn_events(self.local, now)?;
        for candidate in candidates {
            self.add_gathered(candidate)?;
        }
        for packet in incoming {
            self.pc.handle_read(packet)?;
        }
        if !self.gathering_finished && self.ice_io.gathering_complete() {
            self.pc.add_local_candidate(Default::default())?;
            self.gathering_finished = true;
        }
        Ok(())
    }

    pub fn gathered_candidates(&mut self) -> impl Iterator<Item = String> + '_ {
        self.trickle.drain(..)
    }

    pub fn outgoing(&mut self) -> Result<Vec<Datagram>> {
        for _ in 0..128 {
            let Some(packet) = self.pc.poll_write() else {
                break;
            };
            if let Some(packet) = self.ice_io.send(packet)? {
                self.wire.push_back(packet);
            }
        }
        self.wire
            .extend(self.ice_io.poll(self.local, Instant::now())?);
        let mut out = Vec::new();
        while let Some(packet) = self.wire.pop_front() {
            out.push(Datagram {
                peer: packet.transport.peer_addr.to_string(),
                bytes: packet.message.to_vec(),
            });
            if out.len() >= 128 {
                break;
            }
        }
        if self.wire.len() > 256 {
            self.wire.truncate(256);
        }
        Ok(out)
    }
    pub fn send_audio(&mut self, pcm: &[u8], now: Instant) -> Result<u32> {
        if self.local_media_state.muted {
            if pcm.len() != crate::PCM_BYTES {
                return Err(Error::Invalid("invalid PCM length".into()));
            }
            let timestamp = self.timestamp;
            self.timestamp = self.timestamp.wrapping_add(960);
            return Ok(timestamp);
        }
        let timestamp = self.timestamp;
        if let Some(crypto) = self.conference.as_mut() {
            let (mut payload, level) = self.audio_encoder.encode_with_level(pcm)?;
            payload.extend_from_slice(&[1, (level & 0x7f) | if level < 60 { 0x80 } else { 0 }]);
            crypto.stage(self.ssrc, Pending::SendAudio { timestamp }, payload, now)?;
        } else {
            let payload = self.audio_encoder.encode(pcm)?;
            self.send_audio_payload(timestamp, Bytes::from(payload), now)?;
        }
        self.timestamp = self.timestamp.wrapping_add(960);
        Ok(timestamp)
    }

    fn send_audio_payload(&mut self, timestamp: u32, payload: Bytes, now: Instant) -> Result<()> {
        self.pc
            .rtp_sender(self.audio_sender)
            .ok_or(Error::Closed)?
            .write_rtp(
                now,
                Packet {
                    header: Header {
                        version: 2,
                        payload_type: 111,
                        ssrc: self.ssrc,
                        sequence_number: self.sequence,
                        timestamp,
                        ..Default::default()
                    },
                    payload,
                },
            )?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }
    pub fn send_video(&mut self, i420: &[u8], now: Instant) -> Result<()> {
        let input = self
            .video_layers
            .last()
            .ok_or_else(|| Error::Invalid("video disabled".into()))?
            .encoder
            .config()
            .clone();
        if i420.len() != input.bytes() {
            return Err(Error::Invalid("invalid video input size".into()));
        }
        let sender = self.video_sender.ok_or(Error::Closed)?;
        let mut sender = self.pc.rtp_sender(sender).ok_or(Error::Closed)?;
        for layer in &mut self.video_layers {
            let resized;
            let layer_input = if layer.encoder.config() == &input {
                i420
            } else {
                resized = resize_i420(i420, &input, layer.encoder.config());
                &resized
            };
            let access_unit = layer.encoder.encode(layer_input)?;
            if let Some(crypto) = self.conference.as_mut() {
                let (normalized, clear_bytes) = crate::video::conference_h264(&access_unit)?;
                crypto.stage(
                    layer.ssrc,
                    Pending::SendH264 {
                        ssrc: layer.ssrc,
                        timestamp: layer.timestamp,
                        clear_bytes,
                    },
                    normalized,
                    now,
                )?;
            } else {
                write_h264(
                    &mut sender,
                    layer,
                    layer.timestamp,
                    Bytes::from(access_unit),
                    now,
                )?;
            }
            layer.timestamp = layer.timestamp.wrapping_add(90_000 / input.fps);
        }
        Ok(())
    }

    pub(crate) fn apply_conference_frame(
        &mut self,
        id: u32,
        bytes: &[u8],
        now: Instant,
    ) -> Result<()> {
        let frame = self
            .conference
            .as_mut()
            .ok_or_else(|| Error::Invalid("conference mode required".into()))?
            .take(id, now)?;
        if bytes.is_empty() {
            return Ok(());
        }
        match frame {
            Pending::SendAudio { timestamp } if (128..=MAX_AUDIO_FRAME).contains(&bytes.len()) => {
                self.send_audio_payload(timestamp, Bytes::copy_from_slice(bytes), now)
            }
            Pending::ReceiveAudio {
                arrival,
                mut packet,
            } => {
                packet.payload = Bytes::copy_from_slice(strip_audio_trailer(bytes)?);
                self.queue_audio(packet, arrival)
            }
            Pending::SendH264 {
                ssrc,
                timestamp,
                clear_bytes,
            } if bytes.len() >= clear_bytes + 128
                && bytes.len() <= crate::video::MAX_ACCESS_UNIT
                && !bytes[clear_bytes.saturating_sub(2)..]
                    .windows(3)
                    .any(|window| window == [0, 0, 1]) =>
            {
                let mut sender = self
                    .pc
                    .rtp_sender(self.video_sender.ok_or(Error::Closed)?)
                    .ok_or(Error::Closed)?;
                let layer = self
                    .video_layers
                    .iter_mut()
                    .find(|layer| layer.ssrc == ssrc)
                    .ok_or(Error::Closed)?;
                write_h264(
                    &mut sender,
                    layer,
                    timestamp,
                    Bytes::copy_from_slice(bytes),
                    now,
                )
            }
            Pending::ReceiveH264 { ssrc, timestamp } => self.decode_video(ssrc, timestamp, bytes),
            _ => Err(Error::Invalid("invalid conference transform result".into())),
        }
    }

    fn queue_audio(&mut self, packet: Packet, now: Instant) -> Result<()> {
        let ssrc = packet.header.ssrc;
        let has_capacity = self.audio_decoders.len() < 64;
        let decoder = match self.audio_decoders.entry(ssrc) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) if has_capacity => {
                entry.insert(AudioRx {
                    decoder: codec::Decoder::new()?,
                    jitter: Default::default(),
                    next: now,
                    sequence: packet.header.sequence_number,
                    timestamp: packet.header.timestamp,
                })
            }
            _ => return Err(Error::Invalid("audio stream budget".into())),
        };
        decoder.jitter.push(
            packet.header.sequence_number,
            packet.header.timestamp,
            now,
            packet,
        );
        Ok(())
    }

    fn decode_video(&mut self, ssrc: u32, timestamp: u32, data: &[u8]) -> Result<()> {
        let (_, decoder) = self.video_decoders.get_mut(&ssrc).ok_or(Error::Closed)?;
        if let Some(frame) = decoder.decode(ssrc, timestamp, data)?
            && self.video.len() < 4
        {
            self.video.push_back(frame);
        }
        Ok(())
    }
    fn drain_media(&mut self) -> Result<()> {
        while let Some(event) = self.pc.poll_event() {
            use rtc::peer_connection::event::{RTCPeerConnectionEvent, RTCTrackEvent};
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                    self.connection_state = state.to_string()
                }
                RTCPeerConnectionEvent::OnTrack(RTCTrackEvent::OnOpen(init)) => {
                    if self.track_kinds.len() >= 68 {
                        return Err(Error::Invalid("track budget".into()));
                    }
                    if let Some(receiver) = self.pc.rtp_receiver(init.receiver_id) {
                        self.track_kinds
                            .insert(init.track_id, receiver.track().kind());
                    }
                }
                RTCPeerConnectionEvent::OnTrack(RTCTrackEvent::OnClose(id)) => {
                    self.track_kinds.remove(&id);
                }
                _ => {}
            }
        }
        self.service_media_state(Instant::now())?;
        while let Some(message) = self.pc.poll_media_read() {
            let RTCMessage::RtpPacket(track, packet) = message.message else {
                continue;
            };
            let ssrc = packet.header.ssrc;
            if self.track_kinds.get(&track) == Some(&RtpCodecKind::Audio) {
                if let Some(crypto) = self.conference.as_mut() {
                    let mut packet = packet;
                    // A waiting entry holds metadata only, not a second copy of the ciphertext.
                    let bytes = std::mem::take(&mut packet.payload).to_vec();
                    crypto.stage(
                        ssrc,
                        Pending::ReceiveAudio {
                            arrival: message.now,
                            packet,
                        },
                        bytes,
                        message.now,
                    )?;
                } else {
                    self.queue_audio(packet, message.now)?;
                }
            } else if self.track_kinds.get(&track) == Some(&RtpCodecKind::Video) {
                if !self.video_decoders.contains_key(&ssrc) {
                    if self.video_decoders.len() >= 4 {
                        return Err(Error::Invalid("video stream budget".into()));
                    }
                    self.video_decoders
                        .insert(ssrc, (H264Assembler::default(), VideoDecoder::default()));
                }
                let (assembler, _) = self.video_decoders.get_mut(&ssrc).ok_or(Error::Closed)?;
                if let Some(au) = assembler.push(
                    packet.header.sequence_number,
                    packet.header.timestamp,
                    packet.header.marker,
                    &packet.payload,
                    message.now,
                )? {
                    let timestamp = packet.header.timestamp;
                    if let Some(crypto) = self.conference.as_mut() {
                        crypto.stage(
                            ssrc,
                            Pending::ReceiveH264 { ssrc, timestamp },
                            au,
                            message.now,
                        )?;
                    } else {
                        self.decode_video(ssrc, timestamp, &au)?;
                    }
                }
            }
        }
        let now = Instant::now();
        for (&ssrc, rx) in &mut self.audio_decoders {
            if now < rx.next {
                continue;
            }
            if let Some(packet) = rx.jitter.pop(now) {
                let concealed = packet.is_none();
                let (sequence, timestamp, payload) = if let Some(packet) = packet {
                    (
                        packet.header.sequence_number,
                        packet.header.timestamp,
                        packet.payload,
                    )
                } else {
                    (rx.sequence, rx.timestamp, Bytes::new())
                };
                let pcm = rx.decoder.decode(&payload)?;
                let samples = pcm.len() / 4;
                rx.jitter.set_frame_samples(samples);
                rx.next =
                    now + Duration::from_micros((samples * 1_000_000 / crate::SAMPLE_RATE) as u64);
                rx.sequence = sequence.wrapping_add(1);
                rx.timestamp = timestamp.wrapping_add(samples as u32);
                if self.audio.len() < 128 {
                    self.audio.push_back(AudioFrame {
                        ssrc,
                        sequence,
                        timestamp,
                        concealed,
                        target_delay_ms: rx.jitter.target_delay_ms(),
                        pcm,
                    });
                }
            }
        }
        Ok(())
    }
}

fn write_h264(
    sender: &mut rtc::rtp_transceiver::rtp_sender::RTCRtpSender<'_>,
    layer: &mut VideoLayer,
    timestamp: u32,
    access_unit: Bytes,
    now: Instant,
) -> Result<()> {
    use rtc::rtp::packetizer::Payloader;
    let payload_type = sender
        .get_parameters()
        .rtp_parameters
        .codecs
        .iter()
        .find(|codec| codec.rtp_codec.mime_type.eq_ignore_ascii_case("video/h264"))
        .ok_or_else(|| Error::Invalid("H264 was not negotiated".into()))?
        .payload_type;
    let packets = rtc::rtp::codec::h264::H264Payloader::default().payload(1180, &access_unit)?;
    let count = packets.len();
    for (index, payload) in packets.into_iter().enumerate() {
        sender.write_rtp(
            now,
            Packet {
                header: Header {
                    version: 2,
                    payload_type,
                    ssrc: layer.ssrc,
                    sequence_number: layer.sequence,
                    timestamp,
                    marker: index + 1 == count,
                    ..Default::default()
                },
                payload,
            },
        )?;
        layer.sequence = layer.sequence.wrapping_add(1);
    }
    Ok(())
}

fn host_candidate(local: SocketAddr) -> rtc::peer_connection::transport::RTCIceCandidateInit {
    rtc::peer_connection::transport::RTCIceCandidateInit {
        candidate: format!(
            "candidate:1 1 udp 2130706431 {} {} typ host",
            local.ip(),
            local.port()
        ),
        ..Default::default()
    }
}
fn make_track(ssrc: u32, kind: RtpCodecKind, codec: RTCRtpCodec) -> MediaStreamTrack {
    MediaStreamTrack::new(
        "tgcalls-rs".into(),
        format!("track-{ssrc}"),
        "media".into(),
        kind,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec,
            ..Default::default()
        }],
    )
}
