//! Bounded request protocol shared by native mobile services and the iOS Wasm module.
use crate::{
    AudioFrame, Error, Result,
    audio_processing::{VoiceOptions, VoiceProcessor},
    auth::{AuthKey, KeyExchange},
    negotiation::{IceServer, MediaContent, contents_from_sdp, setup_from_sdp, synthesize_answer},
    sansio::{Datagram, Engine, MediaState},
    signaling::Signaling,
    video::{VideoConfig, VideoFrame},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    time::Instant,
};

pub const MAX_REQUEST: usize = 8 * 1024 * 1024;
/// Raw host-supplied buffers. The final header byte selects verified key (1),
/// rendered PCM (2), captured PCM prefixed with a little-endian u16 delay (3).
pub const BUFFER_MAGIC: &[u8] = b"TGC\x01";
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    New {
        local: String,
        ssrc: u32,
        video: Option<VideoConfig>,
        #[serde(default)]
        ice_servers: Vec<IceServer>,
        #[serde(default)]
        relay_only: bool,
        #[serde(default)]
        group: bool,
    },
    Offer,
    Answer {
        sdp: String,
    },
    AcceptAnswer {
        sdp: String,
    },
    Packet {
        peer: String,
        bytes: Vec<u8>,
    },
    Tick,
    SendAudio {
        pcm: Vec<u8>,
    },
    ConfigureAudio {
        capture_rate: u32,
        capture_channels: u16,
        render_rate: u32,
        render_channels: u16,
        options: VoiceOptions,
    },
    RenderAudio {
        pcm: Vec<u8>,
    },
    MediaState {
        state: MediaState,
    },
    CaptureAudio {
        pcm: Vec<u8>,
        delay_ms: u16,
    },
    SendVideo {
        i420: Vec<u8>,
    },
    Drain,
    InitDh {
        g: u32,
        prime: Vec<u8>,
        random: Vec<u8>,
        commitment: Option<Vec<u8>>,
    },
    CompleteDh {
        remote: Vec<u8>,
        fingerprint: Option<i64>,
    },
    StartPersonal {
        outgoing: bool,
    },
    RebindPersonal {
        local: String,
    },
    Signaling {
        packet: Vec<u8>,
    },
    PollSignaling,
    GroupPayload,
    GroupConnect {
        params: String,
    },
    GroupSources {
        ssrcs: Vec<u32>,
    },
}
#[derive(Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub error: Option<String>,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub datagrams: Vec<Datagram>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<AudioFrame>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<VideoFrame>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signaling: Vec<Vec<u8>>,
}
impl Response {
    fn success() -> Self {
        Self {
            ok: true,
            error: None,
            payload: Value::Null,
            datagrams: vec![],
            audio: vec![],
            video: vec![],
            signaling: vec![],
        }
    }
}
struct Personal {
    cipher: Signaling,
    outgoing: bool,
    setup: Option<Value>,
    offer: String,
    pending: bool,
    own: Vec<MediaContent>,
    exchange: Option<u32>,
    sent_ids: BTreeSet<u32>,
    accepted: BTreeSet<u32>,
    remote: Vec<MediaContent>,
    candidates: Vec<String>,
    early: VecDeque<Value>,
    signals: VecDeque<Vec<u8>>,
    waiting_for_restart: bool,
    last_restart: Option<Instant>,
}
#[derive(Default)]
pub struct Worker {
    engine: Option<Engine>,
    exchange: Option<KeyExchange>,
    key: Option<AuthKey>,
    personal: Option<Personal>,
    offer: Option<String>,
    group: bool,
    group_transport: Option<String>,
    group_sources: Vec<u32>,
    ssrc: u32,
    voice: Option<VoiceProcessor>,
    captured: Option<Vec<u8>>,
}
impl Worker {
    pub fn execute(&mut self, bytes: &[u8]) -> Vec<u8> {
        let result = if bytes.len() > MAX_REQUEST {
            Err(Error::Invalid("request budget exceeded".into()))
        } else if let Some(bytes) = bytes.strip_prefix(BUFFER_MAGIC) {
            self.handle_buffer(bytes)
        } else {
            serde_json::from_slice::<Request>(bytes)
                .map_err(|e| Error::Invalid(e.to_string()))
                .and_then(|request| self.handle(request))
        };
        let response = match result {
            Ok(response) => response,
            Err(error) => Response {
                ok: false,
                error: Some(error.to_string()),
                ..Response::success()
            },
        };
        serde_json::to_vec(&response).unwrap_or_else(|_| {
            br#"{"ok":false,"error":"response serialization failed","payload":null}"#.to_vec()
        })
    }
    fn handle_buffer(&mut self, request: &[u8]) -> Result<Response> {
        let mut response = Response::success();
        match request {
            [1, key @ ..]
                if !self.group
                    && self.key.is_none()
                    && self.personal.is_none()
                    && self.exchange.is_none()
                    && self.engine.is_some() =>
            {
                self.key = Some(AuthKey::from_bytes(key)?);
            }
            [2, pcm @ ..] => self
                .voice
                .as_mut()
                .ok_or_else(no_audio_device)?
                .render(pcm)?,
            [3, low, high, pcm @ ..] => {
                response.payload =
                    self.capture(pcm, u16::from_le_bytes([*low, *high]), Instant::now())?;
            }
            _ => return Err(Error::Invalid("invalid host buffer or call state".into())),
        }
        self.collect(response)
    }

