//! Bounded shutdown (RUNTIME.md §8, AT39): revoke every token not yet
//! sending, stop scheduling and collection, tell frontends to stop, wait at
//! most 3 s (boot clock and real time) for config/audit/action completions,
//! answer every accepted command once and report what is unfinished.

use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dgxtop_core::commands::CommandResult;
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::ports::LogLevel;
use uuid::Uuid;

use super::{ControlCore, Draining, Phase, SHUTDOWN_WAIT_NS};
use crate::effects::Effects;
use crate::session::QueuedCommand;
use crate::shutdown::ShutdownSummary;

impl ControlCore {
    /// The out-of-band flag is checked before any queue each iteration.
    pub(crate) fn check_shutdown_flag(&mut self) {
        if self.is_running() && self.shutdown.is_requested() {
            self.begin_shutdown();
        }
    }

    fn begin_shutdown(&mut self) {
        let now = self.now();
        let reason = self.shutdown.reason();
        let mut fx = Effects::default();
        self.process.begin_shutdown(now, &mut fx);
        self.apply(fx);
        // The process actor's store worker writes these while the shutdown
        // drains.
        self.process_history.flush();
        self.scheduler.stop();
        self.ingestion.stop();
        self.history.cancel_all();
        self.sessions.mark_shutdown_requested_all();
        self.logger.log(
            LogLevel::Info,
            "manager.shutdown",
            &format!("shutdown started: {reason}"),
        );
        self.phase = Phase::Draining(Draining {
            reason,
            started_ns: now.boot_ns,
            deadline_ns: now.boot_ns.saturating_add(SHUTDOWN_WAIT_NS),
            real_deadline: Instant::now() + Duration::from_nanos(SHUTDOWN_WAIT_NS),
        });
    }

    /// Returns the summary once nothing is worth waiting for or the bounded
    /// wait is over.
    pub(crate) fn progress_shutdown(
        &mut self,
        commands: &Receiver<QueuedCommand>,
    ) -> Option<ShutdownSummary> {
        let Phase::Draining(draining) = &self.phase else {
            return None;
        };
        let timed_out = self.clock.now_boot_ns() >= draining.deadline_ns
            || Instant::now() >= draining.real_deadline;
        let settled = !self.config.has_pending()
            && !self.process.awaiting_control()
            && self.process_history.settled();
        if !settled && !timed_out {
            return None;
        }
        Some(self.finish_shutdown(commands, !settled))
    }

    fn finish_shutdown(
        &mut self,
        commands: &Receiver<QueuedCommand>,
        timed_out: bool,
    ) -> ShutdownSummary {
        let now = self.now();
        let (reason, started_ns) = match &self.phase {
            Phase::Draining(draining) => (draining.reason.clone(), draining.started_ns),
            _ => (self.shutdown.reason(), now.boot_ns),
        };
        let mut unfinished = Vec::new();
        let config_pending = self.config.pending_request();
        if let Some(request_id) = config_pending {
            unfinished.push(format!(
                "config persist {request_id}: no receipt; on-disk state unknown"
            ));
        }
        let mut fx = Effects::default();
        unfinished.extend(self.process.finish_shutdown(now, &mut fx));
        self.apply(fx);
        // No command can enter the queue after this point.
        self.inbox.close();
        while let Ok(queued) = commands.try_recv() {
            self.reject_for_shutdown(&queued.envelope);
        }
        let pending: Vec<Uuid> = self.routes.keys().copied().collect();
        for request_id in pending {
            let result = if config_pending == Some(request_id) {
                let detail = ErrorDetail::new(
                    ErrorCode::PersistFailed,
                    "shutdown before the persist receipt (commit_uncertain); on-disk state unknown",
                )
                .uncertain();
                CommandResult::from_error(request_id, &detail)
            } else {
                CommandResult::rejected(
                    request_id,
                    ErrorCode::Unavailable,
                    "dgxtop is shutting down",
                )
            };
            self.finish(result);
        }
        self.sessions.mark_shutdown_requested_all();
        self.phase = Phase::Finished;
        if !unfinished.is_empty() {
            self.logger.log(
                LogLevel::Warn,
                "manager.shutdown",
                &format!("shutdown left work unfinished: {}", unfinished.join("; ")),
            );
        }
        ShutdownSummary {
            reason,
            started_boot_ns: started_ns,
            finished_boot_ns: now.boot_ns,
            timed_out,
            unfinished,
        }
    }
}
