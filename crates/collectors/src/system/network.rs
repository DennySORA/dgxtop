//! Network collector: per-interface byte rates from exact counter deltas
//! (with `CounterInterval`s in bytes) and cumulative packet/error/drop
//! counts, which stay totals and are never shown as per-second values. An
//! interface re-created with a new entity or generation restarts at warmup.

use dgxtop_core::canonical::{CollectionBatch, CollectionPlan};
use dgxtop_core::identity::{AdapterId, CollectorId, IdentityError};
use dgxtop_core::metric::{CanonicalUnit, Domain, MetricDescriptor, MetricRole, ids};
use dgxtop_core::naming;
use dgxtop_core::ports::Collector;

use crate::shared::context::{self, CollectorSpec, Route, Session};
use crate::shared::counter_inputs::{self, Step};
use crate::shared::formulas::{self, Selections};

const CUMULATIVE_COUNTS: [&str; 6] = [
    ids::NET_RX_PACKETS,
    ids::NET_TX_PACKETS,
    ids::NET_RX_ERRORS,
    ids::NET_TX_ERRORS,
    ids::NET_RX_DROPS,
    ids::NET_TX_DROPS,
];

/// Byte counter and the rate derived from it.
const BYTE_RATES: [(&str, &str); 2] = [
    (ids::NET_RX_BYTES, ids::NET_RX_BPS),
    (ids::NET_TX_BYTES, ids::NET_TX_BPS),
];

fn accepts(metric: &MetricDescriptor) -> bool {
    metric.domain == Domain::Network && metric.id != ids::NET_RX_BPS && metric.id != ids::NET_TX_BPS
}

/// Byte counters are consumed once per reading; only the published totals
/// are kept for arbitration across batches.
fn retains(_route: &Route<'_>, metric: &MetricDescriptor) -> bool {
    metric.role == MetricRole::Canonical
}

static SPEC: CollectorSpec = CollectorSpec {
    id: naming::collectors::NETWORK,
    accepts,
    retains,
};

/// Network interface collector.
#[derive(Debug, Clone)]
pub struct NetworkCollector {
    id: CollectorId,
}

impl NetworkCollector {
    pub fn new() -> Result<Self, IdentityError> {
        Ok(Self {
            id: CollectorId::new(naming::collectors::NETWORK)?,
        })
    }
}

impl Collector for NetworkCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn consumes(&self, adapter_id: &AdapterId, read_group: &str) -> bool {
        crate::shared::routes_to(adapter_id, read_group, SPEC.id)
    }

    fn collect(&self, plan: &CollectionPlan) -> CollectionBatch {
        context::run(plan, &SPEC, collect_network)
    }
}

fn collect_network(session: &mut Session<'_>) {
    publish_byte_rates(session);
    let mut selections = Selections::new();
    formulas::publish_pending(
        session,
        |metric| CUMULATIVE_COUNTS.contains(&metric),
        &mut selections,
    );
}

fn publish_byte_rates(session: &mut Session<'_>) {
    for (counter_metric, rate_metric) in BYTE_RATES {
        let keys = session.pending_keys(|key| key.metric_id.as_str() == counter_metric);
        for key in keys {
            // Every candidate's counter base advances, so switching sources
            // later still starts from an exact base; but the store rejects a
            // batch carrying a key twice, so one rate is published per key.
            let steps: Vec<Step<'_>> = session
                .records(key)
                .into_iter()
                .map(|record| counter_inputs::step(session, record, Some(CanonicalUnit::Bytes)))
                .collect();
            if let Some(step) = preferred_step(&steps) {
                counter_inputs::publish_rate(session, step, rate_metric);
            }
        }
    }
}

