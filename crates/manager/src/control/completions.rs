//! Completion handling: data-path completions (adapter replies, collector
//! results, store acks, host status) and control-path completions (config,
//! audit, actions, history).

use std::sync::Arc;

use dgxtop_core::canonical::{
    CollectionBatch, CollectorEvent, GapRecord, ImmutableSnapshot, StoreAck,
};
use dgxtop_core::clock::ms_to_ns;
use dgxtop_core::diagnostics::names;
use dgxtop_core::event::EventKind;
use dgxtop_core::identity::{AdapterId, CollectorId};
use dgxtop_core::observation::{CatalogDelta, split_page};
use dgxtop_core::ports::{
    AdapterFailure, AdapterFailureKind, ControlCompletion, IngestionCompletion, IngressBatch,
    LogLevel, PersistError, PersistReceipt,
};
use dgxtop_core::quality::Availability;
use dgxtop_core::text::sanitize_display;
use dgxtop_core::view::HostStatus;
use uuid::Uuid;

use super::ControlCore;
use crate::catalog::SourceKnowledge;
use crate::config::{AppliedChange, PersistOutcome, applied_result};
use crate::effects::Effects;
use crate::hosts::{HostStatusRejection, module_health};
use crate::ingestion::pages::PagePush;
use crate::ingestion::trends::unified_gpus;
use crate::ingestion::{CollectedOutcome, CommitOutcome, CycleInput};
use crate::presentation::run_inputs;
use crate::process::Now;
use crate::process_history::SampleInputs;
use crate::scheduler::{ReadReply, ReplyMatch, RequestKind};

/// Source ids carried by these entities are per-process and never catalog
/// sources, so they must not trigger rediscovery.
const DYNAMIC_ENTITY_PREFIXES: [&str; 2] = ["process:", "gpupid:"];
/// Source ids quoted per conflict event.
const MAX_CONFLICT_SOURCES: usize = 16;

impl ControlCore {
    pub(crate) fn on_control(&mut self, completion: ControlCompletion) {
        let now = self.now();
        match completion {
            ControlCompletion::Config { request_id, result } => {
                self.on_config_result(request_id, result, now);
            }
            ControlCompletion::Audit(audit) => {
                let open = self.sessions.open_ids();
                let mut fx = Effects::default();
                self.process.on_audit(audit, now, &open, &mut fx);
                self.apply(fx);
            }
            ControlCompletion::Action(action) => {
                if let Some((target, signal)) = self.process.signal_sent(&action) {
                    self.process_history.note_signal(&target, signal, now);
                }
                let open = self.sessions.open_ids();
                let mut fx = Effects::default();
                self.process.on_action(action, now, &open, &mut fx);
                self.apply(fx);
            }
            ControlCompletion::Archive {
                revision,
                outcome,
                status,
            } => {
                if self.archive.on_outcome(revision, &outcome, status) {
                    self.sessions.mark_all_data_changed(now.boot_ns);
                }
            }
            ControlCompletion::History {
                request_id,
                session_id,
                session_generation,
                result,
            } => match self
                .history
                .on_result(request_id, &session_id, session_generation, result)
            {
                Some(session) => self.sessions.mark_dirty(&session),
                None => self.count_stale("history"),
            },
            ControlCompletion::ArchiveRead {
                request_id,
                session_id,
                session_generation,
                result,
            } => match self.history.on_archive_result(
                request_id,
                &session_id,
                session_generation,
                result,
            ) {
                Some(session) => self.sessions.mark_dirty(&session),
                None => self.count_stale("history_archive"),
            },
            ControlCompletion::ProcessProbe(report) => {
                self.process_history.on_probe(&report, now);
            }
            ControlCompletion::ProcessHistory(completion) => {
                if self.process_history.on_completion(completion, now) {
                    self.sessions.mark_all_data_changed(now.boot_ns);
                }
            }
        }
    }

