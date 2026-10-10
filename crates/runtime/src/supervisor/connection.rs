//! Parent-side pipes of one live host incarnation.
//!
//! All descriptors are non-blocking. Reads never cross a frame boundary
//! (`FrameDecoder::wanted`), so pausing a pipe for lack of ingress credit
//! needs no extra buffering; writes keep unsent bytes in a small outgoing
//! buffer flushed on `POLLOUT`.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use nix::errno::Errno;

use crate::ipc::{FrameDecoder, IpcError, Step};

use super::launch::{ChildControl, LaunchedHost};
use super::stderr_drain::StderrDrain;

/// Result of one read attempt on the reply pipe.
pub(crate) enum ReadOutcome {
    Step(Step),
    WouldBlock,
    Eof,
    Io(String),
    Protocol(IpcError),
}

/// Result of one read attempt on the diagnostics pipe.
pub(crate) enum DiagnosticsRead {
    Bytes(usize),
    WouldBlock,
    Eof,
}

pub(crate) struct Connection {
    control: Box<dyn ChildControl>,
    pid: Option<i32>,
    nonce: String,
    requests: Option<OwnedFd>,
    outgoing: Vec<u8>,
    close_after_flush: bool,
    replies: Option<OwnedFd>,
    decoder: FrameDecoder,
    diagnostics: Option<OwnedFd>,
    drain: StderrDrain,
    paused_for_credit: bool,
}

impl Connection {
    pub(crate) fn new(
        launched: LaunchedHost,
        nonce: String,
        decoder: FrameDecoder,
        drain: StderrDrain,
    ) -> Self {
        let pid = launched.control.pid();
        Self {
            control: launched.control,
            pid,
            nonce,
            requests: Some(launched.requests),
            outgoing: Vec::new(),
            close_after_flush: false,
            replies: Some(launched.replies),
            decoder,
            diagnostics: launched.diagnostics,
            drain,
            paused_for_credit: false,
        }
    }

    pub(crate) const fn pid(&self) -> Option<i32> {
        self.pid
    }

    pub(crate) fn nonce(&self) -> &str {
        &self.nonce
    }

    pub(crate) fn control(&mut self) -> &mut dyn ChildControl {
        self.control.as_mut()
    }

    /// Closes every pipe and hands back the child control (quarantine).
    pub(crate) fn into_control(self) -> Box<dyn ChildControl> {
        self.control
    }

    pub(crate) fn decoder(&self) -> &FrameDecoder {
        &self.decoder
    }

    pub(crate) fn decoder_mut(&mut self) -> &mut FrameDecoder {
        &mut self.decoder
    }

    pub(crate) const fn is_paused(&self) -> bool {
        self.paused_for_credit
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        self.paused_for_credit = paused;
    }

    pub(crate) fn queue_frame(&mut self, frame: Vec<u8>) {
        self.outgoing.extend_from_slice(&frame);
    }

    /// Closes the request pipe once everything queued is written; the host
    /// then sees EOF even if it misses the shutdown frame.
    pub(crate) fn close_requests_after_flush(&mut self) {
        self.close_after_flush = true;
        if self.outgoing.is_empty() {
            self.requests = None;
        }
    }

    /// Writes as much of the outgoing buffer as the pipe accepts.
    pub(crate) fn flush_requests(&mut self) -> Result<(), Errno> {
        let Some(fd) = self.requests.as_ref() else {
            self.outgoing.clear();
            return Ok(());
        };
        while !self.outgoing.is_empty() {
            match nix::unistd::write(fd, &self.outgoing) {
                Ok(written) => {
                    self.outgoing.drain(..written);
                }
                Err(Errno::EAGAIN) => return Ok(()),
                Err(Errno::EINTR) => {}
                Err(errno) => {
                    self.outgoing.clear();
                    self.requests = None;
                    return Err(errno);
                }
            }
        }
        if self.close_after_flush {
            self.requests = None;
        }
        Ok(())
    }

    pub(crate) fn wants_write(&self) -> bool {
        self.requests.is_some() && !self.outgoing.is_empty()
    }

    pub(crate) fn wants_read(&self) -> bool {
        self.replies.is_some() && !self.paused_for_credit && self.decoder.wanted() > 0
    }

    pub(crate) fn replies_fd(&self) -> Option<BorrowedFd<'_>> {
        self.replies.as_ref().map(AsFd::as_fd)
    }

    pub(crate) fn requests_fd(&self) -> Option<BorrowedFd<'_>> {
        self.requests.as_ref().map(AsFd::as_fd)
    }

    pub(crate) fn diagnostics_fd(&self) -> Option<BorrowedFd<'_>> {
        self.diagnostics.as_ref().map(AsFd::as_fd)
    }

    pub(crate) fn close_replies(&mut self) {
        self.replies = None;
    }

    /// One read of at most what the decoder wants, fed to the decoder.
    pub(crate) fn read_step(&mut self, scratch: &mut [u8]) -> ReadOutcome {
        let Some(fd) = self.replies.as_ref() else {
            return ReadOutcome::Eof;
        };
        let wanted = self.decoder.wanted().min(scratch.len());
        if wanted == 0 {
            return ReadOutcome::WouldBlock;
        }
        match nix::unistd::read(fd, &mut scratch[..wanted]) {
            Ok(0) => ReadOutcome::Eof,
            // `read` returned at most `wanted` bytes, which one push absorbs.
            Ok(read) => match self.decoder.push(&scratch[..read]) {
                Ok(step) => ReadOutcome::Step(step),
                Err(error) => ReadOutcome::Protocol(error),
            },
            Err(Errno::EAGAIN | Errno::EINTR) => ReadOutcome::WouldBlock,
            Err(errno) => ReadOutcome::Io(errno.to_string()),
        }
    }

    pub(crate) fn read_diagnostics(&mut self, scratch: &mut [u8]) -> DiagnosticsRead {
        let Some(fd) = self.diagnostics.as_ref() else {
            return DiagnosticsRead::Eof;
        };
        match nix::unistd::read(fd, scratch) {
            Ok(0) | Err(Errno::EBADF) => {
                self.diagnostics = None;
                DiagnosticsRead::Eof
            }
            Ok(read) => DiagnosticsRead::Bytes(read),
            Err(Errno::EAGAIN | Errno::EINTR) => DiagnosticsRead::WouldBlock,
            Err(_) => {
                self.diagnostics = None;
                DiagnosticsRead::Eof
            }
        }
    }

    pub(crate) fn drain(&mut self) -> &mut StderrDrain {
        &mut self.drain
    }
}