/// The first exact delta among a key's candidates (source-id order), else
/// the first explained gap; unchanged candidates publish nothing.
fn preferred_step<'s, 'a>(steps: &'s [Step<'a>]) -> Option<&'s Step<'a>> {
    steps
        .iter()
        .find(|step| matches!(step, Step::Delta { .. }))
        .or_else(|| steps.iter().find(|step| !matches!(step, Step::Unchanged)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::reasons;
    use crate::testkit::{PlanBuilder, SECOND, collect, number, reading, value_of};
    use dgxtop_core::identity::MetricKey;
    use dgxtop_core::metric::native;
    use dgxtop_core::observation::Observation;
    use dgxtop_core::quality::SelectionState;
    use dgxtop_core::testing::metric_key;

    fn key(entity: &str, metric: &str) -> MetricKey {
        metric_key(entity, metric, "interface")
    }

    fn counter(entity: &str, metric: &str, value: u64, at: u64, generation: u32) -> Observation {
        reading(
            &key(entity, metric),
            "network",
            &format!("network:{entity}:{metric}"),
            at,
        )
        .unit(if metric.ends_with("bytes") {
            native::BYTES
        } else {
            native::COUNT
        })
        .u64(value)
        .generation(generation)
        .build()
    }

    fn interface(
        entity: &str,
        rx_bytes: u64,
        packets: u64,
        at: u64,
        generation: u32,
    ) -> Vec<Observation> {
        vec![
            counter(entity, ids::NET_RX_BYTES, rx_bytes, at, generation),
            counter(entity, ids::NET_TX_BYTES, 5, at, generation),
            counter(entity, ids::NET_RX_PACKETS, packets, at, generation),
            counter(entity, ids::NET_RX_ERRORS, 0, at, generation),
        ]
    }

    #[test]
    fn byte_rate_uses_actual_elapsed_time_and_emits_exact_interval() {
        let collector = NetworkCollector::new().unwrap();
        let eth = "netif:ns:2";
        let first = collect(
            &collector,
            &PlanBuilder::new("network")
                .at(10 * SECOND + 1)
                .batch("network", "fast", interface(eth, 1000, 7, 10 * SECOND, 1))
                .build(),
        );
        let warmup = value_of(&first, &key(eth, ids::NET_RX_BPS)).unwrap();
        assert_eq!(warmup.value, None);
        assert_eq!(warmup.reason_codes, vec![reasons::WARMUP.to_owned()]);
        assert!(first.counter_intervals.is_empty());

        let second = collect(
            &collector,
            &PlanBuilder::new("network")
                .after(&first)
                .at(13 * SECOND + 1)
                .batch("network", "fast", interface(eth, 1300, 9, 13 * SECOND, 1))
                .build(),
        );
        assert_eq!(number(&second, &key(eth, ids::NET_RX_BPS)), Some(100.0));
        let rx_interval = second
            .counter_intervals
            .iter()
            .find(|interval| interval.key == key(eth, ids::NET_RX_BPS))
            .unwrap();
        assert_eq!(rx_interval.delta.as_u64(), Some(300));
        assert_eq!(rx_interval.unit, CanonicalUnit::Bytes);
        assert_eq!(
            (rx_interval.start_boot_ns, rx_interval.end_boot_ns),
            (10 * SECOND, 13 * SECOND)
        );
        // Packets stay a cumulative count, never a per-second figure.
        let packets = value_of(&second, &key(eth, ids::NET_RX_PACKETS)).unwrap();
        assert_eq!(packets.value.and_then(|value| value.as_u64()), Some(9));
        assert_eq!(packets.unit, CanonicalUnit::Count);
        let errors = value_of(&second, &key(eth, ids::NET_RX_ERRORS)).unwrap();
        assert_eq!(errors.value.and_then(|value| value.as_u64()), Some(0));
    }

    #[test]
    fn candidate_byte_counters_publish_each_rate_key_once() {
        // REF workloads give one interface counter up to four candidate
        // sources; the store rejects a whole batch that carries a key twice.
        let collector = NetworkCollector::new().unwrap();
        let eth = "netif:ns:5";
        let candidates = |value: u64, at: u64| -> Vec<Observation> {
            (0..2)
                .map(|candidate| {
                    reading(
                        &key(eth, ids::NET_RX_BYTES),
                        "network",
                        &format!("network:{eth}:rx#{candidate}"),
                        at,
                    )
                    .unit(native::BYTES)
                    .u64(value)
                    .generation(1)
                    .build()
                })
                .collect()
        };
        let first = collect(
            &collector,
            &PlanBuilder::new("network")
                .at(SECOND + 1)
                .batch("network", "fast", candidates(1_000, SECOND))
                .build(),
        );
        let second = collect(
            &collector,
            &PlanBuilder::new("network")
                .after(&first)
                .at(2 * SECOND + 1)
                .batch("network", "fast", candidates(1_500, 2 * SECOND))
                .build(),
        );
        for batch in [&first, &second] {
            let mut keys = std::collections::BTreeSet::new();
            for value in &batch.canonical {
                assert!(
                    keys.insert(value.key.clone()),
                    "{:?} published twice",
                    value.key
                );
            }
            let mut interval_keys = std::collections::BTreeSet::new();
            for interval in &batch.counter_intervals {
                assert!(
                    interval_keys.insert(interval.key.clone()),
                    "{:?} has two intervals",
                    interval.key
                );
            }
        }
        assert_eq!(number(&second, &key(eth, ids::NET_RX_BPS)), Some(500.0));
    }

    #[test]
    fn recreated_interface_restarts_from_warmup() {
        let collector = NetworkCollector::new().unwrap();
        let eth = "netif:ns:2";
        let first = collect(
            &collector,
            &PlanBuilder::new("network")
                .at(SECOND + 1)
                .batch("network", "fast", interface(eth, 5_000, 1, SECOND, 1))
                .build(),
        );
        let recreated = collect(
            &collector,
            &PlanBuilder::new("network")
                .after(&first)
                .at(2 * SECOND + 1)
                .batch("network", "fast", interface(eth, 10, 1, 2 * SECOND, 2))
                .build(),
        );
        let rate = value_of(&recreated, &key(eth, ids::NET_RX_BPS)).unwrap();
        assert_eq!(rate.value, None);
        assert_eq!(rate.selection_state, SelectionState::Unavailable);
        assert_eq!(
            rate.reason_codes,
            vec![reasons::GENERATION_CHANGED.to_owned()]
        );
        assert!(recreated.counter_intervals.is_empty());
    }

    #[test]
    fn counter_reset_is_never_a_negative_or_zero_rate() {
        let collector = NetworkCollector::new().unwrap();
        let eth = "netif:ns:3";
        let first = collect(
            &collector,
            &PlanBuilder::new("network")
                .at(SECOND + 1)
                .batch("network", "fast", interface(eth, 9_000, 1, SECOND, 1))
                .build(),
        );
        let reset = collect(
            &collector,
            &PlanBuilder::new("network")
                .after(&first)
                .at(2 * SECOND + 1)
                .batch("network", "fast", interface(eth, 100, 1, 2 * SECOND, 1))
                .build(),
        );
        let rate = value_of(&reset, &key(eth, ids::NET_RX_BPS)).unwrap();
        assert_eq!(rate.value, None);
        assert_eq!(rate.reason_codes, vec![reasons::COUNTER_RESET.to_owned()]);
    }

    #[test]
    fn replayed_counters_feed_no_live_interval() {
        let collector = NetworkCollector::new().unwrap();
        let eth = "netif:ns:4";
        let replayed = |value: u64, at: u64| {
            let mut records = interface(eth, value, 1, at, 1);
            for record in &mut records {
                record.sample_kind = dgxtop_core::quality::SampleKind::Replay;
                record.provenance.adapter_id =
                    dgxtop_core::identity::AdapterId::new("replay").unwrap();
            }
            records
        };
        let first = collect(
            &collector,
            &PlanBuilder::new("network")
                .policy(crate::testkit::replay_policy())
                .at(SECOND + 1)
                .batch("replay", "network.fast", replayed(0, SECOND))
                .build(),
        );
        let second = collect(
            &collector,
            &PlanBuilder::new("network")
                .policy(crate::testkit::replay_policy())
                .after(&first)
                .at(2 * SECOND + 1)
                .batch("replay", "network.fast", replayed(500, 2 * SECOND))
                .build(),
        );
        let rate = value_of(&second, &key(eth, ids::NET_RX_BPS)).unwrap();
        assert_eq!(rate.value.map(|value| value.to_f64_lossy()), Some(500.0));
        assert_eq!(rate.sample_kind, dgxtop_core::quality::SampleKind::Replay);
        assert!(!rate.eligible_for_history);
        assert!(second.counter_intervals.is_empty());
    }
}