    pub(crate) fn on_ingestion(&mut self, completion: IngestionCompletion) {
        if !self.is_running() {
            // Dropping the completion releases its byte permits.
            self.count_stale("shutdown");
            return;
        }
        let now = self.now_boot_ns();
        match completion {
            IngestionCompletion::Batch {
                adapter_id,
                request_id,
                host_generation,
                result,
            } => self.on_batch(&adapter_id, request_id, host_generation, result, now),
            IngestionCompletion::Catalog {
                adapter_id,
                request_id,
                host_generation,
                result,
            } => self.on_catalog(&adapter_id, request_id, host_generation, result, now),
            IngestionCompletion::Collected(batch) => self.on_collected(batch, now),
            IngestionCompletion::CollectorFailed {
                collector_id,
                batch_id,
                message,
            } => self.on_collector_failed(&collector_id, batch_id, &message, now),
            IngestionCompletion::Committed(ack) => self.on_committed(&ack, now),
            IngestionCompletion::HostStatus(status) => self.on_host_status(status, now),
        }
    }

    pub(super) fn count_stale(&self, label: &str) {
        self.diagnostics
            .increment(names::STALE_RESULT_DROPPED_TOTAL, label, 1);
    }

    // ----- configuration ----------------------------------------------------

    fn on_config_result(
        &mut self,
        request_id: Uuid,
        result: Result<PersistReceipt, PersistError>,
        now: Now,
    ) {
        match self.config.on_persist_result(request_id, result) {
            PersistOutcome::Applied(change) => {
                self.apply_config_change(&change, now);
                if change.reply_pending {
                    let result =
                        applied_result(&change, self.scheduler.effective_fast_interval_ms());
                    self.finish(result);
                }
            }
            PersistOutcome::Failed(_, result) => self.finish(result),
            PersistOutcome::LateFailure => self.logger.log(
                LogLevel::Warn,
                "manager.config",
                "late configuration persist failure; configuration unchanged",
            ),
            PersistOutcome::Stale => self.count_stale("config"),
        }
        self.sessions.mark_all_dirty();
    }

    /// The infallible part of a durable configuration change: new epoch when
    /// scheduling changed, new policy revision when quality/periods changed,
    /// token revocation when the action policy changed.
    fn apply_config_change(&mut self, change: &AppliedChange, now: Now) {
        let effective = self.config.effective().clone();
        if change.previous.scheduling_changed(&effective) {
            self.scheduler.reconfigure(&effective, now.boot_ns);
            let fast = self.scheduler.effective_fast_interval_ms();
            self.trends.set_step(u64::from(fast));
            self.history.set_raw_interval(fast);
        } else {
            self.scheduler.set_config_revision(effective.revision);
        }
        self.refresh_policy();
        let action_changed = change.previous.action_policy_changed(&effective);
        let mut fx = Effects::default();
        self.process
            .apply_policy(&effective, action_changed, now, &mut fx);
        self.apply(fx);
        self.events.emit(
            now.boot_ns,
            change.request_id,
            EventKind::ConfigApplied {
                revision: change.revision,
                epoch: self.scheduler.epoch(),
            },
        );
    }

    // ----- adapter replies --------------------------------------------------

    fn on_batch(
        &mut self,
        adapter_id: &AdapterId,
        request_id: Uuid,
        host_generation: u32,
        result: Result<Arc<IngressBatch>, AdapterFailure>,
        now: u64,
    ) {
        match self
            .scheduler
            .match_reply(adapter_id, request_id, host_generation)
        {
            ReplyMatch::Stale(reason) => self.count_stale(reason.label()),
            ReplyMatch::Discover { .. } => {
                self.catalog.abandon_round(adapter_id);
                self.logger.log(
                    LogLevel::Warn,
                    "manager.ingestion",
                    &format!("{adapter_id}: an observation batch answered a discovery request"),
                );
            }
            ReplyMatch::Read(reply) => match result {
                Ok(batch) => {
                    let identity = ReplyIdentity {
                        adapter_id,
                        request_id,
                        host_generation,
                    };
                    self.read_succeeded(identity, &reply, batch, now);
                }
                Err(failure) => self.read_failed(adapter_id, &failure, now),
            },
        }
    }

