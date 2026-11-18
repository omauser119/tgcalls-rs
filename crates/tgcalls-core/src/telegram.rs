//! Telegram group-call DataJSON <-> SDP bridge. No MTProto credentials are parsed here.
use crate::{Error, Result, packet::validate_sdp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::IpAddr;

const MAX_JSON: usize = 65_536;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    pub hash: String,
    pub setup: String,
    pub fingerprint: String,
}

pub(crate) fn token(value: &str, min: usize, max: usize) -> Result<&str> {
    if !(min..=max).contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::Invalid("invalid signaling token".into()));
    }
    Ok(value)
}

pub(crate) fn fingerprint(fp: &Fingerprint) -> Result<()> {
    let pairs: Vec<_> = fp.fingerprint.split(':').collect();
    if fp.hash != "sha-256"
        || pairs.len() != 32
        || pairs
            .iter()
            .any(|p| p.len() != 2 || !p.bytes().all(|b| b.is_ascii_hexdigit()))
        || !matches!(fp.setup.as_str(), "active" | "passive" | "actpass")
    {
        return Err(Error::Invalid(
            "unsupported or invalid DTLS fingerprint".into(),
        ));
    }
    Ok(())
}

fn attribute<'a>(sdp: &'a str, prefix: &str) -> Result<&'a str> {
    sdp.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .ok_or_else(|| Error::Invalid(format!("missing SDP {prefix}")))
}

pub fn join_payload(offer: &str, ssrc: u32) -> Result<String> {
    validate_sdp(offer)?;
    let (hash, value) = attribute(offer, "a=fingerprint:")?
        .split_once(' ')
        .ok_or_else(|| Error::Invalid("malformed fingerprint".into()))?;
    let fp = Fingerprint {
        hash: hash.into(),
        fingerprint: value.into(),
        setup: "passive".into(),
    };
    fingerprint(&fp)?;
    Ok(json!({
        "ufrag": token(attribute(offer, "a=ice-ufrag:")?, 4, 256)?,
        "pwd": token(attribute(offer, "a=ice-pwd:")?, 22, 256)?,
        "fingerprints": [fp],
        // Telegram TL represents SSRCs as signed int32; preserve the bit pattern.
        "ssrc": ssrc as i32,
        "ssrc-groups": []
    })
    .to_string())
}

fn scalar<'a>(obj: &'a Value, field: &str) -> Result<std::borrow::Cow<'a, str>> {
    match &obj[field] {
        Value::String(s) => Ok(std::borrow::Cow::Borrowed(s)),
        Value::Number(n) if n.is_u64() || n.is_i64() => Ok(std::borrow::Cow::Owned(n.to_string())),
        _ => Err(Error::Invalid(format!("missing/invalid candidate {field}"))),
    }
}

fn candidate(c: &Value) -> Result<Option<String>> {
    let foundation = scalar(c, "foundation")?;
    token(&foundation, 1, 32)?;
    let component: u16 = scalar(c, "component")?
        .parse()
        .map_err(|_| Error::Invalid("candidate component".into()))?;
    let priority: u32 = scalar(c, "priority")?
        .parse()
        .map_err(|_| Error::Invalid("candidate priority".into()))?;
    let port: u16 = scalar(c, "port")?
        .parse()
        .map_err(|_| Error::Invalid("candidate port".into()))?;
    let ip: IpAddr = scalar(c, "ip")?
        .parse()
        .map_err(|_| Error::Invalid("candidate IP".into()))?;
    let protocol = scalar(c, "protocol")?;
    let kind = scalar(c, "type")?;
    if !matches!(component, 1 | 2)
        || port == 0
        || !(protocol.eq_ignore_ascii_case("udp") || protocol.eq_ignore_ascii_case("tcp"))
        || !matches!(kind.as_ref(), "host" | "srflx" | "prflx" | "relay")
    {
        return Err(Error::Invalid(
            "only RTP-mux UDP candidates are supported".into(),
        ));
    }
    // Telegram can include TCP fallback and a non-muxed RTCP component alongside
    // usable UDP/RTP candidates. Ignore supported-but-unused candidates explicitly.
    if component == 2 || protocol.eq_ignore_ascii_case("tcp") {
        return Ok(None);
    }
    Ok(Some(format!(
        "a=candidate:{foundation} {component} UDP {priority} {ip} {port} typ {kind}\r\n"
    )))
}

pub fn answer_sdp(offer: &str, response: &str, remote_ssrcs: &[u32]) -> Result<String> {
    remote_sdp(offer, response, remote_ssrcs, false)
}

pub(crate) fn sfu_offer_sdp(
    template: &str,
    response: &str,
    remote_ssrcs: &[u32],
) -> Result<String> {
    remote_sdp(template, response, remote_ssrcs, true)
}

