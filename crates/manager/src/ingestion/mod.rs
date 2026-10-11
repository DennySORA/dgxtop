//! IngestionService: the single logical writer of telemetry (AT05).
//!
//! * A completed read cycle (all pages) is routed to every collector whose
//!   `consumes(adapter, base_group)` is true.
//! * Each collector has at most one job in flight and one un-acknowledged
//!   delta. Cycles arriving meanwhile wait in a bounded backlog and are
//!   coalesced into the next job's inputs; inputs of an older epoch are
//!   dropped, expired inputs and overflow become gaps.
//! * The plan's delta base is the committed state from the latest snapshot;
//!   `next_state` becomes the base only after the state acknowledges the
//!   commit. A stale-base/epoch rejection re-queues the inputs so they are
//!   collected again on the fresh base (bounded retries).
//! * Lost jobs/acks time out so a collector can never stay blocked.
//! * The groups of one host arrive back to back within a few milliseconds;
//!   inputs from a host that is still reading wait at most
//!   [`COALESCE_HOLD_NS`] so the rest of its cycle joins the same job
//!   (one collection instead of one per group) without delaying the data of
//!   a slow or hung host beyond that bound.

pub(crate) mod pages;

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use dgxtop_core::canonical::{
    AckStatus, CatalogSnapshot, CollectionBatch, CollectionLimits, CollectionPlan, CommitGuard,
    GapRecord, PolicySnapshot, StoreAck,
};
use dgxtop_core::clock::{NANOS_PER_MILLI, NANOS_PER_SECOND};
use dgxtop_core::config::{Config, SamplingConfig};
use dgxtop_core::identity::{AdapterId, BootId, CollectorId, HostId};
use dgxtop_core::ports::{
    Collector, Dispatch, DispatchError, IngressBatch, SnapshotReader, StateJob,
};
use uuid::Uuid;

/// Re-collections of the same inputs after stale rejections.
const MAX_INPUT_RETRIES: u8 = 2;
/// Retry delay after the collector pool answered `Busy`.
const DISPATCH_RETRY_NS: u64 = 20 * NANOS_PER_MILLI;
/// Lower bound of the safety timeout of one collector job or state ack.
const MIN_JOB_TIMEOUT_NS: u64 = 10 * NANOS_PER_SECOND;
/// Collection service samples kept for the overload p95.
const SERVICE_SAMPLES: usize = 32;
/// Longest wait of an input for the rest of its host's cycle.
pub(crate) const COALESCE_HOLD_NS: u64 = 25 * NANOS_PER_MILLI;
/// One completed read cycle (every page) of one adapter group.
#[derive(Debug, Clone)]
pub(crate) struct CycleInput {
    pub(crate) adapter_id: AdapterId,
    pub(crate) group: String,
    pub(crate) cycle_id: u64,
    pub(crate) epoch: u64,
    pub(crate) batches: Vec<Arc<IngressBatch>>,
    pub(crate) expires_ns: u64,
    /// When the cycle reached the collector's backlog.
    pub(crate) arrived_ns: u64,
    retries: u8,
}

impl CycleInput {
    pub(crate) fn new(
        adapter_id: AdapterId,
        group: String,
        cycle_id: u64,
        epoch: u64,
        batches: Vec<Arc<IngressBatch>>,
        expires_ns: u64,
    ) -> Self {
        Self {
            adapter_id,
            group,
            cycle_id,
            epoch,
            batches,
            expires_ns,
            arrived_ns: 0,
            retries: 0,
        }
    }

    /// Latest data time of the cycle (`finished_boot_ns`).
    pub(crate) fn data_ns(&self) -> u64 {
        self.batches
            .iter()
            .map(|batch| batch.finished_boot_ns)
            .max()
            .unwrap_or(0)
    }

    fn started_ns(&self) -> u64 {
        self.batches
            .iter()
            .map(|batch| batch.started_boot_ns)
            .min()
            .unwrap_or(0)
    }

    pub(crate) fn gap(&self, reason: &str) -> GapRecord {
        let start = self.started_ns();
        GapRecord {
            key: None,
            adapter_id: Some(self.adapter_id.clone()),
            start_boot_ns: start,
            end_boot_ns: self.data_ns().max(start),
            reason: reason.to_owned(),
            affected_count: pages::record_count(&self.batches),
        }
    }
}

#[derive(Debug)]
struct InflightJob {
    batch_id: Uuid,
    inputs: Vec<CycleInput>,
    dispatched_ns: u64,
    deadline_ns: u64,
}

#[derive(Debug)]
enum SlotPhase {
    Idle,
    Collecting(InflightJob),
    AwaitingAck(InflightJob),
}

