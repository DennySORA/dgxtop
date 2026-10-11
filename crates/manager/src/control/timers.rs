//! Due timers of one control iteration: session lifecycle, action/config
//! deadlines, scheduling, collector jobs, history refreshes, overload
//! sampling, store maintenance and projection frames.

use dgxtop_core::canonical::{CatalogSnapshot, GapRecord, ImmutableSnapshot};
use dgxtop_core::clock::ms_to_ns;
use dgxtop_core::diagnostics::names;
use dgxtop_core::identity::SessionId;
use dgxtop_core::ports::LogLevel;
use dgxtop_core::view::{HostStatus, IntervalView, ViewModel};
use std::sync::Arc;

use super::{AGE_REFRESH_NS, ControlCore, MAINTAIN_NS, QueueLevels};
use crate::effects::Effects;
use crate::ingestion::PumpContext;
use crate::overload::{OVERLOAD_SAMPLE_MS, OverloadSignals};
use crate::presentation::{
    PresentationContext, SettingsInputs, build_view_model_measured, history_charts,
};
use crate::process::Now;
use crate::projection::PublishOutcome;
use crate::scheduler::{DispatchContext, RequestKind, TickReport};

/// Inputs shared by every frame built in one pass.
struct FrameInputs {
    snapshot: Arc<ImmutableSnapshot>,
    catalog: Arc<CatalogSnapshot>,
    hosts: Vec<HostStatus>,
    intervals: Vec<IntervalView>,
    enabled_modules: Vec<String>,
}

impl ControlCore {
    pub(crate) fn run_timers(&mut self, levels: QueueLevels) {
        let now = self.now();
        self.reap_closed_sessions(now);
        let mut fx = Effects::default();
        self.process.tick(now, &mut fx);
        self.apply(fx);
        if let Some((_, result)) = self.config.expire(now.boot_ns) {
            self.finish(result);
            self.sessions.mark_all_dirty();
        }
        if !self.is_running() {
            return;
        }
        self.tick_scheduler(now.boot_ns);
        let gaps = self.ingestion.expire_jobs(now.boot_ns);
        for gap in gaps {
            self.record_gap(gap);
        }
        self.pump_collectors(now.boot_ns);
        self.tick_history(now.boot_ns);
        self.process_history.tick(now);
        self.sample_overload(now.boot_ns, levels);
        self.periodic(now.boot_ns);
        self.publish_frames(now.boot_ns);
    }

    /// Dispatches one coalesced job to every idle collector with inputs.
    pub(super) fn pump_collectors(&mut self, now_ns: u64) {
        if !self.is_running() {
            return;
        }
        let context = PumpContext {
            busy_adapters: self.scheduler.busy_adapters(),
            epoch: self.scheduler.epoch(),
            config_revision: self.config.effective().revision,
            catalog: self.catalog.snapshot(),
            policy: self.policy.snapshot(),
        };
        let report = self.ingestion.pump(now_ns, &context);
        if report.stale_inputs > 0 {
            self.diagnostics.increment(
                names::STALE_RESULT_DROPPED_TOTAL,
                "old_epoch_input",
                u64::from(report.stale_inputs),
            );
        }
        for gap in report.gaps {
            self.record_gap(gap);
        }
    }

    fn reap_closed_sessions(&mut self, now: Now) {
        for session_id in self.sessions.reap_closed() {
            let mut fx = Effects::default();
            self.process.revoke_session(&session_id, now, &mut fx);
            self.apply(fx);
            self.history.close_session(&session_id);
            self.logger.log(
                LogLevel::Info,
                "manager.sessions",
                &format!("session {session_id} closed"),
            );
        }
    }

    fn tick_scheduler(&mut self, now_ns: u64) {
        let context = DispatchContext {
            catalog_revision: self.catalog.revision(),
            defer_metadata: self.overload.defer_metadata(),
        };
        let report = self.scheduler.tick(now_ns, context);
        self.handle_tick_report(report, now_ns);
    }

