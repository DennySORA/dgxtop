//! Collector outputs and committed state: canonical values, arbitration
//! explanations, counter intervals, process tables, bounded collector state,
//! collection plans/batches, store acknowledgements and immutable snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::budget::LedgerSnapshot;
use crate::identity::{
    AdapterId, BootId, CollectorId, EntityId, HostId, MetricKey, OriginIdentity, ProcessKey,
    SourceId,
};
use crate::metric::{CanonicalUnit, NativeUnit, Scalar};
use crate::observation::{Capability, EntityRecord, Observation, SourceDescriptor};
use crate::ports::IngressBatch;
use crate::quality::{Freshness, Namespace, Quality, ReadStatus, SampleKind, SelectionState};

/// Canonical schema version.
pub const CANONICAL_SCHEMA_VERSION: u32 = 1;
/// Maximum candidate ids recorded per canonical value (schema `maxItems`).
pub const MAX_CANDIDATE_IDS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CanonicalError {
    #[error("{0}")]
    Invalid(String),
}

/// Collector output for one `MetricKey` before commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalValue {
    pub key: MetricKey,
    pub cycle_id: u64,
    pub source_id: Option<SourceId>,
    pub source_generation: Option<u32>,
    pub unit: CanonicalUnit,
    pub value: Option<Scalar>,
    pub sample_kind: SampleKind,
    pub selection_state: SelectionState,
    pub quality: Quality,
    /// Freshness of the latest successful read.
    pub freshness: Freshness,
    /// Freshness of the hardware measurement; `unknown` without device timestamps.
    pub measurement_freshness: Freshness,
    /// Confirmed hardware measurement time on the boot clock, if mappable.
    pub observed_boot_ns: Option<u64>,
    pub read_finished_boot_ns: u64,
    pub policy_revision: u32,
    pub candidate_source_ids: Vec<SourceId>,
    pub reason_codes: Vec<String>,
    pub eligible_for_history: bool,
}

impl CanonicalValue {
    /// Enforces the `canonical.schema.json` conditional rules.
    pub fn validate(&self) -> Result<(), CanonicalError> {
        let fail = |message: &str| Err(CanonicalError::Invalid(message.to_owned()));
        if self.candidate_source_ids.len() > MAX_CANDIDATE_IDS {
            return fail("too many candidate ids");
        }
        if self.reason_codes.len() > 32
            || self
                .reason_codes
                .iter()
                .any(|code| code.is_empty() || code.len() > 80)
        {
            return fail("invalid reason codes");
        }
        if self.policy_revision == 0 {
            return fail("policy_revision starts at 1");
        }
        if self.selection_state.has_value() {
            if self.value.is_none() || self.source_id.is_none() || self.source_generation.is_none()
            {
                return fail("selected/degraded values need value, source and generation");
            }
            if !self.quality.is_canonical_eligible() {
                return fail("selected/degraded values need verified or usable quality");
            }
            if self.freshness != Freshness::Fresh {
                return fail("selected/degraded values must be fresh");
            }
            if self.measurement_freshness == Freshness::Stale {
                return fail("a confirmed stale measurement cannot be authoritative");
            }
        } else if self.value.is_some() || self.source_id.is_some() || self.eligible_for_history {
            return fail(
                "conflict/unavailable carry no value, no source and are not history eligible",
            );
        }
        if self.sample_kind.namespace() == Namespace::Replay && self.eligible_for_history {
            return fail("simulated/replay values are never history eligible");
        }
        Ok(())
    }

    /// Time the value is attributed to: hardware time if known, else read time.
    pub fn sample_time_boot_ns(&self) -> u64 {
        self.observed_boot_ns.unwrap_or(self.read_finished_boot_ns)
    }
}

