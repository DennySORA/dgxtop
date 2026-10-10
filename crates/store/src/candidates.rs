//! Raw candidate retention (the `ObservationStore` of `ARCHITECTURE.md` §7.1).
//!
//! The raw readings behind each committed batch are kept as evidence for the
//! sources view: at most `store.raw_candidate_seconds` old, at most
//! `store.max_sources` sources and at most 600 readings per source, all in
//! the `raw_candidates` partition. Readings are copied into fixed-width
//! records, so the shared adapter batches (and their ingress permits) are
//! released as soon as the commit returns; per-source strings are stored
//! once per source, not per reading. This partition is evicted first.

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::budget::{ByteLedger, Partition};
use dgxtop_core::identity::{AdapterId, MetricKey, ProfileId, ReadGroup, SourceId};
use dgxtop_core::metric::{NativeUnit, Scalar, conversion_for, descriptor};
use dgxtop_core::observation::{Observation, ObservationBatch};
use dgxtop_core::ports::IngressBatch;
use dgxtop_core::quality::{Namespace, Quality, ReadStatus, SampleKind};

use crate::history::accounted::{Account, AccountedDeque};

/// Readings retained per source (`ARCHITECTURE.md` §7.1).
pub const MAX_READINGS_PER_SOURCE: usize = 600;
/// Readings removed from one source per eviction round.
const EVICTION_BATCH: usize = 32;
/// Fixed bookkeeping bytes of one source besides its strings.
const SOURCE_BASE_BYTES: u64 = 512;

/// Per-source description shared by all of its readings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateMeta {
    pub source_id: SourceId,
    pub adapter_id: AdapterId,
    pub read_group: ReadGroup,
    pub key: MetricKey,
    pub native_unit: NativeUnit,
    pub profile_id: ProfileId,
    pub profile_revision: u32,
    /// Explicit conversion to the metric's canonical unit, if one exists.
    pub conversion_id: Option<&'static str>,
}

/// One raw reading in fixed-width form.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateRecord {
    pub cycle_id: u64,
    pub read_started_boot_ns: u64,
    pub read_finished_boot_ns: u64,
    pub device_sample_boot_ns: Option<u64>,
    /// Native value; `None` whenever the read failed (never zero-filled).
    pub value: Option<Scalar>,
    pub read_status: ReadStatus,
    pub quality: Quality,
    pub sample_kind: SampleKind,
    pub generation: u32,
    pub source_update_interval_ms: Option<u32>,
    pub reason_count: u8,
}

/// A retained reading with its source description.
#[derive(Debug, Clone, PartialEq)]
pub struct RetainedCandidate {
    pub meta: Arc<CandidateMeta>,
    pub record: CandidateRecord,
}

/// Limits of the candidate retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CandidateSettings {
    pub retention_ns: u64,
    pub max_sources: usize,
    pub namespace: Namespace,
}

/// Counters of one retention pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CandidateReport {
    pub retained: u64,
    /// Readings not kept: wrong namespace, duplicate, too old or no memory.
    pub dropped: u64,
    /// Readings or sources removed because a limit was reached.
    pub evicted: u64,
}

struct SourceRing {
    meta: Arc<CandidateMeta>,
    records: AccountedDeque<CandidateRecord>,
    _meta_account: Account,
}

impl SourceRing {
    fn newest_read_ns(&self) -> Option<u64> {
        self.records
            .back()
            .map(|record| record.read_finished_boot_ns)
    }

    fn oldest_read_ns(&self) -> Option<u64> {
        self.records
            .front()
            .map(|record| record.read_finished_boot_ns)
    }
}

fn meta_bytes(meta: &CandidateMeta) -> u64 {
    let strings = meta.source_id.as_str().len()
        + meta.adapter_id.as_str().len()
        + meta.read_group.as_str().len()
        + meta.key.host_id.as_str().len()
        + meta.key.boot_id.as_str().len()
        + meta.key.entity_id.as_str().len()
        + meta.key.metric_id.as_str().len()
        + meta.key.scope.as_str().len()
        + meta.native_unit.as_str().len()
        + meta.profile_id.as_str().len();
    let size = std::mem::size_of::<CandidateMeta>().saturating_add(strings);
    SOURCE_BASE_BYTES.saturating_add(u64::try_from(size).unwrap_or(u64::MAX))
}

