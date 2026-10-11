//! CPU collector tests.

use dgxtop_core::canonical::{CandidateDisposition, CollectionBatch, RejectReason};
use dgxtop_core::identity::{MetricKey, OriginId, OriginIdentity};
use dgxtop_core::metric::{CanonicalUnit, ids, native};
use dgxtop_core::observation::Observation;
use dgxtop_core::quality::{Quality, RawKind, SampleKind, SelectionState};
use dgxtop_core::testing::metric_key;

use super::*;
use crate::shared::reasons;
use crate::testkit::{
    MILLISECOND, PlanBuilder, SECOND, SourceSpec, assert_close, catalog, collect, number, reading,
    value_of,
};

// ----- utilization --------------------------------------------------------

/// `[user, nice, system, idle, iowait, irq, softirq, steal]`.
type Ticks = [u64; 8];

const FIELD_IDS: [&str; 8] = [
    ids::CPU_TIME_USER,
    ids::CPU_TIME_NICE,
    ids::CPU_TIME_SYSTEM,
    ids::CPU_TIME_IDLE,
    ids::CPU_TIME_IOWAIT,
    ids::CPU_TIME_IRQ,
    ids::CPU_TIME_SOFTIRQ,
    ids::CPU_TIME_STEAL,
];

fn scope_of(core: Option<u32>) -> (String, String) {
    match core {
        None => ("cpu".to_owned(), "host".to_owned()),
        Some(core) => (format!("cpu.core.{core}"), format!("core:{core}")),
    }
}

fn tick(core: Option<u32>, field: &str, value: u64, at: u64) -> Observation {
    let (entity, scope) = scope_of(core);
    reading(
        &metric_key(&entity, field, &scope),
        "procfs",
        &format!("procfs:stat:{entity}:{field}"),
        at,
    )
    .unit(native::TICKS)
    .u64(value)
    .build()
}

fn stat_line(core: Option<u32>, ticks: Ticks, at: u64) -> Vec<Observation> {
    FIELD_IDS
        .iter()
        .zip(ticks)
        .map(|(field, value)| tick(core, field, value, at))
        .collect()
}

fn utilization_key(core: Option<u32>, metric: &str) -> MetricKey {
    let (entity, scope) = scope_of(core);
    metric_key(&entity, metric, &scope)
}

fn two_cycles(
    first: Vec<Observation>,
    second: Vec<Observation>,
) -> (CollectionBatch, CollectionBatch) {
    let collector = CpuCollector::new().unwrap();
    let warmup = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .at(SECOND + 1)
            .batch("procfs", "cpu", first)
            .build(),
    );
    let measured = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .after(&warmup)
            .cycle(2)
            .at(2 * SECOND + 1)
            .batch("procfs", "cpu", second)
            .build(),
    );
    (warmup, measured)
}

#[test]
fn breakdown_follows_the_formulas() {
    let first = stat_line(None, [100, 10, 50, 800, 20, 5, 5, 10], SECOND);
    let second = stat_line(None, [160, 20, 70, 1000, 40, 10, 10, 20], 2 * SECOND);
    let (warmup, batch) = two_cycles(first, second);
    let warm = value_of(&warmup, &utilization_key(None, ids::CPU_UTILIZATION_TOTAL)).unwrap();
    assert_eq!(warm.value, None);
    assert_eq!(warm.reason_codes, vec![reasons::WARMUP.to_owned()]);
    // Δ: user 60, nice 10, system 20, idle 200, iowait 20, irq 5, softirq 5, steal 10.
    let all = 330.0;
    let value = |metric| number(&batch, &utilization_key(None, metric)).unwrap();
    assert_close(value(ids::CPU_UTILIZATION_TOTAL), 110.0 / all * 100.0);
    assert_close(value(ids::CPU_UTILIZATION_USER), 70.0 / all * 100.0);
    assert_close(value(ids::CPU_UTILIZATION_SYSTEM), 30.0 / all * 100.0);
    assert_close(value(ids::CPU_UTILIZATION_IOWAIT), 20.0 / all * 100.0);
    assert_close(value(ids::CPU_UTILIZATION_STEAL), 10.0 / all * 100.0);
    let total = value_of(&batch, &utilization_key(None, ids::CPU_UTILIZATION_TOTAL)).unwrap();
    assert_eq!(total.sample_kind, SampleKind::Derived);
    assert_eq!(total.candidate_source_ids.len(), 8);
    assert!(total.eligible_for_history);
}

