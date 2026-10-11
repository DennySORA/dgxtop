//! Shared builders for the hot store integration tests.
//!
//! Every `store_hot_*` test crate includes this file as a module and uses a
//! different subset of it, hence the crate-wide `dead_code` allowance.
#![allow(dead_code)]

use std::sync::Arc;

use dgxtop::live_store::LiveStore;
use dgxtop_core::budget::ByteLedger;
use dgxtop_core::canonical::{
    CanonicalValue, CollectionBatch, CollectorState, CommitGuard, CounterInterval, StoreAck,
};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::config::Config;
use dgxtop_core::errors::ErrorDetail;
use dgxtop_core::history::{
    HISTORY_SCHEMA_VERSION, HistoryPoint, HistoryQuery, HistoryResult, HistoryStat,
};
use dgxtop_core::identity::{BootId, CollectorId, MetricKey};
use dgxtop_core::metric::{CanonicalUnit, Scalar, descriptor, ids};
use dgxtop_core::ports::{CancelFlag, HistoryReader, IngressBatch, SnapshotReader, StoreWriter};
use dgxtop_core::quality::{Freshness, Namespace, Quality, SampleKind, SelectionState};
use dgxtop_core::testing::{
    FakeClock, ObservationBuilder, UTC_ON_A_WHOLE_MINUTE_NS, batch_of, metric_key, source_id,
};
use dgxtop_history::HistoryIndex;
use dgxtop_state::{SharedSnapshot, StateWriter};
use uuid::Uuid;

pub const SECOND: u64 = 1_000_000_000;
pub const MINUTE: u64 = 60 * SECOND;
pub const MILLI: u64 = 1_000_000;

pub fn config() -> Config {
    Config::defaults().unwrap_or_else(|error| panic!("embedded defaults: {error}"))
}

/// A hot store plus a fake clock and one collector's commit bookkeeping.
pub struct Harness {
    pub writer: StateWriter,
    pub snapshots: SharedSnapshot,
    pub history: HistoryIndex,
    pub ledger: ByteLedger,
    pub clock: Arc<FakeClock>,
    pub config: Config,
    pub collector: CollectorId,
    pub epoch: u64,
    cycle: u64,
}

impl Default for Harness {
    fn default() -> Self {
        Self::build(
            config(),
            ByteLedger::for_memory_limit_mib(256),
            Namespace::Live,
            false,
        )
    }
}

impl Harness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: Config) -> Self {
        Self::build(
            config,
            ByteLedger::for_memory_limit_mib(256),
            Namespace::Live,
            false,
        )
    }

    /// A harness whose wall clock is paired from the start (as the
    /// monitor's first maintenance step does).
    pub fn build(config: Config, ledger: ByteLedger, namespace: Namespace, archive: bool) -> Self {
        let mut harness = Self::unpaired(config, ledger, namespace, archive);
        harness
            .writer
            .observe_wall_clock(0, UTC_ON_A_WHOLE_MINUTE_NS);
        harness
    }

    /// A harness without a boot-to-UTC pairing yet: the history finishes
    /// no minute until one arrives.
    pub fn unpaired(
        config: Config,
        ledger: ByteLedger,
        namespace: Namespace,
        archive: bool,
    ) -> Self {
        let store = LiveStore::new(
            &config.store,
            &config.sampling,
            &config.runtime,
            ledger.clone(),
            namespace,
            archive,
        );
        let (writer, snapshots, history) = store.into_parts();
        Self {
            writer,
            snapshots,
            history,
            ledger,
            clock: FakeClock::new(0),
            config,
            collector: CollectorId::new("network")
                .unwrap_or_else(|error| panic!("collector id: {error}")),
            epoch: 1,
            cycle: 0,
        }
    }

    pub fn base(&self) -> u64 {
        self.snapshots.latest().collector_state(&self.collector).0
    }

    pub fn batch(
        &mut self,
        values: Vec<CanonicalValue>,
        intervals: Vec<CounterInterval>,
    ) -> CollectionBatch {
        self.cycle += 1;
        CollectionBatch {
            batch_id: Uuid::new_v4(),
            collector_id: self.collector.clone(),
            cycle_id: self.cycle,
            epoch: self.epoch,
            config_revision: 1,
            catalog_revision: 1,
            policy_revision: 1,
            state_base_revision: self.base(),
            canonical: values,
            explanations: Vec::new(),
            counter_intervals: intervals,
            inputs: Vec::new(),
            process_table: None,
            next_state: CollectorState::default(),
            failures: Vec::new(),
        }
    }

    pub fn guard_for(batch: &CollectionBatch) -> CommitGuard {
        CommitGuard {
            expected_epoch: batch.epoch,
            collector_id: batch.collector_id.clone(),
            state_base_revision: batch.state_base_revision,
        }
    }

    pub fn commit_batch(&mut self, batch: CollectionBatch, now: u64) -> StoreAck {
        let guard = Self::guard_for(&batch);
        self.clock.set_boot_ns(now);
        self.writer.commit_collection(batch, &guard, now)
    }

    pub fn commit(
        &mut self,
        values: Vec<CanonicalValue>,
        intervals: Vec<CounterInterval>,
        now: u64,
    ) -> StoreAck {
        let batch = self.batch(values, intervals);
        self.commit_batch(batch, now)
    }

    pub fn maintain(&mut self, now: u64) {
        self.clock.set_boot_ns(now);
        self.writer.maintain(now);
    }

    pub fn query(
        &self,
        key: &MetricKey,
        window: (u64, u64),
        resolution_ms: u32,
        stat: HistoryStat,
        max_points: u32,
    ) -> Result<HistoryResult, ErrorDetail> {
        self.query_with(
            &query(key, window, resolution_ms, stat, max_points),
            u64::MAX,
            &CancelFlag::new(),
            self.clock.as_ref(),
        )
    }

    pub fn query_with(
        &self,
        query: &HistoryQuery,
        deadline: u64,
        cancel: &CancelFlag,
        clock: &dyn TimeProvider,
    ) -> Result<HistoryResult, ErrorDetail> {
        self.history.query(query, deadline, cancel, clock)
    }
}

