//! pidfd process action adapter (`ProcessActionPort`).
//!
//! The only code that can signal a user process. `prepare` applies the
//! fail-closed policy gate, opens a pidfd (flags 0) and re-verifies start
//! time, uids, PID namespace and liveness against `/proc` while holding it;
//! the descriptor stays inside this adapter under an opaque handle (at most
//! eight). `send` re-reads the start time and then makes exactly one
//! `pidfd_send_signal` on the held descriptor: there is no PID-based
//! fallback, no retry and at most one signal per handle. Exit is observed by
//! non-blocking pidfd readiness; `waitpid` is never used (targets are not our
//! children).

mod pidfd;
mod policy;
mod proc_files;
mod verify;

use std::collections::BTreeMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

pub use pidfd::{LinuxPidfd, PidfdReadiness, PidfdSyscalls};
pub use policy::check_policy;
pub use proc_files::{
    HostProcessFiles, PidfdTarget, ProcessFiles, ProcessUids, StatFacts, parse_pidfd_target,
    parse_stat, parse_uids,
};

use dgxtop_core::action::{
    ActionError, ActionHandle, CapabilityContext, DurableReceipt, ExitObservation, PreparedProcess,
    Signal, SignalOutcome,
};
use dgxtop_core::clock::{TimeProvider, ms_to_ns};
use dgxtop_core::identity::ProcessKey;
use dgxtop_core::ports::ProcessActionPort;
use dgxtop_core::text::sanitize_display;
use nix::errno::Errno;

/// Descriptors held at once (spec: at most 8 pending confirmations).
pub const MAX_HELD_HANDLES: usize = 8;
/// A handle older than this is released before new prepares; it covers the
/// longest confirmation TTL (30 s) plus the intent deadline and the 10 s exit
/// observation, so only leaked handles are reclaimed.
pub const MAX_HOLD_MS: u64 = 120_000;

/// One prepared target.
#[derive(Debug)]
struct HeldProcess {
    pidfd: OwnedFd,
    key: ProcessKey,
    signal: Signal,
    prepared_boot_ns: u64,
    signal_sent: bool,
}

/// pidfd-based implementation of [`ProcessActionPort`].
pub struct PidfdActionAdapter {
    clock: Arc<dyn TimeProvider>,
    syscalls: Box<dyn PidfdSyscalls>,
    files: Box<dyn ProcessFiles>,
    held: BTreeMap<ActionHandle, HeldProcess>,
    next_handle: u64,
    own_pid: i32,
}

