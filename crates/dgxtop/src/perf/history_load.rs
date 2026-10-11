//! Realistic-shape canonical history for history-query benchmarks
//! (`PERFORMANCE.md` §2: "preload a real-shape 24 h aggregate, then measure").
//!
//! The shape: up to 24 h of minute rollups, the newest 300 s at full
//! resolution, exact counter intervals crossing every minute boundary
//! (counter bridges), sporadic missing samples and source conflicts, one
//! 10-minute outage and one source change per 20 series (gaps, segments).
//!
//! Bulk history is committed at a 60 s cadence (one commit per minute
//! bucket, phase 30 s so each counter interval crosses a minute boundary);
//! the rollups, bridges and segments a query scans are the same as with
//! 1 s sampling, only the per-bucket sample count differs. The tail is
//! committed at 1 s like live data. Values are deterministic functions of
//! the seed, series and time, and every value carries the reason code
//! [`PRELOAD_REASON`]: it is synthetic workload, never hardware evidence.

use std::collections::BTreeSet;

use dgxtop_core::canonical::{
    CanonicalValue, CollectionBatch, CollectorState, CommitGuard, CounterInterval, StoreAck,
};
use dgxtop_core::clock::{NANOS_PER_SECOND, splitmix64};
use dgxtop_core::config::SamplingConfig;
use dgxtop_core::identity::{
    BootId, CollectorId, EntityId, HostId, IdentityError, MetricId, MetricKey, Scope, SourceId,
};
use dgxtop_core::metric::{CanonicalUnit, METRIC_CATALOG, MetricRole, Scalar, descriptor};
use dgxtop_core::ports::StoreWriter;
use dgxtop_core::quality::{Freshness, Quality, SampleKind, SelectionState};
use dgxtop_state::StateWriter;
use serde::Serialize;
use uuid::Uuid;

/// Reason code on every preloaded value.
pub const PRELOAD_REASON: &str = "SYNTHETIC_HISTORY_PRELOAD";
const COLLECTOR: &str = "history-preload";
const MINUTE_NS: u64 = 60 * NANOS_PER_SECOND;
const DAY_SECONDS: u64 = 86_400;
const OUTAGE_NS: u64 = 10 * MINUTE_NS;
/// One series in this many has an outage, another one a source change.
const SPECIAL_EVERY: usize = 20;
const OUTAGE_SLOT: usize = 7;
const SWITCH_SLOT: usize = 3;
/// Per-mille of samples missing / in conflict.
const MISSING_PER_MILLE: u64 = 2;
const CONFLICT_PER_TEN_THOUSAND: u64 = 5;

/// One history-eligible series to preload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySeries {
    pub key: MetricKey,
    pub unit: CanonicalUnit,
    /// Counter-derived rate: carries exact intervals (window totals, bridges).
    pub rate: bool,
}

impl HistorySeries {
    /// `None` unless the key's metric is a history-eligible canonical metric.
    pub fn for_key(key: MetricKey) -> Option<Self> {
        let known = descriptor(key.metric_id.as_str())?;
        let rate = matches!(
            known.unit,
            CanonicalUnit::BytesPerSecond | CanonicalUnit::CountPerSecond
        );
        (known.history && known.role == MetricRole::Canonical).then_some(Self {
            rate,
            unit: known.unit,
            key,
        })
    }
}

/// History-eligible series among `keys` (deduplicated, sorted, at most `limit`).
pub fn history_series(
    keys: impl IntoIterator<Item = MetricKey>,
    limit: usize,
) -> Vec<HistorySeries> {
    keys.into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(HistorySeries::for_key)
        .take(limit)
        .collect()
}

/// `count` keys cycling over every history-eligible canonical metric.
pub fn synthetic_keys(
    host_id: &HostId,
    boot_id: &BootId,
    count: usize,
) -> Result<Vec<MetricKey>, IdentityError> {
    let metrics: Vec<&str> = METRIC_CATALOG
        .iter()
        .filter(|known| known.history && known.role == MetricRole::Canonical)
        .map(|known| known.id)
        .collect();
    let width = metrics.len().max(1);
    (0..count)
        .map(|index| {
            let metric = metrics
                .get(index % width)
                .copied()
                .unwrap_or("cpu.utilization.total");
            Ok(MetricKey::new(
                host_id.clone(),
                boot_id.clone(),
                EntityId::new(format!("bench.entity.{}", index / width))?,
                MetricId::new(metric)?,
                Scope::new("bench")?,
            ))
        })
        .collect()
}