struct CollectorSlot {
    collector: Arc<dyn Collector>,
    id: CollectorId,
    phase: SlotPhase,
    pending: VecDeque<CycleInput>,
}

impl CollectorSlot {
    fn is_idle(&self) -> bool {
        matches!(self.phase, SlotPhase::Idle)
    }

    fn job_deadline(&self) -> Option<u64> {
        match &self.phase {
            SlotPhase::Idle => None,
            SlotPhase::Collecting(job) | SlotPhase::AwaitingAck(job) => Some(job.deadline_ns),
        }
    }

    /// End of the coalescing wait of an idle slot's backlog.
    fn hold_deadline(&self) -> Option<u64> {
        if !self.is_idle() {
            return None;
        }
        self.pending
            .iter()
            .map(|input| input.arrived_ns)
            .min()
            .map(|oldest| oldest.saturating_add(COALESCE_HOLD_NS))
    }

    /// Whether the backlog waits for more groups of a host still reading.
    fn holds_for(&self, busy_adapters: &BTreeSet<AdapterId>, now_ns: u64) -> bool {
        self.hold_deadline()
            .is_some_and(|deadline| now_ns < deadline)
            && self
                .pending
                .iter()
                .any(|input| busy_adapters.contains(&input.adapter_id))
    }
}

/// Per-pump inputs owned by other services.
#[derive(Debug, Clone)]
pub(crate) struct PumpContext {
    /// Adapters with a read in flight (more of their cycle may follow).
    pub(crate) busy_adapters: BTreeSet<AdapterId>,
    pub(crate) epoch: u64,
    pub(crate) config_revision: u32,
    pub(crate) catalog: Arc<CatalogSnapshot>,
    pub(crate) policy: Arc<PolicySnapshot>,
}

/// What a pump did.
#[derive(Debug, Default)]
pub(crate) struct PumpReport {
    pub(crate) dispatched: u32,
    pub(crate) stale_inputs: u32,
    pub(crate) gaps: Vec<GapRecord>,
}

/// Outcome of a `Collected` completion.
#[derive(Debug)]
pub(crate) enum CollectedOutcome {
    Committing,
    /// Unknown batch id or not in the collecting phase.
    Stale,
    /// Produced under an older epoch; discarded without commit.
    EpochDiscarded,
    /// Failed validation or the state port refused it; data lost as gaps.
    Dropped {
        reason: String,
        gaps: Vec<GapRecord>,
    },
}

/// Outcome of a state acknowledgement.
#[derive(Debug)]
pub(crate) enum CommitOutcome {
    Committed,
    /// Stale epoch/base: inputs re-queued for the fresh base.
    Retrying {
        gaps: Vec<GapRecord>,
    },
    Rejected {
        status: AckStatus,
        gaps: Vec<GapRecord>,
    },
    Stale,
}

/// Ports owned by the ingestion service.
pub(crate) struct IngestionPorts {
    pub(crate) collectors: Box<dyn Dispatch<CollectionPlan>>,
    pub(crate) state: Box<dyn Dispatch<StateJob>>,
    pub(crate) snapshots: Arc<dyn SnapshotReader>,
    pub(crate) registry: Vec<Arc<dyn Collector>>,
}

/// Static settings of the ingestion service.
#[derive(Debug, Clone)]
pub(crate) struct IngestionSettings {
    pub(crate) host_id: HostId,
    pub(crate) boot_id: BootId,
    pub(crate) limits: CollectionLimits,
    pub(crate) max_pending_per_collector: usize,
    pub(crate) job_timeout_ns: u64,
}

pub(crate) struct IngestionService {
    collectors: Box<dyn Dispatch<CollectionPlan>>,
    state: Box<dyn Dispatch<StateJob>>,
    snapshots: Arc<dyn SnapshotReader>,
    slots: Vec<CollectorSlot>,
    settings: IngestionSettings,
    pages: pages::PageAccumulator,
    service_ns: VecDeque<u64>,
    retry_at_ns: Option<u64>,
    /// A sampling change the state has not accepted yet.
    pending_sampling: Option<SamplingConfig>,
    stopped: bool,
}

impl IngestionService {
    pub(crate) fn new(ports: IngestionPorts, settings: IngestionSettings) -> Self {
        let slots = ports
            .registry
            .into_iter()
            .map(|collector| CollectorSlot {
                id: collector.id().clone(),
                collector,
                phase: SlotPhase::Idle,
                pending: VecDeque::new(),
            })
            .collect();
        Self {
            collectors: ports.collectors,
            state: ports.state,
            snapshots: ports.snapshots,
            slots,
            settings: IngestionSettings {
                max_pending_per_collector: settings.max_pending_per_collector.max(1),
                job_timeout_ns: settings.job_timeout_ns.max(MIN_JOB_TIMEOUT_NS),
                ..settings
            },
            pages: pages::PageAccumulator::default(),
            service_ns: VecDeque::new(),
            retry_at_ns: None,
            pending_sampling: None,
            stopped: false,
        }
    }

