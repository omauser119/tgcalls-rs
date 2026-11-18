use anyhow::{Result, bail};
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use nix::{
    errno::Errno,
    sys::eventfd::{EfdFlags, EventFd},
};
use serde::{Serialize, de::DeserializeOwned};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const MAX_CONTROL: usize = 65_536;
const BOOTSTRAP: u8 = b'3';

pub struct AudioSignal(AsyncFd<OwnedFd>);

impl AudioSignal {
    pub fn create() -> Result<Self> {
        Self::attach(EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)?.into())
    }

    pub fn attach(fd: OwnedFd) -> Result<Self> {
        // SAFETY: OwnedFd keeps the descriptor open and its number stable
        // throughout the registration lifetime. The broker sets O_NONBLOCK.
        Ok(Self(unsafe {
            AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE)
        }?))
    }

    pub fn raw_fd(&self) -> RawFd {
        self.0.get_ref().as_raw_fd()
    }

    pub fn signal(&self) -> Result<()> {
        loop {
            match nix::unistd::write(self.0.get_ref(), &1u64.to_ne_bytes()) {
                Ok(8) | Err(Errno::EAGAIN) => return Ok(()),
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
                Ok(_) => bail!("short eventfd write"),
            }
        }
    }

    pub async fn wait(&self) -> Result<()> {
        loop {
            let mut ready = self.0.readable().await?;
            match ready.try_io(|fd| {
                nix::unistd::read(fd.get_ref(), &mut [0; 8]).map_err(std::io::Error::from)
            }) {
                Ok(Ok(8)) => return Ok(()),
                Ok(Ok(_)) => bail!("short eventfd read"),
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => continue,
            }
        }
    }
}

pub fn send_fds(socket: &UnixStream, fds: &[RawFd]) -> Result<()> {
    let written = sendmsg::<()>(
        socket.as_raw_fd(),
        &[IoSlice::new(&[BOOTSTRAP])],
        &[ControlMessage::ScmRights(fds)],
        MsgFlags::MSG_NOSIGNAL,
        None,
    )?;
    if written != 1 {
        bail!("short bootstrap send");
    }
    Ok(())
}

pub fn receive_fds(socket: &UnixStream) -> Result<Vec<OwnedFd>> {
    let mut byte = [0u8; 1];
    let mut buffers = [IoSliceMut::new(&mut byte)];
    let mut space = nix::cmsg_space!([RawFd; 8]);
    let msg = recvmsg::<()>(
        socket.as_raw_fd(),
        &mut buffers,
        Some(&mut space),
        MsgFlags::MSG_CMSG_CLOEXEC,
    )?;
    let mut fds = Vec::new();
    for control in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(raw) = control {
            for fd in raw {
                // SAFETY: SCM_RIGHTS returns fresh descriptors owned by this receiver.
                fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    if msg.flags.contains(MsgFlags::MSG_CTRUNC)
        || msg.bytes != 1
        || fds.len() != 5
        || byte[0] != BOOTSTRAP
    {
        bail!("invalid bootstrap descriptor message");
    }
    Ok(fds)
}

pub async fn write_frame<T: Serialize>(
    socket: &mut tokio::net::UnixStream,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_CONTROL {
        bail!("control message too large");
    }
    socket.write_u32(bytes.len() as u32).await?;
    socket.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_frame<T: DeserializeOwned>(socket: &mut tokio::net::UnixStream) -> Result<T> {
    let length = socket.read_u32().await? as usize;
    if length == 0 || length > MAX_CONTROL {
        bail!("invalid control frame length");
    }
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
