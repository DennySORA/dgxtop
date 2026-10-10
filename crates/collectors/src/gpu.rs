//! GPU collector: per-GPU (`gpu:<UUID>`) gauges through arbitration with
//! explicit normalization (mW -> W, mJ -> J), cumulative energy with exact
//! `CounterInterval`s, ECC/retired counts, and the per-GPU sum of visible
//! process allocations. PCIe readings with an unverified unit stay without a
//! value; the memory bandwidth estimate is never produced (no validated
//! model, never busy% x peak).

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::canonical::{CollectionBatch, CollectionPlan, RejectReason};
use dgxtop_core::identity::{
    AdapterId, CollectorId, EntityId, IdentityError, MetricKey, OriginRelation, Scope, SourceId,
};
use dgxtop_core::metric::{
    self, CanonicalUnit, Domain, MetricDescriptor, MetricRole, NativeUnit, Scalar, ids, native,
};
use dgxtop_core::naming;
use dgxtop_core::observation::Observation;
use dgxtop_core::ports::Collector;
use dgxtop_core::quality::{Freshness, RawKind, ReadStatus};

use crate::arbitration::{self, Candidate, CatalogMapping, Deadlines};
use crate::context::{self, CollectorSpec, Record, Route, Session};
use crate::counter_inputs::{self, Step};
use crate::formulas::{self, Selections};
use crate::gpu_processes::{self, GpuProcesses, PROCESS_GPU_METRICS};
use crate::output::Subject;
use crate::reasons;
use crate::values::{self, Evidence};

fn accepts(metric: &MetricDescriptor) -> bool {
    metric.domain == Domain::Gpu || PROCESS_GPU_METRICS.contains(&metric.id)
}

fn retains(_route: &Route<'_>, metric: &MetricDescriptor) -> bool {
    metric.id != ids::GPU_MEMORY_BANDWIDTH_ESTIMATED && metric.id != ids::PROCESS_GPU_UTILIZATION
}

static SPEC: CollectorSpec = CollectorSpec {
    id: naming::collectors::GPU,
    accepts,
    retains,
};

/// Canonical GPU metrics published straight through arbitration.
fn is_pass_through(metric_id: &str) -> bool {
    metric::descriptor(metric_id).is_some_and(|metric| {
        metric.domain == Domain::Gpu
            && metric.role == MetricRole::Canonical
            && metric.id != ids::GPU_MEMORY_PROCESS_ALLOCATED
            && metric.id != ids::GPU_MEMORY_BANDWIDTH_ESTIMATED
    })
}

/// NVIDIA GPU collector.
#[derive(Debug, Clone)]
pub struct GpuCollector {
    id: CollectorId,
}

impl GpuCollector {
    pub fn new() -> Result<Self, IdentityError> {
        Ok(Self {
            id: CollectorId::new(naming::collectors::GPU)?,
        })
    }
}

impl Collector for GpuCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn consumes(&self, adapter_id: &AdapterId, read_group: &str) -> bool {
        crate::routes_to(adapter_id, read_group, SPEC.id)
    }

    fn collect(&self, plan: &CollectionPlan) -> CollectionBatch {
        context::run(plan, &SPEC, collect_gpu)
    }
}

fn collect_gpu(session: &mut Session<'_>) {
    refuse_bandwidth_estimates(session);
    emit_energy_intervals(session);
    let mut selections = Selections::new();
    formulas::publish_pending(session, is_pass_through, &mut selections);
    publish_process_allocations(session);
}

/// No validated model exists, so no estimate is fabricated from busy% x peak.
fn refuse_bandwidth_estimates(session: &mut Session<'_>) {
    for key in
        session.pending_keys(|key| key.metric_id.as_str() == ids::GPU_MEMORY_BANDWIDTH_ESTIMATED)
    {
        session.failure(
            reasons::NO_VALIDATED_MODEL,
            &format!(
                "{}: memory bandwidth estimate not produced",
                key.series_selector()
            ),
            Subject::key(key),
        );
    }
}