    fn read_failed(&mut self, adapter_id: &AdapterId, failure: &AdapterFailure, now: u64) {
        if failure.kind == AdapterFailureKind::TimedOut {
            self.diagnostics
                .increment(names::ADAPTER_TIMEOUT_TOTAL, adapter_id.as_str(), 1);
        }
        let records = self
            .ingestion
            .pages()
            .abandon(adapter_id)
            .map_or(0, |cycle| cycle.records);
        let reason = format!("read_{}", failure_label(failure.kind));
        let gap = self.adapter_gap(adapter_id, &reason, records, now);
        self.record_gap(gap);
    }

    fn read_succeeded(
        &mut self,
        identity: ReplyIdentity<'_>,
        reply: &ReadReply,
        batch: Arc<IngressBatch>,
        now: u64,
    ) {
        let adapter_id = identity.adapter_id;
        if !identity.matches(&batch, reply) {
            self.abandon_with_gap(adapter_id, "protocol_mismatch", now);
            self.logger.log(
                LogLevel::Warn,
                "manager.ingestion",
                &format!("{adapter_id}: batch does not match its read plan"),
            );
            return;
        }
        self.discover_unknown_sources(adapter_id, &batch, now);
        let final_page = !batch.continues();
        match self
            .ingestion
            .pages()
            .push(adapter_id, reply, batch, final_page)
        {
            PagePush::Continue => {
                let catalog_revision = self.catalog.revision();
                if let Err(error) =
                    self.scheduler
                        .continue_read(adapter_id, reply, now, catalog_revision)
                {
                    self.abandon_with_gap(adapter_id, error.label(), now);
                }
            }
            PagePush::Complete(batches) => self.route_cycle(adapter_id, reply, batches, now),
            PagePush::Discarded { records } => {
                let gap = self.adapter_gap(adapter_id, "page_out_of_order", records, now);
                self.record_gap(gap);
            }
        }
    }

    fn abandon_with_gap(&mut self, adapter_id: &AdapterId, reason: &str, now: u64) {
        let records = self
            .ingestion
            .pages()
            .abandon(adapter_id)
            .map_or(0, |cycle| cycle.records);
        let gap = self.adapter_gap(adapter_id, reason, records, now);
        self.record_gap(gap);
    }

    /// Gap covering the request window that produced no usable data.
    fn adapter_gap(
        &self,
        adapter_id: &AdapterId,
        reason: &str,
        affected: u32,
        now: u64,
    ) -> GapRecord {
        let span = ms_to_ns(u64::from(self.config.effective().sampling.read_deadline_ms));
        GapRecord {
            key: None,
            adapter_id: Some(adapter_id.clone()),
            start_boot_ns: now.saturating_sub(span),
            end_boot_ns: now,
            reason: reason.to_owned(),
            affected_count: affected,
        }
    }

    fn route_cycle(
        &mut self,
        adapter_id: &AdapterId,
        reply: &ReadReply,
        batches: Vec<Arc<IngressBatch>>,
        now: u64,
    ) {
        let period_ms = self
            .scheduler
            .group_effective_ms(adapter_id, &reply.group)
            .unwrap_or_else(|| self.scheduler.effective_fast_interval_ms());
        let stale_factor = u64::from(self.config.effective().sampling.stale_factor);
        let mut input = CycleInput::new(
            adapter_id.clone(),
            reply.group.clone(),
            reply.cycle_id,
            reply.epoch,
            batches,
            0,
        );
        // Data newer than "now" or without a timestamp is aged from arrival.
        let data_ns = input.data_ns();
        let base = if data_ns == 0 || data_ns > now {
            now
        } else {
            data_ns
        };
        input.expires_ns =
            base.saturating_add(ms_to_ns(u64::from(period_ms)).saturating_mul(stale_factor.max(1)));
        input.arrived_ns = now;
        let (consumers, gaps) = self.ingestion.route(input);
        for gap in gaps {
            self.record_gap(gap);
        }
        if consumers == 0 {
            self.diagnostics
                .increment(names::QUEUE_DROPPED_TOTAL, "unrouted_cycle", 1);
        }
        self.pump_collectors(now);
    }