fn meta_of(batch: &ObservationBatch, observation: &Observation) -> CandidateMeta {
    let conversion_id = descriptor(observation.key.metric_id.as_str())
        .and_then(|metric| conversion_for(observation.native_unit.as_str(), metric.unit))
        .map(|conversion| conversion.id);
    CandidateMeta {
        source_id: observation.provenance.source_id.clone(),
        adapter_id: observation.provenance.adapter_id.clone(),
        read_group: batch.read_group.clone(),
        key: observation.key.clone(),
        native_unit: observation.native_unit.clone(),
        profile_id: observation.provenance.profile_id.clone(),
        profile_revision: observation.provenance.profile_revision,
        conversion_id,
    }
}

fn record_of(observation: &Observation) -> CandidateRecord {
    CandidateRecord {
        cycle_id: observation.cycle_id,
        read_started_boot_ns: observation.read_started_boot_ns,
        read_finished_boot_ns: observation.read_finished_boot_ns,
        device_sample_boot_ns: observation.device_sample_boot_ns,
        value: observation.value,
        read_status: observation.read_status,
        quality: observation.quality,
        sample_kind: observation.sample_kind,
        generation: observation.provenance.generation,
        source_update_interval_ms: observation.source_update_interval_ms,
        reason_count: u8::try_from(observation.reason_codes.len()).unwrap_or(u8::MAX),
    }
}

/// Bounded per-source rings of recent raw readings.
pub(crate) struct CandidateStore {
    sources: BTreeMap<SourceId, SourceRing>,
    settings: CandidateSettings,
    ledger: ByteLedger,
}

impl CandidateStore {
    pub(crate) fn new(settings: CandidateSettings, ledger: &ByteLedger) -> Self {
        Self {
            sources: BTreeMap::new(),
            settings,
            ledger: ledger.clone(),
        }
    }

    pub(crate) fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// Retains the readings of the batches a commit consumed.
    pub(crate) fn retain(&mut self, inputs: &[Arc<IngressBatch>], now_ns: u64) -> CandidateReport {
        let mut report = CandidateReport::default();
        for input in inputs {
            let batch = input.batch();
            for observation in &batch.records {
                self.retain_one(batch, observation, now_ns, &mut report);
            }
        }
        report.evicted = report.evicted.saturating_add(self.expire(now_ns));
        report
    }

    fn retain_one(
        &mut self,
        batch: &ObservationBatch,
        observation: &Observation,
        now_ns: u64,
        report: &mut CandidateReport,
    ) {
        let too_old = observation
            .read_finished_boot_ns
            .saturating_add(self.settings.retention_ns)
            <= now_ns;
        if observation.sample_kind.namespace() != self.settings.namespace || too_old {
            report.dropped = report.dropped.saturating_add(1);
            return;
        }
        let source_id = &observation.provenance.source_id;
        if !self.ensure_ring(batch, observation, report) {
            report.dropped = report.dropped.saturating_add(1);
            return;
        }
        let Some(ring) = self.sources.get(source_id) else {
            return;
        };
        let duplicate = ring
            .newest_read_ns()
            .is_some_and(|newest| observation.read_finished_boot_ns <= newest);
        if duplicate {
            report.dropped = report.dropped.saturating_add(1);
            return;
        }
        let full = ring.records.len() >= MAX_READINGS_PER_SOURCE;
        let cost = if full { 0 } else { ring.records.growth_cost() };
        if !self.ensure_headroom(cost, Some(source_id), report) {
            report.dropped = report.dropped.saturating_add(1);
            return;
        }
        let Some(ring) = self.sources.get_mut(source_id) else {
            report.dropped = report.dropped.saturating_add(1);
            return;
        };
        if full {
            ring.records.pop_front();
            report.evicted = report.evicted.saturating_add(1);
        }
        if ring.records.try_push_back(record_of(observation)).is_ok() {
            report.retained = report.retained.saturating_add(1);
        } else {
            report.dropped = report.dropped.saturating_add(1);
        }
    }

    /// Creates or refreshes the ring of the observation's source.
    fn ensure_ring(
        &mut self,
        batch: &ObservationBatch,
        observation: &Observation,
        report: &mut CandidateReport,
    ) -> bool {
        let meta = meta_of(batch, observation);
        if let Some(ring) = self.sources.get_mut(&meta.source_id) {
            if *ring.meta != meta {
                ring.meta = Arc::new(meta);
            }
            return true;
        }
        if self.sources.len() >= self.settings.max_sources.max(1) {
            self.evict_least_recent_source(report);
        }
        let bytes = meta_bytes(&meta);
        if !self.ensure_headroom(bytes, None, report) {
            return false;
        }
        let mut account = Account::new(&self.ledger, Partition::RawCandidates);
        if account.try_grow(bytes).is_err() {
            return false;
        }
        self.sources.insert(
            meta.source_id.clone(),
            SourceRing {
                meta: Arc::new(meta),
                records: AccountedDeque::new(&self.ledger, Partition::RawCandidates),
                _meta_account: account,
            },
        );
        true
    }

