//! Shutdown reporting.

use std::time::Duration;

use dgxtop_core::identity::AdapterId;

use super::launch::ExitSummary;

/// Stage of the stop sequence in which a host was reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopStage {
    /// After the protocol shutdown request.
    Protocol,
    /// After SIGTERM.
    Terminate,
    /// After SIGKILL.
    Kill,
}

impl StopStage {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::Terminate => "sigterm",
            Self::Kill => "sigkill",
        }
    }
}

/// Final state of one host after a supervisor shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// No incarnation was running (circuit open, never started ...).
    NotRunning,
    Reaped {
        stage: StopStage,
        exit: ExitSummary,
    },
    /// Still unreaped: quarantined, or the deadline passed mid-sequence.
    Unreaped {
        pid: Option<i32>,
    },
}

/// Per-host shutdown result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostShutdown {
    pub adapter_id: AdapterId,
    pub generation: u32,
    pub outcome: StopOutcome,
}

/// Result of [`super::SupervisorHandle::shutdown`]; never claims a reap that
/// did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    pub elapsed: Duration,
    /// The supervisor thread finished its sequence before the deadline.
    pub completed_within_deadline: bool,
    pub hosts: Vec<HostShutdown>,
}

impl ShutdownReport {
    /// Hosts that may still be running or are zombies.
    pub fn unreaped(&self) -> Vec<(AdapterId, Option<i32>)> {
        self.hosts
            .iter()
            .filter_map(|host| match host.outcome {
                StopOutcome::Unreaped { pid } => Some((host.adapter_id.clone(), pid)),
                _ => None,
            })
            .collect()
    }
}