#[test]
fn per_core_identity_comes_from_the_entity_not_record_order() {
    let mut first = stat_line(Some(0), [0, 0, 0, 0, 0, 0, 0, 0], SECOND);
    first.extend(stat_line(Some(7), [0, 0, 0, 0, 0, 0, 0, 0], SECOND));
    let mut second = stat_line(Some(7), [90, 0, 0, 10, 0, 0, 0, 0], 2 * SECOND);
    second.extend(stat_line(Some(0), [10, 0, 0, 90, 0, 0, 0, 0], 2 * SECOND));
    second.reverse();
    let (_, batch) = two_cycles(first.clone(), second.clone());
    assert_eq!(
        number(
            &batch,
            &utilization_key(Some(0), ids::CPU_UTILIZATION_TOTAL)
        ),
        Some(10.0)
    );
    assert_eq!(
        number(
            &batch,
            &utilization_key(Some(7), ids::CPU_UTILIZATION_TOTAL)
        ),
        Some(90.0)
    );

    first.reverse();
    second.rotate_left(5);
    let (_, shuffled) = two_cycles(first, second);
    assert_eq!(shuffled.canonical, batch.canonical);
}

#[test]
fn a_field_moving_backwards_is_warmup_never_a_huge_negative() {
    let collector = CpuCollector::new().unwrap();
    let cycle = |ticks: Ticks, at: u64| stat_line(None, ticks, at);
    let first = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .at(SECOND + 1)
            .batch(
                "procfs",
                "cpu",
                cycle([100, 0, 0, 100, 50, 0, 0, 0], SECOND),
            )
            .build(),
    );
    let backwards = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .after(&first)
            .at(2 * SECOND + 1)
            .batch(
                "procfs",
                "cpu",
                cycle([110, 0, 0, 190, 40, 0, 0, 0], 2 * SECOND),
            )
            .build(),
    );
    let total = value_of(
        &backwards,
        &utilization_key(None, ids::CPU_UTILIZATION_TOTAL),
    )
    .unwrap();
    assert_eq!(total.value, None);
    assert_eq!(total.reason_codes, vec![reasons::COUNTER_RESET.to_owned()]);
    let recovered = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .after(&backwards)
            .at(3 * SECOND + 1)
            .batch(
                "procfs",
                "cpu",
                cycle([120, 0, 0, 280, 40, 0, 0, 0], 3 * SECOND),
            )
            .build(),
    );
    assert_close(
        number(
            &recovered,
            &utilization_key(None, ids::CPU_UTILIZATION_TOTAL),
        )
        .unwrap(),
        10.0,
    );
}

#[test]
fn paged_fields_are_combined_into_one_sample_set() {
    let collector = CpuCollector::new().unwrap();
    let first = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .at(SECOND + 1)
            .batch(
                "procfs",
                "cpu",
                stat_line(None, [0, 0, 0, 0, 0, 0, 0, 0], SECOND),
            )
            .build(),
    );
    let line = stat_line(None, [50, 0, 0, 50, 0, 0, 0, 0], 2 * SECOND);
    let (page_one, page_two) = line.split_at(3);
    let after_first_page = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .after(&first)
            .at(2 * SECOND + 1)
            .batch("procfs", "cpu", page_one.to_vec())
            .build(),
    );
    assert!(
        value_of(
            &after_first_page,
            &utilization_key(None, ids::CPU_UTILIZATION_TOTAL)
        )
        .is_none(),
        "waits for the rest of the sample set"
    );
    let complete = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .after(&after_first_page)
            .at(2 * SECOND + 2)
            .batch("procfs", "cpu:1", page_two.to_vec())
            .build(),
    );
    assert_eq!(
        number(
            &complete,
            &utilization_key(None, ids::CPU_UTILIZATION_TOTAL)
        ),
        Some(50.0)
    );
    let again = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .after(&complete)
            .at(2 * SECOND + 3)
            .build(),
    );
    assert!(
        again.canonical.is_empty(),
        "a consumed sample set is never counted twice"
    );
}

