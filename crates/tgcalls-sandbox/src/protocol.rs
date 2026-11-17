use serde::{Deserialize, Serialize};
use tgcalls_core::{Impairment, SessionStats};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Config {
    Loopback { impairment: Impairment },
    Telegram { ssrc: u32 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Configure { config: Config, bind_ip: String },
    JoinPayload,
    Connect { params: String },
    Sources { ssrcs: Vec<u32> },
    Ping,
    Stats,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub error: Option<String>,
    pub payload: Option<String>,
    pub stats: Option<SessionStats>,
    pub hardened: bool,
}
impl Response {
    pub fn success() -> Self {
        Self {
            ok: true,
            error: None,
            payload: None,
            stats: None,
            hardened: true,
        }
    }
    pub fn error(error: impl ToString) -> Self {
        Self {
            ok: false,
            error: Some(error.to_string()),
            ..Self::success()
        }
    }
}