    fn handle_tick_report(&mut self, report: TickReport, now_ns: u64) {
        if report.skipped > 0 {
            self.diagnostics
                .increment(names::QUEUE_DROPPED_TOTAL, "skipped_due", report.skipped);
        }
        for (reason, count) in report.deferred.by_reason() {
            self.diagnostics
                .increment(names::ADAPTER_DISPATCH_DEFERRED_TOTAL, reason, count);
        }
        for expired in report.expired {
            match expired.kind {
                RequestKind::Read { .. } => {
                    self.diagnostics.increment(
                        names::ADAPTER_TIMEOUT_TOTAL,
                        expired.adapter_id.as_str(),
                        1,
                    );
                    let records = self
                        .ingestion
                        .pages()
                        .abandon(&expired.adapter_id)
                        .map_or(0, |cycle| cycle.records);
                    let span =
                        ms_to_ns(u64::from(self.config.effective().sampling.read_deadline_ms));
                    self.record_gap(GapRecord {
                        key: None,
                        adapter_id: Some(expired.adapter_id),
                        start_boot_ns: expired.deadline_ns.saturating_sub(span),
                        end_boot_ns: now_ns,
                        reason: "read_deadline_expired".to_owned(),
                        affected_count: records,
                    });
                }
                RequestKind::Discover { .. } => self.catalog.abandon_round(&expired.adapter_id),
            }
        }
        for missed in report.missed {
            self.record_gap(GapRecord {
                key: None,
                adapter_id: Some(missed.adapter_id),
                start_boot_ns: missed.slot_ns,
                end_boot_ns: missed.next_ns,
                reason: format!("host_unavailable:{}", missed.group),
                affected_count: 0,
            });
        }
    }

    /// Picks the history charts of sessions on the history view from a new
    /// snapshot (D-46), then refreshes and dispatches their queries.
    fn tick_history(&mut self, now_ns: u64) {
        let snapshot = self.ingestion.snapshots().latest();
        if self.history.wants_charts(snapshot.revision) {
            let catalog = self.catalog.snapshot();
            let trends = self.trends.book();
            let dirty = self.history.update_charts(
                snapshot.revision,
                |request| history_charts(&snapshot, &catalog, trends, now_ns, request),
                now_ns,
            );
            for session_id in &dirty {
                self.sessions.mark_dirty(session_id);
            }
        }
        let tick = self.history.tick(now_ns, self.overload.limit_history());
        for session_id in &tick.dirty {
            self.sessions.mark_dirty(session_id);
        }
    }

    fn sample_overload(&mut self, now_ns: u64, levels: QueueLevels) {
        if now_ns < self.timers.next_overload_ns {
            return;
        }
        self.timers.next_overload_ns = now_ns.saturating_add(ms_to_ns(OVERLOAD_SAMPLE_MS));
        let period_ns = ms_to_ns(u64::from(self.scheduler.effective_fast_interval_ms()));
        let backlog = self.ingestion.backlog_utilization();
        let signals = OverloadSignals {
            queue_utilization: levels.commands.max(levels.ingestion).max(backlog),
            ingress_utilization: self.ingress.utilization(),
            memory_utilization: self.ledger.total_utilization(),
            service_overrun: self.ingestion.service_overrun(period_ns),
        };
        self.record_queue_gauges(levels, backlog);
        let Some(transition) = self.overload.sample(now_ns, signals) else {
            return;
        };
        self.diagnostics
            .increment(names::OVERLOAD_TRANSITIONS_TOTAL, &transition.label(), 1);
        self.logger.log(
            LogLevel::Warn,
            "manager.overload",
            &format!(
                "overload {} -> {} (fast multiplier x{})",
                transition.from.label(),
                transition.to.label(),
                transition.multiplier
            ),
        );
        if self
            .scheduler
            .set_multiplier(self.overload.multiplier(), now_ns)
        {
            self.refresh_policy();
        }
        self.sessions.mark_all_dirty();
    }

    fn record_queue_gauges(&self, levels: QueueLevels, backlog: f64) {
        self.diagnostics
            .set_gauge(names::QUEUE_ITEMS, "commands", levels.commands);
        self.diagnostics
            .set_gauge(names::QUEUE_ITEMS, "ingestion_inbox", levels.ingestion);
        self.diagnostics
            .set_gauge(names::QUEUE_ITEMS, "collector_backlog", backlog);
        #[allow(clippy::cast_precision_loss)]
        let ingress_bytes = self.ingress.queued_bytes() as f64;
        self.diagnostics
            .set_gauge(names::QUEUE_BYTES, self.ingress.name(), ingress_bytes);
    }

