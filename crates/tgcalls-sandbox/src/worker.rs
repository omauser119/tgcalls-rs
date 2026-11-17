use crate::{
    ipc, policy,
    protocol::{Config, Request, Response},
    ring::SharedAudio,
};
use anyhow::{Result, anyhow, bail};
use futures::FutureExt;
use std::{
    net::UdpSocket, os::fd::FromRawFd, os::unix::net::UnixStream, sync::Arc, time::Duration,
};
use tgcalls_core::session::Session;
use tokio::sync::Mutex;

pub fn run() -> Result<()> {
    // fd 3 is established by the supervisor using dup2 before exec. There is no path-based socket.
    // SAFETY: this binary owns inherited descriptor 3; it is adopted exactly once.
    let control = unsafe { UnixStream::from_raw_fd(3) };
    control.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut fds = ipc::receive_fds(&control)?.into_iter();
    let shared = Arc::new(SharedAudio::attach(
        fds.next().ok_or_else(|| anyhow!("missing memfd"))?,
    )?);
    let sockets: Vec<UdpSocket> = fds.map(UdpSocket::from).collect();
    if sockets.len() != 2 {
        bail!("missing UDP descriptors");
    }
    control.set_read_timeout(None)?;
    control.set_nonblocking(true)?;
    // SAFETY: prctl affects this helper only and arranges termination on parent death.
    unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::getppid() == 1 {
            bail!("supervisor exited");
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut control = tokio::net::UnixStream::from_std(control)?;
        let mut sockets = Some(sockets);
        let session: Arc<Mutex<Option<Arc<Session>>>> = Arc::new(Mutex::new(None));
        // No external SDP, JSON, RTP or Opus input is processed before this point.
        policy::install()?;
        policy::verify_filesystem_denied()?;
        ipc::write_frame(&mut control, &Response::success()).await?;
        let media_session = session.clone();
        let input_audio = shared.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(1));
            loop {
                timer.tick().await;
                let current = media_session.lock().await.clone();
                let Some(current) = current else {
                    continue;
                };
                let frame = match input_audio.pop(false) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => continue,
                    Err(_) => std::process::exit(70),
                };
                // A codec panic destroys the worker, rather than reusing possibly corrupted state.
                if !matches!(
                    std::panic::AssertUnwindSafe(current.send_pcm(&frame.pcm))
                        .catch_unwind()
                        .await,
                    Ok(Ok(_))
                ) {
                    std::process::exit(70);
                }
            }
        });
        let output_session = session.clone();
        tokio::spawn(async move {
            loop {
                let current = output_session.lock().await.clone();
                if let Some(current) = current {
                    match current.receive(Duration::from_millis(100)).await {
                        Ok(frame) => {
                            let _ = shared.push(true, &frame);
                        } // bounded overflow drops a frame
                        Err(tgcalls_core::Error::Timeout) => {}
                        Err(_) => std::process::exit(70),
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        });
        loop {
            let request: Request = ipc::read_frame(&mut control).await?;
            let stop = matches!(request, Request::Stop);
            let result = std::panic::AssertUnwindSafe(async {
                let mut response = Response::success();
                if let Request::Configure { config, bind_ip } = request {
                    if session.lock().await.is_some() {
                        bail!("worker already configured");
                    }
                    let current = match config {
                        Config::Loopback { impairment } => {
                            Session::loopback(impairment, &bind_ip, sockets.take()).await?
                        }
                        Config::Telegram { ssrc } => {
                            Session::group(&bind_ip, sockets.take(), ssrc).await?
                        }
                    };
                    *session.lock().await = Some(Arc::new(current));
                } else {
                    let current = session
                        .lock()
                        .await
                        .clone()
                        .ok_or_else(|| anyhow!("worker not configured"))?;
                    match request {
                        Request::JoinPayload => {
                            response.payload = Some(current.join_payload().await?)
                        }
                        Request::Connect { params } => current.connect_telegram(&params).await?,
                        Request::Sources { ssrcs } => current.set_sources(ssrcs).await?,
                        Request::Stats => response.stats = Some(current.stats()),
                        Request::Ping => {}
                        Request::Stop => current.close().await?,
                        Request::Configure { .. } => bail!("unexpected configuration"),
                    }
                }
                Ok::<Response, anyhow::Error>(response)
            })
            .catch_unwind()
            .await;
            let response = match result {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => Response::error(error),
                Err(_) => std::process::exit(70),
            };
            ipc::write_frame(&mut control, &response).await?;
            if stop {
                break;
            }
        }
        Ok(())
    })
}

/// CLI-only policy test. The supervisor never exposes this operation to network input.
pub fn policy_self_test() -> Result<()> {
    policy::install()?;
    policy::verify_filesystem_denied()?;
    policy::verify_dangerous_calls_denied()
}
