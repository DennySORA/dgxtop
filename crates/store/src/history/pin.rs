//! Query pins: what a history query takes out of the shared state under a
//! short read lock.
//!
//! Sealed chunks are shared through `Arc` (no copy); only the open chunks and
//! the overlapping gaps are copied. The copies and the expected result are
//! reserved in the `history_query` partition before anything is copied, and
//! the pin pulls at most one series, so a slow query pins bounded memory.

use dgxtop_core::budget::{ByteLedger, BytePermit, CapacityError, Partition};
use dgxtop_core::history::HistoryStat;
use dgxtop_core::identity::MetricKey;
use dgxtop_core::metric::MetricDescriptor;

use super::accounted::bytes_of;
use super::chunk::LogPin;
use super::gaps::GapEntry;
use super::records::{CounterBucket, CounterSample, GaugeBucket, GaugeSample};
use super::segment::SegmentsView;
use super::series::Series;
use super::state::HistoryState;

/// Fixed bytes of one pin (chunk pointer vectors, segment view, bookkeeping).
const PIN_OVERHEAD_BYTES: u64 = 4096;

/// What a query asks to pin.
pub(crate) struct PinRequest<'q> {
    pub key: &'q MetricKey,
    pub start_ns: u64,
    pub end_ns: u64,
    pub stat: HistoryStat,
    /// Estimated bytes of the result the query will build.
    pub result_bytes: u64,
}

/// Pinned gauge history.
pub(crate) struct PinnedGauge {
    pub raw: LogPin<GaugeSample>,
    pub carry: Option<GaugeSample>,
    pub raw_floor_ns: u64,
    pub buckets: LogPin<GaugeBucket>,
    pub rollup_floor_ns: u64,
}

/// Pinned counter history.
pub(crate) struct PinnedCounter {
    pub raw: LogPin<CounterSample>,
    pub raw_floor_ns: u64,
    pub buckets: LogPin<CounterBucket>,
    pub bridges: LogPin<CounterSample>,
    pub rollup_floor_ns: u64,
}

/// The track a query evaluates.
pub(crate) enum PinnedTrack {
    Gauge(PinnedGauge),
    Counter(PinnedCounter),
}

/// Everything one query needs, detached from the shared state.
pub(crate) struct PinnedSeries {
    pub revision: u64,
    pub descriptor: &'static MetricDescriptor,
    pub segments: SegmentsView,
    pub finalized_through_ns: u64,
    pub track: PinnedTrack,
    /// Gaps overlapping the window (series and global), ordered by start.
    pub gaps: Vec<GapEntry>,
    pub discontinuities: Vec<u64>,
    _scratch: BytePermit,
}

/// Outcome of pinning.
pub(crate) enum PinOutcome {
    /// The store holds no series for the key (unknown key or other boot).
    NoSeries {
        revision: u64,
    },
    Pinned(Box<PinnedSeries>),
}

fn wants_counter(series: &Series, stat: HistoryStat) -> bool {
    stat == HistoryStat::Total
        && (series.segments.has_counter_segment() || series.counter.holds_data())
}

fn scratch_bytes(
    series: &Series,
    counter: bool,
    gap_count: usize,
    discontinuity_count: usize,
) -> u64 {
    let open = if counter {
        series
            .counter
            .raw
            .open_bytes()
            .saturating_add(series.counter.buckets.open_bytes())
            .saturating_add(series.counter.bridges.open_bytes())
    } else {
        series
            .gauge
            .raw
            .open_bytes()
            .saturating_add(series.gauge.buckets.open_bytes())
    };
    open.saturating_add(bytes_of::<GapEntry>(gap_count))
        .saturating_add(bytes_of::<u64>(discontinuity_count))
        .saturating_add(PIN_OVERHEAD_BYTES)
}

