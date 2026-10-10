//! Logger for adapter-host processes.
//!
//! A host's stderr is a pipe drained (non-blocking, bounded) by the parent
//! supervisor and forwarded to the parent's logger. Lines are sanitized and
//! bounded, and each window allows a fixed number of lines so a failing
//! driver cannot flood the parent; suppressed lines are summarized.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use dgxtop_core::clock::{NANOS_PER_SECOND, TimeProvider};
use dgxtop_core::ports::{LogLevel, Logger};
use dgxtop_core::text::sanitize_message;

use super::format::level_name;

/// Length of one rate-limit window.
pub const STDERR_WINDOW_NS: u64 = 10 * NANOS_PER_SECOND;
/// Lines allowed per window.
pub const STDERR_LINES_PER_WINDOW: u32 = 20;

struct State {
    sink: Box<dyn Write + Send>,
    window_start_boot_ns: u64,
    lines_in_window: u32,
    suppressed_in_window: u64,
}

/// Bounded, rate-limited line logger (stderr by default).
pub struct StderrLogger {
    state: Mutex<State>,
    clock: Arc<dyn TimeProvider>,
    suppressed_total: AtomicU64,
}

impl std::fmt::Debug for StderrLogger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StderrLogger")
            .field("suppressed_total", &self.suppressed_total())
            .finish()
    }
}

impl StderrLogger {
    pub fn new(clock: Arc<dyn TimeProvider>) -> Self {
        Self::with_sink(Box::new(std::io::stderr()), clock)
    }

    pub fn with_sink(sink: Box<dyn Write + Send>, clock: Arc<dyn TimeProvider>) -> Self {
        let now = clock.now_boot_ns();
        Self {
            state: Mutex::new(State {
                sink,
                window_start_boot_ns: now,
                lines_in_window: 0,
                suppressed_in_window: 0,
            }),
            clock,
            suppressed_total: AtomicU64::new(0),
        }
    }

    /// Lines not written because of the rate limit or lock contention.
    pub fn suppressed_total(&self) -> u64 {
        self.suppressed_total.load(Ordering::Relaxed)
    }

    fn suppress(&self) {
        self.suppressed_total.fetch_add(1, Ordering::Relaxed);
    }
}

impl Logger for StderrLogger {
    fn log(&self, level: LogLevel, target: &'static str, message: &str) {
        // Never wait for another logging thread: contention counts as a
        // suppressed line instead of blocking the caller.
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.suppress();
                return;
            }
        };
        let now = self.clock.now_boot_ns();
        if now.saturating_sub(state.window_start_boot_ns) >= STDERR_WINDOW_NS {
            let suppressed = std::mem::take(&mut state.suppressed_in_window);
            if suppressed > 0 {
                let summary =
                    format!("WARN logger: suppressed {suppressed} lines in the last window\n");
                let _ = state.sink.write_all(summary.as_bytes());
            }
            state.window_start_boot_ns = now;
            state.lines_in_window = 0;
        }
        if state.lines_in_window >= STDERR_LINES_PER_WINDOW {
            state.suppressed_in_window = state.suppressed_in_window.saturating_add(1);
            self.suppress();
            return;
        }
        state.lines_in_window += 1;
        let line = format!(
            "{} {target}: {}\n",
            level_name(level),
            sanitize_message(message)
        );
        // Best effort: a closed stderr must not stop the host.
        let _ = state.sink.write_all(line.as_bytes());
        let _ = state.sink.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::FakeClock;

    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedSink {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn lines_are_sanitized_bounded_and_rate_limited() {
        let sink = SharedSink::default();
        let clock = FakeClock::new(0);
        let logger = StderrLogger::with_sink(Box::new(sink.clone()), clock.clone());
        logger.log(LogLevel::Warn, "nvml", "evil\u{1b}]0;x\u{7}text");
        logger.log(LogLevel::Error, "nvml", &"y".repeat(10_000));
        for index in 0..30 {
            logger.log(LogLevel::Info, "nvml", &format!("line {index}"));
        }
        let text = sink.text();
        assert!(text.starts_with("WARN nvml: eviltext\n"));
        assert_eq!(
            text.lines().count(),
            usize::try_from(STDERR_LINES_PER_WINDOW).unwrap()
        );
        assert!(text.lines().all(|line| line.len() <= 1100));
        assert_eq!(logger.suppressed_total(), 12);
        clock.advance_ms(10_001);
        logger.log(LogLevel::Info, "nvml", "after window");
        let text = sink.text();
        assert!(text.contains("WARN logger: suppressed 12 lines in the last window\n"));
        assert!(text.ends_with("INFO nvml: after window\n"));
    }
}
