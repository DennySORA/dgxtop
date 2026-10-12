//! Fail-stop for unrecoverable invariant failures of the Manager and the hot
//! store (AT39). A panic on either thread is
//! caught at the thread boundary: process actions stop sending, shutdown is
//! requested so the frontend restores the terminal, and the process exits
//! with a failure status. Nothing keeps sending signals with unknown state.
//! Driver aborts, aborting UI modules and host OOM are outside what
//! `catch_unwind` can guarantee.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use dgxtop_core::ports::{LogLevel, Logger};
use dgxtop_core::text::sanitize_message;

/// Requests the process-wide shutdown (the Manager's shutdown trigger).
type ShutdownHook = Box<dyn Fn(&str) + Send + Sync>;

/// Shared fail-stop state; cheap to clone into worker threads.
#[derive(Clone)]
pub struct FailStop {
    inner: Arc<Inner>,
}

struct Inner {
    reason: Mutex<Option<String>>,
    shutdown: OnceLock<ShutdownHook>,
    logger: Arc<dyn Logger>,
}

impl FailStop {
    pub fn new(logger: Arc<dyn Logger>) -> Self {
        Self {
            inner: Arc::new(Inner {
                reason: Mutex::new(None),
                shutdown: OnceLock::new(),
                logger,
            }),
        }
    }

    /// Connects the shutdown request once the Manager exists; a fail-stop
    /// that happened before (e.g. in the state) requests it at once.
    pub fn connect(&self, request_shutdown: impl Fn(&str) + Send + Sync + 'static) {
        // The first connection wins; later ones are ignored.
        let _ = self.inner.shutdown.set(Box::new(request_shutdown));
        if let Some(reason) = self.reason() {
            self.request_shutdown(&reason);
        }
    }

    /// Records the first unrecoverable failure, halts process actions and
    /// requests shutdown.
    pub fn halt(&self, component: &str, detail: &str) {
        let reason = sanitize_message(&format!("{component}: {detail}"));
        let first = {
            let mut stored = self
                .inner
                .reason
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let first = stored.is_none();
            if first {
                *stored = Some(reason.clone());
            }
            first
        };
        if first {
            self.inner.logger.log(
                LogLevel::Error,
                "fail_stop",
                &format!("fail-stop: {reason}; process actions halted"),
            );
        }
        self.request_shutdown(&reason);
    }

    /// The first fail-stop reason, if any.
    pub fn reason(&self) -> Option<String> {
        self.inner
            .reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn is_halted(&self) -> bool {
        self.reason().is_some()
    }

    /// Runs `work`; a panic is turned into a fail-stop and yields `None`.
    pub fn guard<T>(&self, component: &str, work: impl FnOnce() -> T) -> Option<T> {
        match catch_unwind(AssertUnwindSafe(work)) {
            Ok(value) => Some(value),
            Err(payload) => {
                self.halt(component, &panic_text(payload.as_ref()));
                None
            }
        }
    }

    fn request_shutdown(&self, reason: &str) {
        if let Some(request) = self.inner.shutdown.get() {
            request(&format!("fail-stop: {reason}"));
        }
    }
}

fn panic_text(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic with a non-text payload".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::MemoryLogger;

    fn fail_stop() -> (FailStop, Arc<Mutex<Vec<String>>>) {
        let fail_stop = FailStop::new(Arc::new(MemoryLogger::default()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        (fail_stop, requests)
    }

    fn recorder(requests: &Arc<Mutex<Vec<String>>>) -> impl Fn(&str) + Send + Sync + 'static {
        let requests = Arc::clone(requests);
        move |reason: &str| requests.lock().unwrap().push(reason.to_owned())
    }

    #[test]
    fn a_healthy_run_returns_its_value_and_halts_nothing() {
        let (fail_stop, requests) = fail_stop();
        fail_stop.connect(recorder(&requests));
        assert_eq!(fail_stop.guard("manager", || 7), Some(7));
        assert!(!fail_stop.is_halted());
        assert!(requests.lock().unwrap().is_empty());
    }

    #[test]
    fn a_panic_halts_requests_shutdown_and_keeps_the_first_reason() {
        let (fail_stop, requests) = fail_stop();
        fail_stop.connect(recorder(&requests));
        let outcome = fail_stop.guard("state", || -> u8 { panic!("ledger underflow") });
        assert_eq!(outcome, None);
        assert_eq!(
            fail_stop.reason().as_deref(),
            Some("state: ledger underflow")
        );
        fail_stop.halt("manager", "later failure");
        assert_eq!(
            fail_stop.reason().as_deref(),
            Some("state: ledger underflow")
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0], "fail-stop: state: ledger underflow");
    }

    #[test]
    fn a_fail_stop_before_the_connection_requests_shutdown_when_connected() {
        let (fail_stop, requests) = fail_stop();
        let message = String::from("bad state");
        let _ = fail_stop.guard("state", move || -> () { panic!("{message}") });
        assert!(requests.lock().unwrap().is_empty());
        fail_stop.connect(recorder(&requests));
        assert_eq!(
            requests.lock().unwrap().as_slice(),
            ["fail-stop: state: bad state"]
        );
    }

    #[test]
    fn reasons_are_sanitized() {
        let (fail_stop, _) = fail_stop();
        fail_stop.halt("manager", "bad\u{1b}[2J state");
        assert_eq!(fail_stop.reason().as_deref(), Some("manager: bad state"));
    }
}