    pub(crate) fn snapshots(&self) -> &Arc<dyn SnapshotReader> {
        &self.snapshots
    }

    pub(crate) fn pages(&mut self) -> &mut pages::PageAccumulator {
        &mut self.pages
    }

    /// Queues a completed cycle for every consuming collector. Returns the
    /// number of consumers and the gaps of backlog entries pushed out.
    pub(crate) fn route(&mut self, input: CycleInput) -> (usize, Vec<GapRecord>) {
        let mut gaps = Vec::new();
        if self.stopped {
            return (0, gaps);
        }
        let limit = self.settings.max_pending_per_collector;
        let mut consumers = 0;
        for slot in &mut self.slots {
            if !slot.collector.consumes(&input.adapter_id, &input.group) {
                continue;
            }
            consumers += 1;
            if slot.pending.len() >= limit
                && let Some(oldest) = slot.pending.pop_front()
            {
                gaps.push(oldest.gap("collector_backlog_full"));
            }
            slot.pending.push_back(input.clone());
        }
        (consumers, gaps)
    }

    /// Dispatches one coalesced job to every idle collector with inputs.
    pub(crate) fn pump(&mut self, now_ns: u64, context: &PumpContext) -> PumpReport {
        let mut report = PumpReport::default();
        if self.stopped || self.retry_at_ns.is_some_and(|retry_at| now_ns < retry_at) {
            return report;
        }
        self.retry_at_ns = None;
        let snapshot = self.snapshots.latest();
        for index in 0..self.slots.len() {
            let Some(slot) = self.slots.get_mut(index) else {
                continue;
            };
            if !slot.is_idle()
                || slot.pending.is_empty()
                || slot.holds_for(&context.busy_adapters, now_ns)
            {
                continue;
            }
            let inputs = take_live_inputs(slot, now_ns, context.epoch, &mut report);
            if inputs.is_empty() {
                continue;
            }
            let (state_revision, state) = snapshot.collector_state(&slot.id);
            let plan = CollectionPlan {
                collector_id: slot.id.clone(),
                batch_id: Uuid::new_v4(),
                cycle_id: inputs.iter().map(|input| input.cycle_id).max().unwrap_or(0),
                epoch: context.epoch,
                config_revision: context.config_revision,
                host_id: self.settings.host_id.clone(),
                boot_id: self.settings.boot_id.clone(),
                catalog: Arc::clone(&context.catalog),
                policy: Arc::clone(&context.policy),
                state,
                state_revision,
                inputs: inputs
                    .iter()
                    .flat_map(|input| input.batches.iter().cloned())
                    .collect(),
                now_boot_ns: now_ns,
                limits: self.settings.limits,
            };
            let batch_id = plan.batch_id;
            match self.collectors.try_dispatch(plan) {
                Ok(()) => {
                    report.dispatched = report.dispatched.saturating_add(1);
                    slot.phase = SlotPhase::Collecting(InflightJob {
                        batch_id,
                        inputs,
                        dispatched_ns: now_ns,
                        deadline_ns: now_ns.saturating_add(self.settings.job_timeout_ns),
                    });
                }
                Err(DispatchError::Closed) => {
                    report.gaps.extend(
                        inputs
                            .iter()
                            .map(|input| input.gap("collector_pool_closed")),
                    );
                }
                Err(_) => {
                    for input in inputs.into_iter().rev() {
                        slot.pending.push_front(input);
                    }
                    self.retry_at_ns = Some(now_ns.saturating_add(DISPATCH_RETRY_NS));
                    break;
                }
            }
        }
        report
    }

