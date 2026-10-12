//! pidfd system calls: `pidfd_open(pid, 0)`, `pidfd_send_signal(fd, sig,
//! NULL, 0)` and a zero-timeout readiness poll. This is the only module of
//! the crate allowed to use `unsafe`; every block states why it is sound.
//! Flags are always 0: the whole process (thread-group) is targeted, never a
//! single thread or a process group.
#![allow(unsafe_code)]

use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

/// Readiness of a pidfd: readable once the process has terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidfdReadiness {
    Running,
    Exited,
}

/// The pidfd operations the action adapter performs (injectable so errno
/// classification can be tested without real kernels or processes).
pub trait PidfdSyscalls: Send {
    fn open(&self, pid: i32) -> Result<OwnedFd, Errno>;
    /// Exactly one `pidfd_send_signal` on `pidfd`; never a PID-based call.
    fn send_signal(&self, pidfd: BorrowedFd<'_>, signal: i32) -> Result<(), Errno>;
    /// Non-blocking readiness check (`poll` with a zero timeout).
    fn readiness(&self, pidfd: BorrowedFd<'_>) -> Result<PidfdReadiness, Errno>;
}

/// The Linux implementation. pidfd exists only on Linux: on macOS, where
/// dgxtop builds for development, every pidfd call fails with `ENOSYS`.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxPidfd;

impl PidfdSyscalls for LinuxPidfd {
    fn open(&self, pid: i32) -> Result<OwnedFd, Errno> {
        let fd = syscalls::pidfd_open(pid)?;
        // SAFETY: the kernel just created `fd` (O_CLOEXEC) for this call and
        // nothing else refers to it; wrapping it immediately makes the
        // `OwnedFd` its sole owner, so it is closed exactly once.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn send_signal(&self, pidfd: BorrowedFd<'_>, signal: i32) -> Result<(), Errno> {
        syscalls::pidfd_send_signal(pidfd, signal)
    }

    fn readiness(&self, pidfd: BorrowedFd<'_>) -> Result<PidfdReadiness, Errno> {
        let mut descriptors = [PollFd::new(pidfd, PollFlags::POLLIN)];
        poll(&mut descriptors, PollTimeout::ZERO)?;
        let events = descriptors[0].revents().unwrap_or(PollFlags::empty());
        if events.intersects(PollFlags::POLLNVAL | PollFlags::POLLERR) {
            return Err(Errno::EBADF);
        }
        if events.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            return Ok(PidfdReadiness::Exited);
        }
        Ok(PidfdReadiness::Running)
    }
}

#[cfg(target_os = "linux")]
mod syscalls {
    use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

    use nix::errno::Errno;

    const NO_FLAGS: libc::c_long = 0;

    pub(super) fn pidfd_open(pid: i32) -> Result<RawFd, Errno> {
        // SAFETY: pidfd_open takes two integers by value and either returns a
        // new descriptor or -1 with errno; no memory is shared with the kernel.
        let result =
            unsafe { libc::syscall(libc::SYS_pidfd_open, libc::c_long::from(pid), NO_FLAGS) };
        if result < 0 {
            return Err(Errno::last());
        }
        RawFd::try_from(result).map_err(|_| Errno::EOVERFLOW)
    }

    pub(super) fn pidfd_send_signal(pidfd: BorrowedFd<'_>, signal: i32) -> Result<(), Errno> {
        // SAFETY: `pidfd` is a live descriptor borrowed for the duration of the
        // call; `info` is NULL (the kernel builds a SI_USER siginfo itself) and
        // flags are 0, so the kernel reads no user memory.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                libc::c_long::from(pidfd.as_raw_fd()),
                libc::c_long::from(signal),
                std::ptr::null::<libc::siginfo_t>(),
                NO_FLAGS,
            )
        };
        if result < 0 {
            return Err(Errno::last());
        }
        Ok(())
    }
}

/// `ENOSYS` is what a kernel without pidfd answers; the action adapter
/// reports it as `ActionError::PidfdUnsupported`.
#[cfg(not(target_os = "linux"))]
mod syscalls {
    use std::os::fd::{BorrowedFd, RawFd};

    use nix::errno::Errno;

    pub(super) fn pidfd_open(_pid: i32) -> Result<RawFd, Errno> {
        Err(Errno::ENOSYS)
    }

    pub(super) fn pidfd_send_signal(_pidfd: BorrowedFd<'_>, _signal: i32) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn every_pidfd_call_is_unsupported_off_linux() {
        use std::os::fd::AsFd;

        let pid = i32::try_from(std::process::id()).unwrap();
        assert_eq!(LinuxPidfd.open(pid).unwrap_err(), Errno::ENOSYS);
        let signal = LinuxPidfd.send_signal(std::io::stdin().as_fd(), libc::SIGTERM);
        assert_eq!(signal.unwrap_err(), Errno::ENOSYS);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_reports_errno_for_missing_process() {
        // PID i32::MAX is above pid_max on every Linux configuration.
        assert_eq!(LinuxPidfd.open(i32::MAX).unwrap_err(), Errno::ESRCH);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_is_running_and_descriptor_is_cloexec() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let fd = LinuxPidfd.open(pid).unwrap();
        assert_eq!(
            LinuxPidfd.readiness(std::os::fd::AsFd::as_fd(&fd)).unwrap(),
            PidfdReadiness::Running
        );
        let flags = nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_GETFD).unwrap();
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }
}
