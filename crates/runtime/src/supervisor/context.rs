//! Everything the supervisor thread shares across host slots.

use std::sync::Arc;

use dgxtop_core::budget::QueueGate;
use dgxtop_core::clock::{TimeProvider, ms_to_ns};
use dgxtop_core::ports::{IngestionCompletion, LogLevel, Logger};

use crate::ipc::IpcLimits;

use super::config::SupervisorConfig;
use super::launch::HostLauncher;
use super::outbox::Outbox;
use super::report::StopStage;
use super::shared::Shared;

/// Logger target of supervisor messages.
pub(crate) const LOG_TARGET: &str = "supervisor";
/// Logger target of forwarded host stderr lines.
pub(crate) const HOST_LOG_TARGET: &str = "adapter_host";
/// Margin kept before a shutdown deadline for the final report.
const SHUTDOWN_MARGIN_NS: u64 = 10_000_000;

/// Durations of the stop sequence stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StopDurations {
    pub grace_ns: u64,
    pub term_ns: u64,
    pub reap_ns: u64,
}

impl StopDurations {
    pub(crate) const fn for_stage(self, stage: StopStage) -> u64 {
        match stage {
            StopStage::Protocol => self.grace_ns,
            StopStage::Terminate => self.term_ns,
            StopStage::Kill => self.reap_ns,
        }
    }
}

pub(crate) struct LoopContext {
    pub config: SupervisorConfig,
    pub limits: IpcLimits,
    pub shared: Arc<Shared>,
    pub outbox: Outbox,
    pub clock: Arc<dyn TimeProvider>,
    pub logger: Arc<dyn Logger>,
    pub launcher: Arc<dyn HostLauncher>,
    pub ingress: QueueGate,
    pub scratch: Vec<u8>,
    /// Set once a shutdown started; the boot-clock deadline.
    pub shutdown_deadline: Option<u64>,
}

impl LoopContext {
    pub(crate) const fn shutting_down(&self) -> bool {
        self.shutdown_deadline.is_some()
    }

    /// Stage durations; during shutdown they are scaled down proportionally
    /// so the whole sequence fits before the deadline.
    pub(crate) fn stop_durations(&self, now_boot_ns: u64) -> StopDurations {
        let configured = StopDurations {
            grace_ns: ms_to_ns(self.config.child_grace_ms),
            term_ns: ms_to_ns(self.config.child_term_ms),
            reap_ns: ms_to_ns(self.config.child_reap_ms),
        };
        let Some(deadline) = self.shutdown_deadline else {
            return configured;
        };
        let available = deadline
            .saturating_sub(now_boot_ns)
            .saturating_sub(SHUTDOWN_MARGIN_NS)
            .max(3_000_000);
        let total = configured
            .grace_ns
            .saturating_add(configured.term_ns)
            .saturating_add(configured.reap_ns);
        if total <= available {
            return configured;
        }
        let scale = |stage: u64| -> u64 {
            let scaled = u128::from(stage) * u128::from(available) / u128::from(total.max(1));
            u64::try_from(scaled).unwrap_or(stage).max(1_000_000)
        };
        StopDurations {
            grace_ns: scale(configured.grace_ns),
            term_ns: scale(configured.term_ns),
            reap_ns: scale(configured.reap_ns),
        }
    }

    pub(crate) fn emit(&mut self, completion: IngestionCompletion) {
        self.outbox.push(completion);
    }

    pub(crate) fn log(&self, level: LogLevel, message: &str) {
        self.logger.log(level, LOG_TARGET, message);
    }
}