#[test]
fn idle_interval_without_ticks_is_unavailable() {
    let (_, batch) = two_cycles(
        stat_line(None, [5, 5, 5, 5, 5, 5, 5, 5], SECOND),
        stat_line(None, [5, 5, 5, 5, 5, 5, 5, 5], 2 * SECOND),
    );
    let total = value_of(&batch, &utilization_key(None, ids::CPU_UTILIZATION_TOTAL)).unwrap();
    assert_eq!(
        total.reason_codes,
        vec![reasons::NO_ELAPSED_TICKS.to_owned()]
    );
}

// ----- temperatures (ARCHITECTURE §5.5 end to end) -----------------------

fn package_temperature() -> MetricKey {
    metric_key("cpu.package.0", ids::CPU_TEMPERATURE_PACKAGE, "package:0")
}

fn soc_temperature() -> MetricKey {
    metric_key("soc", ids::SOC_TEMPERATURE, "soc")
}

fn millidegrees(key: &MetricKey, adapter: &str, source: &str, value: f64, at: u64) -> Observation {
    reading(key, adapter, source, at)
        .unit(native::MILLI_DEG_C)
        .f64(value)
        .build()
}

fn with_origin(mut observation: Observation, origin: &str) -> Observation {
    observation.provenance.origin_id = Some(OriginId::new(origin).unwrap());
    observation.provenance.origin_relation = dgxtop_core::identity::OriginRelation::Confirmed;
    observation
}

fn temperatures(hwmon: Vec<Observation>, thermal: Vec<Observation>) -> CollectionBatch {
    let collector = CpuCollector::new().unwrap();
    let plan = PlanBuilder::new("cpu")
        .at(SECOND + 1)
        .batch("hwmon", "fast", hwmon)
        .batch("thermal", "fast", thermal)
        .build();
    collect(&collector, &plan)
}

fn cluster_temperature(scope: &str) -> MetricKey {
    metric_key("cpu.package.0", ids::CPU_TEMPERATURE_CLUSTER, scope)
}

/// GB10's four cluster sensors, the missing ones left out.
fn gb10_clusters(celsius: [Option<f64>; 4]) -> Vec<Observation> {
    ["cluster:0e", "cluster:0p", "cluster:1e", "cluster:1p"]
        .iter()
        .zip(celsius)
        .filter_map(|(scope, celsius)| {
            celsius.map(|celsius| {
                millidegrees(
                    &cluster_temperature(scope),
                    "thermal",
                    &format!("thermal:{scope}"),
                    celsius * 1000.0,
                    SECOND,
                )
            })
        })
        .collect()
}

