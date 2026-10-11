//! Memory collector: `/proc/meminfo` KiB fields to exact bytes.
//!
//! `memory.used = total - available` (never `total - free`) and
//! `memory.swap_used = swap_total - swap_free`, both with checked
//! subtraction: an inconsistent pair yields no value plus a reason.

use dgxtop_core::canonical::{CollectionBatch, CollectionPlan};
use dgxtop_core::identity::{AdapterId, CollectorId, IdentityError};
use dgxtop_core::metric::{Domain, MetricDescriptor, ids};
use dgxtop_core::naming;
use dgxtop_core::ports::Collector;

use crate::shared::context::{self, CollectorSpec, Route, Session};
use crate::shared::formulas::{self, Difference, Selections};
use crate::shared::reasons;

const PASS_THROUGH: [&str; 5] = [
    ids::MEMORY_TOTAL,
    ids::MEMORY_AVAILABLE,
    ids::MEMORY_BUFFERS,
    ids::MEMORY_CACHED,
    ids::MEMORY_SWAP_TOTAL,
];

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

fn accepts(metric: &MetricDescriptor) -> bool {
    metric.domain == Domain::Memory
        && metric.id != ids::MEMORY_USED
        && metric.id != ids::MEMORY_SWAP_USED
}

fn retains(_route: &Route<'_>, _metric: &MetricDescriptor) -> bool {
    true
}

static SPEC: CollectorSpec = CollectorSpec {
    id: naming::collectors::MEMORY,
    accepts,
    retains,
};

/// Host memory collector.
#[derive(Debug, Clone)]
pub struct MemoryCollector {
    id: CollectorId,
}

impl MemoryCollector {
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
    let mut selections = Selections::new();
    formulas::publish_pending(
        session,
        |metric| PASS_THROUGH.contains(&metric),
        &mut selections,
    );
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

    fn meminfo(total: u64, available: u64, free: u64, at: u64) -> Vec<Observation> {
        vec![
            field(ids::MEMORY_TOTAL, total, at),
            field(ids::MEMORY_AVAILABLE, available, at),
            field(ids::MEMORY_FREE_RAW, free, at),
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
            .batch("procfs", "memory", meminfo(16_000, 6_000, 1_000, SECOND))
            .build();
        let batch = collect(&collector, &plan);
        let used = value_of(&batch, &key(ids::MEMORY_USED)).unwrap();
        assert_eq!(
            used.value.and_then(|value| value.as_u64()),
            Some(10_000 * 1024)
        );
        assert_eq!(used.selection_state, SelectionState::Selected);
        assert!(used.eligible_for_history);
        assert_eq!(
            value_of(&batch, &key(ids::MEMORY_TOTAL))
                .and_then(|value| value.value)
                .and_then(|value| value.as_u64()),
            Some(16_000 * 1024)
        );
        assert_eq!(
            number(&batch, &key(ids::MEMORY_SWAP_USED)),
            Some(600.0 * 1024.0)
        );
        assert!(
            value_of(&batch, &key(ids::MEMORY_FREE_RAW)).is_none(),
            "raw MemFree is an input, never published"
        );
    }

    #[test]
    fn available_above_total_has_no_value_and_a_reason() {
        let collector = MemoryCollector::new().unwrap();
        let plan = PlanBuilder::new("memory")
            .at(SECOND + 1)
            .batch("procfs", "memory", meminfo(1_000, 2_000, 500, SECOND))
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
        let mut records = meminfo(16_000, 6_000, 1_000, SECOND);
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

    #[test]
    fn retained_inputs_are_not_republished_without_new_evidence() {
        let collector = MemoryCollector::new().unwrap();
        let first = collect(
            &collector,
            &PlanBuilder::new("memory")
                .at(SECOND + 1)
                .batch("procfs", "memory", meminfo(16_000, 6_000, 1_000, SECOND))
                .build(),
        );
        let second = collect(
            &collector,
            &PlanBuilder::new("memory")
                .after(&first)
                .at(SECOND + 2)
                .build(),
        );
        assert!(second.canonical.is_empty());
        assert_eq!(
            second.next_state.candidates.len(),
            first.next_state.candidates.len()
        );
    }
}