    /// Records from a source the catalog has never seen request an early
    /// (rate limited) discovery of that adapter.
    fn discover_unknown_sources(&mut self, adapter_id: &AdapterId, batch: &IngressBatch, now: u64) {
        let unknown = batch.records.iter().any(|record| {
            let entity = record.key.entity_id.as_str();
            !DYNAMIC_ENTITY_PREFIXES
                .iter()
                .any(|prefix| entity.starts_with(prefix))
                && self.catalog.knows(&record.provenance.source_id) == SourceKnowledge::Unknown
        });
        if unknown {
            self.scheduler.request_early_discovery(adapter_id, now);
        }
    }

    fn on_catalog(
        &mut self,
        adapter_id: &AdapterId,
        request_id: Uuid,
        host_generation: u32,
        result: Result<CatalogDelta, AdapterFailure>,
        now: u64,
    ) {
        match self
            .scheduler
            .match_reply(adapter_id, request_id, host_generation)
        {
            ReplyMatch::Stale(reason) => self.count_stale(reason.label()),
            ReplyMatch::Read(_) => {
                self.ingestion.pages().abandon(adapter_id);
                self.logger.log(
                    LogLevel::Warn,
                    "manager.catalog",
                    &format!("{adapter_id}: a catalog page answered a read request"),
                );
            }
            ReplyMatch::Discover { .. } => match result {
                Ok(delta) => self.merge_catalog(adapter_id, request_id, delta, now),
                Err(failure) => {
                    self.catalog.abandon_round(adapter_id);
                    self.logger.log(
                        LogLevel::Info,
                        "manager.catalog",
                        &format!(
                            "{adapter_id}: discovery failed ({}): {}",
                            failure_label(failure.kind),
                            sanitize_display(&failure.message, 256)
                        ),
                    );
                }
            },
        }
    }

    fn merge_catalog(
        &mut self,
        adapter_id: &AdapterId,
        request_id: Uuid,
        delta: CatalogDelta,
        now: u64,
    ) {
        let outcome = self.catalog.merge(adapter_id, delta);
        if let Some(reason) = &outcome.rejected {
            self.logger.log(
                LogLevel::Warn,
                "manager.catalog",
                &format!("{adapter_id}: catalog page rejected: {reason}"),
            );
            return;
        }
        if outcome.dropped_over_capacity > 0 {
            self.diagnostics.increment(
                names::QUEUE_DROPPED_TOTAL,
                "catalog_capacity",
                u64::from(outcome.dropped_over_capacity),
            );
        }
        if let Some(change) = outcome.change {
            self.events.emit(
                now,
                request_id,
                EventKind::CatalogChanged {
                    catalog_revision: change.revision,
                    changed_count: change.changed,
                    removed_count: change.removed,
                },
            );
            self.sessions.mark_all_dirty();
        }
        match outcome.next_page.filter(|_| !outcome.complete) {
            Some(page) => {
                let revision = self.catalog.revision();
                if self
                    .scheduler
                    .continue_discovery(adapter_id, page, now, revision)
                    .is_err()
                {
                    self.catalog.abandon_round(adapter_id);
                    self.finish_discovery(adapter_id, outcome.availability, now);
                }
            }
            None => self.finish_discovery(adapter_id, outcome.availability, now),
        }
    }

    fn finish_discovery(
        &mut self,
        adapter_id: &AdapterId,
        availability: Option<Availability>,
        now: u64,
    ) {
        let availability = availability.or_else(|| self.catalog.availability(adapter_id));
        self.scheduler.discovery_finished(
            adapter_id,
            availability,
            self.catalog.read_groups_of(adapter_id),
            self.catalog.generation(adapter_id),
            now,
        );
        self.refresh_policy();
    }