    fn capture(&mut self, pcm: &[u8], delay: u16, now: Instant) -> Result<Value> {
        let block = self
            .voice
            .as_mut()
            .ok_or_else(no_audio_device)?
            .capture(pcm, delay)?;
        if let Some(mut frame) = self.captured.take() {
            frame.extend(block);
            Ok(json!(self.engine()?.send_audio(&frame, now)?))
        } else {
            self.captured = Some(block);
            Ok(Value::Null)
        }
    }
    fn engine(&mut self) -> Result<&mut Engine> {
        self.engine
            .as_mut()
            .ok_or_else(|| Error::Invalid("engine not initialized".into()))
    }
    fn handle(&mut self, request: Request) -> Result<Response> {
        let mut response = Response::success();
        let now = Instant::now();
        match request {
            Request::New {
                local,
                ssrc,
                video,
                ice_servers,
                relay_only,
                group,
            } => {
                if self.engine.is_some() || ssrc == 0 {
                    return Err(Error::Invalid("already initialized or invalid SSRC".into()));
                }
                let local = local
                    .parse()
                    .map_err(|_| Error::Invalid("local address".into()))?;
                self.engine = Some(if group {
                    if video.is_some() || relay_only || !ice_servers.is_empty() {
                        return Err(Error::Invalid("unsupported group transport options".into()));
                    }
                    Engine::group(local, ssrc, now)?
                } else {
                    Engine::with_ice(local, ssrc, video, ice_servers, relay_only, now)?
                });
                self.group = group;
                self.ssrc = ssrc;
            }
            Request::Offer => {
                let offer = self.engine()?.offer()?;
                response.payload = json!(offer);
                self.offer = Some(offer);
            }
            Request::Answer { sdp } => response.payload = json!(self.engine()?.answer(sdp)?),
            Request::AcceptAnswer { sdp } => self.engine()?.accept_answer(sdp)?,
            Request::Packet { peer, bytes } => self.engine()?.receive_datagram(
                peer.parse()
                    .map_err(|_| Error::Invalid("peer address".into()))?,
                &bytes,
                now,
            )?,
            Request::Tick => {
                self.engine()?.tick(now)?;
                let engine = self.engine()?;
                response.payload = json!({"connection_state":engine.connection_state, "remote_media":engine.remote_media_state});
            }
            Request::MediaState { state } => self.engine()?.set_media_state(state)?,
            Request::SendAudio { pcm } => {
                response.payload = json!(self.engine()?.send_audio(&pcm, now)?)
            }
            Request::ConfigureAudio {
                capture_rate,
                capture_channels,
                render_rate,
                render_channels,
                options,
            } => {
                self.voice = Some(VoiceProcessor::new(
                    capture_rate,
                    capture_channels,
                    render_rate,
                    render_channels,
                    options,
                )?);
                self.captured = None;
            }
            Request::RenderAudio { pcm } => self
                .voice
                .as_mut()
                .ok_or_else(no_audio_device)?
                .render(&pcm)?,
            Request::CaptureAudio { pcm, delay_ms } => {
                response.payload = self.capture(&pcm, delay_ms, now)?;
            }
            Request::SendVideo { i420 } => self.engine()?.send_video(&i420, now)?,
            Request::Drain => {}
            Request::InitDh {
                g,
                prime,
                random,
                commitment,
            } => {
                if self.exchange.is_some() {
                    return Err(Error::Invalid("DH already initialized".into()));
                }
                let exchange = KeyExchange::new(g, &prime, &random, commitment.as_deref())?;
                response.payload = json!(exchange.first_message());
                self.exchange = Some(exchange);
            }
            Request::CompleteDh {
                remote,
                fingerprint,
            } => {
                let exchange = self
                    .exchange
                    .as_mut()
                    .ok_or_else(|| Error::Invalid("DH not initialized".into()))?;
                let (key, verification) = exchange.complete(&remote, fingerprint)?;
                response.payload = json!({"fingerprint":key.fingerprint(),"public":exchange.public().as_slice(),"verification_hash":verification});
                self.key = Some(key);
            }
            Request::StartPersonal { outgoing } => {
                if self.personal.is_some() {
                    return Err(Error::Invalid("personal call already initialized".into()));
                }
                let key = self
                    .key
                    .take()
                    .ok_or_else(|| Error::Invalid("authenticated DH exchange required".into()))?;
                self.engine()?.personal_channel()?;
                self.engine()?.one_way()?;
                let offer = self.engine()?.offer()?;
                let mut personal = Personal {
                    cipher: Signaling::new(key, outgoing),
                    outgoing,
                    setup: None,
                    offer: offer.clone(),
                    pending: true,
                    own: contents_from_sdp(&offer)?,
                    exchange: None,
                    sent_ids: BTreeSet::new(),
                    accepted: BTreeSet::new(),
                    remote: vec![],
                    candidates: vec![],
                    early: VecDeque::new(),
                    signals: VecDeque::new(),
                    waiting_for_restart: false,
                    last_restart: None,
                };
                queue(&mut personal, setup_from_sdp(&offer, outgoing)?)?;
                let candidates:Vec<_>=offer.lines().filter_map(|l|l.strip_prefix("a=candidate:")).map(|c|json!({"sdpString":format!("candidate:{c}"),"sdpMid":"0","sdpMLineIndex":0})).collect();
                queue(
                    &mut personal,
                    json!({"@type":"Candidates","candidates":candidates}),
                )?;
                if outgoing {
                    own_offer(&mut personal)?;
                }
                self.personal = Some(personal);
            }
            Request::RebindPersonal { local } => {
                let local = local
                    .parse()
                    .map_err(|_| Error::Invalid("invalid new socket address".into()))?;
                let mut state = self
                    .personal
                    .take()
                    .ok_or_else(|| Error::Invalid("personal call not initialized".into()))?;
                let result = restart(self.engine()?, &mut state, local, now, true);
                self.personal = Some(state);
                result?;
            }
            Request::Signaling { packet } => {
                let mut state = self
                    .personal
                    .take()
                    .ok_or_else(|| Error::Invalid("personal call not initialized".into()))?;
                let result = receive_personal(self.engine()?, &mut state, &packet);
                self.personal = Some(state);
                result?;
            }
            Request::PollSignaling => {
                let candidates: Vec<_> = self
                    .engine()?
                    .gathered_candidates()
                    .map(|candidate| json!({"sdpString":candidate,"sdpMid":"0","sdpMLineIndex":0}))
                    .collect();
                let state = self
                    .personal
                    .as_mut()
                    .ok_or_else(|| Error::Invalid("personal call not initialized".into()))?;
                if !candidates.is_empty() {
                    queue(state, json!({"@type":"Candidates","candidates":candidates}))?;
                }
                if let Some(packet) = state.cipher.service(now)? {
                    state.signals.push_back(packet);
                }
                response.signaling = state.signals.drain(..).collect();
                response.payload = json!({"media_negotiated":state.exchange.is_none() && !state.accepted.is_empty() && !state.remote.is_empty()});
            }
            Request::GroupPayload => {
                if !self.group {
                    return Err(Error::Invalid("initialize a group worker first".into()));
                }
                let offer = self.engine()?.offer_template()?;
                response.payload = json!(crate::telegram::join_payload(&offer, self.ssrc)?);
                self.offer = Some(offer);
            }
            Request::GroupConnect { params } => {
                let offer = self
                    .offer
                    .as_ref()
                    .filter(|_| self.group)
                    .ok_or_else(|| Error::Invalid("generate group payload first".into()))?;
                let sdp = crate::telegram::sfu_offer_sdp(offer, &params, &self.group_sources)?;
                response.payload = json!(self.engine()?.answer(sdp)?);
                self.group_transport = Some(params);
            }
            Request::GroupSources { ssrcs } => {
                let params = self
                    .group_transport
                    .as_ref()
                    .ok_or_else(|| {
                        Error::Invalid("connect to the SFU before adding sources".into())
                    })?
                    .clone();
                if ssrcs.len() > 64 || ssrcs.contains(&0) || ssrcs.contains(&self.ssrc) {
                    return Err(Error::Invalid("invalid remote group sources".into()));
                }
                let added: BTreeSet<_> = ssrcs
                    .into_iter()
                    .filter(|ssrc| !self.group_sources.contains(ssrc))
                    .collect();
                if self.group_sources.len() + added.len() > 64 {
                    return Err(Error::Invalid(
                        "group source budget exhausted; rejoin required".into(),
                    ));
                }
                if !added.is_empty() {
                    for ssrc in added {
                        self.engine()?.add_receiver(false)?;
                        self.group_sources.push(ssrc);
                    }
                    let template = self.engine()?.offer_template()?;
                    let sdp =
                        crate::telegram::sfu_offer_sdp(&template, &params, &self.group_sources)?;
                    self.engine()?.answer(sdp)?;
                    self.offer = Some(template);
                }
            }
        }
        self.collect(response)
    }

