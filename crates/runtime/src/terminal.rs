//! Non-blocking terminal output and hang-up-aware input readiness
//! (`UI.md` §5).
//!
//! A terminal that stops reading (a stuck PTY reader or a stopped tmux pane)
//! must never block the render thread: writes go through `O_NONBLOCK` and
//! report `Ok(0)` when the kernel buffer is full, and writability is awaited
//! with `poll(POLLOUT)` and a timeout. `O_NONBLOCK` belongs to the shared
//! open file description, so the original flags are saved and restored both
//! explicitly and on drop — otherwise the user's shell would inherit a
//! non-blocking terminal after dgxtop exits.
//!
//! A terminal that hangs up (SSH drop, closed PTY master) makes every read
//! return end-of-file. The input library's reader retries such reads
//! forever, so input readiness is checked here first with `poll(POLLIN)`,
//! where a hang-up is reported explicitly.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use dgxtop_core::ports::{InputGate, InputReadiness, TerminalTransport};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::Mode;

/// Non-blocking writer over a duplicate of the terminal descriptor.
#[derive(Debug)]
pub struct NonBlockingTerminal {
    fd: OwnedFd,
    original_flags: OFlag,
    restored: bool,
}

impl NonBlockingTerminal {
    /// Wraps any open descriptor (`std::io::stdout()`, a PTY, a pipe).
    pub fn from_borrowed(fd: impl AsFd) -> io::Result<Self> {
        // Close-on-exec, so adapter hosts started later never inherit it.
        let duplicate = fd.as_fd().try_clone_to_owned()?;
        let original_flags = current_flags(duplicate.as_fd())?;
        let mut terminal = Self {
            fd: duplicate,
            original_flags,
            restored: true,
        };
        set_flags(terminal.fd.as_fd(), original_flags | OFlag::O_NONBLOCK)?;
        terminal.restored = false;
        Ok(terminal)
    }

    /// Flags that will be put back by [`TerminalTransport::restore`].
    pub fn original_flags(&self) -> OFlag {
        self.original_flags
    }
}

fn current_flags(fd: BorrowedFd<'_>) -> io::Result<OFlag> {
    let bits = fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::from)?;
    Ok(OFlag::from_bits_retain(bits))
}

fn set_flags(fd: BorrowedFd<'_>, flags: OFlag) -> io::Result<()> {
    fcntl(fd, FcntlArg::F_SETFL(flags))
        .map(|_| ())
        .map_err(io::Error::from)
}

impl TerminalTransport for NonBlockingTerminal {
    fn write_nonblocking(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        match nix::unistd::write(&self.fd, bytes) {
            Ok(written) => Ok(written),
            Err(Errno::EAGAIN | Errno::EINTR) => Ok(0),
            Err(errno) => Err(io::Error::from(errno)),
        }
    }