    /// A collector job finished; forwards the batch to the state.
    pub(crate) fn on_collected(
        &mut self,
        batch: Box<CollectionBatch>,
        now_ns: u64,
        epoch: u64,
    ) -> CollectedOutcome {
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.id == batch.collector_id)
        else {
            return CollectedOutcome::Stale;
        };
        let matches =
            matches!(&slot.phase, SlotPhase::Collecting(job) if job.batch_id == batch.batch_id);
        if !matches {
            return CollectedOutcome::Stale;
        }
        let SlotPhase::Collecting(mut job) = std::mem::replace(&mut slot.phase, SlotPhase::Idle)
        else {
            return CollectedOutcome::Stale;
        };
        if batch.epoch != epoch {
            return CollectedOutcome::EpochDiscarded;
        }
        if let Err(error) = batch.validate() {
            let gaps = job
                .inputs
                .iter()
                .map(|input| input.gap("collector_output_invalid"))
                .collect();
            return CollectedOutcome::Dropped {
                reason: error.to_string(),
                gaps,
            };
        }
        let guard = CommitGuard {
            expected_epoch: epoch,
            collector_id: batch.collector_id.clone(),
            state_base_revision: batch.state_base_revision,
        };
        match self.state.try_dispatch(StateJob::Commit { batch, guard }) {
            Ok(()) => {
                job.deadline_ns = now_ns.saturating_add(self.settings.job_timeout_ns);
                slot.phase = SlotPhase::AwaitingAck(job);
                CollectedOutcome::Committing
            }
            Err(error) => CollectedOutcome::Dropped {
                reason: format!("store unavailable: {error}"),
                gaps: job
                    .inputs
                    .iter()
                    .map(|input| input.gap("store_unavailable"))
                    .collect(),
            },
        }
    }

    /// The state acknowledged (or rejected) a commit.
    pub(crate) fn on_committed(&mut self, ack: &StoreAck, now_ns: u64) -> CommitOutcome {
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.id == ack.collector_id)
        else {
            return CommitOutcome::Stale;
        };
        let matches =
            matches!(&slot.phase, SlotPhase::AwaitingAck(job) if job.batch_id == ack.batch_id);
        if !matches {
            return CommitOutcome::Stale;
        }
        let SlotPhase::AwaitingAck(job) = std::mem::replace(&mut slot.phase, SlotPhase::Idle)
        else {
            return CommitOutcome::Stale;
        };
        record_service(
            &mut self.service_ns,
            now_ns.saturating_sub(job.dispatched_ns),
        );
        if ack.status.is_committed() {
            return CommitOutcome::Committed;
        }
        match ack.status {
            AckStatus::RejectedStaleBase | AckStatus::RejectedStaleEpoch => {
                CommitOutcome::Retrying {
                    gaps: requeue(slot, job.inputs),
                }
            }
            status => CommitOutcome::Rejected {
                status,
                gaps: job
                    .inputs
                    .iter()
                    .map(|input| input.gap("commit_rejected"))
                    .collect(),
            },
        }
    }

    /// A collector job failed; its inputs are lost as gaps.
    pub(crate) fn on_collector_failed(
        &mut self,
        collector_id: &CollectorId,
        batch_id: Uuid,
    ) -> Option<Vec<GapRecord>> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| &slot.id == collector_id)?;
        let matches = matches!(&slot.phase, SlotPhase::Collecting(job) if job.batch_id == batch_id);
        if !matches {
            return None;
        }
        let SlotPhase::Collecting(job) = std::mem::replace(&mut slot.phase, SlotPhase::Idle) else {
            return None;
        };
        Some(
            job.inputs
                .iter()
                .map(|input| input.gap("collector_failed"))
                .collect(),
        )
    }

    /// Frees collectors whose job or ack never came back.
    pub(crate) fn expire_jobs(&mut self, now_ns: u64) -> Vec<GapRecord> {
        let mut gaps = Vec::new();
        for slot in &mut self.slots {
            if slot.job_deadline().is_none_or(|deadline| now_ns < deadline) {
                continue;
            }
            let reason = match &slot.phase {
                SlotPhase::Collecting(_) => "collector_job_timeout",
                _ => "store_ack_timeout",
            };
            if let SlotPhase::Collecting(job) | SlotPhase::AwaitingAck(job) =
                std::mem::replace(&mut slot.phase, SlotPhase::Idle)
            {
                gaps.extend(job.inputs.iter().map(|input| input.gap(reason)));
            }
        }
        gaps
    }

    /// Sends a gap record to the state.
    pub(crate) fn record_gap(&self, gap: GapRecord) -> Result<(), DispatchError> {
        self.state.try_dispatch(StateJob::Gap(gap))
    }

    /// Asks the state actor for retention/quota maintenance.
    pub(crate) fn request_maintenance(&self) -> Result<(), DispatchError> {
        self.state.try_dispatch(StateJob::Maintain)
    }

    /// Tells the state about a durable sampling change. A busy state keeps
    /// the change pending for [`Self::retry_sampling`]; dropping it would
    /// leave the state on the old timing for the rest of the run.
    pub(crate) fn apply_sampling(&mut self, sampling: SamplingConfig) -> Result<(), DispatchError> {
        self.pending_sampling = Some(sampling);
        self.retry_sampling()
    }

    /// Sends the pending sampling change, if any.
    pub(crate) fn retry_sampling(&mut self) -> Result<(), DispatchError> {
        let Some(sampling) = self.pending_sampling.take() else {
            return Ok(());
        };
        self.state
            .try_dispatch(StateJob::ApplySampling(sampling.clone()))
            .inspect_err(|_| self.pending_sampling = Some(sampling))
    }

    /// Stops routing and dispatching (shutdown); backlogs are released.
    pub(crate) fn stop(&mut self) {
        self.stopped = true;
        self.pages.clear();
        for slot in &mut self.slots {
            slot.pending.clear();
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<u64> {
        self.slots
            .iter()
            .filter_map(CollectorSlot::job_deadline)
            .chain(self.slots.iter().filter_map(CollectorSlot::hold_deadline))
            .chain(self.retry_at_ns.filter(|_| self.has_backlog()))
            .min()
    }

    /// Whether a collector is working or has inputs waiting: more commits
    /// of the current cycle are on their way.
    pub(crate) fn is_collecting(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| !slot.is_idle() || !slot.pending.is_empty())
    }

    /// Highest backlog fill ratio over all collectors.
    pub(crate) fn backlog_utilization(&self) -> f64 {
        let limit = self.settings.max_pending_per_collector;
        self.slots
            .iter()
            .map(|slot| ratio(slot.pending.len(), limit))
            .fold(0.0, f64::max)
    }

    /// Whether the collection service p95 exceeds `period_ns`.
    pub(crate) fn service_overrun(&self, period_ns: u64) -> bool {
        if self.service_ns.len() < 3 {
            return false;
        }
        let mut sorted: Vec<u64> = self.service_ns.iter().copied().collect();
        sorted.sort_unstable();
        let rank = (sorted.len() * 95).div_ceil(100).saturating_sub(1);
        sorted.get(rank).is_some_and(|p95| *p95 > period_ns)
    }

    fn has_backlog(&self) -> bool {
        self.slots.iter().any(|slot| !slot.pending.is_empty())
    }
}

