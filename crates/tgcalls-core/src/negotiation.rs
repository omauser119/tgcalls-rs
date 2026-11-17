//! Telegram personal calls, legacy V2 signaling compatible with 8.0.0 / 9.0.0.
//! The MTProto caller authenticates the DH exchange before supplying AuthKey.
use crate::{
    Error, Result,
    telegram::{self, Fingerprint},
};
use rtc::peer_connection::transport::RTCIceServer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const CALL_VERSIONS: &[&str] = &["9.0.0", "8.0.0"];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IceServer {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub turn: bool,
}
impl IceServer {
    pub fn rtc(&self) -> Result<RTCIceServer> {
        let ip: std::net::IpAddr = self
            .host
            .parse()
            .map_err(|_| Error::Invalid("ICE server must be a numeric address".into()))?;
        if self.port == 0 || self.username.len() > 256 || self.password.len() > 256 {
            return Err(Error::Invalid("invalid ICE server".into()));
        }
        let address = std::net::SocketAddr::new(ip, self.port);
        Ok(RTCIceServer {
            urls: vec![format!(
                "{}:{address}",
                if self.turn { "turn" } else { "stun" }
            )],
            username: self.username.clone(),
            credential: self.password.clone(),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaContent {
    #[serde(rename = "type")]
    pub kind: String,
    pub ssrc: String,
    #[serde(default)]
    pub payload_types: Vec<Value>,
    #[serde(default)]
    pub ssrc_groups: Vec<Value>,
    #[serde(default)]
    pub rtp_extensions: Vec<Value>,
}
impl MediaContent {
    pub(crate) fn source(&self) -> Result<u32> {
        self.ssrc
            .parse::<u32>()
            .ok()
            .filter(|s| *s != 0)
            .ok_or_else(|| Error::Invalid("invalid personal SSRC".into()))
    }
    pub(crate) fn accepted(&self) -> Result<Option<Self>> {
        self.source()?;
        if self.payload_types.len() > 32
            || self.ssrc_groups.len() > 8
            || self.rtp_extensions.len() > 16
        {
            return Err(Error::Invalid("channel budget exceeded".into()));
        }
        if self.kind != "audio" && self.kind != "video" {
            return Err(Error::Invalid("invalid media kind".into()));
        }
        let mut result = self.clone();
        result.payload_types.retain(|pt| {
            let id = pt["id"].as_u64();
            if id.is_none_or(|id| id > 127) {
                return false;
            }
            let name = pt["name"].as_str().unwrap_or_default();
            if self.kind == "audio" {
                name.eq_ignore_ascii_case("opus") && pt["clockrate"] == 48000 && pt["channels"] == 2
            } else {
                name.eq_ignore_ascii_case("H264")
                    && pt["clockrate"] == 90000
                    && pt["parameters"]["packetization-mode"].as_str() == Some("1")
                    && pt["parameters"]["profile-level-id"]
                        .as_str()
                        .is_some_and(|p| {
                            p.len() == 6
                                && p.starts_with("42")
                                && p.bytes().all(|c| c.is_ascii_hexdigit())
                        })
            }
        });
        result
            .rtp_extensions
            .retain(|e| e["uri"] == "urn:ietf:params:rtp-hdrext:ssrc-audio-level");
        if result.payload_types.is_empty() {
            Ok(None)
        } else {
            Ok(Some(result))
        }
    }
}

fn attribute<'a>(sdp: &'a str, prefix: &str) -> Result<&'a str> {
    sdp.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .ok_or_else(|| Error::Invalid(format!("missing {prefix}")))
}

pub fn contents_from_sdp(sdp: &str) -> Result<Vec<MediaContent>> {
    crate::packet::validate_sdp(sdp)?;
    let mut result = vec![];
    for section in sdp.split("\r\nm=").skip(1) {
        let kind = section.split_whitespace().next().unwrap_or_default();
        if kind != "audio" && kind != "video" {
            continue;
        }
        let Some(ssrc) = section
            .lines()
            .find_map(|l| l.strip_prefix("a=ssrc:"))
            .and_then(|v| v.split_whitespace().next())
        else {
            continue;
        };
        let mut payload_types = vec![];
        for mapping in section.lines().filter_map(|l| l.strip_prefix("a=rtpmap:")) {
            let (id, codec) = mapping
                .split_once(' ')
                .ok_or_else(|| Error::Invalid("invalid local RTP map".into()))?;
            let parts: Vec<_> = codec.split('/').collect();
            if parts.len() < 2 {
                return Err(Error::Invalid("invalid local codec".into()));
            }
            let mut parameters = BTreeMap::new();
            if let Some(fmtp) = section
                .lines()
                .find_map(|l| l.strip_prefix(&format!("a=fmtp:{id} ")))
            {
                for parameter in fmtp.split(';') {
                    if let Some((k, v)) = parameter.trim().split_once('=') {
                        parameters.insert(k.to_string(), v.to_string());
                    }
                }
            }
            payload_types.push(json!({"id":id.parse::<u8>().map_err(|_|Error::Invalid("payload id".into()))?,
                "name":parts[0],"clockrate":parts[1].parse::<u32>().map_err(|_|Error::Invalid("clock rate".into()))?,
                "channels":if kind=="audio" {2}else{0},"feedbackTypes":[],"parameters":parameters}));
        }
        let rtp_extensions = section
            .lines()
            .filter_map(|l| l.strip_prefix("a=extmap:"))
            .filter_map(|v| v.split_once(' '))
            .filter(|(_, uri)| *uri == "urn:ietf:params:rtp-hdrext:ssrc-audio-level")
            .filter_map(|(id, uri)| id.parse::<u8>().ok().map(|id| json!({"id":id,"uri":uri})))
            .collect();
        result.push(MediaContent {
            kind: kind.into(),
            ssrc: ssrc.into(),
            payload_types,
            ssrc_groups: vec![],
            rtp_extensions,
        });
    }
    Ok(result)
}

pub fn setup_from_sdp(sdp: &str, outgoing: bool) -> Result<Value> {
    let (hash, value) = attribute(sdp, "a=fingerprint:")?
        .split_once(' ')
        .ok_or_else(|| Error::Invalid("fingerprint".into()))?;
    Ok(
        json!({"@type":"InitialSetup","ufrag":attribute(sdp,"a=ice-ufrag:")?,"pwd":attribute(sdp,"a=ice-pwd:")?,"renomination":false,
        "fingerprints":[{"hash":hash,"setup":if outgoing {"actpass"} else {"passive"},"fingerprint":value}]}),
    )
}

pub fn synthesize_answer(
    offer: &str,
    setup: &Value,
    outgoing: bool,
    accepted: &BTreeSet<u32>,
    remote: &[MediaContent],
) -> Result<String> {
    crate::packet::validate_sdp(offer)?;
    let ufrag = telegram::token(
        setup["ufrag"]
            .as_str()
            .ok_or_else(|| Error::Invalid("missing ufrag".into()))?,
        4,
        256,
    )?;
    let pwd = telegram::token(
        setup["pwd"]
            .as_str()
            .ok_or_else(|| Error::Invalid("missing pwd".into()))?,
        22,
        256,
    )?;
    let fp: Fingerprint = serde_json::from_value(
        setup["fingerprints"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| Error::Invalid("missing fingerprint".into()))?
            .clone(),
    )
    .map_err(|e| Error::Invalid(e.to_string()))?;
    telegram::fingerprint(&fp)?;
    let bundle = attribute(offer, "a=group:BUNDLE ")?;
    let mut result = format!(
        "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE {bundle}\r\na=msid-semantic: WMS *\r\n"
    );
    let mut indexes = BTreeMap::<String, usize>::new();
    for section in offer.split("\r\nm=").skip(1) {
        let media = section
            .lines()
            .next()
            .ok_or_else(|| Error::Invalid("empty media section".into()))?;
        let kind = media.split_whitespace().next().unwrap_or_default();
        let local_ssrc = section
            .lines()
            .find_map(|l| l.strip_prefix("a=ssrc:"))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<u32>().ok());
        let remote_channel = if local_ssrc.is_none() && matches!(kind, "audio" | "video") {
            let i = indexes.entry(kind.into()).or_default();
            let channel = remote.iter().filter(|c| c.kind == kind).nth(*i);
            *i += 1;
            channel
        } else {
            None
        };
        let remote_payloads = remote_channel.map(|c| {
            c.payload_types
                .iter()
                .filter_map(|p| p["id"].as_u64())
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        });
        if let Some(ids) = &remote_payloads {
            result.push_str(&format!("m={kind} 9 UDP/TLS/RTP/SAVPF {ids}\r\n"));
        } else {
            result.push_str(&format!("m={media}\r\n"));
        }
        result.push_str("c=IN IP4 0.0.0.0\r\n");
        for line in section.lines().skip(1) {
            if line.starts_with("a=mid:")
                || line == "a=rtcp-mux"
                || line == "a=rtcp-rsize"
                || line.starts_with("a=sctp-port:")
                || line.starts_with("a=max-message-size:")
                || (line.starts_with("a=extmap:")
                    && line.ends_with("urn:ietf:params:rtp-hdrext:ssrc-audio-level"))
                || (remote_channel.is_none()
                    && ["a=rtpmap:", "a=fmtp:", "a=rtcp-fb:"]
                        .iter()
                        .any(|p| line.starts_with(p)))
            {
                result.push_str(line);
                result.push_str("\r\n");
            }
        }
        if let Some(content) = remote_channel {
            for pt in &content.payload_types {
                let id = pt["id"]
                    .as_u64()
                    .ok_or_else(|| Error::Invalid("payload id".into()))?;
                let name = telegram::token(
                    pt["name"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("codec name".into()))?,
                    1,
                    16,
                )?;
                let rate = pt["clockrate"]
                    .as_u64()
                    .ok_or_else(|| Error::Invalid("codec clock".into()))?;
                result.push_str(&format!(
                    "a=rtpmap:{id} {name}/{rate}{}\r\n",
                    if kind == "audio" { "/2" } else { "" }
                ));
                if let Some(parameters) = pt["parameters"].as_object() {
                    let mut pairs = vec![];
                    for (k, v) in parameters {
                        let v = telegram::token(
                            v.as_str()
                                .ok_or_else(|| Error::Invalid("codec parameter".into()))?,
                            1,
                            256,
                        )?;
                        telegram::token(k, 1, 128)?;
                        if k.contains(';') || v.contains(';') {
                            return Err(Error::Invalid("codec parameter separator".into()));
                        }
                        pairs.push(format!("{k}={v}"));
                    }
                    if !pairs.is_empty() {
                        result.push_str(&format!("a=fmtp:{id} {}\r\n", pairs.join(";")));
                    }
                }
            }
            result.push_str(&format!(
                "a=sendonly\r\na=ssrc:{} cname:telegram\r\na=ssrc:{} msid:telegram remote-{}\r\n",
                content.ssrc, content.ssrc, content.ssrc
            ));
        } else if let Some(ssrc) = local_ssrc {
            result.push_str(if accepted.contains(&ssrc) {
                "a=recvonly\r\n"
            } else {
                "a=inactive\r\n"
            });
        }
        result.push_str(&format!("a=ice-ufrag:{ufrag}\r\na=ice-pwd:{pwd}\r\na=fingerprint:{} {}\r\na=setup:{}\r\na=ice-options:trickle\r\n",fp.hash,fp.fingerprint,if outgoing{"passive"}else{"active"}));
    }
    crate::packet::validate_sdp(&result)?;
    Ok(result)
}