    fn collect(&mut self, mut response: Response) -> Result<Response> {
        if let Some(engine) = &mut self.engine {
            response.datagrams = engine.outgoing()?;
            response.audio = engine.audio.drain(..).collect();
            response.video = engine.video.drain(..).collect();
        }
        Ok(response)
    }
}

fn no_audio_device() -> Error {
    Error::Invalid("configure the audio device first".into())
}

fn restart(
    engine: &mut Engine,
    state: &mut Personal,
    local: std::net::SocketAddr,
    now: Instant,
    initiating: bool,
) -> Result<()> {
    if state.pending
        || state.setup.is_none()
        || state.accepted.is_empty()
        || state.remote.is_empty()
    {
        return Err(Error::Invalid(
            "finish initial negotiation before restarting ICE".into(),
        ));
    }
    if state
        .last_restart
        .is_some_and(|last| now.saturating_duration_since(last) < std::time::Duration::from_secs(1))
    {
        return Err(Error::Invalid("ICE restart rate exceeded".into()));
    }
    state.offer = engine.restart_offer(local, now)?;
    state.pending = true;
    complete(engine, state)?;
    queue(state, setup_from_sdp(&state.offer, state.outgoing)?)?;
    state.last_restart = Some(now);
    state.waiting_for_restart = initiating;
    Ok(())
}

