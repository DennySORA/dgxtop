//! Async-signal-safe termination flag (`RUNTIME.md` §8, AT39).
//!
//! SIGTERM, SIGINT and SIGHUP only store `true` into an atomic; all real
//! shutdown work happens on normal threads that poll the flag. Nothing in the
//! handler allocates, locks or performs I/O, which is the only way a signal
//! handler stays async-signal-safe.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use dgxtop_core::errors::InfraError;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

/// Signals that request a graceful shutdown.
pub const TERMINATION_SIGNALS: [Signal; 3] = [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP];

/// Flag the handler writes to. Points into an `Arc` that is intentionally
/// never released once installed, so the handler can never see a dangling
/// pointer.
static TERMINATION_TARGET: AtomicPtr<AtomicBool> = AtomicPtr::new(ptr::null_mut());
/// Installation is serialized and idempotent.
static INSTALLED: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// Installs the handlers once and returns the shared flag; later calls return
/// the same flag.
pub fn install_termination_flag() -> Result<Arc<AtomicBool>, InfraError> {
    let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(flag) = installed.as_ref() {
        return Ok(Arc::clone(flag));
    }
    let flag = Arc::new(AtomicBool::new(false));
    // One strong reference is leaked on purpose: the handler may run at any
    // time for the rest of the process lifetime.
    let target = Arc::into_raw(Arc::clone(&flag)).cast_mut();
    TERMINATION_TARGET.store(target, Ordering::Release);
    install_handlers()?;
    *installed = Some(Arc::clone(&flag));
    Ok(flag)
}

extern "C" fn on_termination_signal(_signal: libc::c_int) {
    let target = TERMINATION_TARGET.load(Ordering::Acquire);
    if !target.is_null() {
        store_termination(target);
    }
}

#[allow(unsafe_code)]
fn store_termination(target: *mut AtomicBool) {
    // SAFETY: `target` comes from `Arc::into_raw` of an Arc whose strong
    // count is never decremented again, so the AtomicBool outlives every
    // handler invocation; an atomic store is async-signal-safe.
    unsafe { (*target).store(true, Ordering::SeqCst) };
}

#[allow(unsafe_code)]
fn install_handlers() -> Result<(), InfraError> {
    let action = SigAction::new(
        SigHandler::Handler(on_termination_signal),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for signal in TERMINATION_SIGNALS {
        // SAFETY: the handler only performs an atomic load and an atomic
        // store (async-signal-safe) and replaces the default disposition of
        // termination signals for the whole process, which is the intent.
        unsafe { sigaction(signal, &action) }.map_err(|errno| {
            InfraError::Unavailable(format!("sigaction({signal}) failed: {errno}"))
        })?;
    }
    Ok(())
}

// The end-to-end test (a child process receiving SIGTERM from its parent)
// lives in `tests/supervisor_hosts.rs`, so the unit-test binary keeps the
// default dispositions for Ctrl-C.
