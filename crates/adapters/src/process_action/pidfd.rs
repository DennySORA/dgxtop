//! pidfd system calls: `pidfd_open(pid, 0)`, `pidfd_send_signal(fd, sig,
//! NULL, 0)` and a zero-timeout readiness poll. This is the only module of
//! the crate allowed to use `unsafe`; every block states why it is sound.
//! Flags are always 0: the whole process (thread-group) is targeted, never a
//! single thread or a process group.
#![allow(unsafe_code)]

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

const NO_FLAGS: libc::c_long = 0;

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

/// The Linux implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxPidfd;

impl PidfdSyscalls for LinuxPidfd {
    fn open(&self, pid: i32) -> Result<OwnedFd, Errno> {
        // SAFETY: pidfd_open takes two integers by value and either returns a
        // new descriptor or -1 with errno; no memory is shared with the kernel.
        let result =
            unsafe { libc::syscall(libc::SYS_pidfd_open, libc::c_long::from(pid), NO_FLAGS) };
        if result < 0 {
            return Err(Errno::last());
        }
        let fd = RawFd::try_from(result).map_err(|_| Errno::EOVERFLOW)?;
        // SAFETY: the kernel just created `fd` (O_CLOEXEC) for this call and
        // nothing else refers to it; wrapping it immediately makes the
        // `OwnedFd` its sole owner, so it is closed exactly once.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn send_signal(&self, pidfd: BorrowedFd<'_>, signal: i32) -> Result<(), Errno> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_reports_errno_for_missing_process() {
        // PID i32::MAX is above pid_max on every Linux configuration.
        assert_eq!(LinuxPidfd.open(i32::MAX).unwrap_err(), Errno::ESRCH);
    }

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
