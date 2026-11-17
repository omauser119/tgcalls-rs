//! Synchronous host-driven transport, also compiled into the iOS WASI sandbox.
//! Only encrypted datagrams and an explicit clock cross the network capability boundary.
use crate::{
    AudioFrame, Error, Result, codec,
    video::{H264_FMTP, H264Assembler, VideoConfig, VideoDecoder, VideoEncoder, VideoFrame},
};
use bytes::{Bytes, BytesMut};
use rtc::rtp_transceiver::RTCRtpSenderId;
use rtc::{
    media_stream::MediaStreamTrack,
    peer_connection::{
        RTCPeerConnection, RTCPeerConnectionBuilder,
        configuration::{media_engine::MediaEngine, setting_engine::SettingEngineBuilder},
        message::RTCMessage,
        sdp::RTCSessionDescription,
    },
    rtp::{Packet, header::Header},
    rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RtpCodecKind,
    },
    sansio::Protocol,
    shared::{TaggedBytesMut, TransportContext, TransportProtocol},
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
struct AudioRx {
    decoder: codec::Decoder,
    jitter: crate::jitter::JitterBuffer<Packet>,
    next: Instant,
    sequence: u16,
    timestamp: u32,
}

pub struct Engine {
    pc: RTCPeerConnection,
    local: SocketAddr,
    audio_sender: RTCRtpSenderId,
    video_sender: Option<RTCRtpSenderId>,
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    video_sequence: u16,
    video_timestamp: u32,
    audio_encoder: codec::Encoder,
    audio_decoders: BTreeMap<u32, AudioRx>,
    track_kinds: HashMap<rtc::media_stream::MediaStreamTrackId, RtpCodecKind>,
    video_encoder: Option<VideoEncoder>,
    video_decoders: BTreeMap<u32, (H264Assembler, VideoDecoder)>,
    pub audio: VecDeque<AudioFrame>,
    pub video: VecDeque<VideoFrame>,
    pub connection_state: String,
}