impl HistoryState {
    /// Pins one series for a query. Fails with a capacity error, before
    /// copying anything, when the `history_query` partition cannot hold the
    /// copies and the result.
    pub(crate) fn pin(
        &self,
        request: &PinRequest<'_>,
        ledger: &ByteLedger,
    ) -> Result<PinOutcome, CapacityError> {
        let Some(series) = self
            .handle_of(request.key)
            .and_then(|handle| self.slots.get(handle))
        else {
            return Ok(PinOutcome::NoSeries {
                revision: self.revision,
            });
        };
        let (start, end) = (request.start_ns, request.end_ns);
        let mut gaps = series.gaps.overlapping(request.key, start, end);
        gaps.extend(self.global_gaps.overlapping(request.key, start, end));
        gaps.sort_by_key(|gap| gap.start_ns);
        let discontinuities: Vec<u64> = self
            .discontinuities
            .iter()
            .copied()
            .filter(|boot_ns| *boot_ns >= start && *boot_ns < end)
            .collect();
        let counter = wants_counter(series, request.stat);
        let bytes = scratch_bytes(series, counter, gaps.len(), discontinuities.len())
            .saturating_add(request.result_bytes);
        let scratch = ledger.try_reserve(Partition::HistoryQuery, bytes)?;
        let track = if counter {
            PinnedTrack::Counter(PinnedCounter {
                raw: series.counter.raw.pin_around(start, end),
                raw_floor_ns: series.counter.raw_floor_ns,
                buckets: series.counter.buckets.pin_around(start, end),
                bridges: series.counter.bridges.pin_around(start, end),
                rollup_floor_ns: series.counter.rollup_floor_ns,
            })
        } else {
            PinnedTrack::Gauge(PinnedGauge {
                raw: series.gauge.raw.pin_around(start, end),
                carry: series.gauge.carry,
                raw_floor_ns: series.gauge.raw_floor_ns,
                buckets: series.gauge.buckets.pin_around(start, end),
                rollup_floor_ns: series.gauge.rollup_floor_ns,
            })
        };
        Ok(PinOutcome::Pinned(Box::new(PinnedSeries {
            revision: self.revision,
            descriptor: series.descriptor,
            segments: series.segments.view(),
            finalized_through_ns: series.finalized_through_ns,
            track,
            gaps,
            discontinuities,
            _scratch: scratch,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::chunk::chunk_bytes;
    use crate::history::segment::SegmentIdentity;
    use crate::history::state::{GaugeAppend, HistoryAppends, HistorySettings};
    use dgxtop_core::history::HistoryMethod;
    use dgxtop_core::metric::{CanonicalUnit, Scalar, descriptor, ids};
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::testing::{metric_key, source_id};

    const SECOND: u64 = 1_000_000_000;

    fn state(ledger: &ByteLedger) -> HistoryState {
        HistoryState::new(
            HistorySettings {
                raw_history_ns: 30 * SECOND,
                aggregate_retention_ns: 24 * 3600 * SECOND,
                max_series: 8,
                finalize_grace_ns: 5 * SECOND,
                gap_merge_tolerance_ns: SECOND,
                archive_enabled: false,
            },
            ledger,
        )
    }

    fn samples(key: &MetricKey, seconds: u64) -> HistoryAppends {
        let gauges = (0..seconds)
            .map(|second| GaugeAppend {
                key: key.clone(),
                descriptor: descriptor(ids::GPU_TEMPERATURE).unwrap(),
                identity: SegmentIdentity {
                    source_id: source_id("nvml:0:temp"),
                    source_generation: 1,
                    policy_revision: 1,
                    sample_kind: SampleKind::Measured,
                    method: HistoryMethod::TimeWeightedStep,
                },
                unit: CanonicalUnit::DegC,
                time_ns: second * SECOND,
                stale_deadline_ns: (second + 3) * SECOND,
                value: Scalar::from_f64(50.0).unwrap(),
            })
            .collect();
        HistoryAppends {
            gauges,
            ..HistoryAppends::default()
        }
    }

    #[test]
    fn chunks_pinned_by_a_running_query_stay_accounted_after_eviction() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut state = state(&ledger);
        let key = metric_key("gpu:0", ids::GPU_TEMPERATURE, "gpu");
        state.apply(samples(&key, 200), 200 * SECOND, 1);
        let request = PinRequest {
            key: &key,
            start_ns: 0,
            end_ns: 200 * SECOND,
            stat: HistoryStat::Mean,
            result_bytes: 1024,
        };
        let Ok(PinOutcome::Pinned(pinned)) = state.pin(&request, &ledger) else {
            panic!("series must pin");
        };
        assert!(ledger.used(Partition::HistoryQuery) > 1024);
        state.maintain(400 * SECOND);
        // Three sealed chunks were evicted from the log but the query still
        // references them; only the copied open chunk was released.
        assert_eq!(
            ledger.used(Partition::RawHistory),
            3 * chunk_bytes::<GaugeSample>()
        );
        drop(pinned);
        assert_eq!(ledger.used(Partition::RawHistory), 0);
        assert_eq!(ledger.used(Partition::HistoryQuery), 0);
    }

    #[test]
    fn unknown_keys_and_full_query_partitions_pin_nothing() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut state = state(&ledger);
        let key = metric_key("gpu:0", ids::GPU_TEMPERATURE, "gpu");
        state.apply(samples(&key, 10), 10 * SECOND, 3);
        let other = metric_key("gpu:9", ids::GPU_TEMPERATURE, "gpu");
        let request = PinRequest {
            key: &other,
            start_ns: 0,
            end_ns: 10 * SECOND,
            stat: HistoryStat::Mean,
            result_bytes: 0,
        };
        assert!(matches!(
            state.pin(&request, &ledger),
            Ok(PinOutcome::NoSeries { revision: 3 })
        ));
        let huge = PinRequest {
            key: &key,
            start_ns: 0,
            end_ns: 10 * SECOND,
            stat: HistoryStat::Mean,
            result_bytes: u64::MAX / 2,
        };
        assert!(state.pin(&huge, &ledger).is_err());
        assert_eq!(ledger.used(Partition::HistoryQuery), 0);
    }
}
