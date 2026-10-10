//! Cross-collector invariants: plan identity, immutable committed state,
//! bounded state under churn, routing and failure reporting.

use std::collections::BTreeMap;

use dgxtop_core::canonical::{
    CollectionBatch, CollectionLimits, CollectorState, CounterBase, LastGood, SelectionMemo,
};
use dgxtop_core::identity::{BootId, MetricKey};
use dgxtop_core::metric::{CanonicalUnit, Scalar, ids, native};
use dgxtop_core::observation::{BatchStatus, Observation};
use dgxtop_core::ports::Collector;
use dgxtop_core::testing::{boot_id, metric_key, source_id};

use crate::reasons;
use crate::testkit::{PlanBuilder, SECOND, collect, limits, reading, value_of};
use crate::{NetworkCollector, collector_for, default_collectors};

fn seeded_state() -> CollectorState {
    let key = metric_key("cpu.package.0", ids::CPU_TEMPERATURE_PACKAGE, "package:0");
    CollectorState {
        revision: 7,
        counters: BTreeMap::from([(
            source_id("network:netif:ns:2:network.rx.bytes"),
            CounterBase {
                value: 10,
                boot_id: boot_id("fake-boot"),
                generation: 1,
                sample_boot_ns: SECOND,
            },
        )]),
        selections: BTreeMap::from([(
            key.clone(),
            SelectionMemo {
                selected: Some(source_id("hwmon:a")),
                selected_since_boot_ns: 0,
                challenger: None,
                challenger_streak: 0,
                challenger_since_boot_ns: 0,
            },
        )]),
        candidates: BTreeMap::new(),
        last_good: BTreeMap::from([(
            key,
            LastGood {
                value: Scalar::from_u64(1),
                unit: CanonicalUnit::DegC,
                sample_boot_ns: SECOND,
                source_id: source_id("hwmon:a"),
            },
        )]),
        processes: BTreeMap::new(),
    }
}

fn mixed_records(at: u64) -> Vec<(&'static str, &'static str, Vec<Observation>)> {
    let temperature = metric_key("cpu.package.0", ids::CPU_TEMPERATURE_PACKAGE, "package:0");
    let rx = metric_key("netif:ns:2", ids::NET_RX_BYTES, "interface");
    let total = metric_key("memory", ids::MEMORY_TOTAL, "system");
    vec![
        (
            "hwmon",
            "fast",
            vec![
                reading(&temperature, "hwmon", "hwmon:a", at)
                    .unit(native::MILLI_DEG_C)
                    .f64(50_000.0)
                    .build(),
            ],
        ),
        (
            "network",
            "fast",
            vec![
                reading(&rx, "network", "network:netif:ns:2:network.rx.bytes", at)
                    .unit(native::BYTES)
                    .u64(500)
                    .build(),
            ],
        ),
        (
            "procfs",
            "memory",
            vec![
                reading(&total, "procfs", "procfs:meminfo:total", at)
                    .unit(native::KIB)
                    .u64(4)
                    .build(),
            ],
        ),
    ]
}

#[test]
fn every_collector_leaves_the_plan_state_untouched_and_carries_plan_identity() {
    for collector in default_collectors() {
        let mut builder = PlanBuilder::new(collector.id().as_str())
            .state(seeded_state())
            .at(2 * SECOND);
        for (adapter, group, records) in mixed_records(2 * SECOND - 1) {
            builder = builder.batch(adapter, group, records);
        }
        let plan = builder.build();
        // `collect` asserts the plan state is unchanged and inputs are shared.
        let batch = collect(collector.as_ref(), &plan);
        assert_eq!(batch.collector_id, plan.collector_id);
        assert_eq!(batch.cycle_id, plan.cycle_id);
        assert_eq!(batch.epoch, plan.epoch);
        assert_eq!(batch.config_revision, plan.config_revision);
        assert_eq!(batch.catalog_revision, plan.catalog.revision);
        assert_eq!(batch.policy_revision, plan.policy.revision);
        assert_eq!(batch.next_state.revision, plan.state_revision + 1);
    }
}

#[test]
fn plan_addressed_to_another_collector_is_refused() {
    let network = NetworkCollector::new().unwrap();
    let mut builder = PlanBuilder::new("memory").state(seeded_state());
    for (adapter, group, records) in mixed_records(SECOND) {
        builder = builder.batch(adapter, group, records);
    }
    let plan = builder.at(SECOND + 1).build();
    let batch = network.collect(&plan);
    assert!(batch.canonical.is_empty());
    assert_eq!(batch.failures[0].code, reasons::PLAN_COLLECTOR_MISMATCH);
    assert_eq!(batch.next_state.counters, seeded_state().counters);
}