    // ----- collectors and store ---------------------------------------------

    fn on_collected(&mut self, batch: Box<CollectionBatch>, now: u64) {
        let epoch = self.scheduler.epoch();
        match self.ingestion.on_collected(batch, now, epoch) {
            CollectedOutcome::Committing => {}
            CollectedOutcome::Stale => self.count_stale("collected"),
            CollectedOutcome::EpochDiscarded => {
                self.count_stale("collected_old_epoch");
                self.pump_collectors(now);
            }
            CollectedOutcome::Dropped { reason, gaps } => {
                self.logger.log(
                    LogLevel::Warn,
                    "manager.ingestion",
                    &format!("collection batch dropped: {reason}"),
                );
                for gap in gaps {
                    self.record_gap(gap);
                }
                self.pump_collectors(now);
            }
        }
    }

    fn on_collector_failed(
        &mut self,
        collector_id: &CollectorId,
        batch_id: Uuid,
        message: &str,
        now: u64,
    ) {
        self.logger.log(
            LogLevel::Warn,
            "manager.ingestion",
            &format!(
                "collector {collector_id} failed: {}",
                sanitize_display(message, 256)
            ),
        );
        self.diagnostics
            .increment(names::QUEUE_DROPPED_TOTAL, "collector_failed", 1);
        match self.ingestion.on_collector_failed(collector_id, batch_id) {
            Some(gaps) => {
                for gap in gaps {
                    self.record_gap(gap);
                }
                self.pump_collectors(now);
            }
            None => self.count_stale("collector_failed"),
        }
    }

    fn on_committed(&mut self, ack: &StoreAck, now: u64) {
        match self.ingestion.on_committed(ack, now) {
            CommitOutcome::Committed(batch) => {
                let snapshot = self.ingestion.snapshots().latest();
                self.process_memory.record(&snapshot.process_table);
                self.sample_process_history(&snapshot);
                self.trends.record(
                    &snapshot,
                    &unified_gpus(&self.catalog.snapshot()),
                    &self.process_memory,
                );
                self.pending_commit_ns.get_or_insert(now);
                self.events.emit(
                    now,
                    batch.batch_id,
                    EventKind::CollectionCommitted {
                        batch_id: batch.batch_id,
                        snapshot_revision: batch.snapshot_revision,
                        sample_count: batch.sample_count,
                    },
                );
                for event in &batch.events {
                    self.emit_collector_event(event, batch.batch_id, now);
                }
                self.sessions.mark_all_data_changed(now);
                self.pump_collectors(now);
            }
            CommitOutcome::Retrying { gaps, .. } => {
                self.count_stale("commit_retry");
                for gap in gaps {
                    self.record_gap(gap);
                }
                self.pump_collectors(now);
            }
            CommitOutcome::Rejected { status, gaps } => {
                self.logger.log(
                    LogLevel::Warn,
                    "manager.ingestion",
                    &format!("{}: commit rejected ({status:?})", ack.collector_id),
                );
                for gap in gaps {
                    self.record_gap(gap);
                }
                self.pump_collectors(now);
            }
            CommitOutcome::Stale => self.count_stale("ack"),
        }
    }

    fn emit_collector_event(&mut self, event: &CollectorEvent, correlation_id: Uuid, now: u64) {
        let kind = match event {
            CollectorEvent::SourceChanged {
                key,
                previous_source,
                source_id,
                policy_revision,
            } => {
                self.diagnostics
                    .increment(names::SOURCE_CHANGED_TOTAL, key.metric_id.as_str(), 1);
                EventKind::SourceChanged {
                    entity_id: key.entity_id.to_string(),
                    metric_id: key.metric_id.to_string(),
                    previous_source: previous_source.as_ref().map(ToString::to_string),
                    source_id: source_id.to_string(),
                    policy_revision: *policy_revision,
                }
            }
            CollectorEvent::SourceConflict { key, source_ids } => {
                self.diagnostics
                    .increment(names::SOURCE_CONFLICT_TOTAL, key.metric_id.as_str(), 1);
                EventKind::SourceConflict {
                    entity_id: key.entity_id.to_string(),
                    metric_id: key.metric_id.to_string(),
                    source_ids: source_ids
                        .iter()
                        .take(MAX_CONFLICT_SOURCES)
                        .map(ToString::to_string)
                        .collect(),
                }
            }
            CollectorEvent::SourceUnavailable { source_id, reason } => {
                EventKind::SourceUnavailable {
                    source_id: source_id.to_string(),
                    reason: sanitize_display(reason, 256),
                }
            }
        };
        self.events.emit(now, correlation_id, kind);
    }