/// GB10 (D-68): with no sensor of the package itself, the package
/// temperature is the hottest of its four cluster sensors, a derived value
/// that lists them, and each sensor stays a series of its own. Without all
/// four there is no package value; a sensor of the package itself is
/// published as measured instead.
#[test]
fn gb10_package_temperature_is_the_hottest_cluster() {
    let readings = [Some(74.8), Some(78.8), Some(75.3), Some(87.6)];
    let batch = temperatures(Vec::new(), gb10_clusters(readings));
    let package = value_of(&batch, &package_temperature()).unwrap();
    assert_close(package.value.unwrap().to_f64_lossy(), 87.6);
    assert_eq!(package.sample_kind, SampleKind::Derived);
    assert!(
        package
            .reason_codes
            .contains(&reasons::FORMULA_GB10_CLUSTER_MAX.to_owned()),
        "{:?}",
        package.reason_codes
    );
    assert_eq!(package.candidate_source_ids.len(), 4);
    assert_eq!(
        number(&batch, &cluster_temperature("cluster:0p")),
        Some(78.8)
    );

    let partial = temperatures(
        Vec::new(),
        gb10_clusters([Some(74.8), Some(78.8), None, Some(87.6)]),
    );
    assert_eq!(number(&partial, &package_temperature()), None);

    let measured = temperatures(
        vec![millidegrees(
            &package_temperature(),
            "hwmon",
            "hwmon:package0:temp1",
            68000.0,
            SECOND,
        )],
        gb10_clusters(readings),
    );
    let package = value_of(&measured, &package_temperature()).unwrap();
    assert_eq!(package.value.unwrap().to_f64_lossy(), 68.0);
    assert_eq!(package.sample_kind, SampleKind::Measured);
}

#[test]
fn architecture_example_soc_scope_is_published_separately() {
    let batch = temperatures(
        vec![millidegrees(
            &package_temperature(),
            "hwmon",
            "hwmon:package0:temp1",
            68000.0,
            SECOND,
        )],
        vec![millidegrees(
            &soc_temperature(),
            "thermal",
            "thermal:zone0",
            42000.0,
            SECOND,
        )],
    );
    assert_eq!(number(&batch, &package_temperature()), Some(68.0));
    assert_eq!(number(&batch, &soc_temperature()), Some(42.0));
}

#[test]
fn architecture_example_independent_sources_conflict_without_value() {
    let batch = temperatures(
        vec![with_origin(
            millidegrees(
                &package_temperature(),
                "hwmon",
                "hwmon:package0:temp1",
                68000.0,
                SECOND,
            ),
            "sensor:package0",
        )],
        vec![with_origin(
            millidegrees(
                &package_temperature(),
                "thermal",
                "thermal:package0",
                42000.0,
                SECOND,
            ),
            "sensor:independent-package0",
        )],
    );
    let value = value_of(&batch, &package_temperature()).unwrap();
    assert_eq!(value.selection_state, SelectionState::Conflict);
    assert_eq!(value.value, None);
    let explanation = batch
        .explanations
        .iter()
        .find(|explanation| explanation.key == package_temperature())
        .unwrap();
    assert_eq!(explanation.candidates.len(), 2);
}

#[test]
fn architecture_example_alias_counts_once_and_fault_is_excluded() {
    let alias = temperatures(
        vec![with_origin(
            millidegrees(
                &package_temperature(),
                "hwmon",
                "hwmon:package0:temp1",
                68000.0,
                SECOND,
            ),
            "sensor:package0",
        )],
        vec![with_origin(
            millidegrees(
                &package_temperature(),
                "thermal",
                "thermal:package0",
                42000.0,
                SECOND,
            ),
            "sensor:package0",
        )],
    );
    assert_eq!(number(&alias, &package_temperature()), Some(68.0));

    let mut faulted = millidegrees(
        &package_temperature(),
        "thermal",
        "thermal:package0",
        42000.0,
        SECOND,
    );
    faulted.reason_codes.push(reasons::SENSOR_FAULT.to_owned());
    let fault = temperatures(
        vec![millidegrees(
            &package_temperature(),
            "hwmon",
            "hwmon:package0:temp1",
            68000.0,
            SECOND,
        )],
        vec![faulted],
    );
    assert_eq!(number(&fault, &package_temperature()), Some(68.0));
    let explanation = fault
        .explanations
        .iter()
        .find(|explanation| explanation.key == package_temperature())
        .unwrap();
    assert!(explanation.candidates.iter().any(|candidate| {
        candidate.source_id.as_str() == "thermal:package0"
            && candidate.disposition == CandidateDisposition::Rejected(RejectReason::DriverFault)
    }));
}

