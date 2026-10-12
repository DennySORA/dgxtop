//! Execution of one collection plan on the collector pool. Shared by the
//! composition root, the integration rigs and the benches, so a panicking
//! or unknown collector is reported the same way everywhere.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use dgxtop_core::canonical::CollectionPlan;
use dgxtop_core::ports::{Collector, IngestionCompletion};

/// The failure message of a collector that panicked on a plan.
pub const COLLECTOR_PANICKED: &str = "collector panicked";
/// The failure message of a plan no registered collector owns.
pub const UNKNOWN_COLLECTOR: &str = "unknown collector";

/// Runs `plan` on its collector in `registry`. A panic is caught: the
/// collectors are pure, so nothing they hold can be left half-changed, and
/// the Manager retries from the last acknowledged state.
pub fn run_collection(
    registry: &[Arc<dyn Collector>],
    plan: &CollectionPlan,
) -> IngestionCompletion {
    let failure = |message: &str| IngestionCompletion::CollectorFailed {
        collector_id: plan.collector_id.clone(),
        batch_id: plan.batch_id,
        message: message.to_owned(),
    };
    let Some(collector) = registry
        .iter()
        .find(|collector| collector.id() == &plan.collector_id)
    else {
        return failure(UNKNOWN_COLLECTOR);
    };
    match catch_unwind(AssertUnwindSafe(|| collector.collect(plan))) {
        Ok(batch) => IngestionCompletion::Collected(Box::new(batch)),
        Err(_) => failure(COLLECTOR_PANICKED),
    }
}

/// Whether `completion` reports a collector that panicked.
pub fn is_collector_panic(completion: &IngestionCompletion) -> bool {
    matches!(
        completion,
        IngestionCompletion::CollectorFailed { message, .. } if message == COLLECTOR_PANICKED
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use dgxtop_core::canonical::{
        CatalogSnapshot, CollectionBatch, CollectionLimits, CollectorState, PolicySnapshot,
    };
    use dgxtop_core::identity::{AdapterId, CollectorId};
    use dgxtop_core::quality::Namespace;
    use dgxtop_core::testing::{boot_id, host_id};
    use uuid::Uuid;

    use super::*;

    /// A collector that answers an empty batch, or panics when told to.
    struct ScriptedCollector {
        id: CollectorId,
        panics: bool,
    }

    impl Collector for ScriptedCollector {
        fn id(&self) -> &CollectorId {
            &self.id
        }

        fn consumes(&self, _adapter_id: &AdapterId, _read_group: &str) -> bool {
            true
        }

        fn collect(&self, plan: &CollectionPlan) -> CollectionBatch {
            assert!(!self.panics, "injected collector failure");
            CollectionBatch {
                batch_id: plan.batch_id,
                collector_id: plan.collector_id.clone(),
                cycle_id: plan.cycle_id,
                epoch: plan.epoch,
                config_revision: plan.config_revision,
                catalog_revision: 0,
                policy_revision: plan.policy.revision,
                state_base_revision: plan.state_revision,
                canonical: Vec::new(),
                explanations: Vec::new(),
                counter_intervals: Vec::new(),
                inputs: Vec::new(),
                process_table: None,
                next_state: CollectorState::default(),
                failures: Vec::new(),
            }
        }
    }

    fn collector(id: &str, panics: bool) -> Arc<dyn Collector> {
        Arc::new(ScriptedCollector {
            id: CollectorId::new(id).unwrap(),
            panics,
        })
    }

    fn plan_for(collector: &str) -> CollectionPlan {
        CollectionPlan {
            collector_id: CollectorId::new(collector).unwrap(),
            batch_id: Uuid::new_v4(),
            cycle_id: 1,
            epoch: 1,
            config_revision: 1,
            host_id: host_id("host-acme"),
            boot_id: boot_id("boot-acme"),
            catalog: Arc::new(CatalogSnapshot::default()),
            policy: Arc::new(PolicySnapshot {
                revision: 1,
                switch_successes: 3,
                switch_min_hold_ms: 5000,
                temperature_disagreement_deg_c: 8.0,
                relative_tolerance_percent: 10.0,
                alignment_tolerance_ms: 250,
                excluded_sources: BTreeSet::new(),
                pinned_sources: BTreeMap::new(),
                stale_factor: 3,
                effective_periods_ms: BTreeMap::new(),
                namespace: Namespace::Live,
            }),
            state: Arc::new(CollectorState::default()),
            state_revision: 0,
            inputs: Vec::new(),
            now_boot_ns: 1_000,
            limits: CollectionLimits {
                max_series: 16,
                max_sources: 16,
                max_candidates_per_series: 4,
                max_processes: 16,
                max_process_gpu_links: 16,
            },
            committed: Arc::default(),
        }
    }

    fn failure_of(completion: &IngestionCompletion) -> (&CollectorId, Uuid, &str) {
        match completion {
            IngestionCompletion::CollectorFailed {
                collector_id,
                batch_id,
                message,
            } => (collector_id, *batch_id, message.as_str()),
            other => panic!("expected a collector failure, got {other:?}"),
        }
    }

    /// The plan's collector answers its batch.
    #[test]
    fn a_plan_runs_on_its_own_collector() {
        let registry = [collector("cpu", true), collector("memory", false)];
        let plan = plan_for("memory");
        let completion = run_collection(&registry, &plan);
        let IngestionCompletion::Collected(batch) = &completion else {
            panic!("expected a batch, got {completion:?}");
        };
        assert_eq!(batch.batch_id, plan.batch_id);
        assert!(!is_collector_panic(&completion));
    }

    /// A panic is caught and reported as the plan's failure, never unwound
    /// into the pool.
    #[test]
    fn a_panicking_collector_is_reported_as_its_plans_failure() {
        let registry = [collector("cpu", true)];
        let plan = plan_for("cpu");
        let completion = run_collection(&registry, &plan);
        let (collector_id, batch_id, message) = failure_of(&completion);
        assert_eq!(collector_id.as_str(), "cpu");
        assert_eq!(batch_id, plan.batch_id);
        assert_eq!(message, COLLECTOR_PANICKED);
        assert!(is_collector_panic(&completion));
    }

    /// A plan no collector owns fails as unknown (an empty registry too).
    #[test]
    fn a_plan_without_a_collector_is_unknown() {
        for registry in [Vec::new(), vec![collector("cpu", false)]] {
            let plan = plan_for("disk");
            let completion = run_collection(&registry, &plan);
            assert_eq!(failure_of(&completion).2, UNKNOWN_COLLECTOR);
            assert!(!is_collector_panic(&completion));
        }
    }
}