    fn evict_least_recent_source(&mut self, report: &mut CandidateReport) {
        let victim = self
            .sources
            .iter()
            .min_by_key(|(_, ring)| ring.newest_read_ns().unwrap_or(0))
            .map(|(source_id, _)| source_id.clone());
        if let Some(source_id) = victim
            && let Some(ring) = self.sources.remove(&source_id)
        {
            let readings = u64::try_from(ring.records.len()).unwrap_or(u64::MAX);
            report.evicted = report.evicted.saturating_add(readings.max(1));
        }
    }

    fn available(&self) -> u64 {
        self.ledger
            .limits()
            .limit(Partition::RawCandidates)
            .saturating_sub(self.ledger.used(Partition::RawCandidates))
    }

    /// Evicts the globally oldest readings until `bytes` fit; the source
    /// being appended to is never removed as a whole.
    fn ensure_headroom(
        &mut self,
        bytes: u64,
        protect: Option<&SourceId>,
        report: &mut CandidateReport,
    ) -> bool {
        while self.available() < bytes {
            let evicted = self.evict_oldest_readings(protect);
            if evicted == 0 {
                return false;
            }
            report.evicted = report.evicted.saturating_add(evicted);
        }
        true
    }

    fn evict_oldest_readings(&mut self, protect: Option<&SourceId>) -> u64 {
        let victim = self
            .sources
            .iter()
            .filter(|(source_id, ring)| Some(*source_id) != protect || ring.records.len() > 1)
            .min_by_key(|(_, ring)| ring.oldest_read_ns().unwrap_or(0))
            .map(|(source_id, _)| source_id.clone());
        let Some(source_id) = victim else {
            return 0;
        };
        let Some(ring) = self.sources.get_mut(&source_id) else {
            return 0;
        };
        if ring.records.is_empty() {
            self.sources.remove(&source_id);
            return 1;
        }
        let keep = usize::from(protect == Some(&source_id));
        let mut evicted = 0u64;
        while evicted < u64::try_from(EVICTION_BATCH).unwrap_or(u64::MAX)
            && ring.records.len() > keep
            && ring.records.pop_front().is_some()
        {
            evicted += 1;
        }
        ring.records.release_spare();
        if ring.records.is_empty() && keep == 0 {
            self.sources.remove(&source_id);
        }
        evicted
    }

    /// Drops readings older than the retention and sources left empty.
    pub(crate) fn expire(&mut self, now_ns: u64) -> u64 {
        self.expire_within(now_ns, self.settings.retention_ns)
    }

    /// Drops readings older than `retention_ns` (overload relief uses a
    /// shorter retention than configured).
    pub(crate) fn expire_within(&mut self, now_ns: u64, retention_ns: u64) -> u64 {
        let retention = retention_ns.min(self.settings.retention_ns);
        let mut expired = 0u64;
        for ring in self.sources.values_mut() {
            while ring
                .oldest_read_ns()
                .is_some_and(|oldest| oldest.saturating_add(retention) <= now_ns)
            {
                ring.records.pop_front();
                expired = expired.saturating_add(1);
            }
            ring.records.release_spare();
        }
        self.sources.retain(|_, ring| !ring.records.is_empty());
        expired
    }

    /// Newest readings of one source, oldest first (at most `max`).
    pub(crate) fn recent(&self, source_id: &SourceId, max: usize) -> Vec<RetainedCandidate> {
        let Some(ring) = self.sources.get(source_id) else {
            return Vec::new();
        };
        let skip = ring
            .records
            .len()
            .saturating_sub(max.min(MAX_READINGS_PER_SOURCE));
        ring.records
            .iter()
            .skip(skip)
            .map(|record| RetainedCandidate {
                meta: Arc::clone(&ring.meta),
                record: *record,
            })
            .collect()
    }

