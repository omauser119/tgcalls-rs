use crate::{
    ipc,
    protocol::{Config, Request, Response},
    ring::SharedAudio,
};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::VecDeque,
    net::{IpAddr, SocketAddr, UdpSocket},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tgcalls_core::{AudioFrame, PCM_BYTES, SessionStats};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

struct Process {
    child: Child,
    control: tokio::net::UnixStream,
    audio: SharedAudio,
    timestamp: u32,
}

impl Process {
    async fn spawn(helper: &Path, config: &Config, ip: IpAddr) -> Result<Self> {
        let (parent, child_socket) = UnixStream::pair()?;
        let audio = SharedAudio::create()?;
        let sockets = [
            UdpSocket::bind(SocketAddr::new(ip, 0))?,
            UdpSocket::bind(SocketAddr::new(ip, 0))?,
        ];
        for socket in &sockets {
            socket.set_nonblocking(true)?;
        }
        let child_fd = child_socket.as_raw_fd();
        let mut command = Command::new(helper);
        command
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // SAFETY: pre_exec uses only async-signal-safe descriptor syscalls, without allocating
        // or locking after fork. Descriptor 3 is reserved for this worker's control socket.
        unsafe {
            command.pre_exec(move || {
                if child_fd != 3 && libc::dup2(child_fd, 3) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(3, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().context("cannot execute sandbox helper")?;
        drop(child_socket);
        ipc::send_fds(
            &parent,
            &[
                audio.fd().as_raw_fd(),
                sockets[0].as_raw_fd(),
                sockets[1].as_raw_fd(),
            ],
        )?;
        parent.set_nonblocking(true)?;
        let control = tokio::net::UnixStream::from_std(parent)?;
        let mut process = Self {
            child,
            control,
            audio,
            timestamp: 0,
        };
        let hello: Response = tokio::time::timeout(
            Duration::from_secs(5),
            ipc::read_frame(&mut process.control),
        )
        .await
        .context("sandbox policy bootstrap timeout")??;
        if !hello.ok || !hello.hardened {
            bail!("worker did not activate its security policy");
        }
        let configured = process
            .request(
                Request::Configure {
                    config: config.clone(),
                    bind_ip: ip.to_string(),
                },
                Duration::from_secs(20),
            )
            .await?;
        if !configured.ok {
            bail!(
                "worker configuration failed: {}",
                configured.error.unwrap_or_default()
            );
        }
        Ok(process)
    }

    async fn request(&mut self, request: Request, timeout: Duration) -> Result<Response> {
        tokio::time::timeout(timeout, async {
            ipc::write_frame(&mut self.control, &request).await?;
            let response: Response = ipc::read_frame(&mut self.control).await?;
            Ok::<Response, anyhow::Error>(response)
        })
        .await
        .context("sandbox request timed out")?
    }
    async fn terminate(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

struct State {
    process: Option<Process>,
    failures: VecDeque<Instant>,
    error: Option<String>,
}

pub struct Sandbox {
    state: Mutex<State>,
    helper: PathBuf,
    config: Config,
    ip: IpAddr,
    generation: AtomicU64,
    needs_rejoin: AtomicBool,
    closed: AtomicBool,
}

impl Sandbox {
    pub async fn start(helper: PathBuf, config: Config, ip: IpAddr) -> Result<Arc<Self>> {
        let process = Process::spawn(&helper, &config, ip).await?;
        let sandbox = Arc::new(Self {
            state: Mutex::new(State {
                process: Some(process),
                failures: VecDeque::new(),
                error: None,
            }),
            helper,
            needs_rejoin: AtomicBool::new(matches!(config, Config::Telegram { .. })),
            config,
            ip,
            generation: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&sandbox);
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(100));
            loop {
                timer.tick().await;
                let Some(sandbox) = weak.upgrade() else {
                    break;
                };
                if sandbox.closed.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut state) = sandbox.state.try_lock() else {
                    continue;
                };
                let failure = if let Some(process) = &mut state.process {
                    match process
                        .request(Request::Ping, Duration::from_millis(500))
                        .await
                    {
                        Ok(response) if response.ok => None,
                        Ok(response) => Some(
                            response
                                .error
                                .unwrap_or_else(|| "worker heartbeat error".into()),
                        ),
                        Err(error) => Some(error.to_string()),
                    }
                } else if state.failures.len() <= 5 {
                    Some(
                        state
                            .error
                            .clone()
                            .unwrap_or_else(|| "worker unavailable".into()),
                    )
                } else {
                    continue; // fail closed until the application creates a new Sandbox
                };
                if let Some(reason) = failure {
                    sandbox.restart_locked(&mut state, reason).await;
                }
            }
        });
        Ok(sandbox)
    }

    async fn restart_locked(&self, state: &mut State, reason: String) {
        if let Some(mut process) = state.process.take() {
            process.terminate().await;
        }
        state.error = Some(reason);
        self.needs_rejoin.store(
            matches!(self.config, Config::Telegram { .. }),
            Ordering::Release,
        );
        let now = Instant::now();
        while state
            .failures
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
        {
            state.failures.pop_front();
        }
        state.failures.push_back(now);
        if state.failures.len() > 5 {
            state.error = Some("worker restart budget exceeded (5/minute)".into());
            return;
        }
        // Bounded backoff prevents a malformed stream from creating an unbounded fork loop.
        tokio::time::sleep(Duration::from_millis(25 * state.failures.len() as u64)).await;
        match Process::spawn(&self.helper, &self.config, self.ip).await {
            Ok(process) => {
                state.process = Some(process);
                self.generation.fetch_add(1, Ordering::AcqRel);
            }
            Err(error) => {
                state.error = Some(format!("worker restart failed: {error:#}"));
            }
        }
    }

    async fn request(&self, request: Request, timeout: Duration) -> Result<Response> {
        if self.closed.load(Ordering::Acquire) {
            bail!("sandbox closed");
        }
        let mut state = self.state.lock().await;
        let unavailable = state
            .error
            .clone()
            .unwrap_or_else(|| "worker unavailable".into());
        let process = state.process.as_mut().ok_or_else(|| anyhow!(unavailable))?;
        match process.request(request, timeout).await {
            Ok(response) if response.ok => Ok(response),
            Ok(response) => bail!(
                "worker rejected request: {}",
                response.error.unwrap_or_default()
            ),
            Err(error) => {
                let message = error.to_string();
                self.restart_locked(&mut state, message.clone()).await;
                bail!("worker failed and was restarted; retry/rejoin required: {message}");
            }
        }
    }

    pub async fn join_payload(&self) -> Result<String> {
        self.request(Request::JoinPayload, Duration::from_secs(2))
            .await?
            .payload
            .ok_or_else(|| anyhow!("missing join payload"))
    }
    pub async fn connect(&self, params: String) -> Result<()> {
        self.request(Request::Connect { params }, Duration::from_secs(20))
            .await?;
        self.needs_rejoin.store(false, Ordering::Release);
        Ok(())
    }
    pub async fn set_sources(&self, ssrcs: Vec<u32>) -> Result<()> {
        self.request(Request::Sources { ssrcs }, Duration::from_secs(20))
            .await?;
        Ok(())
    }
    pub async fn send_pcm(&self, pcm: Vec<u8>) -> Result<u32> {
        if pcm.len() != PCM_BYTES {
            bail!("invalid PCM frame length");
        }
        if self.needs_rejoin.load(Ordering::Acquire) {
            bail!("Telegram transport requires join/rejoin");
        }
        if self.closed.load(Ordering::Acquire) {
            bail!("sandbox closed");
        }
        let mut state = self.state.lock().await;
        let process = state
            .process
            .as_mut()
            .ok_or_else(|| anyhow!("worker unavailable"))?;
        let timestamp = process.timestamp;
        process.audio.push(
            false,
            &AudioFrame {
                pcm,
                ssrc: 0,
                sequence: 0,
                timestamp,
                concealed: false,
                target_delay_ms: 0,
            },
        )?;
        process.timestamp = timestamp.wrapping_add(tgcalls_core::FRAME_SAMPLES as u32);
        Ok(timestamp)
    }
    pub async fn receive(&self, timeout: Duration) -> Result<AudioFrame> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.closed.load(Ordering::Acquire) {
                bail!("sandbox closed");
            }
            {
                let state = self.state.lock().await;
                let process = state
                    .process
                    .as_ref()
                    .ok_or_else(|| anyhow!("worker unavailable"))?;
                if let Some(frame) = process.audio.pop(true)? {
                    return Ok(frame);
                }
            }
            if Instant::now() >= deadline {
                bail!("audio receive timed out");
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    pub async fn stats(&self) -> Result<SessionStats> {
        self.request(Request::Stats, Duration::from_secs(2))
            .await?
            .stats
            .ok_or_else(|| anyhow!("missing stats"))
    }
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    pub fn needs_rejoin(&self) -> bool {
        self.needs_rejoin.load(Ordering::Acquire)
    }
    pub async fn worker_pid(&self) -> Option<u32> {
        self.state
            .lock()
            .await
            .process
            .as_ref()
            .and_then(|p| p.child.id())
    }
    pub async fn last_error(&self) -> Option<String> {
        self.state.lock().await.error.clone()
    }
    pub async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(mut process) = self.state.lock().await.process.take() {
            process.terminate().await;
        }
    }
}
