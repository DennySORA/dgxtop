//! Memory collector: `/proc/meminfo` KiB fields to exact bytes, the swap
//! rates, the OOM kills and the memory's pressure averages (D-135).
//!
//! `memory.used = total - available` (never `total - free`) and
//! `memory.swap_used = swap_total - swap_free`, both with checked
//! subtraction: an inconsistent pair yields no value plus a reason. What is
//! swapped in and out each second comes from the exact deltas of the
//! kernel's page counters; the OOM kills stay the total since boot, and the
//! pressure averages pass through as the kernel computes them.

use dgxtop_core::canonical::{CollectionBatch, CollectionPlan};
use dgxtop_core::identity::{AdapterId, CollectorId, IdentityError};
use dgxtop_core::metric::{CanonicalUnit, Domain, MetricDescriptor, ids};
use dgxtop_core::naming;
use dgxtop_core::ports::Collector;
use dgxtop_core::pressure::PressureResource;

use crate::shared::context::{self, CollectorSpec, Route, Session};
use crate::shared::counter_inputs;
use crate::shared::formulas::{self, Difference, Selections};
use crate::shared::reasons;

const PASS_THROUGH: [&str; 6] = [
    ids::MEMORY_TOTAL,
    ids::MEMORY_AVAILABLE,
    ids::MEMORY_BUFFERS,
    ids::MEMORY_CACHED,
    ids::MEMORY_SWAP_TOTAL,
    ids::MEMORY_OOM_KILLS,
];

/// Gauges and totals published straight through arbitration.
fn is_pass_through(metric: &str) -> bool {
    PASS_THROUGH.contains(&metric) || PressureResource::Memory.reports(metric)
}

const USED: Difference = Difference {
    minuend: ids::MEMORY_TOTAL,
    subtrahend: ids::MEMORY_AVAILABLE,
    result: ids::MEMORY_USED,
    negative: reasons::AVAILABLE_EXCEEDS_TOTAL,
};

const SWAP_USED: Difference = Difference {
    minuend: ids::MEMORY_SWAP_TOTAL,
    subtrahend: ids::MEMORY_SWAP_FREE_RAW,
    result: ids::MEMORY_SWAP_USED,
    negative: reasons::SWAP_FREE_EXCEEDS_TOTAL,
};

/// Swap byte counter and the rate derived from it.
const SWAP_RATES: [(&str, &str); 2] = [
    (ids::MEMORY_SWAP_IN_BYTES, ids::MEMORY_SWAP_IN_BPS),
    (ids::MEMORY_SWAP_OUT_BYTES, ids::MEMORY_SWAP_OUT_BPS),
];

/// Metrics only this collector derives; adapters never provide them.
const DERIVED: [&str; 4] = [
    ids::MEMORY_USED,
    ids::MEMORY_SWAP_USED,
    ids::MEMORY_SWAP_IN_BPS,
    ids::MEMORY_SWAP_OUT_BPS,
];

fn accepts(metric: &MetricDescriptor) -> bool {
    metric.domain == Domain::Memory && !DERIVED.contains(&metric.id)
}

/// The swap counters are consumed once per reading; every gauge is kept
/// for arbitration and the formulas across batches.
fn retains(_route: &Route<'_>, metric: &MetricDescriptor) -> bool {
    SWAP_RATES.iter().all(|(counter, _)| *counter != metric.id)
}

static SPEC: CollectorSpec = CollectorSpec {
    id: naming::collectors::MEMORY,
    accepts,
    retains,
    leaves: context::leaves_none,
};

/// Host memory collector.
#[derive(Debug, Clone)]
pub struct MemoryCollector {
    id: CollectorId,
}

impl MemoryCollector {
    /// # Errors
    ///
    /// Returns an `IdentityError` when the value breaks the rules of its kind:
    /// its length, a control character or its syntax.
    pub fn new() -> Result<Self, IdentityError> {
        Ok(Self {
            id: CollectorId::new(naming::collectors::MEMORY)?,
        })
    }
}

impl Collector for MemoryCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn consumes(&self, adapter_id: &AdapterId, read_group: &str) -> bool {
        crate::shared::routes_to(adapter_id, read_group, SPEC.id)
    }

    fn collect(&self, plan: &CollectionPlan) -> CollectionBatch {
        context::run(plan, &SPEC, collect_memory)
    }
}