fn queue(state: &mut Personal, message: Value) -> Result<()> {
    if state.signals.len() >= 64 {
        return Err(Error::Invalid("signaling queue budget".into()));
    }
    let bytes = serde_json::to_vec(&message).map_err(|e| Error::Invalid(e.to_string()))?;
    state
        .signals
        .push_back(state.cipher.send(&bytes, Instant::now())?);
    Ok(())
}
fn own_offer(state: &mut Personal) -> Result<()> {
    let mut bytes = [0; 4];
    getrandom::fill(&mut bytes).map_err(|e| Error::Invalid(e.to_string()))?;
    let id = u32::from_ne_bytes(bytes);
    state.exchange = Some(id);
    state.sent_ids.insert(id);
    queue(
        state,
        json!({"@type":"NegotiateChannels","exchangeId":id.to_string(),"contents":state.own}),
    )
}
fn complete(engine: &mut Engine, state: &mut Personal) -> Result<()> {
    if !state.pending {
        return Ok(());
    }
    let setup = state
        .setup
        .as_ref()
        .ok_or_else(|| Error::Invalid("missing remote setup".into()))?;
    engine.accept_answer(synthesize_answer(
        &state.offer,
        setup,
        state.outgoing,
        &state.accepted,
        &state.remote,
    )?)?;
    state.pending = false;
    Ok(())
}
fn receive_personal(engine: &mut Engine, state: &mut Personal, packet: &[u8]) -> Result<()> {
    let mut messages: VecDeque<Value> = state
        .cipher
        .receive(packet)?
        .iter()
        .map(|m| serde_json::from_slice(m).map_err(|e| Error::Invalid(e.to_string())))
        .collect::<Result<_>>()?;
    while let Some(message) = messages.pop_front() {
        match message["@type"].as_str() {
            Some("InitialSetup") => {
                if let Some(previous) = &state.setup {
                    if previous["fingerprints"] != message["fingerprints"] {
                        return Err(Error::Invalid(
                            "DTLS fingerprint changed during ICE restart".into(),
                        ));
                    }
                    if previous["ufrag"] == message["ufrag"] && previous["pwd"] == message["pwd"] {
                        continue;
                    }
                    state.setup = Some(message);
                    if state.waiting_for_restart {
                        state.offer = engine.offer()?;
                        state.pending = true;
                        complete(engine, state)?;
                        state.waiting_for_restart = false;
                    } else {
                        restart(engine, state, engine.local_addr(), Instant::now(), false)?;
                    }
                } else {
                    state.setup = Some(message);
                    messages.extend(state.early.drain(..));
                }
            }
            Some("Candidates") => {
                let candidates = message["candidates"]
                    .as_array()
                    .ok_or_else(|| Error::Invalid("candidate list".into()))?;
                if state.candidates.len() + candidates.len() > 128 {
                    return Err(Error::Invalid("candidate budget".into()));
                }
                for c in candidates {
                    state.candidates.push(
                        c["sdpString"]
                            .as_str()
                            .ok_or_else(|| Error::Invalid("candidate value".into()))?
                            .into(),
                    );
                }
            }
            Some("NegotiateChannels") => {
                if state.setup.is_none() {
                    if state.early.len() >= 8 {
                        return Err(Error::Invalid("early signal budget".into()));
                    }
                    state.early.push_back(message);
                    continue;
                }
                let id = message["exchangeId"]
                    .as_str()
                    .and_then(|v| v.parse::<u32>().ok())
                    .or_else(|| {
                        message["exchangeId"]
                            .as_u64()
                            .and_then(|v| u32::try_from(v).ok())
                    })
                    .ok_or_else(|| Error::Invalid("exchange ID".into()))?;
                let contents: Vec<MediaContent> =
                    serde_json::from_value(message["contents"].clone())
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                if contents.len() > 8 {
                    return Err(Error::Invalid("channel budget".into()));
                }
                if Some(id) == state.exchange {
                    for c in contents {
                        if c.accepted()?.is_some() && state.own.iter().any(|o| o.ssrc == c.ssrc) {
                            state.accepted.insert(c.source()?);
                        }
                    }
                    state.exchange = None;
                } else if state.sent_ids.contains(&id) {
                    continue;
                } else {
                    let accepted = contents
                        .iter()
                        .map(MediaContent::accepted)
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>();
                    complete(engine, state)?;
                    for c in &accepted {
                        if state.remote.iter().any(|r| r.ssrc == c.ssrc) {
                            continue;
                        }
                        if state.remote.len() >= 8 {
                            return Err(Error::Invalid("remote source budget".into()));
                        }
                        engine.add_receiver(c.kind == "video")?;
                        state.remote.push(c.clone());
                    }
                    queue(
                        state,
                        json!({"@type":"NegotiateChannels","exchangeId":id.to_string(),"contents":accepted}),
                    )?;
                    if state.sent_ids.is_empty() {
                        own_offer(state)?;
                    }
                }
                complete(engine, state)?;
                state.offer = engine.offer()?;
                state.pending = true;
                complete(engine, state)?;
            }
            Some("MediaState") => {}
            _ => return Err(Error::Invalid("unsupported signaling type".into())),
        }
        if !state.pending {
            for candidate in std::mem::take(&mut state.candidates) {
                engine.add_candidate(candidate)?;
            }
        }
    }
    Ok(())
}