/// Committed canonical sample (`canonical.schema.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalSample {
    pub schema_version: u32,
    #[serde(with = "crate::wire::u64_decimal")]
    pub snapshot_revision: u64,
    #[serde(with = "crate::wire::u64_decimal")]
    pub cycle_id: u64,
    pub key: MetricKey,
    pub source_id: Option<SourceId>,
    pub source_generation: Option<u32>,
    pub unit: CanonicalUnit,
    pub value: Option<Scalar>,
    pub sample_kind: SampleKind,
    pub selection_state: SelectionState,
    pub quality: Quality,
    pub freshness: Freshness,
    #[serde(with = "crate::wire::opt_u64_decimal")]
    pub observed_boot_ns: Option<u64>,
    #[serde(with = "crate::wire::u64_decimal")]
    pub committed_boot_ns: u64,
    pub policy_revision: u32,
    pub candidate_source_ids: Vec<SourceId>,
    pub reason_codes: Vec<String>,
    pub eligible_for_history: bool,
    pub measurement_freshness: Freshness,
    #[serde(with = "crate::wire::u64_decimal")]
    pub read_finished_boot_ns: u64,
}

impl CanonicalSample {
    /// Stamps a collector value with the commit revision and time.
    pub fn commit(value: CanonicalValue, snapshot_revision: u64, committed_boot_ns: u64) -> Self {
        Self {
            schema_version: CANONICAL_SCHEMA_VERSION,
            snapshot_revision,
            cycle_id: value.cycle_id,
            key: value.key,
            source_id: value.source_id,
            source_generation: value.source_generation,
            unit: value.unit,
            value: value.value,
            sample_kind: value.sample_kind,
            selection_state: value.selection_state,
            quality: value.quality,
            freshness: value.freshness,
            observed_boot_ns: value.observed_boot_ns,
            committed_boot_ns,
            policy_revision: value.policy_revision,
            candidate_source_ids: value.candidate_source_ids,
            reason_codes: value.reason_codes,
            eligible_for_history: value.eligible_for_history,
            measurement_freshness: value.measurement_freshness,
            read_finished_boot_ns: value.read_finished_boot_ns,
        }
    }

    pub fn sample_time_boot_ns(&self) -> u64 {
        self.observed_boot_ns.unwrap_or(self.read_finished_boot_ns)
    }
}

/// Last good value retained for annotation only; it is never a new measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastGood {
    pub value: Scalar,
    pub unit: CanonicalUnit,
    pub sample_boot_ns: u64,
    pub source_id: SourceId,
}

/// Why a candidate did not win.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RejectReason {
    DifferentScope,
    DifferentNamespace,
    ReadFailed { status: ReadStatus },
    DriverFault,
    QualityNotEligible { quality: Quality },
    Stale,
    InvalidValue,
    UnknownUnit,
    Excluded,
    UnprovenPurpose,
    LowerPriority,
    Hysteresis,
}

impl RejectReason {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DifferentScope => "different_scope",
            Self::DifferentNamespace => "different_namespace",
            Self::ReadFailed { .. } => "read_failed",
            Self::DriverFault => "driver_fault",
            Self::QualityNotEligible { .. } => "quality_not_eligible",
            Self::Stale => "stale",
            Self::InvalidValue => "invalid_value",
            Self::UnknownUnit => "unknown_unit",
            Self::Excluded => "excluded_by_policy",
            Self::UnprovenPurpose => "unproven_purpose",
            Self::LowerPriority => "lower_priority",
            Self::Hysteresis => "held_by_hysteresis",
        }
    }
}

/// Outcome of one candidate in arbitration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
pub enum CandidateDisposition {
    Selected,
    /// Valid standby candidate (fallback) that did not win.
    Standby,
    /// Proven alias of another candidate's origin; not an independent vote.
    Alias {
        of: SourceId,
    },
    /// Member of a conflicting set; value withheld.
    Conflicting,
    Rejected(RejectReason),
}

/// Per-candidate evidence shown by the sources view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateExplanation {
    pub source_id: SourceId,
    pub adapter_id: AdapterId,
    pub origin: OriginIdentity,
    pub profile_rank: u16,
    pub native_value: Option<Scalar>,
    pub native_unit: NativeUnit,
    pub value: Option<Scalar>,
    pub conversion_id: Option<String>,
    pub read_status: ReadStatus,
    pub quality: Quality,
    pub sample_kind: SampleKind,
    pub freshness: Freshness,
    pub measurement_freshness: Freshness,
    pub read_finished_boot_ns: u64,
    pub observed_boot_ns: Option<u64>,
    pub disposition: CandidateDisposition,
    /// Diagnostic-only suspicions (e.g. constant value); never auto-invalidating.
    pub suspect_reasons: Vec<String>,
}