#[test]
fn candidates_of_different_batches_are_arbitrated_together() {
    let collector = CpuCollector::new().unwrap();
    let first = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .at(SECOND + 1)
            .batch(
                "hwmon",
                "fast",
                vec![with_origin(
                    millidegrees(
                        &package_temperature(),
                        "hwmon",
                        "hwmon:package0:temp1",
                        68000.0,
                        SECOND,
                    ),
                    "o1",
                )],
            )
            .build(),
    );
    assert_eq!(number(&first, &package_temperature()), Some(68.0));
    let second = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .after(&first)
            .at(SECOND + 100 * MILLISECOND)
            .batch(
                "thermal",
                "fast",
                vec![with_origin(
                    millidegrees(
                        &package_temperature(),
                        "thermal",
                        "thermal:package0",
                        42000.0,
                        SECOND + 50 * MILLISECOND,
                    ),
                    "o2",
                )],
            )
            .build(),
    );
    let value = value_of(&second, &package_temperature()).unwrap();
    assert_eq!(value.selection_state, SelectionState::Conflict);
}

#[test]
fn retained_reading_expires_once_as_stale() {
    let collector = CpuCollector::new().unwrap();
    let first = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .at(SECOND + 1)
            .batch(
                "hwmon",
                "fast",
                vec![millidegrees(
                    &package_temperature(),
                    "hwmon",
                    "hwmon:a",
                    68000.0,
                    SECOND,
                )],
            )
            .build(),
    );
    let expired = collect(
        &collector,
        &PlanBuilder::new("cpu").after(&first).at(5 * SECOND).build(),
    );
    let value = value_of(&expired, &package_temperature()).unwrap();
    assert_eq!(value.value, None);
    assert_eq!(value.freshness, dgxtop_core::quality::Freshness::Stale);
    assert!(expired.next_state.candidates.is_empty());
    assert!(
        expired
            .next_state
            .last_good
            .contains_key(&package_temperature()),
        "last good is kept for annotation only"
    );
    let later = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .after(&expired)
            .at(6 * SECOND)
            .build(),
    );
    assert!(later.canonical.is_empty());
}

// ----- power ----------------------------------------------------------------

fn package_power() -> MetricKey {
    metric_key("cpu.package.0", ids::CPU_POWER_PACKAGE, "package:0")
}

fn energy_record(micro_joules: u64, at: u64) -> Observation {
    reading(
        &metric_key("cpu.package.0", ids::CPU_ENERGY_PACKAGE, "package:0"),
        "powercap",
        "powercap:intel-rapl:0:energy",
        at,
    )
    .unit(native::MICRO_JOULE)
    .u64(micro_joules)
    .build()
}

fn energy_catalog(
    modulus: Option<u64>,
    origin: OriginIdentity,
) -> dgxtop_core::canonical::CatalogSnapshot {
    let energy_key = metric_key("cpu.package.0", ids::CPU_ENERGY_PACKAGE, "package:0");
    let mut spec = SourceSpec::new(
        "powercap:intel-rapl:0:energy",
        "powercap",
        &energy_key,
        "fast",
    );
    spec.raw_kind = RawKind::Counter;
    spec.native_unit = native::MICRO_JOULE;
    spec.modulus = modulus;
    spec.origin = origin;
    catalog(&[spec], Vec::new())
}

