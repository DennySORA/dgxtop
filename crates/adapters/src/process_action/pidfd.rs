//! pidfd system calls: `pidfd_open(pid, 0)`, `pidfd_send_signal(fd, sig,
//! NULL, 0)` and a zero-timeout readiness poll, through rustix's safe
//! wrappers, so the crate holds no `unsafe` of its own. Flags are always
//! empty: the whole process (thread-group) is targeted, never a single
//! thread or a process group.

use std::os::fd::{BorrowedFd, OwnedFd};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};

/// Readiness of a pidfd: readable once the process has terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidfdReadiness {
    Running,
    Exited,
}

/// The pidfd operations the action adapter performs (injectable so errno
/// classification can be tested without real kernels or processes).
pub trait PidfdSyscalls: Send {
    /// # Errors
    ///
    /// Returns the `Errno` of the system call when it fails.
    fn open(&self, pid: i32) -> Result<OwnedFd, Errno>;
    /// Exactly one `pidfd_send_signal` on `pidfd`; never a PID-based call.
    ///
    /// # Errors
    ///
    /// Returns the `Errno` of the system call when it fails.
    fn send_signal(&self, pidfd: BorrowedFd<'_>, signal: i32) -> Result<(), Errno>;
    /// Non-blocking readiness check (`poll` with a zero timeout).
    ///
    /// # Errors
    ///
    /// Returns the `Errno` of the system call when it fails.
    fn readiness(&self, pidfd: BorrowedFd<'_>) -> Result<PidfdReadiness, Errno>;
}

/// The Linux implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxPidfd;

/// The system call's error as the `Errno` the adapter classifies.
const fn errno(error: rustix::io::Errno) -> Errno {
    Errno::from_raw(error.raw_os_error())
}

impl PidfdSyscalls for LinuxPidfd {
    fn open(&self, pid: i32) -> Result<OwnedFd, Errno> {
        // A PID that is not positive is answered as the kernel answers it,
        // here: rustix takes a negative one for a caller's mistake. The
        // descriptor the kernel creates is close-on-exec and the caller's.
        if pid <= 0 {
            return Err(Errno::EINVAL);
        }
        let pid = Pid::from_raw(pid).ok_or(Errno::EINVAL)?;
        pidfd_open(pid, PidfdFlags::empty()).map_err(errno)
    }

    fn send_signal(&self, pidfd: BorrowedFd<'_>, signal: i32) -> Result<(), Errno> {
        // `info` is NULL and the flags 0: the kernel builds a SI_USER
        // siginfo itself.
        let signal = Signal::from_named_raw(signal).ok_or(Errno::EINVAL)?;
        pidfd_send_signal(pidfd, signal).map_err(errno)
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

    /// A PID that is not positive is the kernel's `EINVAL`, never a panic
    /// and never a process group (`-pid`) or the caller itself (`0`).
    #[test]
    fn a_pid_that_is_not_positive_is_invalid() {
        for pid in [0, -1, i32::MIN] {
            assert_eq!(LinuxPidfd.open(pid).unwrap_err(), Errno::EINVAL, "{pid}");
        }
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
