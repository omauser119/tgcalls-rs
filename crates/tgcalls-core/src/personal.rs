//! Async personal-call transport using the shared Telegram negotiation codec.
pub use crate::negotiation::{
    CALL_VERSIONS, IceServer, MediaContent, contents_from_sdp, setup_from_sdp, synthesize_answer,
};
use crate::{
    Error, Result, auth::AuthKey, session::Session, signaling::Signaling, video::VideoConfig,
};
use rtc::rtp_transceiver::{
    RTCRtpTransceiverDirection, RTCRtpTransceiverInit, rtp_sender::RtpCodecKind,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
    time::Instant,
};
use tokio::sync::Mutex;
use webrtc::peer_connection::RTCSessionDescription;
struct State {
    cipher: Signaling,
    outgoing: bool,
    setup: Option<Value>,
    local_offer: String,
    local_pending: bool,
    own_contents: Vec<MediaContent>,
    own_exchange: Option<u32>,
    accepted: BTreeSet<u32>,
    remote: Vec<MediaContent>,
    candidates: Vec<Value>,
    queued: VecDeque<Vec<u8>>,
    own_offered: bool,
    early: VecDeque<Value>,
    sent_ids: BTreeSet<u32>,
}
pub struct PersonalCall {
    pub media: Arc<Session>,
    state: Mutex<State>,
}
impl PersonalCall {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        key: AuthKey,
        outgoing: bool,
        video: Option<VideoConfig>,
        servers: Vec<IceServer>,
        bind_ip: &str,
        sockets: Option<Vec<std::net::UdpSocket>>,
        ssrc: u32,
    ) -> Result<Self> {
        if servers.len() > 16 {
            return Err(Error::Invalid("ICE server budget exceeded".into()));
        }
        let servers = servers
            .iter()
            .map(IceServer::rtc)
            .collect::<Result<Vec<_>>>()?;
        let media =
            Arc::new(Session::personal_media(bind_ip, sockets, ssrc, video, servers).await?);
        media
            .rtc()
            .create_data_channel(
                "data",
                Some(webrtc::data_channel::RTCDataChannelInit {
                    negotiated: Some(0),
                    ..Default::default()
                }),
            )
            .await?;
        let offer = media.refresh_offer().await?;
        let own_contents = contents_from_sdp(&offer)?;
        let mut state = State {
            cipher: Signaling::new(key, outgoing),
            outgoing,
            setup: None,
            local_offer: offer.clone(),
            local_pending: true,
            own_contents,
            own_exchange: None,
            accepted: BTreeSet::new(),
            remote: vec![],
            candidates: vec![],
            queued: VecDeque::new(),
            own_offered: false,
            early: VecDeque::new(),
            sent_ids: BTreeSet::new(),
        };
        queue(&mut state, setup_from_sdp(&offer, outgoing)?)?;
        let candidates:Vec<_>=offer.lines().filter_map(|l|l.strip_prefix("a=candidate:")).map(|line|json!({"sdpString":format!("candidate:{line}"),"sdpMid":"0","sdpMLineIndex":0})).collect();
        queue(
            &mut state,
            json!({"@type":"Candidates","candidates":candidates}),
        )?;
        if outgoing {
            own_offer(&mut state)?;
        }
        Ok(Self {
            media,
            state: Mutex::new(state),
        })
    }
    pub async fn poll_signaling(&self) -> Result<Vec<Vec<u8>>> {
        let mut state = self.state.lock().await;
        if let Some(packet) = state.cipher.service(Instant::now())? {
            state.queued.push_back(packet);
        }
        Ok(state.queued.drain(..).collect())
    }
    pub async fn ready(&self) -> bool {
        let state = self.state.lock().await;
        state.own_exchange.is_none()
            && !state.accepted.is_empty()
            && !state.remote.is_empty()
            && self.media.stats().connection_state == "Connected"
    }
    pub async fn receive_signaling(&self, packet: &[u8]) -> Result<()> {
        let mut state = self.state.lock().await;
        let packets = state.cipher.receive(packet)?;
        let mut messages: VecDeque<Value> = VecDeque::new();
        for message in packets {
            if message.len() > 16_000 {
                return Err(Error::Invalid("signaling JSON budget exceeded".into()));
            }
            let message: Value =
                serde_json::from_slice(&message).map_err(|e| Error::Invalid(e.to_string()))?;
            messages.push_back(message);
        }
        while let Some(message) = messages.pop_front() {
            match message["@type"].as_str() {
                Some("InitialSetup") => {
                    if state.setup.is_some() {
                        return Err(Error::Invalid(
                            "unexpected ICE credential replacement; re-establish call".into(),
                        ));
                    }
                    let _ = synthesize_answer(
                        &state.local_offer,
                        &message,
                        state.outgoing,
                        &state.accepted,
                        &state.remote,
                    )?;
                    state.setup = Some(message);
                    messages.extend(state.early.drain(..));
                }
                Some("Candidates") => {
                    let candidates = message["candidates"]
                        .as_array()
                        .ok_or_else(|| Error::Invalid("missing candidates".into()))?;
                    if state.candidates.len() + candidates.len() > 128 {
                        return Err(Error::Invalid("candidate budget exceeded".into()));
                    }
                    state.candidates.extend(candidates.iter().cloned());
                }
                Some("NegotiateChannels") => {
                    if state.setup.is_none() {
                        if state.early.len() >= 8 {
                            return Err(Error::Invalid("early signaling budget".into()));
                        }
                        state.early.push_back(message);
                        continue;
                    }
                    let exchange = message["exchangeId"]
                        .as_str()
                        .and_then(|s| s.parse::<u32>().ok())
                        .or_else(|| {
                            message["exchangeId"]
                                .as_u64()
                                .and_then(|n| u32::try_from(n).ok())
                        })
                        .ok_or_else(|| Error::Invalid("invalid exchange ID".into()))?;
                    let contents: Vec<MediaContent> =
                        serde_json::from_value(message["contents"].clone())
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                    if contents.len() > 8 {
                        return Err(Error::Invalid("media channel budget exceeded".into()));
                    }
                    if Some(exchange) == state.own_exchange {
                        let own: BTreeSet<_> = state
                            .own_contents
                            .iter()
                            .map(MediaContent::source)
                            .collect::<Result<_>>()?;
                        for content in contents {
                            if content.accepted()?.is_some() && own.contains(&content.source()?) {
                                state.accepted.insert(content.source()?);
                            }
                        }
                        state.own_exchange = None;
                    } else if state.sent_ids.contains(&exchange) {
                        continue;
                    } else {
                        let accepted = contents
                            .iter()
                            .map(MediaContent::accepted)
                            .collect::<Result<Vec<_>>>()?
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>();
                        if state.remote.len() + accepted.len() > 8 {
                            return Err(Error::Invalid("remote media budget exceeded".into()));
                        }
                        // Clear the initial provisional offer before adding remote one-way sections.
                        self.complete_local(&mut state).await?;
                        for content in &accepted {
                            if state.remote.iter().any(|old| old.ssrc == content.ssrc) {
                                continue;
                            }
                            self.media
                                .rtc()
                                .add_transceiver_from_kind(
                                    if content.kind == "audio" {
                                        RtpCodecKind::Audio
                                    } else {
                                        RtpCodecKind::Video
                                    },
                                    Some(RTCRtpTransceiverInit {
                                        direction: RTCRtpTransceiverDirection::Recvonly,
                                        ..Default::default()
                                    }),
                                )
                                .await?;
                            state.remote.push(content.clone());
                        }
                        queue(
                            &mut state,
                            json!({"@type":"NegotiateChannels","exchangeId":exchange.to_string(),"contents":accepted}),
                        )?;
                        if !state.own_offered {
                            own_offer(&mut state)?;
                        }
                    }
                    self.complete_local(&mut state).await?;
                    state.local_offer = self.media.refresh_offer().await?;
                    state.local_pending = true;
                    self.complete_local(&mut state).await?;
                }
                Some("MediaState") => {}
                _ => return Err(Error::Invalid("unsupported signaling message".into())),
            }
            if !state.local_pending {
                for candidate in std::mem::take(&mut state.candidates) {
                    let value = candidate["sdpString"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("candidate string".into()))?;
                    if value.len() > 2048 || value.contains(['\r', '\n', '\0']) {
                        return Err(Error::Invalid("invalid candidate string".into()));
                    }
                    self.media
                        .rtc()
                        .add_ice_candidate(webrtc::peer_connection::RTCIceCandidateInit {
                            candidate: value.into(),
                            sdp_mid: Some("0".into()),
                            sdp_mline_index: Some(0),
                            username_fragment: None,
                            url: None,
                        })
                        .await?;
                }
            }
        }
        Ok(())
    }
    async fn complete_local(&self, state: &mut State) -> Result<()> {
        if !state.local_pending {
            return Ok(());
        }
        let setup = state.setup.as_ref().ok_or_else(|| {
            Error::Invalid("NegotiateChannels arrived before InitialSetup".into())
        })?;
        let answer = synthesize_answer(
            &state.local_offer,
            setup,
            state.outgoing,
            &state.accepted,
            &state.remote,
        )?;
        self.media
            .rtc()
            .set_remote_description(RTCSessionDescription::answer(answer)?)
            .await?;
        state.local_pending = false;
        Ok(())
    }
}
fn queue(state: &mut State, message: Value) -> Result<()> {
    if state.queued.len() >= 64 {
        return Err(Error::Invalid("outbound signaling budget exceeded".into()));
    }
    let bytes = serde_json::to_vec(&message).map_err(|e| Error::Invalid(e.to_string()))?;
    let packet = state.cipher.send(&bytes, Instant::now())?;
    state.queued.push_back(packet);
    Ok(())
}
fn own_offer(state: &mut State) -> Result<()> {
    let mut random = [0; 4];
    getrandom::fill(&mut random).map_err(|e| Error::Invalid(e.to_string()))?;
    let exchange = u32::from_ne_bytes(random);
    state.own_exchange = Some(exchange);
    state.own_offered = true;
    state.sent_ids.insert(exchange);
    queue(
        state,
        json!({"@type":"NegotiateChannels","exchangeId":exchange.to_string(),"contents":state.own_contents}),
    )
}
