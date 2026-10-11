//! Termination signals (SIGTERM, SIGINT, SIGHUP) turned into a shutdown
//! request, and the bounded wait for the terminal frontend after one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dgxtop_core::ports::{LogLevel, Logger, SessionPort};
use dgxtop_manager::{SessionHandle, ShutdownTrigger};
use dgxtop_runtime::logger::FileLogger;
use dgxtop_runtime::signals::install_termination_flag;

use super::StartError;
use crate::modes::EXIT_FAILURE;

/// Longest wait for the terminal frontend after a termination signal before
/// the process exits anyway (AT39: never wait without bound).
const FRONTEND_EXIT_GRACE: Duration = Duration::from_secs(5);
/// Cadence of the signal watcher while nothing happens. The handler may
/// only store into an atomic (async-signal-safe, `dgxtop_runtime::signals`),
/// so there is nothing to block on; 200 ms keeps a signal's shutdown well
/// inside the 3 s budget at five wakes a second.
const SIGNAL_POLL: Duration = Duration::from_millis(200);
/// Cadence of the bounded wait for the frontend once a signal arrived.
const FRONTEND_EXIT_POLL: Duration = Duration::from_millis(50);

/// Turns termination signals into a shutdown request, then bounds the wait
/// for the terminal frontend: the terminal library can spin on a terminal
/// that hangs up in the middle of a read, and nothing else could end the
/// process then.
pub(super) struct SignalWatch {
    pub(super) trigger: ShutdownTrigger,
    pub(super) session: SessionHandle,
    pub(super) frontend_finished: Arc<AtomicBool>,
    pub(super) logger: Arc<FileLogger>,
}

impl SignalWatch {
    pub(super) fn spawn(self) -> Result<(), StartError> {
        let flag = install_termination_flag()?;
        std::thread::Builder::new()
            .name("dgxtop-signals".to_owned())
            .spawn(move || self.run(&flag))?;
        Ok(())
    }

    fn run(self, flag: &AtomicBool) {
        while !flag.load(Ordering::Acquire) {
            if self.trigger.is_triggered() {
                return;
            }
            std::thread::sleep(SIGNAL_POLL);
        }
        self.session.request_shutdown("termination signal");
        self.trigger.trigger("termination signal");
        if wait_for_flag(&self.frontend_finished, FRONTEND_EXIT_GRACE) {
            return;
        }
        self.logger.log(
            LogLevel::Error,
            "composition",
            "terminal frontend still running 5 s after a termination signal; exiting",
        );
        let _ = self.logger.flush(Duration::from_millis(250));
        dgxtop_ui::restore_terminal();
        std::process::exit(i32::from(EXIT_FAILURE));
    }
}

/// Waits until `flag` is set or `timeout` passes; returns whether it was set.
fn wait_for_flag(flag: &AtomicBool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if flag.load(Ordering::Acquire) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(FRONTEND_EXIT_POLL));
    }
}

#[cfg(test)]
mod tests {
    use super::super::SHUTDOWN_BUDGET;
    use super::*;

    /// The signal watcher polls (no handler can wake it), slowly enough to
    /// stay idle and fast enough that a signal's shutdown keeps its budget.
    #[test]
    fn the_signal_poll_is_slow_but_inside_the_shutdown_budget() {
        assert!(SIGNAL_POLL >= Duration::from_millis(200));
        assert!(SIGNAL_POLL * 10 <= SHUTDOWN_BUDGET);
        assert!(FRONTEND_EXIT_POLL < SIGNAL_POLL);
    }

    #[test]
    fn an_unset_flag_gives_up_at_the_deadline() {
        let flag = AtomicBool::new(false);
        let begin = Instant::now();
        assert!(!wait_for_flag(&flag, Duration::from_millis(120)));
        assert!(begin.elapsed() >= Duration::from_millis(110));
        assert!(begin.elapsed() < Duration::from_secs(2));
        assert!(!wait_for_flag(&flag, Duration::ZERO));
    }
}