fn energy_run(readings: &[(u64, u64)], modulus: Option<u64>) -> Vec<CollectionBatch> {
    let collector = CpuCollector::new().unwrap();
    let mut batches: Vec<CollectionBatch> = Vec::new();
    for (index, (micro_joules, at)) in readings.iter().enumerate() {
        let mut plan = PlanBuilder::new("cpu")
            .catalog(energy_catalog(modulus, OriginIdentity::unknown()))
            .cycle(u64::try_from(index).unwrap() + 1)
            .at(at + 1)
            .batch("powercap", "fast", vec![energy_record(*micro_joules, *at)]);
        if let Some(previous) = batches.last() {
            plan = plan.after(previous);
        }
        batches.push(collect(&collector, &plan.build()));
    }
    batches
}

#[test]
fn package_power_from_energy_counter_rate() {
    let batches = energy_run(&[(1_000_000, SECOND), (31_000_000, 3 * SECOND)], None);
    let warmup = value_of(&batches[0], &package_power()).unwrap();
    assert_eq!(warmup.value, None);
    assert!(warmup.reason_codes.contains(&reasons::WARMUP.to_owned()));
    let power = value_of(&batches[1], &package_power()).unwrap();
    assert_eq!(power.value.map(|value| value.to_f64_lossy()), Some(15.0));
    assert_eq!(power.sample_kind, SampleKind::Derived);
    assert!(
        power
            .reason_codes
            .contains(&reasons::FORMULA_ENERGY_DELTA.to_owned())
    );
    let interval = &batches[1].counter_intervals[0];
    assert_eq!(interval.key, package_power());
    assert_eq!(interval.unit, CanonicalUnit::Joules);
    assert_close(interval.delta.to_f64_lossy(), 30.0);
}

#[test]
fn energy_and_hwmon_power_of_one_origin_are_aliases() {
    let collector = CpuCollector::new().unwrap();
    let origin = OriginIdentity::confirmed(OriginId::new("rapl:package0").unwrap());
    let hwmon_power = |watts: f64, at: u64| {
        with_origin(
            reading(&package_power(), "hwmon", "hwmon:package0:power1", at)
                .unit(native::WATT)
                .f64(watts)
                .build(),
            "rapl:package0",
        )
    };
    let mut energy = energy_record(1_000_000, SECOND);
    energy.provenance.origin_id = origin.origin_id.clone();
    energy.provenance.origin_relation = origin.relation;
    let first = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .catalog(energy_catalog(None, origin.clone()))
            .at(SECOND + 1)
            .batch("powercap", "fast", vec![energy])
            .batch("hwmon", "fast", vec![hwmon_power(14.0, SECOND)])
            .build(),
    );
    assert_eq!(number(&first, &package_power()), Some(14.0));
    let mut energy = energy_record(16_000_000, 2 * SECOND);
    energy.provenance.origin_id = origin.origin_id.clone();
    energy.provenance.origin_relation = origin.relation;
    let second = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .catalog(energy_catalog(None, origin))
            .after(&first)
            .cycle(2)
            .at(2 * SECOND + 1)
            .batch("powercap", "fast", vec![energy])
            .batch("hwmon", "fast", vec![hwmon_power(30.0, 2 * SECOND)])
            .build(),
    );
    let value = value_of(&second, &package_power()).unwrap();
    assert_eq!(
        value.selection_state,
        SelectionState::Selected,
        "one physical origin never conflicts with itself"
    );
    let explanation = second
        .explanations
        .iter()
        .find(|explanation| explanation.key == package_power())
        .unwrap();
    assert!(
        explanation
            .candidates
            .iter()
            .any(|candidate| matches!(candidate.disposition, CandidateDisposition::Alias { .. }))
    );
}

fn cluster(scope: &str, watts: Option<f64>, at: u64) -> Observation {
    let key = metric_key("cpu.package.0", ids::CPU_POWER_CLUSTER, scope);
    let builder =
        reading(&key, "hwmon", &format!("hwmon:spbm:{scope}"), at).unit(native::MICRO_WATT);
    match watts {
        Some(watts) => builder.f64(watts * 1_000_000.0).build(),
        None => builder
            .failed(dgxtop_core::quality::ReadStatus::Error)
            .build(),
    }
}

