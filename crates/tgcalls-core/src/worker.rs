//! Bounded request protocol shared by native mobile services and the iOS Wasm module.
use crate::{
    AudioFrame, Error, Result,
    auth::{AuthKey, KeyExchange},
    negotiation::{MediaContent, contents_from_sdp, setup_from_sdp, synthesize_answer},
    sansio::{Datagram, Engine},
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
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    New {
        local: String,
        ssrc: u32,
        video: Option<VideoConfig>,
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
    Signaling {
        packet: Vec<u8>,
    },
    PollSignaling,
    GroupPayload,
    GroupConnect {
        params: String,
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
}
#[derive(Default)]
pub struct Worker {
    engine: Option<Engine>,
    exchange: Option<KeyExchange>,
    key: Option<AuthKey>,
    personal: Option<Personal>,
    offer: Option<String>,
    ssrc: u32,
}
impl Worker {
    pub fn execute(&mut self, bytes: &[u8]) -> Vec<u8> {
        let result = if bytes.len() > MAX_REQUEST {
            Err(Error::Invalid("request budget exceeded".into()))
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
    fn engine(&mut self) -> Result<&mut Engine> {
        self.engine
            .as_mut()
            .ok_or_else(|| Error::Invalid("engine not initialized".into()))
    }
    fn handle(&mut self, request: Request) -> Result<Response> {
        let mut response = Response::success();
        let now = Instant::now();
        match request {
            Request::New { local, ssrc, video } => {
                if self.engine.is_some() || ssrc == 0 {
                    return Err(Error::Invalid("already initialized or invalid SSRC".into()));
                }
                self.engine = Some(Engine::new(
                    local
                        .parse()
                        .map_err(|_| Error::Invalid("local address".into()))?,
                    ssrc,
                    video,
                    now,
                )?);
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
                response.payload = json!({"connection_state":self.engine()?.connection_state});
            }
            Request::SendAudio { pcm } => {
                response.payload = json!(self.engine()?.send_audio(&pcm, now)?)
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
                let state = self
                    .personal
                    .as_mut()
                    .ok_or_else(|| Error::Invalid("personal call not initialized".into()))?;
                if let Some(packet) = state.cipher.service(now)? {
                    state.signals.push_back(packet);
                }
                response.signaling = state.signals.drain(..).collect();
                response.payload = json!({"media_negotiated":state.exchange.is_none() && !state.accepted.is_empty() && !state.remote.is_empty()});
            }
            Request::GroupPayload => {
                self.engine()?.one_way()?;
                let offer = self.engine()?.offer()?;
                response.payload = json!(crate::telegram::join_payload(&offer, self.ssrc)?);
                self.offer = Some(offer);
            }
            Request::GroupConnect { params } => {
                let offer = self
                    .offer
                    .as_ref()
                    .ok_or_else(|| Error::Invalid("generate group payload first".into()))?;
                let answer = crate::telegram::answer_sdp(offer, &params, &[])?;
                self.engine()?.accept_answer(answer)?;
            }
        }
        if let Some(engine) = &mut self.engine {
            response.datagrams = engine.outgoing();
            response.audio = engine.audio.drain(..).collect();
            response.video = engine.video.drain(..).collect();
        }
        Ok(response)
    }
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
                if state.setup.is_some() {
                    return Err(Error::Invalid("ICE replacement requires new call".into()));
                }
                state.setup = Some(message);
                messages.extend(state.early.drain(..));
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