/// Collector bounds taken from `config.store`.
pub(crate) fn collection_limits(config: &Config) -> CollectionLimits {
    CollectionLimits {
        max_series: config.store.max_series,
        max_sources: config.store.max_sources,
        max_candidates_per_series: config.store.max_candidates_per_series,
        max_processes: config.store.max_processes,
        max_process_gpu_links: config.store.max_process_gpu_links,
    }
}

/// Removes the backlog of a slot, dropping inputs of older epochs (stale)
/// and expired ones (gap).
fn take_live_inputs(
    slot: &mut CollectorSlot,
    now_ns: u64,
    epoch: u64,
    report: &mut PumpReport,
) -> Vec<CycleInput> {
    let mut live = Vec::new();
    while let Some(input) = slot.pending.pop_front() {
        if input.epoch != epoch {
            report.stale_inputs = report.stale_inputs.saturating_add(1);
        } else if now_ns >= input.expires_ns {
            report.gaps.push(input.gap("collector_backlog_expired"));
        } else {
            live.push(input);
        }
    }
    live
}

/// Puts the inputs of a stale-rejected job back in front of the backlog.
fn requeue(slot: &mut CollectorSlot, inputs: Vec<CycleInput>) -> Vec<GapRecord> {
    let mut gaps = Vec::new();
    for mut input in inputs.into_iter().rev() {
        if input.retries >= MAX_INPUT_RETRIES {
            gaps.push(input.gap("commit_retries_exhausted"));
            continue;
        }
        input.retries += 1;
        slot.pending.push_front(input);
    }
    gaps
}

fn record_service(samples: &mut VecDeque<u64>, duration_ns: u64) {
    if samples.len() == SERVICE_SAMPLES {
        samples.pop_front();
    }
    samples.push_back(duration_ns);
}