impl Engine {
    pub fn personal_channel(&mut self) -> Result<()> {
        self.pc.create_data_channel(
            "data",
            Some(rtc::data_channel::RTCDataChannelInit {
                negotiated: Some(0),
                ..Default::default()
            }),
        )?;
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
        if ssrc == 0 || ssrc >= u32::MAX - 1 || local.port() == 0 {
            return Err(Error::Invalid("invalid local transport or SSRC".into()));
        }
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
        if video.is_some() {
            media.register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: h264.clone(),
                    payload_type: 100,
                },
                RtpCodecKind::Video,
            )?;
        }
        let mut pc = RTCPeerConnectionBuilder::new()
            .with_media_engine(media)
            .with_setting_engine(
                SettingEngineBuilder::default()
                    .with_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled)
                    .build(),
            )
            .build(now)?;
        let audio_sender = pc.add_track(make_track(ssrc, RtpCodecKind::Audio, opus))?;
        let video_sender = if video.is_some() {
            Some(pc.add_track(make_track(ssrc.wrapping_add(1), RtpCodecKind::Video, h264))?)
        } else {
            None
        };
        let candidate = format!(
            "candidate:1 1 udp 2130706431 {} {} typ host",
            local.ip(),
            local.port()
        );
        pc.add_local_candidate(rtc::peer_connection::transport::RTCIceCandidateInit {
            candidate,
            ..Default::default()
        })?;
        pc.add_local_candidate(rtc::peer_connection::transport::RTCIceCandidateInit::default())?;
        Ok(Self {
            pc,
            local,
            audio_sender,
            video_sender,
            ssrc,
            sequence: 0,
            timestamp: 0,
            video_sequence: 0,
            video_timestamp: 0,
            audio_encoder: codec::Encoder::new()?,
            audio_decoders: BTreeMap::new(),
            track_kinds: HashMap::new(),
            video_encoder: video.map(VideoEncoder::new).transpose()?,
            video_decoders: BTreeMap::new(),
            audio: VecDeque::new(),
            video: VecDeque::new(),
            connection_state: "New".into(),
        })
    }
    pub fn offer(&mut self) -> Result<String> {
        let offer = self.pc.create_offer(None)?;
        self.pc.set_local_description(Instant::now(), offer)?;
        self.pc
            .local_description()
            .map(|s| s.sdp.clone())
            .ok_or(Error::Closed)
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
        self.drain_media()
    }
    pub fn receive_datagram(&mut self, peer: SocketAddr, bytes: &[u8], now: Instant) -> Result<()> {
        if bytes.len() > 65535 {
            return Err(Error::Invalid("datagram too large".into()));
        }
        self.pc.handle_read(TaggedBytesMut {
            now,
            transport: TransportContext {
                local_addr: self.local,
                peer_addr: peer,
                transport_protocol: TransportProtocol::UDP,
                ecn: None,
            },
            message: BytesMut::from(bytes),
        })?;
        self.drain_media()
    }
    pub fn outgoing(&mut self) -> Vec<Datagram> {
        let mut out = vec![];
        while let Some(packet) = self.pc.poll_write() {
            out.push(Datagram {
                peer: packet.transport.peer_addr.to_string(),
                bytes: packet.message.to_vec(),
            });
            if out.len() >= 128 {
                break;
            }
        }
        out
    }
    pub fn send_audio(&mut self, pcm: &[u8], now: Instant) -> Result<u32> {
        let payload = self.audio_encoder.encode(pcm)?;
        let timestamp = self.timestamp;
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
                    payload: Bytes::from(payload),
                },
            )?;
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(960);
        Ok(timestamp)
    }
    pub fn send_video(&mut self, i420: &[u8], now: Instant) -> Result<()> {
        use rtc::rtp::packetizer::Payloader;
        let encoder = self
            .video_encoder
            .as_mut()
            .ok_or_else(|| Error::Invalid("video disabled".into()))?;
        let au = encoder.encode(i420)?;
        let fps = encoder.config().fps;
        let mut payloader = rtc::rtp::codec::h264::H264Payloader::default();
        let packets = payloader.payload(1180, &Bytes::from(au))?;
        let count = packets.len();
        let sender = self.video_sender.ok_or(Error::Closed)?;
        for (index, payload) in packets.into_iter().enumerate() {
            self.pc.rtp_sender(sender).ok_or(Error::Closed)?.write_rtp(
                now,
                Packet {
                    header: Header {
                        version: 2,
                        payload_type: 100,
                        ssrc: self.ssrc.wrapping_add(1),
                        sequence_number: self.video_sequence,
                        timestamp: self.video_timestamp,
                        marker: index + 1 == count,
                        ..Default::default()
                    },
                    payload,
                },
            )?;
            self.video_sequence = self.video_sequence.wrapping_add(1);
        }
        self.video_timestamp = self.video_timestamp.wrapping_add(90000 / fps);
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
        while let Some(message) = self.pc.poll_media_read() {
            let RTCMessage::RtpPacket(track, packet) = message.message else {
                continue;
            };
            let ssrc = packet.header.ssrc;
            if self.track_kinds.get(&track) == Some(&RtpCodecKind::Audio) {
                if !self.audio_decoders.contains_key(&ssrc) {
                    if self.audio_decoders.len() >= 64 {
                        return Err(Error::Invalid("audio stream budget".into()));
                    }
                    self.audio_decoders.insert(
                        ssrc,
                        AudioRx {
                            decoder: codec::Decoder::new()?,
                            jitter: Default::default(),
                            next: message.now,
                            sequence: packet.header.sequence_number,
                            timestamp: packet.header.timestamp,
                        },
                    );
                }
                self.audio_decoders
                    .get_mut(&ssrc)
                    .ok_or(Error::Closed)?
                    .jitter
                    .push(
                        packet.header.sequence_number,
                        packet.header.timestamp,
                        message.now,
                        packet,
                    );
            } else if self.track_kinds.get(&track) == Some(&RtpCodecKind::Video) {
                if !self.video_decoders.contains_key(&ssrc) {
                    if self.video_decoders.len() >= 4 {
                        return Err(Error::Invalid("video stream budget".into()));
                    }
                    self.video_decoders
                        .insert(ssrc, (H264Assembler::default(), VideoDecoder::default()));
                }
                let (assembler, decoder) =
                    self.video_decoders.get_mut(&ssrc).ok_or(Error::Closed)?;
                if let Some(au) = assembler.push(
                    packet.header.sequence_number,
                    packet.header.timestamp,
                    packet.header.marker,
                    &packet.payload,
                    message.now,
                )? && let Some(frame) = decoder.decode(ssrc, packet.header.timestamp, &au)?
                    && self.video.len() < 4
                {
                    self.video.push_back(frame);
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