#[test]
fn gb10_package_power_is_the_sum_of_both_clusters() {
    let collector = CpuCollector::new().unwrap();
    let plan = PlanBuilder::new("cpu")
        .at(SECOND + 1)
        .batch(
            "hwmon",
            "fast",
            vec![
                cluster("cluster:p", Some(8.5), SECOND),
                cluster("cluster:e", Some(1.2), SECOND),
            ],
        )
        .build();
    let batch = collect(&collector, &plan);
    let power = value_of(&batch, &package_power()).unwrap();
    assert_close(power.value.unwrap().to_f64_lossy(), 9.7);
    assert_eq!(power.sample_kind, SampleKind::Derived);
    assert!(
        power
            .reason_codes
            .contains(&reasons::FORMULA_GB10_CLUSTER_SUM.to_owned())
    );
    let ids: Vec<&str> = power
        .candidate_source_ids
        .iter()
        .map(|id| id.as_str())
        .collect();
    assert!(ids.contains(&"hwmon:spbm:cluster:p") && ids.contains(&"hwmon:spbm:cluster:e"));
    assert!(
        value_of(
            &batch,
            &metric_key("cpu.package.0", ids::CPU_POWER_CLUSTER, "cluster:p")
        )
        .is_none(),
        "cluster components are raw inputs"
    );
}

#[test]
fn gb10_formula_needs_both_components() {
    let collector = CpuCollector::new().unwrap();
    for records in [
        vec![cluster("cluster:p", Some(8.5), SECOND)],
        vec![
            cluster("cluster:p", Some(8.5), SECOND),
            cluster("cluster:e", None, SECOND),
        ],
        vec![
            cluster("cluster:p", Some(8.5), SECOND),
            cluster("cluster:e", Some(1.2), SECOND - 900 * MILLISECOND),
        ],
    ] {
        let plan = PlanBuilder::new("cpu")
            .at(SECOND + 1)
            .batch("hwmon", "fast", records)
            .build();
        let batch = collect(&collector, &plan);
        let power = value_of(&batch, &package_power()).unwrap();
        assert_eq!(power.value, None, "one component missing: no derived value");
        assert!(
            power
                .reason_codes
                .contains(&reasons::FORMULA_GB10_CLUSTER_SUM.to_owned())
        );
    }
}

#[test]
fn platform_power_never_masquerades_as_package_power() {
    let collector = CpuCollector::new().unwrap();
    let platform = metric_key("cpu.package.0", ids::CPU_POWER_PLATFORM, "platform");
    let plan = PlanBuilder::new("cpu")
        .at(SECOND + 1)
        .batch(
            "hwmon",
            "fast",
            vec![
                reading(&platform, "hwmon", "hwmon:spbm:soc_pkg", SECOND)
                    .unit(native::MICRO_WATT)
                    .f64(17_000_000.0)
                    .quality(Quality::Verified)
                    .build(),
            ],
        )
        .build();
    let batch = collect(&collector, &plan);
    assert_eq!(number(&batch, &platform), Some(17.0));
    assert!(value_of(&batch, &package_power()).is_none());
}

