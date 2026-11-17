//! Termination flag (AT39).
//!
//! SIGTERM, SIGINT and SIGHUP only store `true` into an atomic; all real
//! shutdown work happens on normal threads that poll the flag. The handler
//! is signal-hook's flag handler, which crossterm links already: it stores
//! into the atomic and does nothing else, the only way a signal handler
//! stays async-signal-safe, and dgxtop needs no unsafe code of its own.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

use dgxtop_core::errors::InfraError;
use nix::sys::signal::Signal;

/// Signals that request a graceful shutdown.
pub const TERMINATION_SIGNALS: [Signal; 3] = [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP];

/// Installation is serialized and idempotent.
static INSTALLED: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// Installs the handlers once and returns the shared flag; later calls return
/// the same flag.
///
/// # Errors
///
/// Returns an `InfraError` when the system refuses to install a handler.
pub fn install_termination_flag() -> Result<Arc<AtomicBool>, InfraError> {
    let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(flag) = installed.as_ref() {
        return Ok(Arc::clone(flag));
    }
    let flag = Arc::new(AtomicBool::new(false));
    for signal in TERMINATION_SIGNALS {
        // The registry keeps its own reference for the rest of the process
        // lifetime, so the handler never sees a dangling flag; it replaces
        // the default disposition of the signal, which is the intent.
        signal_hook::flag::register(signal as i32, Arc::clone(&flag)).map_err(|error| {
            InfraError::Unavailable(format!("sigaction({signal}) failed: {error}"))
        })?;
    }
    *installed = Some(Arc::clone(&flag));
    drop(installed);
    Ok(flag)
}

// The end-to-end test (a child process receiving SIGTERM from its parent)
// lives in `tests/supervisor_hosts.rs`, so the unit-test binary keeps the
// default dispositions for Ctrl-C.