/// Hysteresis state exposed for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchExplanation {
    pub challenger: Option<SourceId>,
    pub challenger_streak: u32,
    pub required_streak: u32,
    pub held_since_boot_ns: u64,
    pub min_hold_ms: u32,
}

/// Explainable arbitration result for one `MetricKey`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    pub key: MetricKey,
    pub policy_revision: u32,
    pub selection_state: SelectionState,
    pub selected_source: Option<SourceId>,
    pub pinned_source: Option<SourceId>,
    pub candidates: Vec<CandidateExplanation>,
    /// Spread between conflicting independent origins (canonical unit).
    pub conflict_spread: Option<f64>,
    pub tolerance: Option<f64>,
    pub switch: Option<SwitchExplanation>,
    pub reason_codes: Vec<String>,
}

/// Exact counter movement over a complete measured interval `[start, end)`.
/// Totals are only exact for intervals fully inside a query window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CounterInterval {
    /// Series the total belongs to (the derived rate metric's key).
    pub key: MetricKey,
    pub source_id: SourceId,
    pub source_generation: u32,
    pub start_boot_ns: u64,
    pub end_boot_ns: u64,
    /// Delta in canonical units; `u64` whenever the conversion is integral.
    pub delta: Scalar,
    pub unit: CanonicalUnit,
}

/// How a process uses one GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuUsageKind {
    Compute,
    Graphics,
    ComputeAndGraphics,
    Unknown,
}

/// One `(ProcessKey, GPU)` association; host resources are NOT repeated here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessGpuLink {
    pub gpu_entity_id: EntityId,
    pub usage: GpuUsageKind,
    pub utilization_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

/// Host resources of one process, sampled once per cycle per `ProcessKey`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessRow {
    pub key: ProcessKey,
    pub name: String,
    pub uid: Option<u32>,
    pub state: Option<char>,
    /// `None` during warmup (first sample of a ProcessKey) or when unreadable.
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub missing_reason: Option<String>,
    pub gpu: Vec<ProcessGpuLink>,
    pub sample_kind: SampleKind,
    pub read_finished_boot_ns: u64,
}

/// Bounded live process table.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProcessTable {
    pub rows: Vec<ProcessRow>,
    /// More processes existed than `max_processes`; a stable-priority subset is kept.
    pub truncated: bool,
    pub gpu_links_truncated: bool,
    pub total_seen: u32,
    pub read_finished_boot_ns: u64,
    pub cycle_id: u64,
}

/// Previous counter reading for exact delta computation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterBase {
    pub value: u64,
    pub boot_id: BootId,
    pub generation: u32,
    pub sample_boot_ns: u64,
}

/// Hysteresis memo for one `MetricKey`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionMemo {
    pub selected: Option<SourceId>,
    pub selected_since_boot_ns: u64,
    pub challenger: Option<SourceId>,
    pub challenger_streak: u32,
    pub challenger_since_boot_ns: u64,
}

/// Previous CPU ticks of one process instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessBase {
    pub ticks: u64,
    pub sample_boot_ns: u64,
}

/// Bounded collector state. It becomes the next delta base only after the
/// Store acknowledges the commit that carried it (`next_state`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CollectorState {
    pub revision: u64,
    pub counters: BTreeMap<SourceId, CounterBase>,
    pub selections: BTreeMap<MetricKey, SelectionMemo>,
    /// Latest raw candidate per source, for arbitration across adapter
    /// batches. Shared: every collection starts from a copy of the state, and
    /// unchanged candidates must not be deep-copied each time.
    pub candidates: BTreeMap<SourceId, Arc<Observation>>,
    pub last_good: BTreeMap<MetricKey, LastGood>,
    pub processes: BTreeMap<ProcessKey, ProcessBase>,
}

