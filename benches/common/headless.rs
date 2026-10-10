//! Headless frontend pieces for driving the real terminal runner
//! (`dgxtop_ui::TuiLoop`) without a terminal: a null transport (frame
//! bytes are counted and discarded — "terminal transport excluded"), an
//! input gate that never reports input, a manually advanced loop clock, and
//! a private pseudo-terminal on stdin so crossterm's reader can never see
//! the user's real terminal (or fail on CI where there is none).

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dgxtop_core::ports::{InputGate, InputReadiness, TerminalTransport};
use dgxtop_ui::LoopClock;

use super::BenchResult;

/// Accepts every byte at once and counts it.
pub struct NullTransport {
    written: Arc<AtomicU64>,
}

impl NullTransport {
    pub fn new(written: Arc<AtomicU64>) -> Self {
        Self { written }
    }
}

impl TerminalTransport for NullTransport {
    fn write_nonblocking(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written.fetch_add(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        Ok(bytes.len())
    }

    fn wait_writable(&mut self, _timeout: Duration) -> std::io::Result<bool> {
        Ok(true)
    }

    fn restore(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An idle terminal: never readable, never hung up. With `sleeps`, a wait
/// lasts its timeout like `poll(2)` on a quiet tty and the time spent is
/// accumulated so callers can separate waiting from frame work.
pub struct IdleGate {
    sleeps: bool,
    waited_ns: Arc<AtomicU64>,
}

impl IdleGate {
    pub fn new(sleeps: bool, waited_ns: Arc<AtomicU64>) -> Self {
        Self { sleeps, waited_ns }
    }

    fn idle(&self, timeout: Duration) {
        if self.sleeps && !timeout.is_zero() {
            let started = Instant::now();
            std::thread::sleep(timeout);
            let slept = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.waited_ns.fetch_add(slept, Ordering::Relaxed);
        }
    }
}

impl InputGate for IdleGate {
    fn wait(&mut self, timeout: Duration) -> std::io::Result<InputReadiness> {
        self.idle(timeout);
        Ok(InputReadiness::TimedOut)
    }

    fn wait_for_hang_up(&mut self, timeout: Duration) -> std::io::Result<bool> {
        self.idle(timeout);
        Ok(false)
    }
}

/// Loop clock the bench advances explicitly (frame pacing without sleeping).
pub struct ManualLoopClock {
    now_ns: Arc<AtomicU64>,
}

impl ManualLoopClock {
    pub fn new(now_ns: Arc<AtomicU64>) -> Self {
        Self { now_ns }
    }
}

impl LoopClock for ManualLoopClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.now_ns.load(Ordering::Acquire))
    }
}

/// Keeps the private pseudo-terminal open for the bench's lifetime.
pub struct TerminalIsolation {
    _master: OwnedFd,
    _slave: OwnedFd,
}

/// Points this process's stdin at a fresh pseudo-terminal nobody writes to.
/// crossterm reads stdin when it is a tty (and `/dev/tty` otherwise), so the
/// terminal runner's input reader stays silent and never touches the real
/// terminal; only this bench process is affected.
pub fn isolate_terminal_input() -> BenchResult<TerminalIsolation> {
    let pty = nix::pty::openpty(None, None).map_err(|errno| format!("openpty: {errno}"))?;
    nix::unistd::dup2_stdin(&pty.slave).map_err(|errno| format!("stdin redirect: {errno}"))?;
    Ok(TerminalIsolation {
        _master: pty.master,
        _slave: pty.slave,
    })
}