    fn wait_writable(&mut self, timeout: Duration) -> io::Result<bool> {
        let mut descriptors = [PollFd::new(self.fd.as_fd(), PollFlags::POLLOUT)];
        match poll(&mut descriptors, poll_timeout(timeout)) {
            Ok(0) | Err(Errno::EINTR) => Ok(false),
            Ok(_) => {
                let events = descriptors[0].revents().unwrap_or(PollFlags::empty());
                // Linux reports POLLOUT together with POLLERR for a pipe whose
                // reader is gone, so the error bits must win.
                if events.intersects(HANG_UP_EVENTS) {
                    Err(io::Error::from(io::ErrorKind::BrokenPipe))
                } else {
                    Ok(events.contains(PollFlags::POLLOUT))
                }
            }
            Err(errno) => Err(io::Error::from(errno)),
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        set_flags(self.fd.as_fd(), self.original_flags)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for NonBlockingTerminal {
    fn drop(&mut self) {
        // Best effort: a descriptor that vanished cannot be restored anyway.
        let _ = self.restore();
    }
}

/// Poll results that mean the peer is gone (a hung-up terminal, a closed
/// pipe) or the descriptor is unusable.
const HANG_UP_EVENTS: PollFlags = PollFlags::POLLHUP
    .union(PollFlags::POLLERR)
    .union(PollFlags::POLLNVAL);

/// Rounds up so a sub-millisecond remainder never becomes a 0 ms busy poll.
fn poll_timeout(timeout: Duration) -> PollTimeout {
    let millis = timeout.as_micros().div_ceil(1000);
    PollTimeout::from(u16::try_from(millis).unwrap_or(u16::MAX))
}

/// Hang-up-aware input readiness over a duplicate of the terminal
/// descriptor; reading stays with the input library.
#[derive(Debug)]
pub struct TtyInputGate {
    fd: OwnedFd,
}

impl TtyInputGate {
    /// Gate over the device the input library reads: standard input when it
    /// is a terminal, otherwise the controlling terminal (`/dev/tty`).
    pub fn controlling_terminal() -> io::Result<Self> {
        let stdin = io::stdin();
        if nix::unistd::isatty(stdin.as_fd()).unwrap_or(false) {
            return Self::from_borrowed(stdin.as_fd());
        }
        let fd = nix::fcntl::open(
            "/dev/tty",
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOCTTY,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        Ok(Self { fd })
    }

    /// Gate over any readable descriptor (a PTY, a pipe); the duplicate is
    /// close-on-exec.
    pub fn from_borrowed(fd: impl AsFd) -> io::Result<Self> {
        Ok(Self {
            fd: fd.as_fd().try_clone_to_owned()?,
        })
    }
}

impl TtyInputGate {
    /// Polls for `interest`; `None` on timeout or an interrupting signal.
    fn poll_events(&self, interest: PollFlags, timeout: Duration) -> io::Result<Option<PollFlags>> {
        let mut descriptors = [PollFd::new(self.fd.as_fd(), interest)];
        match poll(&mut descriptors, poll_timeout(timeout)) {
            Ok(0) | Err(Errno::EINTR) => Ok(None),
            Ok(_) => Ok(Some(descriptors[0].revents().unwrap_or(PollFlags::empty()))),
            Err(errno) => Err(io::Error::from(errno)),
        }
    }
}

impl InputGate for TtyInputGate {
    fn wait(&mut self, timeout: Duration) -> io::Result<InputReadiness> {
        Ok(self
            .poll_events(PollFlags::POLLIN, timeout)?
            .map_or(InputReadiness::TimedOut, input_readiness))
    }

    fn wait_for_hang_up(&mut self, timeout: Duration) -> io::Result<bool> {
        // poll(2) reports the hang-up bits even when no event is requested.
        Ok(self
            .poll_events(PollFlags::empty(), timeout)?
            .is_some_and(|events| events.intersects(HANG_UP_EVENTS)))
    }
}

/// A hung-up terminal also reports POLLIN (its reads return end-of-file),
/// so the hang-up bits must win; unread input of a hung-up peer is dropped.
fn input_readiness(events: PollFlags) -> InputReadiness {
    if events.intersects(HANG_UP_EVENTS) {
        InputReadiness::HungUp
    } else if events.contains(PollFlags::POLLIN) {
        InputReadiness::Readable
    } else {
        InputReadiness::TimedOut
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn pipe() -> (OwnedFd, OwnedFd) {
        crate::pipe::pipe2(OFlag::O_CLOEXEC).unwrap()
    }

    fn flags_of(fd: &OwnedFd) -> OFlag {
        current_flags(fd.as_fd()).unwrap()
    }

    #[test]
    fn full_pipe_returns_zero_instead_of_blocking() {
        let (reader, writer) = pipe();
        assert!(!flags_of(&writer).contains(OFlag::O_NONBLOCK));
        let mut terminal = NonBlockingTerminal::from_borrowed(&writer).unwrap();
        assert!(flags_of(&writer).contains(OFlag::O_NONBLOCK));
        let chunk = vec![b'x'; 8192];
        let begin = Instant::now();
        let mut total = 0usize;
        let mut zero_seen = false;
        for _ in 0..10_000 {
            let written = terminal.write_nonblocking(&chunk).unwrap();
            if written == 0 {
                zero_seen = true;
                break;
            }
            total += written;
        }
        assert!(zero_seen, "writes never reported a full pipe");
        assert!(total > 0);
        assert!(begin.elapsed() < Duration::from_secs(2));
        // Still full: waiting is bounded by the timeout.
        let wait_begin = Instant::now();
        assert!(!terminal.wait_writable(Duration::from_millis(30)).unwrap());
        assert!(wait_begin.elapsed() >= Duration::from_millis(25));
        assert!(wait_begin.elapsed() < Duration::from_secs(1));
        // Draining the reader makes the terminal writable again.
        let mut sink = vec![0u8; 1 << 20];
        let _ = nix::unistd::read(&reader, &mut sink).unwrap();
        assert!(terminal.wait_writable(Duration::from_millis(500)).unwrap());
        assert!(terminal.write_nonblocking(b"more").unwrap() > 0);
    }

    #[test]
    fn flags_are_restored_explicitly_idempotently_and_on_drop() {
        let (_reader, writer) = pipe();
        let before = flags_of(&writer);
        let mut terminal = NonBlockingTerminal::from_borrowed(&writer).unwrap();
        assert_eq!(terminal.original_flags(), before);
        terminal.restore().unwrap();
        assert_eq!(flags_of(&writer), before);
        terminal.restore().unwrap();
        assert_eq!(flags_of(&writer), before);
        drop(terminal);
        assert_eq!(flags_of(&writer), before);

        let dropped = NonBlockingTerminal::from_borrowed(&writer).unwrap();
        assert!(flags_of(&writer).contains(OFlag::O_NONBLOCK));
        drop(dropped);
        assert_eq!(flags_of(&writer), before);
    }

    #[test]
    fn closed_reader_reports_broken_pipe() {
        let (reader, writer) = pipe();
        let mut terminal = NonBlockingTerminal::from_borrowed(&writer).unwrap();
        drop(reader);
        // Rust ignores SIGPIPE, so the write fails with EPIPE.
        let error = terminal.write_nonblocking(b"x").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(terminal.wait_writable(Duration::from_millis(10)).is_err());
    }

    #[test]
    fn duplicates_are_close_on_exec() {
        use nix::fcntl::FdFlag;
        let (reader, writer) = pipe();
        let descriptor_flags =
            |fd: &OwnedFd| FdFlag::from_bits_retain(fcntl(fd.as_fd(), FcntlArg::F_GETFD).unwrap());
        let terminal = NonBlockingTerminal::from_borrowed(&writer).unwrap();
        assert!(descriptor_flags(&terminal.fd).contains(FdFlag::FD_CLOEXEC));
        let gate = TtyInputGate::from_borrowed(&reader).unwrap();
        assert!(descriptor_flags(&gate.fd).contains(FdFlag::FD_CLOEXEC));
    }

    #[test]
    fn poll_timeouts_round_up_and_saturate() {
        assert_eq!(poll_timeout(Duration::ZERO), PollTimeout::ZERO);
        assert_eq!(
            poll_timeout(Duration::from_micros(500)),
            PollTimeout::from(1u16)
        );
        assert_eq!(
            poll_timeout(Duration::from_millis(50)),
            PollTimeout::from(50u16)
        );
        assert_eq!(
            poll_timeout(Duration::from_secs(3_600)),
            PollTimeout::from(u16::MAX)
        );
    }

    #[test]
    fn hang_up_bits_win_over_readable() {
        assert_eq!(
            input_readiness(PollFlags::POLLIN | PollFlags::POLLHUP),
            InputReadiness::HungUp
        );
        assert_eq!(input_readiness(PollFlags::POLLERR), InputReadiness::HungUp);
        assert_eq!(input_readiness(PollFlags::POLLNVAL), InputReadiness::HungUp);
        assert_eq!(input_readiness(PollFlags::POLLIN), InputReadiness::Readable);
        assert_eq!(
            input_readiness(PollFlags::empty()),
            InputReadiness::TimedOut
        );
    }

    #[test]
    fn pending_bytes_make_the_gate_readable_without_consuming_them() {
        let (reader, writer) = pipe();
        let mut gate = TtyInputGate::from_borrowed(&reader).unwrap();
        assert_eq!(nix::unistd::write(&writer, b"q").unwrap(), 1);
        assert_eq!(
            gate.wait(Duration::from_millis(500)).unwrap(),
            InputReadiness::Readable
        );
        // The reader still owns the byte.
        let mut buffer = [0u8; 4];
        assert_eq!(nix::unistd::read(&reader, &mut buffer).unwrap(), 1);
        assert_eq!(buffer[0], b'q');
    }

    #[test]
    fn closed_writer_reports_hang_up() {
        let (reader, writer) = pipe();
        let mut gate = TtyInputGate::from_borrowed(&reader).unwrap();
        drop(writer);
        assert_eq!(
            gate.wait(Duration::from_millis(500)).unwrap(),
            InputReadiness::HungUp
        );
        let begin = Instant::now();
        assert!(gate.wait_for_hang_up(Duration::from_millis(500)).unwrap());
        assert!(begin.elapsed() < Duration::from_millis(400));
    }

    #[test]
    fn hang_up_wait_ignores_pending_input() {
        let (reader, writer) = pipe();
        let mut gate = TtyInputGate::from_borrowed(&reader).unwrap();
        assert_eq!(nix::unistd::write(&writer, b"unread").unwrap(), 6);
        let begin = Instant::now();
        assert!(!gate.wait_for_hang_up(Duration::from_millis(30)).unwrap());
        assert!(begin.elapsed() >= Duration::from_millis(25));
        assert!(begin.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn closing_the_pty_master_hangs_up_the_terminal() {
        use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
        let pty = nix::pty::openpty(None, None).unwrap();
        // The TUI runs in raw mode, where single bytes are readable at once.
        let mut attributes = tcgetattr(&pty.slave).unwrap();
        cfmakeraw(&mut attributes);
        tcsetattr(&pty.slave, SetArg::TCSANOW, &attributes).unwrap();
        let mut gate = TtyInputGate::from_borrowed(&pty.slave).unwrap();
        assert_eq!(
            gate.wait(Duration::from_millis(20)).unwrap(),
            InputReadiness::TimedOut
        );
        assert_eq!(nix::unistd::write(&pty.master, b"k").unwrap(), 1);
        assert_eq!(
            gate.wait(Duration::from_millis(500)).unwrap(),
            InputReadiness::Readable
        );
        drop(pty.master);
        let begin = Instant::now();
        assert_eq!(
            gate.wait(Duration::from_secs(2)).unwrap(),
            InputReadiness::HungUp
        );
        assert!(gate.wait_for_hang_up(Duration::from_secs(2)).unwrap());
        assert!(begin.elapsed() < Duration::from_secs(1));
    }
}