#[allow(clippy::cast_precision_loss)]
fn ratio(used: usize, limit: usize) -> f64 {
    if limit == 0 {
        return 1.0;
    }
    used as f64 / limit as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeCollector, FakeSnapshots, ingress_page};
    use dgxtop_core::canonical::{ArchiveState, CollectorState};
    use dgxtop_core::quality::Namespace;
    use dgxtop_core::testing::{CapturingDispatch, adapter_id, boot_id, host_id};
    use std::collections::BTreeMap;

    const SECOND: u64 = NANOS_PER_SECOND;

    struct Bench {
        service: IngestionService,
        plans: CapturingDispatch<CollectionPlan>,
        state: CapturingDispatch<StateJob>,
        snapshots: Arc<FakeSnapshots>,
    }

    fn bench(max_pending: usize) -> Bench {
        let plans = CapturingDispatch::new();
        let state = CapturingDispatch::new();
        let snapshots = Arc::new(FakeSnapshots::default());
        let config = Config::defaults().unwrap();
        let service = IngestionService::new(
            IngestionPorts {
                collectors: Box::new(plans.clone()),
                state: Box::new(state.clone()),
                snapshots: snapshots.clone(),
                registry: vec![
                    Arc::new(FakeCollector::new(
                        "cpu",
                        &[("procfs", "cpu"), ("hwmon", "fast")],
                    )),
                    Arc::new(FakeCollector::new("process", &[("procfs", "process")])),
                ],
            },
            IngestionSettings {
                host_id: host_id("test-host"),
                boot_id: boot_id("fake-boot"),
                limits: collection_limits(&config),
                max_pending_per_collector: max_pending,
                job_timeout_ns: 10 * SECOND,
            },
        );
        Bench {
            service,
            plans,
            state,
            snapshots,
        }
    }

    fn context(epoch: u64) -> PumpContext {
        context_with_busy(epoch, &[])
    }

    fn context_with_busy(epoch: u64, busy: &[&str]) -> PumpContext {
        let config = Config::defaults().unwrap();
        PumpContext {
            busy_adapters: busy.iter().map(|adapter| adapter_id(adapter)).collect(),
            epoch,
            config_revision: 1,
            catalog: Arc::new(CatalogSnapshot::default()),
            policy: crate::policy::QualityPolicyService::new(
                &config,
                BTreeMap::new(),
                Namespace::Live,
            )
            .snapshot(),
        }
    }

    fn cycle(adapter: &str, group: &str, cycle_id: u64, epoch: u64, pages: usize) -> CycleInput {
        let batches = (0..pages)
            .map(|_| ingress_page(adapter, group, cycle_id, 2, cycle_id * SECOND))
            .collect();
        CycleInput::new(
            adapter_id(adapter),
            group.into(),
            cycle_id,
            epoch,
            batches,
            u64::MAX,
        )
    }

    fn collected(plan: &CollectionPlan) -> Box<CollectionBatch> {
        Box::new(CollectionBatch {
            batch_id: plan.batch_id,
            collector_id: plan.collector_id.clone(),
            cycle_id: plan.cycle_id,
            epoch: plan.epoch,
            config_revision: plan.config_revision,
            catalog_revision: plan.catalog.revision,
            policy_revision: plan.policy.revision,
            state_base_revision: plan.state_revision,
            canonical: Vec::new(),
            explanations: Vec::new(),
            counter_intervals: Vec::new(),
            inputs: plan.inputs.clone(),
            process_table: None,
            next_state: CollectorState::default(),
            failures: Vec::new(),
        })
    }

    fn ack(plan: &CollectionPlan, status: AckStatus, state_revision: u64) -> StoreAck {
        StoreAck {
            batch_id: plan.batch_id,
            collector_id: plan.collector_id.clone(),
            status,
            committed_revision: 9,
            state_revision,
            archive: ArchiveState::Disabled,
            reason: None,
        }
    }

    #[test]
    fn a_cycle_of_pages_becomes_one_job_per_consuming_collector() {
        let mut bench = bench(8);
        let (consumers, gaps) = bench.service.route(cycle("procfs", "cpu", 1, 1, 3));
        assert_eq!(consumers, 1);
        assert!(gaps.is_empty());
        let report = bench.service.pump(SECOND, &context(1));
        assert_eq!(report.dispatched, 1);
        let plans = bench.plans.take();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].collector_id.as_str(), "cpu");
        assert_eq!(plans[0].inputs.len(), 3, "all pages in one job");
        assert_eq!(plans[0].epoch, 1);
        assert_eq!(plans[0].state_revision, 0);
        // Unconsumed groups route nowhere.
        assert_eq!(bench.service.route(cycle("nvml", "fast", 1, 1, 1)).0, 0);
    }

    fn arriving(mut input: CycleInput, arrived_ns: u64) -> CycleInput {
        input.arrived_ns = arrived_ns;
        input
    }

    #[test]
    fn the_wait_for_a_busy_adapter_is_bounded() {
        const MS: u64 = NANOS_PER_MILLI;
        let mut held = bench(8);
        held.service
            .route(arriving(cycle("procfs", "cpu", 1, 1, 1), SECOND));
        let busy = context_with_busy(1, &["procfs"]);
        assert_eq!(
            held.service
                .pump(SECOND + COALESCE_HOLD_NS - 1, &busy)
                .dispatched,
            0
        );
        // A slow or hung adapter never holds other data past the bound.
        assert_eq!(
            held.service
                .pump(SECOND + COALESCE_HOLD_NS, &busy)
                .dispatched,
            1
        );
        // Inputs of idle adapters are never held.
        let mut idle = bench(8);
        idle.service
            .route(arriving(cycle("hwmon", "fast", 1, 1, 1), SECOND));
        assert_eq!(idle.service.pump(SECOND + MS, &busy).dispatched, 1);
    }

    #[test]
    fn one_job_in_flight_and_backlog_coalesces_into_the_next_job() {
        let mut bench = bench(8);
        bench.service.route(cycle("procfs", "cpu", 1, 1, 1));
        bench.service.pump(SECOND, &context(1));
        let first = bench.plans.take().pop().unwrap();
        // Busy collector: two more cycles wait; nothing else is dispatched.
        bench.service.route(cycle("procfs", "cpu", 2, 1, 1));
        bench.service.route(cycle("hwmon", "fast", 2, 1, 1));
        assert_eq!(bench.service.pump(2 * SECOND, &context(1)).dispatched, 0);
        assert!(bench.plans.is_empty());
        // Collected -> commit with the epoch/base guard; still one un-acked delta.
        assert!(matches!(
            bench.service.on_collected(collected(&first), 2 * SECOND, 1),
            CollectedOutcome::Committing
        ));
        assert_eq!(bench.service.pump(2 * SECOND, &context(1)).dispatched, 0);
        let jobs = bench.state.take();
        let StateJob::Commit { guard, .. } = &jobs[0] else {
            panic!("commit expected");
        };
        assert_eq!(guard.expected_epoch, 1);
        assert_eq!(guard.state_base_revision, 0);
        // Ack: the state holds next_state; the next job uses the new base.
        bench.snapshots.set_collector_state("cpu", 1);
        let outcome = bench
            .service
            .on_committed(&ack(&first, AckStatus::Committed, 1), 3 * SECOND);
        assert!(matches!(outcome, CommitOutcome::Committed));
        bench.service.pump(3 * SECOND, &context(1));
        let next = bench.plans.take().pop().unwrap();
        assert_eq!(next.inputs.len(), 2, "both waiting cycles coalesced");
        assert_eq!(next.cycle_id, 2);
        assert_eq!(next.state_revision, 1, "state adopted only after the ack");
    }

    #[test]
    fn stale_base_rejection_retries_with_the_fresh_base() {
        let mut bench = bench(8);
        bench.service.route(cycle("procfs", "cpu", 1, 1, 1));
        bench.service.pump(SECOND, &context(1));
        let first = bench.plans.take().pop().unwrap();
        bench.service.on_collected(collected(&first), SECOND, 1);
        bench.state.take();
        // Another commit advanced the stored state meanwhile.
        bench.snapshots.set_collector_state("cpu", 5);
        let outcome = bench
            .service
            .on_committed(&ack(&first, AckStatus::RejectedStaleBase, 5), 2 * SECOND);
        assert!(matches!(outcome, CommitOutcome::Retrying { .. }));
        bench.service.pump(2 * SECOND, &context(1));
        let retry = bench.plans.take().pop().unwrap();
        assert_eq!(retry.state_revision, 5);
        assert_eq!(retry.inputs.len(), 1);
        assert_ne!(retry.batch_id, first.batch_id);
        // Retries are bounded: after the limit the inputs become a gap.
        let mut current = retry;
        for _ in 0..MAX_INPUT_RETRIES {
            bench
                .service
                .on_collected(collected(&current), 3 * SECOND, 1);
            bench.state.take();
            let outcome = bench
                .service
                .on_committed(&ack(&current, AckStatus::RejectedStaleBase, 5), 3 * SECOND);
            match outcome {
                CommitOutcome::Retrying { gaps, .. } if gaps.is_empty() => {
                    bench.service.pump(3 * SECOND, &context(1));
                    current = bench.plans.take().pop().unwrap();
                }
                CommitOutcome::Retrying { gaps, .. } => {
                    assert_eq!(gaps[0].reason, "commit_retries_exhausted");
                    return;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        panic!("retries were not bounded");
    }

    #[test]
    fn stale_and_unknown_completions_are_ignored() {
        let mut bench = bench(8);
        bench.service.route(cycle("procfs", "cpu", 1, 1, 1));
        bench.service.pump(SECOND, &context(1));
        let plan = bench.plans.take().pop().unwrap();
        let mut foreign = collected(&plan);
        foreign.batch_id = Uuid::new_v4();
        assert!(matches!(
            bench.service.on_collected(foreign, SECOND, 1),
            CollectedOutcome::Stale
        ));
        assert!(matches!(
            bench
                .service
                .on_committed(&ack(&plan, AckStatus::Committed, 1), SECOND),
            CommitOutcome::Stale
        ));
        // A batch built under an older epoch is discarded, never committed.
        assert!(matches!(
            bench.service.on_collected(collected(&plan), SECOND, 2),
            CollectedOutcome::EpochDiscarded
        ));
        assert!(bench.state.is_empty());
    }

    #[test]
    fn old_epoch_inputs_never_reach_a_job_and_expired_ones_become_gaps() {
        let mut bench = bench(8);
        bench.service.route(cycle("procfs", "cpu", 1, 1, 1));
        let mut expiring = cycle("procfs", "cpu", 2, 2, 1);
        expiring.expires_ns = 5 * SECOND;
        bench.service.route(expiring);
        bench.service.route(cycle("procfs", "cpu", 3, 2, 1));
        let report = bench.service.pump(6 * SECOND, &context(2));
        assert_eq!(report.stale_inputs, 1);
        assert_eq!(report.gaps.len(), 1);
        assert_eq!(report.gaps[0].reason, "collector_backlog_expired");
        let plan = bench.plans.take().pop().unwrap();
        assert_eq!(plan.cycle_id, 3);
        assert_eq!(plan.inputs.len(), 1);
    }

    #[test]
    fn backlog_is_bounded_and_overflow_is_a_gap() {
        let mut bench = bench(2);
        bench.service.route(cycle("procfs", "process", 1, 1, 1));
        bench.service.pump(SECOND, &context(1));
        bench.service.route(cycle("procfs", "process", 2, 1, 1));
        bench.service.route(cycle("procfs", "process", 3, 1, 1));
        assert!((bench.service.backlog_utilization() - 1.0).abs() < f64::EPSILON);
        let (_, gaps) = bench.service.route(cycle("procfs", "process", 4, 1, 1));
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].reason, "collector_backlog_full");
        assert_eq!(gaps[0].affected_count, 2);
    }

    #[test]
    fn busy_pool_keeps_inputs_and_retries() {
        let mut bench = bench(8);
        bench.plans.set_busy(true);
        bench.service.route(cycle("procfs", "cpu", 1, 1, 1));
        assert_eq!(bench.service.pump(SECOND, &context(1)).dispatched, 0);
        assert!(bench.service.next_deadline().is_some());
        bench.plans.set_busy(false);
        // Too early for the retry.
        assert_eq!(bench.service.pump(SECOND, &context(1)).dispatched, 0);
        assert_eq!(
            bench
                .service
                .pump(SECOND + DISPATCH_RETRY_NS, &context(1))
                .dispatched,
            1
        );
    }

    #[test]
    fn failures_and_timeouts_free_the_collector() {
        let mut bench = bench(8);
        bench.service.route(cycle("procfs", "cpu", 1, 1, 1));
        bench.service.pump(SECOND, &context(1));
        let plan = bench.plans.take().pop().unwrap();
        let gaps = bench
            .service
            .on_collector_failed(&plan.collector_id, plan.batch_id)
            .unwrap();
        assert_eq!(gaps[0].reason, "collector_failed");
        bench.service.route(cycle("procfs", "cpu", 2, 1, 1));
        bench.service.pump(2 * SECOND, &context(1));
        assert_eq!(bench.plans.take().len(), 1);
        let gaps = bench.service.expire_jobs(13 * SECOND);
        assert_eq!(gaps[0].reason, "collector_job_timeout");
        assert!(bench.service.next_deadline().is_none());
    }

    #[test]
    fn store_refusal_drops_the_batch_as_gaps() {
        let mut bench = bench(8);
        bench.service.route(cycle("procfs", "cpu", 1, 1, 1));
        bench.service.pump(SECOND, &context(1));
        let plan = bench.plans.take().pop().unwrap();
        bench.state.set_busy(true);
        let CollectedOutcome::Dropped { gaps, .. } =
            bench.service.on_collected(collected(&plan), SECOND, 1)
        else {
            panic!("store refusal drops");
        };
        assert_eq!(gaps[0].reason, "store_unavailable");
        assert!(bench.service.record_gap(gaps[0].clone()).is_err());
        bench.state.set_busy(false);
        assert!(bench.service.request_maintenance().is_ok());
    }

    #[test]
    fn service_time_p95_detects_overrun_and_stop_releases_backlog() {
        let mut bench = bench(8);
        for index in 0..5u64 {
            bench.service.route(cycle("procfs", "cpu", index + 1, 1, 1));
            bench.service.pump(index * SECOND, &context(1));
            let plan = bench.plans.take().pop().unwrap();
            bench
                .service
                .on_collected(collected(&plan), index * SECOND, 1);
            bench.state.take();
            bench.service.on_committed(
                &ack(&plan, AckStatus::Committed, 1),
                index * SECOND + 2 * SECOND,
            );
        }
        assert!(bench.service.service_overrun(SECOND));
        assert!(!bench.service.service_overrun(3 * SECOND));
        bench.service.route(cycle("procfs", "cpu", 99, 1, 1));
        bench.service.stop();
        assert_eq!(bench.service.backlog_utilization(), 0.0);
        assert_eq!(bench.service.route(cycle("procfs", "cpu", 100, 1, 1)).0, 0);
        assert_eq!(bench.service.pump(10 * SECOND, &context(1)).dispatched, 0);
    }
}