/// Time layout of a preload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PreloadShape {
    pub span_ns: u64,
    pub bulk_step_ns: u64,
    pub fine_tail_ns: u64,
    pub fine_step_ns: u64,
}

impl PreloadShape {
    /// 24 h, minute bulk, 300 s at 1 s.
    pub const fn reference() -> Self {
        Self {
            span_ns: DAY_SECONDS * NANOS_PER_SECOND,
            bulk_step_ns: MINUTE_NS,
            fine_tail_ns: 300 * NANOS_PER_SECOND,
            fine_step_ns: NANOS_PER_SECOND,
        }
    }

    pub const fn with_span(self, span_ns: u64) -> Self {
        Self { span_ns, ..self }
    }
}

/// What a preload committed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PreloadReport {
    pub series: usize,
    pub start_ns: u64,
    pub end_ns: u64,
    pub commits: u64,
    pub rejected: u64,
    pub values: u64,
    pub intervals: u64,
    pub first_rejection: Option<String>,
}

impl PreloadReport {
    pub fn span_seconds(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns) / NANOS_PER_SECOND
    }
}

/// Where special events of the timeline happen.
#[derive(Debug, Clone, Copy)]
struct Timeline {
    outage_start_ns: u64,
    switch_ns: u64,
}

enum Sample {
    Absent,
    Conflict,
    Value {
        value: f64,
        interval: Option<(u64, u64)>,
    },
}

/// Commits synthetic history into one hot store (the only writer of it).
#[derive(Debug)]
pub struct HistoryFeeder {
    series: Vec<HistorySeries>,
    seed: u64,
    collector: CollectorId,
    sampling: SamplingConfig,
    state_revision: u64,
    cycle: u64,
    last_end_ns: Vec<Option<u64>>,
    timeline: Option<Timeline>,
    report: PreloadReport,
}

impl HistoryFeeder {
    pub fn new(
        series: Vec<HistorySeries>,
        seed: u64,
        sampling: &SamplingConfig,
    ) -> Result<Self, IdentityError> {
        Ok(Self {
            last_end_ns: vec![None; series.len()],
            report: PreloadReport {
                series: series.len(),
                ..PreloadReport::default()
            },
            series,
            seed,
            collector: CollectorId::new(COLLECTOR)?,
            sampling: sampling.clone(),
            state_revision: 0,
            cycle: 0,
            timeline: None,
        })
    }

    pub fn series(&self) -> &[HistorySeries] {
        &self.series
    }

    /// Fills `[end − span, end]` and returns what was committed.
    pub fn preload(
        &mut self,
        writer: &mut StateWriter,
        shape: PreloadShape,
        end_ns: u64,
    ) -> PreloadReport {
        let start = end_ns.saturating_sub(shape.span_ns).max(shape.bulk_step_ns);
        let fine_start = end_ns.saturating_sub(shape.fine_tail_ns).max(start);
        self.timeline = Some(Timeline {
            outage_start_ns: start + (end_ns - start) / 4,
            switch_ns: start + (end_ns - start) / 2,
        });
        self.report.start_ns = start;
        let bulk = shape.bulk_step_ns.max(NANOS_PER_SECOND);
        self.use_cadence(writer, bulk);
        let mut at = start.div_ceil(bulk) * bulk + bulk / 2;
        while at < fine_start {
            self.commit_at(writer, at, bulk);
            writer.maintain(at);
            at += bulk;
        }
        let fine = shape.fine_step_ns.max(1);
        self.use_cadence(writer, fine);
        let mut at = fine_start.div_ceil(fine) * fine + fine / 2;
        while at <= end_ns {
            self.commit_at(writer, at, fine);
            if at % MINUTE_NS < fine {
                writer.maintain(at);
            }
            at += fine;
        }
        writer.maintain(end_ns);
        self.report.end_ns = end_ns;
        self.report.clone()
    }

    /// One more live-cadence step at `at_ns` (continuous feeding after a
    /// preload); the caller runs `maintain` as the Manager would.
    pub fn append(&mut self, writer: &mut StateWriter, at_ns: u64, step_ns: u64) -> StoreAck {
        let ack = self.commit_at(writer, at_ns, step_ns);
        self.report.end_ns = self.report.end_ns.max(at_ns);
        ack
    }