fn collect_memory(session: &mut Session<'_>) {
    for (counter_metric, rate_metric) in SWAP_RATES {
        counter_inputs::publish_key_rates(
            session,
            counter_metric,
            CanonicalUnit::Bytes,
            rate_metric,
        );
    }
    let mut selections = Selections::new();
    formulas::publish_pending(session, is_pass_through, &mut selections);
    formulas::publish_differences(session, USED, &mut selections);
    formulas::publish_differences(session, SWAP_USED, &mut selections);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{PlanBuilder, SECOND, collect, number, reading, value_of};
    use dgxtop_core::metric::native;
    use dgxtop_core::observation::Observation;
    use dgxtop_core::quality::{ReadStatus, SelectionState};
    use dgxtop_core::testing::metric_key;

    fn field(metric: &str, kib: u64, at: u64) -> Observation {
        reading(
            &metric_key("memory", metric, "system"),
            "procfs",
            &format!("procfs:meminfo:{metric}"),
            at,
        )
        .unit(native::KIB)
        .u64(kib)
        .build()
    }

    fn meminfo(total: u64, available: u64, at: u64) -> Vec<Observation> {
        vec![
            field(ids::MEMORY_TOTAL, total, at),
            field(ids::MEMORY_AVAILABLE, available, at),
            field(ids::MEMORY_BUFFERS, 10, at),
            field(ids::MEMORY_CACHED, 20, at),
            field(ids::MEMORY_SWAP_TOTAL, 1000, at),
            field(ids::MEMORY_SWAP_FREE_RAW, 400, at),
        ]
    }

    fn key(metric: &str) -> dgxtop_core::identity::MetricKey {
        metric_key("memory", metric, "system")
    }

    #[test]
    fn used_is_total_minus_available_in_exact_bytes() {
        let collector = MemoryCollector::new().unwrap();
        let plan = PlanBuilder::new("memory")
            .at(SECOND + 1)
            .batch("procfs", "memory", meminfo(16_000, 6_000, SECOND))
            .build();
        let batch = collect(&collector, &plan);
        let used = value_of(&batch, &key(ids::MEMORY_USED)).unwrap();
        assert_eq!(
            used.value.and_then(dgxtop_core::metric::Scalar::as_u64),
            Some(10_000 * 1024)
        );
        assert_eq!(used.selection_state, SelectionState::Selected);
        assert!(used.eligible_for_history);
        assert_eq!(
            value_of(&batch, &key(ids::MEMORY_TOTAL))
                .and_then(|value| value.value)
                .and_then(dgxtop_core::metric::Scalar::as_u64),
            Some(16_000 * 1024)
        );
        assert_eq!(
            number(&batch, &key(ids::MEMORY_SWAP_USED)),
            Some(600.0 * 1024.0)
        );
        assert!(
            value_of(&batch, &key(ids::MEMORY_SWAP_FREE_RAW)).is_none(),
            "raw SwapFree is an input, never published"
        );
    }

    #[test]
    fn available_above_total_has_no_value_and_a_reason() {
        let collector = MemoryCollector::new().unwrap();
        let plan = PlanBuilder::new("memory")
            .at(SECOND + 1)
            .batch("procfs", "memory", meminfo(1_000, 2_000, SECOND))
            .build();
        let batch = collect(&collector, &plan);
        let used = value_of(&batch, &key(ids::MEMORY_USED)).unwrap();
        assert_eq!(used.value, None);
        assert_eq!(
            used.reason_codes,
            vec![reasons::AVAILABLE_EXCEEDS_TOTAL.to_owned()]
        );
    }

    #[test]
    fn missing_available_never_falls_back_to_free() {
        let collector = MemoryCollector::new().unwrap();
        let mut records = meminfo(16_000, 6_000, SECOND);
        records[1] = reading(
            &key(ids::MEMORY_AVAILABLE),
            "procfs",
            "procfs:meminfo:memory.available",
            SECOND,
        )
        .failed(ReadStatus::Unsupported)
        .build();
        let plan = PlanBuilder::new("memory")
            .at(SECOND + 1)
            .batch("procfs", "memory", records)
            .build();
        let batch = collect(&collector, &plan);
        let used = value_of(&batch, &key(ids::MEMORY_USED)).unwrap();
        assert_eq!(used.value, None);
        assert_eq!(used.reason_codes, vec![reasons::MISSING_INPUT.to_owned()]);
        let available = value_of(&batch, &key(ids::MEMORY_AVAILABLE)).unwrap();
        assert_eq!(available.selection_state, SelectionState::Unavailable);
    }

    fn gauge(metric: &str, percent: f64, at: u64) -> Observation {
        reading(
            &key(metric),
            "procfs",
            &format!("procfs:pressure.memory:{metric}"),
            at,
        )
        .unit(native::PERCENT)
        .f64(percent)
        .build()
    }

    fn counter(metric: &str, unit: &str, value: u64, at: u64) -> Observation {
        reading(
            &key(metric),
            "procfs",
            &format!("procfs:vmstat:{metric}"),
            at,
        )
        .unit(unit)
        .u64(value)
        .build()
    }

    /// The pressure averages are what the kernel computed, in percent, and
    /// the OOM kills the total since boot (D-135); only the memory's 10 s
    /// averages are kept as history.
    #[test]
    fn pressure_averages_and_oom_kills_pass_through() {
        let collector = MemoryCollector::new().unwrap();
        let plan = PlanBuilder::new("memory")
            .at(SECOND + 1)
            .batch(
                "procfs",
                "memory",
                vec![
                    gauge(ids::MEMORY_PRESSURE_SOME_AVG10, 2.4, SECOND),
                    gauge(ids::MEMORY_PRESSURE_SOME_AVG300, 0.2, SECOND),
                    gauge(ids::MEMORY_PRESSURE_FULL_AVG10, 0.8, SECOND),
                    counter(ids::MEMORY_OOM_KILLS, native::COUNT, 4, SECOND),
                ],
            )
            .build();
        let batch = collect(&collector, &plan);
        let some = value_of(&batch, &key(ids::MEMORY_PRESSURE_SOME_AVG10)).unwrap();
        assert_eq!(
            some.value.map(dgxtop_core::metric::Scalar::to_f64_lossy),
            Some(2.4)
        );
        assert_eq!(some.unit, CanonicalUnit::Percent);
        assert_eq!(some.selection_state, SelectionState::Selected);
        assert!(some.eligible_for_history);
        let long = value_of(&batch, &key(ids::MEMORY_PRESSURE_SOME_AVG300)).unwrap();
        assert_eq!(
            long.value.map(dgxtop_core::metric::Scalar::to_f64_lossy),
            Some(0.2)
        );
        assert!(!long.eligible_for_history);
        assert_eq!(
            number(&batch, &key(ids::MEMORY_PRESSURE_FULL_AVG10)),
            Some(0.8)
        );
        let kills = value_of(&batch, &key(ids::MEMORY_OOM_KILLS)).unwrap();
        assert_eq!(
            kills.value.and_then(dgxtop_core::metric::Scalar::as_u64),
            Some(4)
        );
    }

    /// What is swapped in and out each second is the exact movement of the
    /// kernel's counters over the time between two reads; the first read
    /// only sets the base, and the counters themselves are never published.
    #[test]
    fn swap_rates_come_from_exact_counter_deltas() {
        let collector = MemoryCollector::new().unwrap();
        let swapped = |bytes_in: u64, bytes_out: u64, at: u64| {
            vec![
                counter(ids::MEMORY_SWAP_IN_BYTES, native::BYTES, bytes_in, at),
                counter(ids::MEMORY_SWAP_OUT_BYTES, native::BYTES, bytes_out, at),
            ]
        };
        let first = collect(
            &collector,
            &PlanBuilder::new("memory")
                .at(10 * SECOND + 1)
                .batch("procfs", "memory", swapped(4096, 8192, 10 * SECOND))
                .build(),
        );
        let warmup = value_of(&first, &key(ids::MEMORY_SWAP_IN_BPS)).unwrap();
        assert_eq!(warmup.value, None);
        assert_eq!(warmup.reason_codes, vec![reasons::WARMUP.to_owned()]);
        let second = collect(
            &collector,
            &PlanBuilder::new("memory")
                .after(&first)
                .at(12 * SECOND + 1)
                .batch(
                    "procfs",
                    "memory",
                    swapped(4096, 8192 + 2 * 1_048_576, 12 * SECOND),
                )
                .build(),
        );
        assert_eq!(number(&second, &key(ids::MEMORY_SWAP_IN_BPS)), Some(0.0));
        assert_eq!(
            number(&second, &key(ids::MEMORY_SWAP_OUT_BPS)),
            Some(1_048_576.0)
        );
        let out = value_of(&second, &key(ids::MEMORY_SWAP_OUT_BPS)).unwrap();
        assert!(!out.eligible_for_history);
        assert!(
            value_of(&second, &key(ids::MEMORY_SWAP_OUT_BYTES)).is_none(),
            "the raw counter is an input, never published"
        );
        // A counter that steps back (a reboot it was not told of) restarts.
        let third = collect(
            &collector,
            &PlanBuilder::new("memory")
                .after(&second)
                .at(13 * SECOND + 1)
                .batch("procfs", "memory", swapped(0, 0, 13 * SECOND))
                .build(),
        );
        let reset = value_of(&third, &key(ids::MEMORY_SWAP_OUT_BPS)).unwrap();
        assert_eq!(reset.value, None);
        assert_eq!(reset.reason_codes, vec![reasons::COUNTER_RESET.to_owned()]);
    }

    #[test]
    fn retained_inputs_are_not_republished_without_new_evidence() {
        let collector = MemoryCollector::new().unwrap();
        let first = collect(
            &collector,
            &PlanBuilder::new("memory")
                .at(SECOND + 1)
                .batch("procfs", "memory", meminfo(16_000, 6_000, SECOND))
                .build(),
        );
        let second = collect(
            &collector,
            &PlanBuilder::new("memory")
                .after(&first)
                .at(SECOND + 2)
                .build(),
        );
        assert_eq!(
            second.canonical,
            [] as [dgxtop_core::canonical::CanonicalValue; 0]
        );
        assert_eq!(
            second.next_state.candidates.len(),
            first.next_state.candidates.len()
        );
    }
}