impl std::fmt::Debug for PidfdActionAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PidfdActionAdapter")
            .field("held", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl PidfdActionAdapter {
    /// Production adapter on the Linux pidfd API and the host `/proc`.
    pub fn new(clock: Arc<dyn TimeProvider>) -> Self {
        Self::with_parts(clock, Box::new(LinuxPidfd), Box::new(HostProcessFiles))
    }

    /// Adapter on injected system access (fault-injection tests).
    pub fn with_parts(
        clock: Arc<dyn TimeProvider>,
        syscalls: Box<dyn PidfdSyscalls>,
        files: Box<dyn ProcessFiles>,
    ) -> Self {
        Self {
            clock,
            syscalls,
            files,
            held: BTreeMap::new(),
            next_handle: 1,
            own_pid: i32::try_from(std::process::id()).unwrap_or(i32::MAX),
        }
    }

    /// Number of descriptors currently held (descriptor-leak tests).
    #[cfg(any(test, feature = "test-support"))]
    pub fn held_count(&self) -> usize {
        self.held.len()
    }

    fn release_expired(&mut self, now_boot_ns: u64) {
        let max_hold_ns = ms_to_ns(MAX_HOLD_MS);
        self.held
            .retain(|_, held| now_boot_ns.saturating_sub(held.prepared_boot_ns) < max_hold_ns);
    }

    fn allocate_handle(&mut self) -> Result<ActionHandle, ActionError> {
        let handle = ActionHandle(self.next_handle);
        self.next_handle = self
            .next_handle
            .checked_add(1)
            .ok_or_else(|| ActionError::Unavailable("action handles exhausted".to_owned()))?;
        Ok(handle)
    }

    fn open_pidfd(&self, pid: i32) -> Result<OwnedFd, ActionError> {
        self.syscalls.open(pid).map_err(|errno| match errno {
            Errno::ENOSYS | Errno::EINVAL => ActionError::PidfdUnsupported,
            Errno::ESRCH => ActionError::Exited,
            other => ActionError::Unavailable(format!("pidfd_open failed: errno {}", other as i32)),
        })
    }

    /// Checks done right before the single send; `Some` means do not send.
    fn refusal_before_send(
        &self,
        held: &HeldProcess,
        signal: Signal,
        receipt: &DurableReceipt,
    ) -> Option<SignalOutcome> {
        if held.signal_sent {
            return Some(SignalOutcome::NotSent {
                reason: "a signal was already sent for this handle".to_owned(),
            });
        }
        if signal != held.signal {
            return Some(SignalOutcome::NotSent {
                reason: "signal differs from the prepared signal".to_owned(),
            });
        }
        if receipt.committed_boot_ns < held.prepared_boot_ns {
            return Some(SignalOutcome::NotSent {
                reason: "audit receipt predates the prepare".to_owned(),
            });
        }
        match verify::read_stat(self.files.as_ref(), held.key.pid.get()) {
            Ok(stat) if stat.start_time_ticks == held.key.start_time_ticks => None,
            Ok(_) | Err(ActionError::IdentityChanged) => Some(SignalOutcome::Changed),
            Err(ActionError::Exited) => Some(SignalOutcome::Exited),
            Err(error) => Some(SignalOutcome::Unavailable {
                reason: error.to_string(),
            }),
        }
    }
}

fn display_summary(signal: Signal, basename: &str, key: &ProcessKey) -> String {
    sanitize_display(
        &format!("{} → {basename} ({})", signal.name(), key.summary()),
        dgxtop_core::text::MAX_MESSAGE_BYTES,
    )
}

fn outcome_of_send(result: Result<(), Errno>) -> SignalOutcome {
    match result {
        Ok(()) => SignalOutcome::Sent,
        Err(Errno::ESRCH) => SignalOutcome::Exited,
        Err(Errno::EPERM) => SignalOutcome::PermissionDenied,
        Err(errno) => SignalOutcome::Error {
            errno: errno as i32,
        },
    }
}

impl ProcessActionPort for PidfdActionAdapter {
    fn prepare(
        &mut self,
        key: &ProcessKey,
        signal: Signal,
        context: &CapabilityContext,
    ) -> Result<PreparedProcess, ActionError> {
        check_policy(key, signal, context, self.own_pid)?;
        let now = self.clock.now_boot_ns();
        self.release_expired(now);
        if self.held.len() >= MAX_HELD_HANDLES {
            return Err(ActionError::Unavailable(
                "too many prepared process actions".to_owned(),
            ));
        }
        let pidfd = self.open_pidfd(key.pid.get())?;
        // On any verification error `pidfd` is dropped (closed) here.
        let verified = verify::verify_identity(
            self.files.as_ref(),
            self.syscalls.as_ref(),
            &pidfd,
            key,
            context,
        )?;
        let handle = self.allocate_handle()?;
        let display_summary = display_summary(signal, &verified.basename, key);
        self.held.insert(
            handle,
            HeldProcess {
                pidfd,
                key: key.clone(),
                signal,
                prepared_boot_ns: now,
                signal_sent: false,
            },
        );
        Ok(PreparedProcess {
            handle,
            verified,
            display_summary,
        })
    }

    fn send(
        &mut self,
        handle: ActionHandle,
        signal: Signal,
        receipt: &DurableReceipt,
    ) -> SignalOutcome {
        let Some(held) = self.held.get(&handle) else {
            return SignalOutcome::Unavailable {
                reason: ActionError::UnknownHandle.to_string(),
            };
        };
        if let Some(refusal) = self.refusal_before_send(held, signal, receipt) {
            return refusal;
        }
        let result = self
            .syscalls
            .send_signal(held.pidfd.as_fd(), signal.number());
        if let Some(held) = self.held.get_mut(&handle) {
            // Whatever the kernel answered, this handle never signals again.
            held.signal_sent = true;
        }
        outcome_of_send(result)
    }

    fn observe_exit(&mut self, handle: ActionHandle) -> ExitObservation {
        let Some(held) = self.held.get(&handle) else {
            return ExitObservation::Unknown;
        };
        match self.syscalls.readiness(held.pidfd.as_fd()) {
            Ok(PidfdReadiness::Exited) => ExitObservation::ExitObserved,
            Ok(PidfdReadiness::Running) => ExitObservation::Running,
            Err(_) => ExitObservation::Unknown,
        }
    }

    fn release(&mut self, handle: ActionHandle) {
        self.held.remove(&handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_errnos_are_classified() {
        assert_eq!(outcome_of_send(Ok(())), SignalOutcome::Sent);
        assert_eq!(outcome_of_send(Err(Errno::ESRCH)), SignalOutcome::Exited);
        assert_eq!(
            outcome_of_send(Err(Errno::EPERM)),
            SignalOutcome::PermissionDenied
        );
        assert_eq!(
            outcome_of_send(Err(Errno::EINVAL)),
            SignalOutcome::Error {
                errno: libc::EINVAL
            }
        );
    }

    #[test]
    fn summary_is_sanitized() {
        let key = ProcessKey {
            host_id: dgxtop_core::testing::host_id("h"),
            boot_id: dgxtop_core::testing::boot_id("b"),
            pid_namespace: dgxtop_core::identity::PidNamespace::new("pid:[1]").unwrap(),
            pid: dgxtop_core::identity::Pid::new(42).unwrap(),
            start_time_ticks: 7,
        };
        let summary = display_summary(Signal::Sigterm, "evil\u{1b}[2J", &key);
        assert!(!summary.contains('\u{1b}'));
        assert!(summary.starts_with("SIGTERM"));
        assert!(summary.contains("pid 42"));
    }
}