/// Exact energy deltas (mJ -> J) for window totals; a backend reset or a
/// new generation restarts at warmup and emits nothing.
fn emit_energy_intervals(session: &mut Session<'_>) {
    for key in session.pending_keys(|key| key.metric_id.as_str() == ids::GPU_ENERGY_TOTAL) {
        for record in session.records(key) {
            if let Step::Delta {
                record,
                delta,
                conversion: Some(conversion),
            } = counter_inputs::step(session, record, Some(CanonicalUnit::Joules))
            {
                counter_inputs::emit_interval(session, key, record, &delta, conversion);
            }
        }
    }
}

/// `gpu.memory.process_allocated` per GPU: the de-duplicated sum of the
/// visible processes' allocations, arbitrated with any direct reading.
fn publish_process_allocations(session: &mut Session<'_>) {
    let (groups, invalid) = gpu_processes::group_by_gpu(session);
    for record in invalid {
        session.failure(
            reasons::INVALID_ENTITY,
            &format!("not a gpupid entity: {}", record.key().entity_id),
            Subject::source(record.source_id()),
        );
    }
    let direct = direct_keys(session);
    let mut targets: BTreeSet<EntityId> = groups
        .values()
        .filter(|group| group.has_new || group.expiring)
        .map(|group| group.gpu_entity.clone())
        .collect();
    targets.extend(
        direct
            .iter()
            .filter(|(_, key)| session.has_new(key) || session.is_expiring(key))
            .map(|(gpu, _)| gpu.clone()),
    );
    for gpu in targets {
        let group = groups.get(&gpu);
        let Some(key) = target_key(&gpu, direct.get(&gpu).copied(), group) else {
            continue;
        };
        publish_allocation(session, &key, group);
    }
}

/// Direct per-GPU readings of the metric, if an adapter reports them.
fn direct_keys<'a>(session: &Session<'a>) -> BTreeMap<EntityId, &'a MetricKey> {
    session
        .keys()
        .into_iter()
        .filter(|key| key.metric_id.as_str() == ids::GPU_MEMORY_PROCESS_ALLOCATED)
        .map(|key| (key.entity_id.clone(), key))
        .collect()
}

/// The direct reading's key when present (so both compete for one key),
/// else `gpu:<UUID>` with the GPU-wide scope.
fn target_key(
    gpu: &EntityId,
    direct: Option<&MetricKey>,
    group: Option<&GpuProcesses<'_>>,
) -> Option<MetricKey> {
    if let Some(direct) = direct {
        return Some(direct.clone());
    }
    let reference = group?.reference_key;
    values::with_series(
        reference,
        gpu.clone(),
        ids::GPU_MEMORY_PROCESS_ALLOCATED,
        Scope::new(naming::entity::SCOPE_GPU).ok()?,
    )
    .ok()
}

fn publish_allocation(
    session: &mut Session<'_>,
    key: &MetricKey,
    group: Option<&GpuProcesses<'_>>,
) {
    let sum = group
        .map(|group| sum_allocations(session, group))
        .unwrap_or_default();
    let note = sum.note();
    let synthetic = sum.observation(key);
    let mut candidates = session.candidates(key);
    candidates.extend(
        synthetic
            .as_ref()
            .map(|observation| sum.candidate(session, observation)),
    );
    if candidates.is_empty() {
        let reason = note.unwrap_or(reasons::NO_PROCESS_MEMORY_VISIBLE);
        let value = values::unavailable(
            session.context(),
            key.clone(),
            &sum.unknown_evidence,
            &[reason],
            Freshness::Unknown,
        );
        if let Some(value) = value {
            session.publish_value(value);
        }
        return;
    }
    let notes = note.into_iter().map(str::to_owned).collect();
    session.arbitrate_and_publish(key, candidates, notes);
}

/// Known per-process allocations of one GPU snapshot.
#[derive(Debug, Default)]
struct AllocationSum<'a> {
    /// Per PID: the record and bytes of its largest known reading.
    contributors: Vec<(Record<'a>, u64)>,
    /// Records of PIDs whose allocation is unknown.
    unknown_evidence: Vec<Evidence>,
    unknown_pids: usize,
    /// The process list itself is known to be incomplete.
    incomplete_list: bool,
    deadline_boot_ns: Option<u64>,
}

