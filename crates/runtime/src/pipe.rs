//! Pipes with `pipe2(2)` flags on every platform dgxtop builds on.

use std::os::fd::OwnedFd;

use nix::fcntl::OFlag;

/// `pipe2(2)`: a pipe whose two ends carry `flags` from the start.
#[cfg(target_os = "linux")]
pub(crate) fn pipe2(flags: OFlag) -> nix::Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(flags)
}

/// macOS has no `pipe2`, so the flags are set right after `pipe(2)`, and a
/// child another thread spawns in between could inherit the ends. dgxtop
/// runs on Linux and builds on macOS only for development, where that window
/// is accepted. Only `O_CLOEXEC` and `O_NONBLOCK` are supported.
#[cfg(not(target_os = "linux"))]
pub(crate) fn pipe2(flags: OFlag) -> nix::Result<(OwnedFd, OwnedFd)> {
    use std::os::fd::AsFd;

    use nix::errno::Errno;
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};

    if !(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).contains(flags) {
        return Err(Errno::EINVAL);
    }
    let (reader, writer) = nix::unistd::pipe()?;
    for end in [&reader, &writer] {
        if flags.contains(OFlag::O_CLOEXEC) {
            fcntl(end.as_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        }
        if flags.contains(OFlag::O_NONBLOCK) {
            let status = OFlag::from_bits_retain(fcntl(end.as_fd(), FcntlArg::F_GETFL)?);
            fcntl(end.as_fd(), FcntlArg::F_SETFL(status | OFlag::O_NONBLOCK))?;
        }
    }
    Ok((reader, writer))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use nix::fcntl::{FcntlArg, FdFlag, fcntl};

    use super::*;

    fn closes_on_exec(end: &OwnedFd) -> bool {
        let bits = fcntl(end.as_fd(), FcntlArg::F_GETFD).unwrap();
        FdFlag::from_bits_retain(bits).contains(FdFlag::FD_CLOEXEC)
    }

    fn is_nonblocking(end: &OwnedFd) -> bool {
        let bits = fcntl(end.as_fd(), FcntlArg::F_GETFL).unwrap();
        OFlag::from_bits_retain(bits).contains(OFlag::O_NONBLOCK)
    }

    #[test]
    fn both_ends_close_on_exec_and_stay_blocking() {
        let (reader, writer) = pipe2(OFlag::O_CLOEXEC).unwrap();
        for end in [&reader, &writer] {
            assert!(closes_on_exec(end));
            assert!(!is_nonblocking(end));
        }
    }

    #[test]
    fn both_ends_can_also_be_nonblocking() {
        let (reader, writer) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).unwrap();
        for end in [&reader, &writer] {
            assert!(closes_on_exec(end));
            assert!(is_nonblocking(end));
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn flags_beyond_cloexec_and_nonblock_are_refused() {
        assert_eq!(
            pipe2(OFlag::O_APPEND).unwrap_err(),
            nix::errno::Errno::EINVAL
        );
    }
}