    pub fn report(&self) -> &PreloadReport {
        &self.report
    }

    fn use_cadence(&mut self, writer: &mut StateWriter, step_ns: u64) {
        let milliseconds = u32::try_from(step_ns / 1_000_000)
            .unwrap_or(u32::MAX)
            .clamp(100, 60_000);
        self.sampling.interval_ms = milliseconds;
        writer.set_sampling(&self.sampling);
    }

    fn commit_at(&mut self, writer: &mut StateWriter, at_ns: u64, step_ns: u64) -> StoreAck {
        let batch = self.batch_at(at_ns, step_ns);
        let guard = CommitGuard {
            expected_epoch: batch.epoch,
            collector_id: batch.collector_id.clone(),
            state_base_revision: batch.state_base_revision,
        };
        let values = u64::try_from(batch.canonical.len()).unwrap_or(u64::MAX);
        let intervals = u64::try_from(batch.counter_intervals.len()).unwrap_or(u64::MAX);
        let ack = writer.commit_collection(batch, &guard, at_ns);
        self.report.commits += 1;
        if ack.status.is_committed() {
            self.state_revision = ack.state_revision;
            self.report.values += values;
            self.report.intervals += intervals;
        } else {
            self.report.rejected += 1;
            if self.report.first_rejection.is_none() {
                self.report.first_rejection = Some(
                    ack.reason
                        .clone()
                        .unwrap_or_else(|| format!("{:?}", ack.status)),
                );
            }
        }
        ack
    }

    fn batch_at(&mut self, at_ns: u64, step_ns: u64) -> CollectionBatch {
        self.cycle += 1;
        let mut canonical = Vec::with_capacity(self.series.len());
        let mut counter_intervals = Vec::new();
        for index in 0..self.series.len() {
            match self.sample(index, at_ns, step_ns) {
                Sample::Absent => {}
                Sample::Conflict => {
                    self.last_end_ns[index] = None;
                    canonical.push(self.conflict_value(index, at_ns));
                }
                Sample::Value { value, interval } => {
                    if let Some(interval) =
                        interval.and_then(|(start, end)| self.interval(index, start, end, value))
                    {
                        counter_intervals.push(interval);
                    }
                    if let Some(selected) = self.selected_value(index, at_ns, value) {
                        canonical.push(selected);
                    }
                    if self.series[index].rate {
                        self.last_end_ns[index] = Some(at_ns);
                    }
                }
            }
        }
        CollectionBatch {
            batch_id: Uuid::new_v4(),
            collector_id: self.collector.clone(),
            cycle_id: self.cycle,
            epoch: 1,
            config_revision: 1,
            catalog_revision: 1,
            policy_revision: 1,
            state_base_revision: self.state_revision,
            canonical,
            explanations: Vec::new(),
            counter_intervals,
            inputs: Vec::new(),
            process_table: None,
            next_state: CollectorState::default(),
            failures: Vec::new(),
            events: Vec::new(),
        }
    }

    fn hash(&self, parts: &[u64]) -> u64 {
        parts.iter().fold(splitmix64(self.seed), |state, part| {
            splitmix64(state ^ part)
        })
    }

    fn sample(&self, index: usize, at_ns: u64, step_ns: u64) -> Sample {
        let slot = u64::try_from(index).unwrap_or(u64::MAX);
        let step = at_ns / step_ns.max(1);
        let in_outage = self.timeline.is_some_and(|timeline| {
            index % SPECIAL_EVERY == OUTAGE_SLOT
                && (timeline.outage_start_ns..timeline.outage_start_ns + OUTAGE_NS).contains(&at_ns)
        });
        if in_outage || self.hash(&[1, slot, step]) % 1000 < MISSING_PER_MILLE {
            return Sample::Absent;
        }
        if self.hash(&[2, slot, step]) % 10_000 < CONFLICT_PER_TEN_THOUSAND {
            return Sample::Conflict;
        }
        let value = self.level(index, at_ns);
        let interval = self.series[index]
            .rate
            .then(|| self.last_end_ns[index].map(|start| (start, at_ns)))
            .flatten();
        Sample::Value { value, interval }
    }

