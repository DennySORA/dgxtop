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
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::{BootId, CollectorId, MetricKey};
use dgxtop_core::metric::{CanonicalUnit, Scalar, descriptor, ids};
use dgxtop_core::overview_history::{OverviewAnswer, OverviewBucket, OverviewRequest};
use dgxtop_core::ports::{
    ArchiveJournal, CancelFlag, IngressBatch, OverviewReader, SnapshotReader, StoreWriter,
};
use dgxtop_core::quality::{Freshness, Namespace, Quality, SampleKind, SelectionState};
use dgxtop_core::testing::{
    FakeClock, ObservationBuilder, UTC_ON_A_WHOLE_MINUTE_NS, batch_of, metric_key, source_id,
};
use dgxtop_core::tiers::Tier;
use dgxtop_history::HistoryIndex;
use dgxtop_state::{SharedSnapshot, StateWriter};
use uuid::Uuid;

/// The longest window a test reads: everything a test series holds.
pub const EVERYTHING_NS: u64 = 24 * 3600 * SECOND;

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

    /// A paired harness whose history queues finished minutes for the
    /// archive.
    pub fn archived(config: Config) -> Self {
        Self::build(
            config,
            ByteLedger::for_memory_limit_mib(256),
            Namespace::Live,
            true,
        )
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
            events: Vec::new(),
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

    /// A clock at the harness's boot time on the wall clock
    /// [`Harness::build`] paired (boot 0 on a whole UTC minute), as the
    /// Manager's history reads see it.
    pub fn reader_clock(&self) -> Arc<FakeClock> {
        let boot_ns = self.clock.now_boot_ns();
        let clock = FakeClock::new(boot_ns);
        let delta = utc_of(boot_ns) - i64::try_from(clock.now_utc_ns()).unwrap_or(i64::MAX);
        clock.step_utc_ns(delta);
        clock
    }

    /// The Overview's answer to `request`, with no deadline.
    pub fn read(&self, request: &OverviewRequest) -> Result<OverviewAnswer, ErrorDetail> {
        self.read_with(
            request,
            u64::MAX,
            &CancelFlag::new(),
            self.reader_clock().as_ref(),
        )
    }

    pub fn read_with(
        &self,
        request: &OverviewRequest,
        deadline: u64,
        cancel: &CancelFlag,
        clock: &dyn TimeProvider,
    ) -> Result<OverviewAnswer, ErrorDetail> {
        self.history.read(request, deadline, cancel, clock)
    }

    /// What `key` holds in the `span_ns` ending now, merged from the
    /// Overview's minutes (the finalized and the ones still filling).
    pub fn window(&self, key: &MetricKey, span_ns: u64) -> OverviewBucket {
        let request = OverviewRequest::Windows {
            series: SeriesInEveryBoot::from(key),
            spans_ns: vec![span_ns],
            at_utc_ns: utc_of(self.clock.now_boot_ns()),
        };
        match self.read(&request) {
            Ok(OverviewAnswer::Windows(windows)) => windows[0],
            other => panic!("asked for a window, got {other:?}"),
        }
    }

    /// The bars of `tier` starting in boot `[from, end)`, holding a value.
    pub fn bars(
        &self,
        key: &MetricKey,
        tier: Tier,
        (from, end): (u64, u64),
    ) -> Vec<OverviewBucket> {
        let request = OverviewRequest::Bars {
            series: SeriesInEveryBoot::from(key),
            tier,
            from_utc_ns: utc_of(from),
            end_utc_ns: utc_of(end),
            at_utc_ns: utc_of(self.clock.now_boot_ns()),
        };
        match self.read(&request) {
            Ok(OverviewAnswer::Bars(bars)) => bars.bars,
            other => panic!("asked for bars, got {other:?}"),
        }
    }

    /// Finalizes every minute ending by `now` and takes what the history
    /// queued for the archive.
    pub fn archive_through(&mut self, now: u64) -> ArchiveJournal {
        self.maintain(now);
        self.writer
            .take_archive_journal(64 << 20)
            .unwrap_or_else(|| panic!("no archive journal by {now}"))
    }
}

/// UTC of `boot_ns` on the pairing of [`Harness::build`].
pub fn utc_of(boot_ns: u64) -> i64 {
    i64::try_from(UTC_ON_A_WHOLE_MINUTE_NS + i128::from(boot_ns)).unwrap_or(i64::MAX)
}

/// Counter total a journal holds: its minute buckets' exact totals plus
/// its bridges, each interval counted once.
pub fn archived_total(journal: &ArchiveJournal) -> u128 {
    let buckets: u128 = journal
        .buckets
        .iter()
        .filter_map(|bucket| bucket.counter_delta)
        .sum();
    buckets
        + journal
            .bridges
            .iter()
            .map(|bridge| bridge.delta)
            .sum::<u128>()
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