fn remote_sdp(
    template: &str,
    response: &str,
    remote_ssrcs: &[u32],
    sfu_offer: bool,
) -> Result<String> {
    validate_sdp(template)?;
    if response.len() > MAX_JSON || remote_ssrcs.len() > 64 {
        return Err(Error::Invalid("oversized Telegram signaling".into()));
    }
    let value: Value = serde_json::from_str(response).map_err(|e| Error::Invalid(e.to_string()))?;
    if value["stream"] == true {
        return Err(Error::Invalid(
            "Telegram broadcast transport is unsupported".into(),
        ));
    }
    let transport = &value["transport"];
    let ufrag = token(
        transport["ufrag"]
            .as_str()
            .ok_or_else(|| Error::Invalid("missing ufrag".into()))?,
        4,
        256,
    )?;
    let pwd = token(
        transport["pwd"]
            .as_str()
            .ok_or_else(|| Error::Invalid("missing pwd".into()))?,
        22,
        256,
    )?;
    let fps: Vec<Fingerprint> = serde_json::from_value(transport["fingerprints"].clone())
        .map_err(|e| Error::Invalid(e.to_string()))?;
    if fps.is_empty() || fps.len() > 4 {
        return Err(Error::Invalid("missing fingerprints".into()));
    }
    let fp = fps
        .iter()
        .find(|fp| fp.hash == "sha-256")
        .ok_or_else(|| Error::Invalid("sha-256 fingerprint required".into()))?;
    fingerprint(fp)?;
    if fp.setup != "active" {
        return Err(Error::Invalid("Telegram DTLS active role required".into()));
    }
    let candidates = transport["candidates"]
        .as_array()
        .ok_or_else(|| Error::Invalid("missing candidates".into()))?;
    if candidates.is_empty() || candidates.len() > 64 {
        return Err(Error::Invalid("invalid candidate count".into()));
    }
    let mut candidate_lines = String::new();
    for c in candidates {
        if let Some(line) = candidate(c)? {
            candidate_lines.push_str(&line);
        }
    }
    if candidate_lines.is_empty() {
        return Err(Error::Invalid("no usable UDP RTP candidates".into()));
    }
    let mut result = String::from("v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n");
    let setup = if sfu_offer {
        // Telegram takes the controlling ICE role and initiates DTLS. The
        // full ICE offer selects that role without relying on conflict recovery.
        "actpass"
    } else {
        result.push_str("a=ice-lite\r\n");
        "active"
    };
    if let Ok(bundle) = attribute(template, "a=group:BUNDLE ") {
        token(bundle, 1, 128).or_else(|_| {
            if bundle.split(' ').all(|s| token(s, 1, 16).is_ok()) {
                Ok(bundle)
            } else {
                Err(Error::Invalid("invalid bundle mids".into()))
            }
        })?;
        result.push_str(&format!("a=group:BUNDLE {bundle}\r\n"));
    }
    result.push_str("a=msid-semantic: WMS *\r\n");
    let mut audio_index = 0;
    for section in template.split("\r\nm=").skip(1) {
        let mut lines = section.lines();
        let media = lines
            .next()
            .ok_or_else(|| Error::Invalid("missing media".into()))?;
        if !media.starts_with("audio ") && !media.starts_with("application ") {
            return Err(Error::Invalid("audio-only Telegram engine".into()));
        }
        result.push_str(&format!("m={media}\r\nc=IN IP4 0.0.0.0\r\n"));
        for line in lines {
            if [
                "a=mid:",
                "a=rtpmap:",
                "a=fmtp:",
                "a=rtcp-fb:",
                "a=extmap:",
                "a=sctp-port:",
                "a=max-message-size:",
            ]
            .iter()
            .any(|p| line.starts_with(p))
                || matches!(line, "a=rtcp-mux" | "a=rtcp-rsize")
            {
                result.push_str(line);
                result.push_str("\r\n");
            }
        }
        result.push_str(&format!(
            "a=ice-ufrag:{ufrag}\r\na=ice-pwd:{pwd}\r\na=fingerprint:{} {}\r\na=setup:{setup}\r\n",
            fp.hash, fp.fingerprint
        ));
        result.push_str(&candidate_lines);
        result.push_str("a=end-of-candidates\r\n");
        if media.starts_with("audio ") {
            // First m-line sends our audio. Further lines receive individual SFU SSRCs.
            if audio_index == 0 {
                result.push_str("a=recvonly\r\n");
            } else if let Some(ssrc) = remote_ssrcs.get(audio_index - 1) {
                result.push_str(&format!("a=sendonly\r\na=ssrc:{ssrc} cname:telegram\r\na=ssrc:{ssrc} msid:telegram audio-{ssrc}\r\n"));
            } else {
                return Err(Error::Invalid("unmatched remote audio transceiver".into()));
            }
            audio_index += 1;
        }
    }
    validate_sdp(&result)?;
    Ok(result)
}
