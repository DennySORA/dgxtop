//! How a run ended (D-76), from what dgxtop can see without root: its own
//! signals, a zombie's wait status, and the kernel's OOM-kill count. Who
//! else signalled a process is not visible to an unprivileged reader; a
//! wait status says only which signal it was.

use dgxtop_core::action::Signal;
use dgxtop_core::process_history::EndCause;
use dgxtop_core::process_probe::ExitStatus;

/// A signal dgxtop sent counts as the cause of an end this soon after.
pub(super) const SIGNAL_WINDOW_NS: u64 = 60 * 1_000_000_000;

const SIGKILL: u8 = 9;

/// What was known when a run was found ended.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct EndEvidence {
    /// The signal dgxtop sent it within [`SIGNAL_WINDOW_NS`].
    pub(super) signal: Option<Signal>,
    /// Its wait status, read while it was a zombie.
    pub(super) status: Option<ExitStatus>,
    /// The kernel's OOM-kill count rose since the previous probe.
    pub(super) oom_rose: bool,
    /// It was restored at startup and never listed since.
    pub(super) while_away: bool,
}

/// dgxtop's own signal first (it is why the process ended, whatever its
/// status), then the OOM killer where its count rose as the run ended with
/// a SIGKILL or no status, then the status, else not visible.
pub(super) fn end_cause(evidence: &EndEvidence) -> EndCause {
    if evidence.while_away {
        return EndCause::WhileAway;
    }
    if let Some(signal) = evidence.signal {
        return EndCause::SignaledByDgxtop(signal);
    }
    match evidence.status {
        Some(ExitStatus::Signaled(SIGKILL)) | None if evidence.oom_rose => EndCause::OutOfMemory,
        Some(ExitStatus::Signaled(signal)) => EndCause::Signal(signal),
        Some(ExitStatus::Exited(code)) => EndCause::Exit(code),
        None => EndCause::NotVisible,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cause(signal: Option<Signal>, status: Option<ExitStatus>, oom_rose: bool) -> EndCause {
        end_cause(&EndEvidence {
            signal,
            status,
            oom_rose,
            while_away: false,
        })
    }

    #[test]
    fn dgxtops_signal_explains_any_end() {
        assert_eq!(
            cause(Some(Signal::Sigterm), Some(ExitStatus::Exited(0)), true),
            EndCause::SignaledByDgxtop(Signal::Sigterm)
        );
    }

    #[test]
    fn a_rising_oom_count_explains_a_sigkill_or_a_silent_end() {
        assert_eq!(
            cause(None, Some(ExitStatus::Signaled(9)), true),
            EndCause::OutOfMemory
        );
        assert_eq!(cause(None, None, true), EndCause::OutOfMemory);
        assert_eq!(
            cause(None, Some(ExitStatus::Exited(1)), true),
            EndCause::Exit(1),
            "an exit code is not the OOM killer"
        );
        assert_eq!(
            cause(None, Some(ExitStatus::Signaled(9)), false),
            EndCause::Signal(9),
            "a SIGKILL from someone else"
        );
    }

    #[test]
    fn without_evidence_the_cause_is_not_visible() {
        assert_eq!(cause(None, None, false), EndCause::NotVisible);
        assert_eq!(
            end_cause(&EndEvidence {
                signal: Some(Signal::Sigkill),
                while_away: true,
                ..EndEvidence::default()
            }),
            EndCause::WhileAway
        );
    }
}