impl CollectorState {
    /// Entry count used for bound checks.
    pub fn entries(&self) -> usize {
        self.counters.len()
            + self.selections.len()
            + self.candidates.len()
            + self.last_good.len()
            + self.processes.len()
    }

    /// Approximate retained bytes (for the catalog/state partition ledger).
    pub fn estimated_bytes(&self) -> usize {
        // Conservative per-entry costs including keys and map node overhead.
        self.counters.len() * 192
            + self.selections.len() * 384
            + self.candidates.len() * 1024
            + self.last_good.len() * 320
            + self.processes.len() * 256
    }
}

/// Immutable catalog view handed to collectors.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CatalogSnapshot {
    pub revision: u32,
    pub host_id: Option<HostId>,
    pub boot_id: Option<BootId>,
    pub sources: BTreeMap<SourceId, Arc<SourceDescriptor>>,
    pub entities: BTreeMap<EntityId, Arc<EntityRecord>>,
    /// Sources grouped by MetricKey, built once per catalog revision.
    pub by_metric: BTreeMap<MetricKey, Vec<SourceId>>,
    pub capabilities: BTreeMap<AdapterId, Vec<Capability>>,
}

impl CatalogSnapshot {
    pub fn source(&self, source_id: &SourceId) -> Option<&Arc<SourceDescriptor>> {
        self.sources.get(source_id)
    }

    pub fn entity(&self, entity_id: &EntityId) -> Option<&Arc<EntityRecord>> {
        self.entities.get(entity_id)
    }

    /// Rebuilds the `by_metric` grouping.
    pub fn regroup(&mut self) {
        let mut grouped: BTreeMap<MetricKey, Vec<SourceId>> = BTreeMap::new();
        for (source_id, descriptor) in &self.sources {
            if let Some(key) = &descriptor.metric {
                grouped
                    .entry(key.clone())
                    .or_default()
                    .push(source_id.clone());
            }
        }
        self.by_metric = grouped;
    }
}

/// Versioned arbitration policy handed to collectors.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicySnapshot {
    pub revision: u32,
    pub switch_successes: u32,
    pub switch_min_hold_ms: u32,
    pub temperature_disagreement_deg_c: f64,
    /// Engineering default for non-temperature gauges (percent of magnitude).
    pub relative_tolerance_percent: f64,
    pub alignment_tolerance_ms: u32,
    pub excluded_sources: BTreeSet<SourceId>,
    pub pinned_sources: BTreeMap<MetricKey, SourceId>,
    pub stale_factor: u32,
    /// Effective period per `adapter_id/read_group` (milliseconds).
    pub effective_periods_ms: BTreeMap<String, u32>,
    /// Namespace accepted into canonical state (live, or replay in `--replay`).
    pub namespace: Namespace,
}

impl PolicySnapshot {
    /// Effective period of `adapter/read_group`, found without building the
    /// key string (this runs for every record of every collection).
    fn period_ms(&self, adapter: &str, read_group: &str) -> Option<u32> {
        let slash = adapter.len();
        self.effective_periods_ms
            .range::<str, _>((Bound::Included(adapter), Bound::Unbounded))
            .take_while(|(key, _)| key.starts_with(adapter))
            .find(|(key, _)| {
                key.len() == slash + 1 + read_group.len()
                    && key.as_bytes().get(slash) == Some(&b'/')
                    && key.ends_with(read_group)
            })
            .map(|(_, period)| *period)
    }

    /// Freshness deadline for a source read at `read_boot_ns`.
    pub fn fresh_deadline(
        &self,
        adapter: &AdapterId,
        read_group: &str,
        update_interval_ms: Option<u32>,
        read_boot_ns: u64,
    ) -> u64 {
        let period = self.period_ms(adapter.as_str(), read_group).unwrap_or(1000);
        let basis = u64::from(period.max(update_interval_ms.unwrap_or(0)));
        read_boot_ns.saturating_add(
            u64::from(self.stale_factor.max(1)).saturating_mul(crate::clock::ms_to_ns(basis)),
        )
    }
}