#[test]
fn unknown_and_unhandled_metrics_and_failed_batches_are_reported() {
    let memory = collector_for("memory").unwrap();
    let foreign = metric_key("memory", ids::CPU_LOAD_1M, "system");
    let unknown_key = metric_key("memory", "memory.mystery", "system");
    let plan = PlanBuilder::new("memory")
        .at(SECOND + 1)
        .batch(
            "procfs",
            "memory",
            vec![
                reading(&foreign, "procfs", "procfs:x", SECOND).build(),
                reading(&unknown_key, "procfs", "procfs:y", SECOND).build(),
            ],
        )
        .build();
    let mut failed = plan.clone();
    let mut batch = (*failed.inputs[0]).batch().clone();
    batch.records.clear();
    batch.status = BatchStatus::Failed;
    batch.error_code = Some("HOST_TIMEOUT".to_owned());
    failed.inputs = vec![std::sync::Arc::new(dgxtop_core::ports::IngressBatch::new(
        batch, None,
    ))];
    let reported = collect(memory.as_ref(), &plan);
    let codes: Vec<&str> = reported
        .failures
        .iter()
        .map(|failure| failure.code.as_str())
        .collect();
    assert!(codes.contains(&reasons::METRIC_NOT_HANDLED));
    assert!(codes.contains(&reasons::UNKNOWN_METRIC));
    assert!(reported.canonical.is_empty());
    let failed_batch = collect(memory.as_ref(), &failed);
    assert_eq!(failed_batch.failures[0].code, reasons::BATCH_FAILED);
    assert!(
        failed_batch.canonical.is_empty(),
        "a failed read is never a zero reading"
    );
}

fn interface_counter(index: u64, boot: &BootId, value: u64, at: u64) -> Observation {
    let entity = format!("netif:ns:{index}");
    let mut key = metric_key(&entity, ids::NET_RX_BYTES, "interface");
    key.boot_id = boot.clone();
    reading(&key, "network", &format!("network:{entity}:rx"), at)
        .unit(native::BYTES)
        .u64(value)
        .build()
}

#[test]
fn boot_change_restarts_counters_at_warmup() {
    let collector = NetworkCollector::new().unwrap();
    let first_boot = boot_id("fake-boot");
    let second_boot = boot_id("next-boot");
    let first = collect(
        &collector,
        &PlanBuilder::new("network")
            .at(SECOND + 1)
            .batch(
                "network",
                "fast",
                vec![interface_counter(1, &first_boot, 100, SECOND)],
            )
            .build(),
    );
    let second = collect(
        &collector,
        &PlanBuilder::new("network")
            .after(&first)
            .at(2 * SECOND + 1)
            .batch(
                "network",
                "fast",
                vec![interface_counter(1, &second_boot, 900, 2 * SECOND)],
            )
            .build(),
    );
    let mut rate_key = metric_key("netif:ns:1", ids::NET_RX_BPS, "interface");
    rate_key.boot_id = second_boot;
    let rate = value_of(&second, &rate_key).unwrap();
    assert_eq!(rate.value, None);
    assert_eq!(rate.reason_codes, vec![reasons::BOOT_CHANGED.to_owned()]);
}

fn small_limits() -> CollectionLimits {
    CollectionLimits {
        max_series: 32,
        max_sources: 64,
        ..limits()
    }
}

fn assert_bounded(batch: &CollectionBatch, limits: &CollectionLimits) {
    let state = &batch.next_state;
    let max_sources = usize::try_from(limits.max_sources).unwrap();
    let max_series = usize::try_from(limits.max_series).unwrap();
    assert!(
        state.counters.len() <= max_sources,
        "counters {}",
        state.counters.len()
    );
    assert!(
        state.candidates.len() <= max_sources,
        "candidates {}",
        state.candidates.len()
    );
    assert!(
        state.selections.len() <= max_series,
        "selections {}",
        state.selections.len()
    );
    assert!(
        state.last_good.len() <= max_series,
        "last good {}",
        state.last_good.len()
    );
}

#[test]
fn state_stays_bounded_under_ten_thousand_churned_sources() {
    let limits = small_limits();
    let boot = boot_id("fake-boot");
    // Network: 10 000 interfaces appearing and vanishing (counters, values).
    let network = NetworkCollector::new().unwrap();
    let mut previous: Option<CollectionBatch> = None;
    for cycle in 0..100u64 {
        let at = (cycle + 1) * SECOND;
        let records = (0..100u64)
            .map(|offset| interface_counter(cycle * 100 + offset, &boot, offset, at))
            .collect();
        let mut builder = PlanBuilder::new("network")
            .limits(limits)
            .cycle(cycle + 1)
            .at(at + 1)
            .batch("network", "fast", records);
        if let Some(previous) = &previous {
            builder = builder.after(previous);
        }
        let batch = collect(&network, &builder.build());
        assert_bounded(&batch, &limits);
        previous = Some(batch);
    }
    // CPU: 10 000 temperature sources, two per key (candidates and memos).
    let cpu = collector_for("cpu").unwrap();
    let mut previous: Option<CollectionBatch> = None;
    for cycle in 0..50u64 {
        let at = (cycle + 1) * SECOND;
        let records: Vec<Observation> = (0..200u64)
            .map(|offset| {
                let package = cycle * 100 + offset / 2;
                let key: MetricKey = metric_key(
                    &format!("cpu.package.{package}"),
                    ids::CPU_TEMPERATURE_PACKAGE,
                    &format!("package:{package}"),
                );
                reading(&key, "hwmon", &format!("hwmon:{cycle}:{offset}"), at)
                    .unit(native::MILLI_DEG_C)
                    .f64(50_000.0)
                    .build()
            })
            .collect();
        let mut builder = PlanBuilder::new("cpu")
            .limits(limits)
            .cycle(cycle + 1)
            .at(at + 1)
            .batch("hwmon", "fast", records);
        if let Some(previous) = &previous {
            builder = builder.after(previous);
        }
        let batch = collect(cpu.as_ref(), &builder.build());
        assert_bounded(&batch, &limits);
        assert_eq!(batch.canonical.len(), 100, "every key still publishes");
        previous = Some(batch);
    }
}
