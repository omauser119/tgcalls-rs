//! Fail-closed Linux syscall allowlist. Installed with TSYNC before WebRTC parses network data.
use anyhow::{Result, bail};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use std::collections::BTreeMap;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

pub fn install() -> Result<()> {
    // SAFETY: prctl with integer arguments changes only this worker's process properties.
    unsafe {
        // Drop capabilities even when a test/benchmark launcher happens to run as root.
        let header = CapHeader {
            version: 0x2008_0522,
            pid: 0,
        };
        let data = [CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        if libc::syscall(libc::SYS_capset, &header, data.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        ) != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            || libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CORE, &core) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let memory = libc::rlimit {
            rlim_cur: 1024 * 1024 * 1024,
            rlim_max: 1024 * 1024 * 1024,
        };
        if libc::setrlimit(libc::RLIMIT_AS, &memory) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let files = libc::rlimit {
            rlim_cur: 256,
            rlim_max: 256,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &files) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let calls = [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_close,
        libc::SYS_fcntl,
        libc::SYS_fstat,
        libc::SYS_lseek,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mprotect,
        libc::SYS_madvise,
        libc::SYS_brk,
        libc::SYS_mremap,
        libc::SYS_futex,
        libc::SYS_sched_yield,
        libc::SYS_sched_getaffinity,
        libc::SYS_clock_gettime,
        libc::SYS_clock_nanosleep,
        libc::SYS_nanosleep,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getrandom,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_restart_syscall,
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_pwait,
        libc::SYS_eventfd2,
        libc::SYS_poll,
        libc::SYS_ppoll,
        libc::SYS_bind,
        libc::SYS_connect,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
        libc::SYS_setsockopt,
        libc::SYS_getsockopt,
        libc::SYS_shutdown,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ];
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        calls.into_iter().map(|n| (n, vec![])).collect();
    #[cfg(target_arch = "x86_64")]
    rules.insert(libc::SYS_epoll_wait, vec![]);
    // Socket creation is restricted to Internet datagrams. New AF_UNIX, raw sockets,
    // filesystem descriptors, process spawning, ptrace, and io_uring are unavailable.
    let mut socket_rules = Vec::new();
    for family in [libc::AF_INET, libc::AF_INET6] {
        socket_rules.push(SeccompRule::new(vec![
            SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, family as u64)?,
            SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::MaskedEq(0xf),
                libc::SOCK_DGRAM as u64,
            )?,
        ])?);
    }
    rules.insert(libc::SYS_socket, socket_rules);
    // std::net::UdpSocket::set_nonblocking uses FIONBIO on Linux. Do not grant
    // general ioctl access (which exposes device and filesystem operations).
    rules.insert(
        libc::SYS_ioctl,
        vec![SeccompRule::new(vec![SeccompCondition::new(
            1,
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::Eq,
            libc::FIONBIO,
        )?])?],
    );
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::EPERM as u32),
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into()?,
    )?
    .try_into()?;
    seccompiler::apply_filter_all_threads(&filter)?;
    Ok(())
}

pub fn verify_filesystem_denied() -> Result<()> {
    match std::fs::File::open("/etc/passwd") {
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => Ok(()),
        _ => bail!("seccomp failed to block open"),
    }
}

pub fn verify_dangerous_calls_denied() -> Result<()> {
    // Null arguments make these probes harmless even if a policy regresses: the
    // expected failure must be EPERM from seccomp rather than EFAULT from the kernel.
    for syscall in [
        libc::SYS_openat,
        libc::SYS_openat2,
        libc::SYS_mkdirat,
        libc::SYS_unlinkat,
        libc::SYS_execve,
        libc::SYS_io_uring_setup,
    ] {
        // SAFETY: these intentionally invalid syscall arguments point to no Rust storage.
        let result = unsafe { libc::syscall(syscall, -1i64, 0i64, 0i64, 0i64, 0i64, 0i64) };
        if result != -1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
            bail!("dangerous syscall {syscall} was not blocked by seccomp");
        }
    }
    Ok(())
}