/// Bounds a collector must respect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionLimits {
    pub max_series: u32,
    pub max_sources: u32,
    pub max_candidates_per_series: u32,
    pub max_processes: u32,
    pub max_process_gpu_links: u32,
}

/// Input of one collector job.
#[derive(Debug, Clone)]
pub struct CollectionPlan {
    pub collector_id: CollectorId,
    pub batch_id: Uuid,
    pub cycle_id: u64,
    pub epoch: u64,
    pub config_revision: u32,
    pub host_id: HostId,
    pub boot_id: BootId,
    pub catalog: Arc<CatalogSnapshot>,
    pub policy: Arc<PolicySnapshot>,
    /// Committed state (delta base); never mutated in place.
    pub state: Arc<CollectorState>,
    pub state_revision: u64,
    /// Admitted adapter batches (shared, accounted in the ingress partition).
    pub inputs: Vec<Arc<IngressBatch>>,
    pub now_boot_ns: u64,
    pub limits: CollectionLimits,
}

/// Non-fatal collection failure (one source or one metric).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionFailure {
    pub source_id: Option<SourceId>,
    pub key: Option<MetricKey>,
    pub code: String,
    pub message: String,
}

/// Domain events produced by collectors and committed with the batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CollectorEvent {
    SourceChanged {
        key: MetricKey,
        previous_source: Option<SourceId>,
        source_id: SourceId,
        policy_revision: u32,
    },
    SourceConflict {
        key: MetricKey,
        source_ids: Vec<SourceId>,
    },
    SourceUnavailable {
        source_id: SourceId,
        reason: String,
    },
}

/// Output of one collector job; committed atomically or not at all.
#[derive(Debug, Clone)]
pub struct CollectionBatch {
    pub batch_id: Uuid,
    pub collector_id: CollectorId,
    pub cycle_id: u64,
    pub epoch: u64,
    pub config_revision: u32,
    pub catalog_revision: u32,
    pub policy_revision: u32,
    pub state_base_revision: u64,
    pub canonical: Vec<CanonicalValue>,
    pub explanations: Vec<Explanation>,
    pub counter_intervals: Vec<CounterInterval>,
    /// Raw batches consumed (shared, not copied) for candidate retention.
    pub inputs: Vec<Arc<IngressBatch>>,
    pub process_table: Option<ProcessTable>,
    /// Replacement state; becomes the base only after a successful Ack.
    pub next_state: CollectorState,
    pub failures: Vec<CollectionFailure>,
    pub events: Vec<CollectorEvent>,
}

impl CollectionBatch {
    /// Validates every canonical value before the batch reaches the Store.
    pub fn validate(&self) -> Result<(), CanonicalError> {
        for value in &self.canonical {
            value.validate()?;
        }
        Ok(())
    }

    /// Content digest used to detect "same batch_id, different payload".
    pub fn content_fingerprint(&self) -> String {
        let summary = (
            &self.collector_id,
            self.cycle_id,
            self.epoch,
            self.state_base_revision,
            self.canonical.len(),
            self.counter_intervals.len(),
            self.canonical
                .iter()
                .map(|value| {
                    (
                        value.key.series_selector(),
                        value.value.map(|scalar| scalar.to_f64_lossy().to_bits()),
                    )
                })
                .collect::<Vec<_>>(),
        );
        crate::digest::sha256_json(&summary).unwrap_or_default()
    }
}

/// Epoch/base guard checked by the Store before committing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitGuard {
    pub expected_epoch: u64,
    pub collector_id: CollectorId,
    pub state_base_revision: u64,
}

/// Commit outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    Committed,
    /// Same batch_id and content already committed; original Ack returned.
    Duplicate,
    /// Same batch_id with different content.
    Conflict,
    RejectedStaleEpoch,
    RejectedStaleBase,
    RejectedInvalid,
    RejectedCapacity,
}

impl AckStatus {
    pub const fn is_committed(self) -> bool {
        matches!(self, Self::Committed | Self::Duplicate)
    }
}