    /// Daily triangle wave plus ±5 % noise inside the unit's typical range.
    #[allow(clippy::cast_precision_loss)]
    fn level(&self, index: usize, at_ns: u64) -> f64 {
        let (low, high) = typical_range(self.series[index].unit);
        let slot = u64::try_from(index).unwrap_or(u64::MAX);
        let second = (at_ns / NANOS_PER_SECOND + slot * 3_607) % DAY_SECONDS;
        let triangle = ((2 * second) as f64 / DAY_SECONDS as f64 - 1.0).abs();
        let noise = (self.hash(&[3, slot, at_ns / NANOS_PER_SECOND]) % 2001) as f64 / 1000.0 - 1.0;
        let share = (0.2 + 0.6 * triangle + 0.05 * noise).clamp(0.0, 1.0);
        low + (high - low) * share
    }

    fn source(&self, index: usize, at_ns: u64) -> (Option<SourceId>, u32) {
        let switched = index % SPECIAL_EVERY == SWITCH_SLOT
            && self
                .timeline
                .is_some_and(|timeline| at_ns >= timeline.switch_ns);
        let (suffix, generation) = if switched { (":b", 2) } else { ("", 1) };
        (
            SourceId::new(format!("{COLLECTOR}:{index}{suffix}")).ok(),
            generation,
        )
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn interval(&self, index: usize, start: u64, end: u64, rate: f64) -> Option<CounterInterval> {
        let (source_id, generation) = self.source(index, end);
        let seconds = end.checked_sub(start)? as f64 / NANOS_PER_SECOND as f64;
        let delta = (rate * seconds).round().max(0.0) as u64;
        Some(CounterInterval {
            key: self.series[index].key.clone(),
            source_id: source_id?,
            source_generation: generation,
            start_boot_ns: start,
            end_boot_ns: end,
            delta: Scalar::from_u64(delta),
            unit: match self.series[index].unit {
                CanonicalUnit::BytesPerSecond => CanonicalUnit::Bytes,
                _ => CanonicalUnit::Count,
            },
        })
    }

    fn selected_value(&self, index: usize, at_ns: u64, value: f64) -> Option<CanonicalValue> {
        let (source_id, generation) = self.source(index, at_ns);
        Some(CanonicalValue {
            source_id: Some(source_id?),
            source_generation: Some(generation),
            value: Some(Scalar::from_f64(value).ok()?),
            selection_state: SelectionState::Selected,
            eligible_for_history: true,
            ..self.empty_value(index, at_ns)
        })
    }

    fn conflict_value(&self, index: usize, at_ns: u64) -> CanonicalValue {
        CanonicalValue {
            selection_state: SelectionState::Conflict,
            reason_codes: vec![PRELOAD_REASON.to_owned(), "source_conflict".to_owned()],
            ..self.empty_value(index, at_ns)
        }
    }

    fn empty_value(&self, index: usize, at_ns: u64) -> CanonicalValue {
        CanonicalValue {
            key: self.series[index].key.clone(),
            cycle_id: self.cycle,
            source_id: None,
            source_generation: None,
            unit: self.series[index].unit,
            value: None,
            sample_kind: SampleKind::Measured,
            selection_state: SelectionState::Unavailable,
            quality: Quality::Verified,
            freshness: Freshness::Fresh,
            measurement_freshness: Freshness::Unknown,
            observed_boot_ns: None,
            read_finished_boot_ns: at_ns,
            policy_revision: 1,
            candidate_source_ids: Vec::new(),
            reason_codes: vec![PRELOAD_REASON.to_owned()],
            eligible_for_history: false,
        }
    }
}

fn typical_range(unit: CanonicalUnit) -> (f64, f64) {
    match unit {
        CanonicalUnit::Percent => (0.0, 100.0),
        CanonicalUnit::DegC => (25.0, 90.0),
        CanonicalUnit::Watts => (5.0, 700.0),
        CanonicalUnit::MHz => (300.0, 3000.0),
        CanonicalUnit::Bytes => (1_073_741_824.0, 68_719_476_736.0),
        CanonicalUnit::BytesPerSecond => (0.0, 1_250_000_000.0),
        CanonicalUnit::CountPerSecond => (0.0, 100_000.0),
        CanonicalUnit::Count => (0.0, 1000.0),
        CanonicalUnit::Milliseconds => (0.0, 50.0),
        CanonicalUnit::Joules => (0.0, 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::budget::ByteLedger;
    use dgxtop_core::clock::TimeProvider;
    use dgxtop_core::config::Config;
    use dgxtop_core::history::{HISTORY_SCHEMA_VERSION, HistoryQuery, HistoryStat};
    use dgxtop_core::ports::{CancelFlag, HistoryReader};
    use dgxtop_core::quality::Namespace;

    use crate::live_store::LiveStore;
    use crate::perf::pipeline::ManualClock;

    const HOUR_NS: u64 = 3_600 * NANOS_PER_SECOND;

    fn preloaded(series: usize, span_ns: u64) -> (LiveStore, HistoryFeeder, PreloadReport, u64) {
        let config = Config::defaults().unwrap();
        let store = LiveStore::new(
            &config.store,
            &config.sampling,
            &config.runtime,
            ByteLedger::for_memory_limit_mib(256),
            Namespace::Live,
            false,
        );
        let host = HostId::new("h").unwrap();
        let boot = BootId::new("bench-manual-boot").unwrap();
        let keys = synthetic_keys(&host, &boot, series).unwrap();
        let mut feeder =
            HistoryFeeder::new(history_series(keys, series), 9, &config.sampling).unwrap();
        let end = 30 * HOUR_NS;
        let mut store = store;
        let report = feeder.preload(
            &mut store.writer,
            PreloadShape::reference().with_span(span_ns),
            end,
        );
        (store, feeder, report, end)
    }

    fn query(
        key: &MetricKey,
        start: u64,
        end: u64,
        stat: HistoryStat,
        resolution_ms: u32,
    ) -> HistoryQuery {
        HistoryQuery {
            schema_version: HISTORY_SCHEMA_VERSION,
            request_id: Uuid::new_v4(),
            key: key.clone(),
            start_boot_ns: start,
            end_boot_ns: end,
            resolution_ms,
            stat,
            max_points: 161,
            source_filter: None,
        }
    }

    #[test]
    fn preload_commits_every_batch_and_answers_window_queries() {
        let (store, feeder, report, end) = preloaded(24, 2 * HOUR_NS);
        assert_eq!(report.rejected, 0, "{:?}", report.first_rejection);
        assert!(report.commits > 300);
        assert!(report.intervals > 0, "rate series carry exact intervals");
        assert_eq!(report.span_seconds(), 2 * 3_600);
        let clock = ManualClock::new(end).unwrap();
        let gauge = feeder.series().iter().find(|series| !series.rate).unwrap();
        let result = store
            .history
            .query(
                &query(
                    &gauge.key,
                    end - 2 * HOUR_NS,
                    end,
                    HistoryStat::Mean,
                    45_000,
                ),
                u64::MAX,
                &CancelFlag::new(),
                clock.as_ref() as &dyn TimeProvider,
            )
            .unwrap();
        assert!(
            result.coverage_ratio > 0.9,
            "coverage {}",
            result.coverage_ratio
        );
        assert!(!result.points.is_empty() && result.points.len() <= 161);
        let rate = feeder.series().iter().find(|series| series.rate).unwrap();
        let total = store
            .history
            .query(
                &query(&rate.key, end - HOUR_NS, end, HistoryStat::Total, 3_600_000),
                u64::MAX,
                &CancelFlag::new(),
                clock.as_ref() as &dyn TimeProvider,
            )
            .unwrap();
        assert!(total.points.iter().any(|point| point.value.is_some()));
    }

    #[test]
    fn outage_series_show_a_gap_and_only_eligible_keys_are_kept() {
        let (store, feeder, _, end) = preloaded(SPECIAL_EVERY, 2 * HOUR_NS);
        let outage = &feeder.series()[OUTAGE_SLOT];
        let clock = ManualClock::new(end).unwrap();
        let result = store
            .history
            .query(
                &query(
                    &outage.key,
                    end - 2 * HOUR_NS,
                    end,
                    HistoryStat::Mean,
                    60_000,
                ),
                u64::MAX,
                &CancelFlag::new(),
                clock.as_ref() as &dyn TimeProvider,
            )
            .unwrap();
        assert!(
            result.coverage_ratio < 0.97,
            "10 min outage visible: {}",
            result.coverage_ratio
        );
        let host = HostId::new("h").unwrap();
        let boot = BootId::new("b").unwrap();
        let raw = MetricKey::new(
            host,
            boot,
            EntityId::new("cpu").unwrap(),
            MetricId::new("cpu.time.user").unwrap(),
            Scope::new("host").unwrap(),
        );
        assert!(
            HistorySeries::for_key(raw).is_none(),
            "raw counters are not history series"
        );
    }
}