fn sum_allocations<'a>(session: &Session<'a>, group: &GpuProcesses<'a>) -> AllocationSum<'a> {
    let mut sum = AllocationSum {
        incomplete_list: group.incomplete,
        ..AllocationSum::default()
    };
    let Some(descriptor) = metric::descriptor(ids::PROCESS_GPU_MEMORY) else {
        return sum;
    };
    for records in group.by_pid.values() {
        let mut known: Option<(Record<'a>, u64)> = None;
        let mut visible = false;
        for record in &records.memory {
            let candidate = session.candidate(*record);
            let assessment = arbitration::assess(
                &candidate,
                record.key(),
                descriptor,
                session.policy(),
                session.now(),
            );
            match assessment.verdict {
                Ok(value) => {
                    visible = true;
                    let bytes = value.as_u64();
                    if let Some(bytes) = bytes
                        && known.is_none_or(|(_, best)| bytes > best)
                    {
                        known = Some((*record, bytes));
                    }
                }
                // A stale record belongs to an earlier list: not visible now.
                Err(RejectReason::Stale) => {}
                Err(_) => visible = true,
            }
        }
        match known {
            Some((record, bytes)) => {
                let deadline = session.deadlines(record).read_boot_ns;
                sum.deadline_boot_ns =
                    Some(sum.deadline_boot_ns.map_or(deadline, |d| d.min(deadline)));
                sum.contributors.push((record, bytes));
            }
            None if visible => {
                sum.unknown_pids += 1;
                sum.unknown_evidence.extend(
                    records
                        .memory
                        .iter()
                        .map(|record| Evidence::of(record.observation)),
                );
            }
            None => {}
        }
    }
    sum
}

impl<'a> AllocationSum<'a> {
    fn total(&self) -> Option<u64> {
        self.contributors
            .iter()
            .try_fold(0u64, |total, (_, bytes)| total.checked_add(*bytes))
    }

    fn note(&self) -> Option<&'static str> {
        if self.contributors.is_empty() {
            Some(reasons::NO_PROCESS_MEMORY_VISIBLE)
        } else if self.unknown_pids > 0 || self.incomplete_list {
            Some(reasons::PARTIAL_PROCESS_VISIBILITY)
        } else {
            None
        }
    }

    fn evidence(&self) -> Vec<Evidence> {
        self.contributors
            .iter()
            .map(|(record, _)| Evidence::of(record.observation))
            .collect()
    }

    /// Derived candidate observation for `key`: the sum over known PIDs.
    fn observation(&self, key: &MetricKey) -> Option<Observation> {
        let total = self.total()?;
        let representative = self
            .contributors
            .iter()
            .map(|(record, _)| record)
            .min_by_key(|record| record.source_id())?;
        let evidence = self.evidence();
        let mut observation = representative.observation.clone();
        observation.key = key.clone();
        observation.value = Some(Scalar::from_u64(total));
        observation.native_unit = NativeUnit::new(native::BYTES).ok()?;
        observation.read_status = ReadStatus::Ok;
        observation.sample_kind = values::derived_kind(&evidence);
        observation.quality = values::weakest_quality(&evidence);
        observation.read_started_boot_ns = self
            .contributors
            .iter()
            .map(|(record, _)| record.observation.read_started_boot_ns)
            .min()?;
        observation.read_finished_boot_ns = self
            .contributors
            .iter()
            .map(|(record, _)| record.observation.read_finished_boot_ns)
            .max()?;
        observation.device_sample_boot_ns = None;
        observation.provenance.origin_id = None;
        observation.provenance.origin_relation = OriginRelation::Unknown;
        observation.reason_codes = self.note().into_iter().map(str::to_owned).collect();
        Some(observation)
    }

    fn candidate<'o>(&self, session: &Session<'_>, observation: &'o Observation) -> Candidate<'o> {
        let mut inputs: Vec<SourceId> = self
            .contributors
            .iter()
            .map(|(record, _)| record.source_id().clone())
            .collect();
        inputs.sort();
        inputs.dedup();
        Candidate {
            observation,
            profile_rank: session
                .source_descriptor(&observation.provenance.source_id)
                .map_or(0, |descriptor| descriptor.profile_rank),
            raw_kind: RawKind::Gauge,
            mapping: CatalogMapping::Derived,
            deadlines: Deadlines {
                read_boot_ns: self.deadline_boot_ns.unwrap_or(0),
                measurement_boot_ns: None,
            },
            input_source_ids: inputs,
            previous_value: None,
        }
    }
}

#[cfg(test)]
mod tests;
