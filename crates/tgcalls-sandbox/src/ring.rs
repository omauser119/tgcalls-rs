//! Two fixed-size SPSC queues in a sealed memfd. All shared fields are atomic,
//! including payload words: a compromised writer cannot cause a Rust data race.
use anyhow::{Context, Result, bail};
use memmap2::{MmapMut, MmapOptions};
use nix::fcntl::{FcntlArg, SealFlag, fcntl};
use nix::sys::memfd::{MFdFlags, memfd_create};
use std::fs::File;
use std::mem::size_of;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tgcalls_core::AudioFrame;

const CAPACITY: usize = 32;
const PAYLOAD: usize = 23_040; // 120ms stereo s16le @ 48kHz

#[repr(C, align(64))]
struct Slot {
    length: AtomicU32,
    ssrc: AtomicU32,
    sequence: AtomicU32,
    timestamp: AtomicU32,
    concealed: AtomicU32,
    target_ms: AtomicU32,
    data: [AtomicU64; PAYLOAD / 8],
}
#[repr(C, align(64))]
struct Queue {
    head: AtomicU64,
    tail: AtomicU64,
    slots: [Slot; CAPACITY],
}
#[repr(C)]
struct Layout {
    input: Queue,
    output: Queue,
}

pub struct SharedAudio {
    file: File,
    mapping: MmapMut,
}

impl SharedAudio {
    pub fn create() -> Result<Self> {
        let fd = memfd_create(
            c"tgcalls-audio",
            MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
        )?;
        let file = File::from(fd);
        file.set_len(size_of::<Layout>() as u64)?;
        // SAFETY: memfd has a fixed size; seals below prevent resize before it is shared.
        let mapping = unsafe { MmapOptions::new().len(size_of::<Layout>()).map_mut(&file)? };
        fcntl(
            &file,
            FcntlArg::F_ADD_SEALS(
                SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_GROW | SealFlag::F_SEAL_SEAL,
            ),
        )?;
        // New memfds are zero-filled. Zero is a valid initialized representation for
        // every AtomicU* field in Layout; no non-atomic shared accesses occur afterwards.
        Ok(Self { file, mapping })
    }

    pub fn attach(fd: OwnedFd) -> Result<Self> {
        let file = File::from(fd);
        if file.metadata()?.len() != size_of::<Layout>() as u64 {
            bail!("invalid shared memory size");
        }
        let seals = SealFlag::from_bits_retain(fcntl(&file, FcntlArg::F_GET_SEALS)?);
        if !seals.contains(SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_GROW | SealFlag::F_SEAL_SEAL)
        {
            bail!("shared memory is not sealed");
        }
        // SAFETY: size and resize-preventing seals have been checked. Payload is accessed atomically.
        let mapping = unsafe { MmapOptions::new().len(size_of::<Layout>()).map_mut(&file)? };
        Ok(Self { file, mapping })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    fn queue(&self, output: bool) -> &Queue {
        // SAFETY: mmap is page aligned, at least size_of::<Layout>(), permanently sealed
        // against resize, and all-zero valid atomic storage. Only shared atomic references
        // are created, including for payload. Mapping outlives this returned reference.
        let layout = unsafe { &*self.mapping.as_ptr().cast::<Layout>() };
        if output {
            &layout.output
        } else {
            &layout.input
        }
    }

    /// The caller serializes the single producer for each direction.
    pub fn push(&self, output: bool, frame: &AudioFrame) -> Result<()> {
        if frame.pcm.len() > PAYLOAD {
            bail!("audio frame too large");
        }
        let queue = self.queue(output);
        let head = queue.head.load(Ordering::Relaxed);
        let tail = queue.tail.load(Ordering::Acquire);
        let distance = head.wrapping_sub(tail);
        if distance > CAPACITY as u64 {
            bail!("corrupt shared queue counters");
        }
        if distance == CAPACITY as u64 {
            bail!("audio queue full");
        }
        let slot = &queue.slots[(head % CAPACITY as u64) as usize];
        let (words, remaining) = frame.pcm.as_chunks::<8>();
        for (destination, source) in slot.data.iter().zip(words) {
            destination.store(u64::from_ne_bytes(*source), Ordering::Relaxed);
        }
        if !remaining.is_empty() {
            let mut last = [0; 8];
            last[..remaining.len()].copy_from_slice(remaining);
            slot.data[words.len()].store(u64::from_ne_bytes(last), Ordering::Relaxed);
        }
        slot.length.store(frame.pcm.len() as u32, Ordering::Relaxed);
        slot.ssrc.store(frame.ssrc, Ordering::Relaxed);
        slot.sequence
            .store(u32::from(frame.sequence), Ordering::Relaxed);
        slot.timestamp.store(frame.timestamp, Ordering::Relaxed);
        slot.concealed
            .store(u32::from(frame.concealed), Ordering::Relaxed);
        slot.target_ms
            .store(frame.target_delay_ms, Ordering::Relaxed);
        queue.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// The caller serializes the single consumer for each direction.
    pub fn pop(&self, output: bool) -> Result<Option<AudioFrame>> {
        let queue = self.queue(output);
        let tail = queue.tail.load(Ordering::Relaxed);
        let head = queue.head.load(Ordering::Acquire);
        let distance = head.wrapping_sub(tail);
        if distance > CAPACITY as u64 {
            bail!("corrupt shared queue counters");
        }
        if distance == 0 {
            return Ok(None);
        }
        let slot = &queue.slots[(tail % CAPACITY as u64) as usize];
        let length = usize::try_from(slot.length.load(Ordering::Relaxed))
            .context("invalid shared length")?;
        if length > PAYLOAD {
            bail!("corrupt shared audio length");
        }
        let mut pcm = vec![0; length];
        let (words, remaining) = pcm.as_chunks_mut::<8>();
        for (destination, source) in words.iter_mut().zip(&slot.data) {
            *destination = source.load(Ordering::Relaxed).to_ne_bytes();
        }
        if !remaining.is_empty() {
            let last = slot.data[words.len()].load(Ordering::Relaxed).to_ne_bytes();
            remaining.copy_from_slice(&last[..remaining.len()]);
        }
        let frame = AudioFrame {
            pcm,
            ssrc: slot.ssrc.load(Ordering::Relaxed),
            sequence: slot.sequence.load(Ordering::Relaxed) as u16,
            timestamp: slot.timestamp.load(Ordering::Relaxed),
            concealed: slot.concealed.load(Ordering::Relaxed) != 0,
            target_delay_ms: slot.target_ms.load(Ordering::Relaxed).min(200),
        };
        queue.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(Some(frame))
    }
}
