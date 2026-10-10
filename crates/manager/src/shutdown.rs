//! Out-of-band shutdown request and the bounded shutdown summary
//! (RUNTIME.md §4/§8).
//!
//! The request is an atomic flag plus a one-slot wake channel: it never
//! queues behind normal commands, so a command flood cannot starve it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crossbeam_channel::Sender;
use dgxtop_core::event::Event;
use dgxtop_core::text::sanitize_display;

/// Longest stored shutdown reason (matches the command schema limit).
const MAX_REASON_BYTES: usize = 256;

/// Shared shutdown flag; the first recorded reason wins.
#[derive(Debug)]
pub(crate) struct ShutdownSignal {
    requested: AtomicBool,
    reason: Mutex<Option<String>>,
    wake: Sender<()>,
}

impl ShutdownSignal {
    pub(crate) fn new(wake: Sender<()>) -> Self {
        Self {
            requested: AtomicBool::new(false),
            reason: Mutex::new(None),
            wake,
        }
    }

    pub(crate) fn request(&self, reason: &str) {
        {
            let mut stored = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
            if stored.is_none() {
                *stored = Some(sanitize_display(reason, MAX_REASON_BYTES));
            }
        }
        self.requested.store(true, Ordering::Release);
        self.wake();
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    pub(crate) fn reason(&self) -> String {
        self.reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| "shutdown requested".to_owned())
    }

    /// Wakes the control loop; a full slot already guarantees a wake-up.
    pub(crate) fn wake(&self) {
        let _ = self.wake.try_send(());
    }
}

/// Clonable shutdown handle for signal-flag watchers and frontends.
#[derive(Debug, Clone)]
pub struct ShutdownTrigger {
    signal: Arc<ShutdownSignal>,
}

impl ShutdownTrigger {
    pub(crate) fn new(signal: Arc<ShutdownSignal>) -> Self {
        Self { signal }
    }

    /// Requests shutdown without blocking; safe to call repeatedly.
    pub fn trigger(&self, reason: &str) {
        self.signal.request(reason);
    }

    pub fn is_triggered(&self) -> bool {
        self.signal.is_requested()
    }
}

/// Result of [`crate::Manager::run`]: what the bounded shutdown finished and
/// what it had to leave behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownSummary {
    pub reason: String,
    pub started_boot_ns: u64,
    pub finished_boot_ns: u64,
    /// The bounded wait ended before every control completion arrived.
    pub timed_out: bool,
    /// Work that did not finish (pending config persist, audit intents,
    /// actions whose outcome is unknown, ...). Empty on a clean shutdown.
    pub unfinished: Vec<String>,
    /// Confirmation tokens revoked because they had not reached `sending`.
    pub revoked_tokens: u32,
    /// Accepted commands answered with `Unavailable` during shutdown.
    pub rejected_commands: u32,
    /// Newest events of the bounded event log.
    pub recent_events: Vec<Event>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_reason_wins_and_wakes() {
        let (wake, woken) = crossbeam_channel::bounded(1);
        let signal = Arc::new(ShutdownSignal::new(wake));
        let trigger = ShutdownTrigger::new(Arc::clone(&signal));
        assert!(!trigger.is_triggered());
        trigger.trigger("signal\u{1b}[2J SIGTERM");
        trigger.trigger("second");
        assert!(trigger.is_triggered());
        assert_eq!(signal.reason(), "signal SIGTERM");
        assert!(woken.try_recv().is_ok());
        // The wake slot holds one token; repeated requests never block.
        signal.wake();
        signal.wake();
        assert!(woken.try_recv().is_ok());
        assert!(woken.try_recv().is_err());
    }
}