    pub(crate) fn source_ids(&self) -> Vec<SourceId> {
        self.sources.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::budget::PartitionLimits;
    use dgxtop_core::testing::{ObservationBuilder, batch_of, metric_key, source_id};

    const SECOND: u64 = 1_000_000_000;

    fn settings(max_sources: usize) -> CandidateSettings {
        CandidateSettings {
            retention_ns: 60 * SECOND,
            max_sources,
            namespace: Namespace::Live,
        }
    }

    fn reading(source: &str, at_s: u64, value: f64) -> Observation {
        ObservationBuilder::new(
            metric_key("cpu.package.0", "cpu.temperature.package", "package:0"),
            "hwmon",
            source,
        )
        .unit("mdegC")
        .f64(value)
        .read_at(at_s * SECOND)
        .build()
    }

    fn input(records: Vec<Observation>) -> Arc<IngressBatch> {
        Arc::new(IngressBatch::new(
            batch_of("hwmon", "fast", 1, records),
            None,
        ))
    }

    #[test]
    fn readings_are_retained_with_conversion_and_failures_stay_null() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut store = CandidateStore::new(settings(8), &ledger);
        let failed = ObservationBuilder::new(
            metric_key("cpu.package.0", "cpu.temperature.package", "package:0"),
            "hwmon",
            "hwmon:a:1",
        )
        .unit("mdegC")
        .failed(ReadStatus::Timeout)
        .read_at(2 * SECOND)
        .build();
        let report = store.retain(
            &[input(vec![reading("hwmon:a:1", 1, 68_000.0), failed])],
            3 * SECOND,
        );
        assert_eq!(report.retained, 2);
        let recent = store.recent(&source_id("hwmon:a:1"), 10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].meta.conversion_id, Some("mdegC_to_degC"));
        assert_eq!(recent[1].record.value, None);
        assert_eq!(recent[1].record.read_status, ReadStatus::Timeout);
        assert!(ledger.used(Partition::RawCandidates) > 0);
    }

    #[test]
    fn duplicates_old_and_replay_readings_are_not_retained() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut store = CandidateStore::new(settings(8), &ledger);
        let shared = input(vec![reading("hwmon:a:1", 10, 1.0)]);
        store.retain(&[Arc::clone(&shared)], 11 * SECOND);
        let again = store.retain(&[shared], 11 * SECOND);
        assert_eq!(again.retained, 0);
        assert_eq!(again.dropped, 1);
        let ancient = store.retain(&[input(vec![reading("hwmon:a:2", 1, 1.0)])], 100 * SECOND);
        assert_eq!(ancient.retained, 0);
        let simulated = ObservationBuilder::new(
            metric_key("cpu.package.0", "cpu.temperature.package", "package:0"),
            "hwmon",
            "hwmon:a:3",
        )
        .kind(SampleKind::Simulated)
        .f64(1.0)
        .read_at(100 * SECOND)
        .build();
        assert_eq!(
            store
                .retain(&[input(vec![simulated])], 100 * SECOND)
                .retained,
            0
        );
    }

    #[test]
    fn retention_per_source_and_source_caps_hold() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut store = CandidateStore::new(settings(2), &ledger);
        for second in 1..=700u64 {
            store.retain(
                &[input(vec![reading("hwmon:a:1", second, 1.0)])],
                second * SECOND,
            );
        }
        assert_eq!(store.recent(&source_id("hwmon:a:1"), usize::MAX).len(), 60);
        store.settings.retention_ns = 1000 * SECOND;
        for second in 701..=1400u64 {
            store.retain(
                &[input(vec![reading("hwmon:a:1", second, 1.0)])],
                second * SECOND,
            );
        }
        assert_eq!(
            store.recent(&source_id("hwmon:a:1"), usize::MAX).len(),
            MAX_READINGS_PER_SOURCE
        );
        store.retain(
            &[input(vec![reading("hwmon:b:1", 1401, 1.0)])],
            1401 * SECOND,
        );
        store.retain(
            &[input(vec![reading("hwmon:c:1", 1402, 1.0)])],
            1402 * SECOND,
        );
        assert_eq!(store.source_count(), 2);
        assert!(store.recent(&source_id("hwmon:a:1"), 1).is_empty());
        assert_eq!(store.expire(5000 * SECOND), 2);
        assert_eq!(store.source_count(), 0);
        drop(store);
        assert_eq!(ledger.used(Partition::RawCandidates), 0);
    }

    #[test]
    fn full_partition_evicts_oldest_readings_and_never_overcommits() {
        let ledger = ByteLedger::new(PartitionLimits::for_memory_limit_mib(1));
        let limit = ledger.limits().limit(Partition::RawCandidates);
        let mut store = CandidateStore::new(
            CandidateSettings {
                retention_ns: u64::MAX / 4,
                max_sources: 8192,
                namespace: Namespace::Live,
            },
            &ledger,
        );
        let mut evicted = 0;
        for second in 1..=3000u64 {
            let source = format!("hwmon:s{}:1", second % 300);
            evicted += store
                .retain(
                    &[input(vec![reading(&source, second, 1.0)])],
                    second * SECOND,
                )
                .evicted;
            assert!(ledger.used(Partition::RawCandidates) <= limit);
        }
        assert!(evicted > 0);
        let usage = ledger.snapshot();
        let partition = usage
            .partitions
            .iter()
            .find(|usage| usage.partition == Partition::RawCandidates)
            .unwrap();
        assert!(partition.peak <= limit);
    }
}