#[test]
fn replayed_inputs_never_publish_into_the_live_namespace() {
    let collector = CpuCollector::new().unwrap();
    let replayed = |ticks: Ticks, at: u64| {
        let mut records = stat_line(None, ticks, at);
        for record in &mut records {
            record.sample_kind = SampleKind::Replay;
            record.provenance.adapter_id = dgxtop_core::identity::AdapterId::new("replay").unwrap();
        }
        records
    };
    let first = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .at(SECOND + 1)
            .batch(
                "replay",
                "procfs.cpu",
                replayed([0, 0, 0, 0, 0, 0, 0, 0], SECOND),
            )
            .build(),
    );
    let second = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .after(&first)
            .at(2 * SECOND + 1)
            .batch(
                "replay",
                "procfs.cpu",
                replayed([5, 0, 0, 5, 0, 0, 0, 0], 2 * SECOND),
            )
            .build(),
    );
    assert!(first.canonical.is_empty() && second.canonical.is_empty());
    assert!(
        second.next_state.counters.is_empty(),
        "replay never seeds live bases"
    );

    let replay_policy = crate::testkit::replay_policy();
    let first = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .policy(replay_policy.clone())
            .at(SECOND + 1)
            .batch(
                "replay",
                "procfs.cpu",
                replayed([0, 0, 0, 0, 0, 0, 0, 0], SECOND),
            )
            .build(),
    );
    let second = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .policy(replay_policy)
            .after(&first)
            .at(2 * SECOND + 1)
            .batch(
                "replay",
                "procfs.cpu",
                replayed([5, 0, 0, 5, 0, 0, 0, 0], 2 * SECOND),
            )
            .build(),
    );
    let total = value_of(&second, &utilization_key(None, ids::CPU_UTILIZATION_TOTAL)).unwrap();
    assert_eq!(total.value.map(|value| value.to_f64_lossy()), Some(50.0));
    assert_eq!(total.sample_kind, SampleKind::Replay);
    assert!(!total.eligible_for_history);
}

#[test]
fn value_returning_after_a_conflict_is_published_again() {
    let collector = CpuCollector::new().unwrap();
    // hwmon is the profile-preferred source, so it is re-selected after the
    // conflict although its retained sample is older.
    let key = package_temperature();
    let mut preferred = SourceSpec::new("hwmon:a", "hwmon", &key, "fast");
    preferred.native_unit = native::MILLI_DEG_C;
    let mut fallback = SourceSpec::new("thermal:b", "thermal", &key, "fast");
    fallback.native_unit = native::MILLI_DEG_C;
    fallback.rank = 1;
    let ranked = catalog(&[preferred, fallback], Vec::new());
    let hwmon = |at: u64| {
        with_origin(
            millidegrees(&package_temperature(), "hwmon", "hwmon:a", 68000.0, at),
            "o1",
        )
    };
    let thermal = |value: f64, at: u64| {
        with_origin(
            millidegrees(&package_temperature(), "thermal", "thermal:b", value, at),
            "o2",
        )
    };
    let agreed = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .catalog(ranked.clone())
            .at(SECOND + 1)
            .batch("hwmon", "fast", vec![hwmon(SECOND)])
            .batch("thermal", "fast", vec![thermal(67000.0, SECOND)])
            .build(),
    );
    assert_eq!(number(&agreed, &package_temperature()), Some(68.0));
    let conflict = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .catalog(ranked.clone())
            .after(&agreed)
            .at(SECOND + 2)
            .batch("thermal", "fast", vec![thermal(40000.0, SECOND + 1)])
            .build(),
    );
    assert_eq!(
        value_of(&conflict, &package_temperature())
            .unwrap()
            .selection_state,
        SelectionState::Conflict
    );
    // Only thermal reports again; the retained hwmon sample is unchanged but
    // must replace the committed conflict.
    let resolved = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .catalog(ranked.clone())
            .after(&conflict)
            .at(SECOND + 3)
            .batch("thermal", "fast", vec![thermal(67500.0, SECOND + 2)])
            .build(),
    );
    let value = value_of(&resolved, &package_temperature()).unwrap();
    assert_eq!(value.selection_state, SelectionState::Selected);
    assert_eq!(value.value.map(|value| value.to_f64_lossy()), Some(68.0));
    // A further round with nothing new for the selected source publishes
    // nothing new for it.
    let quiet = collect(
        &collector,
        &PlanBuilder::new("cpu")
            .catalog(ranked)
            .after(&resolved)
            .at(SECOND + 4)
            .batch("thermal", "fast", vec![thermal(67400.0, SECOND + 3)])
            .build(),
    );
    assert!(value_of(&quiet, &package_temperature()).is_none());
}