/// Durability of the optional archive for a committed revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveState {
    Disabled,
    Pending,
    Committed,
    Gap,
}

/// Store acknowledgement; collectors advance their base only on commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreAck {
    pub batch_id: Uuid,
    pub collector_id: CollectorId,
    pub status: AckStatus,
    pub committed_revision: u64,
    pub state_revision: u64,
    pub archive: ArchiveState,
    pub reason: Option<String>,
}

/// Visible gap in collected data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapRecord {
    pub key: Option<MetricKey>,
    pub adapter_id: Option<AdapterId>,
    pub start_boot_ns: u64,
    pub end_boot_ns: u64,
    pub reason: String,
    pub affected_count: u32,
}

/// One committed canonical value plus its last good annotation.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotEntry {
    pub sample: CanonicalSample,
    pub last_good: Option<LastGood>,
    /// From this boot time on the value is no longer current (STORAGE.md §3:
    /// sample time + stale factor x source period). A backend that stops
    /// answering commits nothing, so readers compare it with their own now.
    /// `None` when the entry carries no value.
    pub stale_after_boot_ns: Option<u64>,
}

/// Committed state of one collector domain.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DomainSnapshot {
    pub revision: u64,
    pub cycle_id: u64,
    pub committed_boot_ns: u64,
    /// Shared entries: each commit copies the map, not the unchanged samples.
    pub samples: BTreeMap<MetricKey, Arc<SnapshotEntry>>,
    pub explanations: BTreeMap<MetricKey, Arc<Explanation>>,
}

/// Health of the hot store.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StoreHealth {
    pub ledger: Option<LedgerSnapshot>,
    pub series: u32,
    pub sources_retained: u32,
    pub gaps_total: u64,
    pub evicted_total: u64,
    pub rejected_batches_total: u64,
    pub truncated_history: bool,
}

/// One consistent Manager commit view. Asynchronous sources keep their own
/// timestamps; this is not a claim that every sensor sampled simultaneously.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImmutableSnapshot {
    pub revision: u64,
    pub committed_boot_ns: u64,
    pub domains: BTreeMap<CollectorId, Arc<DomainSnapshot>>,
    pub process_table: Arc<ProcessTable>,
    pub collector_states: BTreeMap<CollectorId, (u64, Arc<CollectorState>)>,
    pub health: StoreHealth,
    pub recent_gaps: Arc<Vec<GapRecord>>,
}

impl ImmutableSnapshot {
    /// Looks up a committed sample across domains.
    pub fn sample(&self, key: &MetricKey) -> Option<&SnapshotEntry> {
        self.domains
            .values()
            .find_map(|domain| domain.samples.get(key).map(Arc::as_ref))
    }

    /// Looks up an arbitration explanation across domains.
    pub fn explanation(&self, key: &MetricKey) -> Option<&Arc<Explanation>> {
        self.domains
            .values()
            .find_map(|domain| domain.explanations.get(key))
    }