    // ----- hosts --------------------------------------------------------------

    /// Samples the process history from the committed process table (D-76).
    fn sample_process_history(&mut self, snapshot: &ImmutableSnapshot) {
        // Most commits of a cycle carry the table already sampled; the
        // inputs (a reader, an index, the GPU directory) are built only for
        // a new one.
        if self.process_history.has_sampled(&snapshot.process_table) {
            self.process_history.on_sampled_table();
            return;
        }
        let now = self.now();
        let catalog = self.catalog.snapshot();
        let inputs = run_inputs(snapshot, &catalog, now.boot_ns);
        self.process_history.on_table(
            &snapshot.process_table,
            &SampleInputs {
                now,
                clk_tck: inputs.clk_tck,
                gpu_labels: &inputs.gpu_labels,
                host_network: inputs.host_network,
            },
        );
    }

    fn on_host_status(&mut self, status: HostStatus, now: u64) {
        let update = match self.hosts.apply(status) {
            Ok(update) => update,
            Err(HostStatusRejection::UnknownAdapter) => return self.count_stale("host_unknown"),
            Err(HostStatusRejection::OutOfOrder) => return self.count_stale("host_out_of_order"),
        };
        let lost = self.scheduler.on_host_status(
            &update.adapter_id,
            update.generation,
            update.dispatchable,
            now,
        );
        match lost.map(|request| request.kind) {
            Some(RequestKind::Read { .. }) => {
                self.abandon_with_gap(&update.adapter_id, "host_restarted", now);
            }
            Some(RequestKind::Discover { .. }) => self.catalog.abandon_round(&update.adapter_id),
            None => {}
        }
        if update.state_changed {
            self.events.emit(
                now,
                Uuid::new_v4(),
                EventKind::ModuleHealthChanged {
                    module_id: update.adapter_id.to_string(),
                    state: module_health(update.state),
                    reason: update
                        .detail
                        .clone()
                        .unwrap_or_else(|| update.state.label().to_owned()),
                },
            );
            self.sessions.mark_all_dirty();
        }
    }
}

/// The identity a batch must echo from its read plan.
#[derive(Debug, Clone, Copy)]
struct ReplyIdentity<'a> {
    adapter_id: &'a AdapterId,
    request_id: Uuid,
    host_generation: u32,
}

impl ReplyIdentity<'_> {
    fn matches(&self, batch: &IngressBatch, reply: &ReadReply) -> bool {
        let (group, page) = split_page(batch.read_group.as_str());
        &batch.adapter_id == self.adapter_id
            && batch.request_id == self.request_id
            && batch.host_generation == self.host_generation
            && batch.cycle_id == reply.cycle_id
            && group == reply.group
            && page == reply.page
    }
}

fn failure_label(kind: AdapterFailureKind) -> &'static str {
    match kind {
        AdapterFailureKind::TimedOut => "timed_out",
        AdapterFailureKind::HostUnavailable => "host_unavailable",
        AdapterFailureKind::Protocol => "protocol",
        AdapterFailureKind::CapacityExceeded => "capacity_exceeded",
        AdapterFailureKind::Quarantined => "quarantined",
        AdapterFailureKind::CircuitOpen => "circuit_open",
        AdapterFailureKind::Shutdown => "shutdown",
    }
}
