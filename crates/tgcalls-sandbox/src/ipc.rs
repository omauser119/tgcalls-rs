use anyhow::{Result, bail};
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use serde::{Serialize, de::DeserializeOwned};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const MAX_CONTROL: usize = 65_536;

pub fn send_fds(socket: &UnixStream, fds: &[RawFd]) -> Result<()> {
    let written = sendmsg::<()>(
        socket.as_raw_fd(),
        &[IoSlice::new(b"T")],
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
    if msg.flags.contains(MsgFlags::MSG_CTRUNC) || msg.bytes != 1 || fds.len() != 3 {
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