    /// Committed state and its revision for one collector (empty at start).
    pub fn collector_state(&self, collector_id: &CollectorId) -> (u64, Arc<CollectorState>) {
        self.collector_states
            .get(collector_id)
            .cloned()
            .unwrap_or_else(|| (0, Arc::new(CollectorState::default())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> MetricKey {
        serde_json::from_value(serde_json::json!({
            "host_id":"demo-host","boot_id":"demo-boot","entity_id":"cpu.package.0",
            "metric_id":"cpu.temperature.package","scope":"package:0"
        }))
        .unwrap()
    }

    fn selected() -> CanonicalValue {
        CanonicalValue {
            key: key(),
            cycle_id: 42,
            source_id: Some(SourceId::new("hwmon:coretemp:temp1").unwrap()),
            source_generation: Some(1),
            unit: CanonicalUnit::DegC,
            value: Some(Scalar::from_f64(68.0).unwrap()),
            sample_kind: SampleKind::Measured,
            selection_state: SelectionState::Selected,
            quality: Quality::Verified,
            freshness: Freshness::Fresh,
            measurement_freshness: Freshness::Unknown,
            observed_boot_ns: None,
            read_finished_boot_ns: 42_000_000_000,
            policy_revision: 1,
            candidate_source_ids: vec![],
            reason_codes: vec![],
            eligible_for_history: true,
        }
    }

    #[test]
    fn selected_value_rules() {
        let value = selected();
        value.validate().unwrap();
        let mut zero = selected();
        zero.value = Some(Scalar::from_f64(0.0).unwrap());
        zero.validate().unwrap();
        let mut stale = selected();
        stale.freshness = Freshness::Stale;
        assert!(stale.validate().is_err());
        let mut suspect = selected();
        suspect.quality = Quality::Suspect;
        assert!(suspect.validate().is_err());
    }

    #[test]
    fn conflict_and_unavailable_carry_no_value() {
        let mut conflict = selected();
        conflict.selection_state = SelectionState::Conflict;
        assert!(conflict.validate().is_err());
        conflict.value = None;
        conflict.source_id = None;
        conflict.source_generation = None;
        conflict.eligible_for_history = false;
        conflict.validate().unwrap();
    }

    #[test]
    fn replay_values_are_not_history_eligible() {
        let mut replay = selected();
        replay.sample_kind = SampleKind::Simulated;
        assert!(replay.validate().is_err());
        replay.eligible_for_history = false;
        replay.validate().unwrap();
    }

    #[test]
    fn spec_conflict_example_round_trips() {
        let text = include_str!("../../../spec/dgxtop-v2/examples/canonical.conflict.json");
        let sample: CanonicalSample = serde_json::from_str(text).unwrap();
        assert_eq!(sample.selection_state, SelectionState::Conflict);
        assert!(sample.value.is_none());
        let original: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_value(&sample).unwrap(), original);
    }

    #[test]
    fn fresh_deadline_uses_max_of_period_and_update_interval() {
        let policy = PolicySnapshot {
            revision: 1,
            switch_successes: 3,
            switch_min_hold_ms: 5000,
            temperature_disagreement_deg_c: 8.0,
            relative_tolerance_percent: 10.0,
            alignment_tolerance_ms: 250,
            excluded_sources: BTreeSet::new(),
            pinned_sources: BTreeMap::new(),
            stale_factor: 3,
            effective_periods_ms: BTreeMap::from([("hwmon/fast".to_owned(), 1000)]),
            namespace: Namespace::Live,
        };
        let adapter = AdapterId::new("hwmon").unwrap();
        assert_eq!(
            policy.fresh_deadline(&adapter, "fast", None, 0),
            3_000_000_000
        );
        assert_eq!(
            policy.fresh_deadline(&adapter, "fast", Some(2000), 0),
            6_000_000_000
        );
    }

    #[test]
    fn periods_are_matched_exactly_even_when_adapter_ids_share_a_prefix() {
        let policy = PolicySnapshot {
            revision: 1,
            switch_successes: 3,
            switch_min_hold_ms: 5000,
            temperature_disagreement_deg_c: 8.0,
            relative_tolerance_percent: 10.0,
            alignment_tolerance_ms: 250,
            excluded_sources: BTreeSet::new(),
            pinned_sources: BTreeMap::new(),
            stale_factor: 1,
            effective_periods_ms: BTreeMap::from([
                ("replay/fast".to_owned(), 100),
                ("replay-1/fast".to_owned(), 200),
                ("replay-1/hwmon.fast".to_owned(), 300),
                ("replay/fast2".to_owned(), 400),
            ]),
            namespace: Namespace::Live,
        };
        let period = |adapter: &str, group: &str| {
            policy.fresh_deadline(&AdapterId::new(adapter).unwrap(), group, None, 0) / 1_000_000
        };
        assert_eq!(period("replay", "fast"), 100);
        assert_eq!(period("replay-1", "fast"), 200);
        assert_eq!(period("replay-1", "hwmon.fast"), 300);
        assert_eq!(period("replay", "fast2"), 400);
        // Unknown groups fall back to one second.
        assert_eq!(period("replay", "slow"), 1000);
        assert_eq!(period("rep", "fast"), 1000);
    }
}