pub fn query(
    key: &MetricKey,
    (start, end): (u64, u64),
    resolution_ms: u32,
    stat: HistoryStat,
    max_points: u32,
) -> HistoryQuery {
    HistoryQuery {
        schema_version: HISTORY_SCHEMA_VERSION,
        request_id: Uuid::new_v4(),
        key: key.clone(),
        start_boot_ns: start,
        end_boot_ns: end,
        resolution_ms,
        stat,
        max_points,
        source_filter: None,
    }
}

pub fn temperature_key(entity: &str) -> MetricKey {
    metric_key(entity, ids::GPU_TEMPERATURE, "gpu")
}

pub fn rx_key(entity: &str) -> MetricKey {
    metric_key(entity, ids::NET_RX_BPS, "interface")
}

pub fn with_boot(key: &MetricKey, boot: &str) -> MetricKey {
    let mut key = key.clone();
    key.boot_id = BootId::new(boot).unwrap_or_else(|error| panic!("boot id: {error}"));
    key
}

fn unit_of(key: &MetricKey) -> CanonicalUnit {
    descriptor(key.metric_id.as_str()).map_or(CanonicalUnit::Count, |entry| entry.unit)
}

/// A selected, history-eligible value read at `at`.
pub fn gauge(key: &MetricKey, value: f64, at: u64) -> CanonicalValue {
    gauge_from(key, value, at, "nvml:0:temp", 1)
}

pub fn gauge_from(
    key: &MetricKey,
    value: f64,
    at: u64,
    source: &str,
    generation: u32,
) -> CanonicalValue {
    CanonicalValue {
        key: key.clone(),
        cycle_id: 1,
        source_id: Some(source_id(source)),
        source_generation: Some(generation),
        unit: unit_of(key),
        value: Some(Scalar::from_f64(value).unwrap_or_else(|error| panic!("value: {error}"))),
        sample_kind: SampleKind::Measured,
        selection_state: SelectionState::Selected,
        quality: Quality::Verified,
        freshness: Freshness::Fresh,
        measurement_freshness: Freshness::Unknown,
        observed_boot_ns: None,
        read_finished_boot_ns: at,
        policy_revision: 1,
        candidate_source_ids: Vec::new(),
        reason_codes: Vec::new(),
        eligible_for_history: true,
    }
}

/// A value without a value (conflict) at `at`.
pub fn conflict(key: &MetricKey, at: u64) -> CanonicalValue {
    CanonicalValue {
        source_id: None,
        source_generation: None,
        value: None,
        selection_state: SelectionState::Conflict,
        eligible_for_history: false,
        reason_codes: vec!["source_conflict".to_owned()],
        ..gauge(key, 0.0, at)
    }
}

/// Exact counter interval `[start, end)` of `delta` bytes for a rate key.
pub fn interval(key: &MetricKey, start: u64, end: u64, delta: u64) -> CounterInterval {
    CounterInterval {
        key: key.clone(),
        source_id: source_id("network:eth0:rx"),
        source_generation: 1,
        start_boot_ns: start,
        end_boot_ns: end,
        delta: Scalar::from_u64(delta),
        unit: CanonicalUnit::Bytes,
    }
}

/// Raw adapter batch with one reading of `source` at `at`.
pub fn raw_input(key: &MetricKey, source: &str, at: u64) -> Arc<IngressBatch> {
    let observation = ObservationBuilder::new(key.clone(), "nvml", source)
        .unit("degC")
        .f64(50.0)
        .read_at(at)
        .build();
    Arc::new(IngressBatch::new(
        batch_of("nvml", "fast", 1, vec![observation]),
        None,
    ))
}

pub fn value_f64(point: &HistoryPoint) -> f64 {
    match point.value {
        Some(value) => value.to_f64_lossy(),
        None => panic!("point has no value: {point:?}"),
    }
}

pub fn value_u64(point: &HistoryPoint) -> u64 {
    match point.value.and_then(Scalar::as_u64) {
        Some(value) => value,
        None => panic!("point has no exact u64 value: {point:?}"),
    }
}

/// Points in time order, contiguous, inside the window, coverage <= span.
pub fn assert_well_formed(result: &HistoryResult) {
    let mut cursor = result.start_boot_ns;
    for point in &result.points {
        assert!(
            point.start_boot_ns >= cursor,
            "points overlap or go back: {point:?}"
        );
        assert!(
            point.end_boot_ns > point.start_boot_ns,
            "empty point {point:?}"
        );
        assert!(
            point.end_boot_ns <= result.end_boot_ns,
            "point outside window {point:?}"
        );
        assert!(
            point.covered_ns <= point.end_boot_ns - point.start_boot_ns,
            "over-covered point {point:?}"
        );
        assert!(point.source_ids.len() <= 4);
        if point.value.is_none() {
            assert_eq!(point.covered_ns, 0, "null point with coverage {point:?}");
        }
        cursor = point.end_boot_ns;
    }
    assert!(result.covered_ns <= result.end_boot_ns - result.start_boot_ns);
    assert!((0.0..=1.0).contains(&result.coverage_ratio));
    let json = serde_json::to_value(result).unwrap_or_else(|error| panic!("serialize: {error}"));
    let back: HistoryResult =
        serde_json::from_value(json).unwrap_or_else(|error| panic!("round trip: {error}"));
    assert_eq!(&back, result);
}