    fn periodic(&mut self, now_ns: u64) {
        if now_ns >= self.timers.next_age_refresh_ns {
            // Ages in projections advance even when no data arrives.
            self.timers.next_age_refresh_ns = now_ns.saturating_add(AGE_REFRESH_NS);
            self.sessions.mark_all_dirty();
        }
        if now_ns >= self.timers.next_maintain_ns {
            self.timers.next_maintain_ns = now_ns.saturating_add(MAINTAIN_NS);
            if self.ingestion.request_maintenance().is_err() {
                self.diagnostics
                    .increment(names::QUEUE_DROPPED_TOTAL, "store_maintenance_busy", 1);
            }
        }
    }

    fn publish_frames(&mut self, now_ns: u64) {
        for session_id in self.sessions.viewport_changes() {
            self.sync_history(&session_id, now_ns);
        }
        let due = self
            .sessions
            .due_for_frame(now_ns, self.projection_interval_ns());
        if due.is_empty() {
            return;
        }
        let inputs = FrameInputs {
            snapshot: self.ingestion.snapshots().latest(),
            catalog: self.catalog.snapshot(),
            hosts: self.hosts.statuses(),
            intervals: self.scheduler.intervals(),
            enabled_modules: self.config.effective().viewer.enabled_modules.clone(),
        };
        for session_id in due {
            self.publish_frame(&session_id, &inputs, now_ns);
        }
    }

    fn publish_frame(&mut self, session_id: &SessionId, inputs: &FrameInputs, now_ns: u64) {
        let Some(shared) = self.sessions.shared(session_id).cloned() else {
            return;
        };
        let frame_revision = self.presentation.next_frame_revision();
        // The frame budget is admitted before the model is built (AT21).
        let outcome = match self.presentation.reserve_frame(&shared) {
            Ok(permit) => {
                let Some((model, measured_bytes)) =
                    self.build_model(session_id, frame_revision, inputs, now_ns)
                else {
                    return;
                };
                self.presentation
                    .publish_reserved(model, measured_bytes, permit, &shared)
            }
            Err(skipped) => skipped,
        };
        match outcome {
            PublishOutcome::Published { .. } => {
                self.sessions
                    .record_frame(session_id, frame_revision, now_ns);
                if let Some(commit_ns) = self.pending_commit_ns.take() {
                    self.diagnostics.observe_us(
                        names::COMMIT_TO_PROJECTION_MS,
                        "projection",
                        now_ns.saturating_sub(commit_ns) / 1_000,
                    );
                }
            }
            PublishOutcome::TooLarge { bytes } | PublishOutcome::LedgerFull { bytes } => {
                self.sessions.frame_skipped(session_id, now_ns);
                self.logger.log(
                    LogLevel::Warn,
                    "manager.presentation",
                    &format!("projection of {bytes} bytes skipped (presentation budget)"),
                );
            }
        }
    }

    fn build_model(
        &self,
        session_id: &SessionId,
        frame_revision: u64,
        inputs: &FrameInputs,
        now_ns: u64,
    ) -> Option<(ViewModel, usize)> {
        let state = self.sessions.state(session_id)?;
        let shared = self.sessions.shared(session_id)?;
        let context = PresentationContext {
            frame_revision,
            now_boot_ns: now_ns,
            snapshot: &inputs.snapshot,
            catalog: &inputs.catalog,
            session: state,
            viewport: shared.viewport(),
            settings: SettingsInputs {
                saved: self.config.saved(),
                effective: self.config.effective(),
                origins: self.config.origins(),
                restart_pending: self.config.restart_pending(),
                pending: self.config.pending_label(),
                last_result: self.config.last_result(),
                status: self.config.status(),
            },
            hosts: &inputs.hosts,
            intervals: &inputs.intervals,
            overload: self.overload.state(),
            actions: self.process.view_for(session_id, now_ns),
            history: self.history.view(session_id),
            trends: self.trends.book(),
            process_memory: &self.process_memory,
            process_history: self.process_history.book(),
            process_history_kept: self.process_history.keeps_runs_on_disk(),
            boot_to_utc_ns: self
                .clock
                .now_utc_ns()
                .saturating_sub(i128::from(self.clock.now_boot_ns())),
            facts: &self.facts,
            enabled_modules: &inputs.enabled_modules,
            archive: self.archive.status(),
        };
        Some(build_view_model_measured(&context))
    }
}
