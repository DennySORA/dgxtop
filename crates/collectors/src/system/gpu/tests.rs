//! GPU collector tests.

use dgxtop_core::canonical::CollectionBatch;
use dgxtop_core::identity::MetricKey;
use dgxtop_core::metric::{CanonicalUnit, ids, native};
use dgxtop_core::observation::BatchStatus;
use dgxtop_core::observation::Observation;
use dgxtop_core::quality::{Quality, ReadStatus, SelectionState};
use dgxtop_core::testing::metric_key;

use super::*;
use crate::testkit::{
    PlanBuilder, SECOND, SourceSpec, assert_close, catalog, collect, number, reading, value_of,
};

const GPU_A: &str = "GPU-aaaa";
const GPU_B: &str = "GPU-bbbb";

fn gpu_key(uuid: &str, metric: &str) -> MetricKey {
    metric_key(&format!("gpu:{uuid}"), metric, "gpu")
}

fn gauge(uuid: &str, metric: &str, unit: &str, value: u64, at: u64) -> Observation {
    reading(
        &gpu_key(uuid, metric),
        "nvml",
        &format!("nvml:{uuid}:{metric}"),
        at,
    )
    .unit(unit)
    .u64(value)
    .build()
}

/// NVML adapter convention: one source per GPU and metric (records carry
/// their own `gpupid:<UUID>:<PID>` key), usage as a `gpu_usage:*` reason
/// code, `usable` quality, `unsupported` when the figure is unavailable.
fn process_memory(
    uuid: &str,
    pid: i32,
    usage: &str,
    bytes: Option<u64>,
    at: u64,
    cycle: u64,
) -> Observation {
    let key = metric_key(
        &format!("gpupid:{uuid}:{pid}"),
        ids::PROCESS_GPU_MEMORY,
        &format!("gpu:{uuid}"),
    );
    let builder = reading(
        &key,
        "nvml",
        &format!("nvml:{uuid}:process.memory_allocated"),
        at,
    )
    .unit(native::BYTES)
    .quality(Quality::Usable)
    .reason(usage)
    .cycle(cycle);
    match bytes {
        Some(bytes) => builder.u64(bytes).build(),
        None => builder.failed(ReadStatus::Unsupported).build(),
    }
}

/// Per-GPU process sources are catalogued without a fixed key.
fn nvml_catalog() -> dgxtop_core::canonical::CatalogSnapshot {
    let names: Vec<String> = [GPU_A, GPU_B]
        .iter()
        .map(|uuid| format!("nvml:{uuid}:process.memory_allocated"))
        .collect();
    let placeholder = gpu_key(GPU_A, ids::PROCESS_GPU_MEMORY);
    let specs: Vec<SourceSpec<'_>> = names
        .iter()
        .map(|name| {
            let mut spec = SourceSpec::new(name, "nvml", &placeholder, "process");
            spec.key = None;
            spec.rank = 2;
            spec
        })
        .collect();
    catalog(&specs, Vec::new())
}

fn run(records_fast: Vec<Observation>, records_process: Vec<Observation>) -> CollectionBatch {
    let collector = GpuCollector::new().unwrap();
    let mut plan = PlanBuilder::new("gpu")
        .catalog(nvml_catalog())
        .at(SECOND + 1);
    if !records_fast.is_empty() {
        plan = plan.batch("nvml", "fast", records_fast);
    }
    if !records_process.is_empty() {
        plan = plan.batch("nvml", "process", records_process);
    }
    collect(&collector, &plan.build())
}

#[test]
fn gauges_are_normalized_and_true_zero_is_kept() {
    let batch = run(
        vec![
            gauge(
                GPU_A,
                ids::GPU_POWER_DRAW,
                native::MILLI_WATT,
                251_500,
                SECOND,
            ),
            gauge(GPU_A, ids::GPU_TEMPERATURE, native::DEG_C, 0, SECOND),
            gauge(
                GPU_A,
                ids::GPU_UTILIZATION_COMPUTE,
                native::PERCENT,
                0,
                SECOND,
            ),
            gauge(
                GPU_A,
                ids::GPU_MEMORY_FB_USED,
                native::BYTES,
                1 << 30,
                SECOND,
            ),
            gauge(
                GPU_B,
                ids::GPU_UTILIZATION_COMPUTE,
                native::PERCENT,
                87,
                SECOND,
            ),
        ],
        vec![],
    );
    assert_close(
        number(&batch, &gpu_key(GPU_A, ids::GPU_POWER_DRAW)).unwrap(),
        251.5,
    );
    assert_eq!(
        number(&batch, &gpu_key(GPU_A, ids::GPU_TEMPERATURE)),
        Some(0.0)
    );
    assert_eq!(
        number(&batch, &gpu_key(GPU_A, ids::GPU_UTILIZATION_COMPUTE)),
        Some(0.0)
    );
    assert_eq!(
        number(&batch, &gpu_key(GPU_B, ids::GPU_UTILIZATION_COMPUTE)),
        Some(87.0)
    );
    assert_eq!(
        value_of(&batch, &gpu_key(GPU_A, ids::GPU_MEMORY_FB_USED))
            .and_then(|value| value.value)
            .and_then(|value| value.as_u64()),
        Some(1 << 30)
    );
}

#[test]
fn unsupported_metric_is_unavailable_not_zero() {
    let fan = reading(
        &gpu_key(GPU_A, ids::GPU_FAN_SPEED),
        "nvml",
        "nvml:a:fan",
        SECOND,
    )
    .unit(native::PERCENT)
    .failed(ReadStatus::Unsupported)
    .build();
    let batch = run(vec![fan], vec![]);
    let value = value_of(&batch, &gpu_key(GPU_A, ids::GPU_FAN_SPEED)).unwrap();
    assert_eq!(value.value, None);
    assert_eq!(
        value.reason_codes,
        vec![
            reasons::NO_ELIGIBLE_CANDIDATE.to_owned(),
            reasons::READ_UNSUPPORTED.to_owned()
        ]
    );
}

#[test]
fn pcie_with_unverified_unit_has_no_value() {
    // NVML convention: KB of unverified definition, graded suspect.
    let pcie = reading(
        &gpu_key(GPU_A, ids::GPU_PCIE_TX),
        "nvml",
        "nvml:a:pcie_tx",
        SECOND,
    )
    .unit(native::KB_UNVERIFIED_PER_SECOND)
    .u64(1000)
    .quality(Quality::Suspect)
    .reason("NVML_KB_DEFINITION_UNVERIFIED")
    .build();
    let batch = run(vec![pcie], vec![]);
    let value = value_of(&batch, &gpu_key(GPU_A, ids::GPU_PCIE_TX)).unwrap();
    assert_eq!(
        value.value, None,
        "never converted with a guessed KB definition"
    );
    assert_eq!(value.selection_state, SelectionState::Unavailable);
    assert!(
        value
            .reason_codes
            .contains(&reasons::UNIT_UNVERIFIED.to_owned())
    );
}

#[test]
fn static_values_use_the_discovery_period_for_freshness() {
    let limit = |at: u64| gauge(GPU_A, ids::GPU_POWER_LIMIT, native::MILLI_WATT, 700_000, at);
    let collector = GpuCollector::new().unwrap();
    let mut policy = crate::testkit::policy();
    policy
        .effective_periods_ms
        .insert(naming::static_period_key("nvml"), 10_000);
    let cached = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .policy(policy)
            .at(20 * SECOND)
            .batch("nvml", "slow_health", vec![limit(SECOND)])
            .build(),
    );
    assert_eq!(
        number(&cached, &gpu_key(GPU_A, ids::GPU_POWER_LIMIT)),
        Some(700.0)
    );
    let stale = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .at(20 * SECOND)
            .batch("nvml", "slow_health", vec![limit(SECOND)])
            .build(),
    );
    let value = value_of(&stale, &gpu_key(GPU_A, ids::GPU_POWER_LIMIT)).unwrap();
    assert_eq!(value.value, None, "older than the static freshness window");
}

/// No bandwidth is estimated from busy% x peak (D-11): the catalog has no
/// such metric, so a record claiming one is refused as unknown.
#[test]
fn a_bandwidth_estimate_is_never_produced() {
    const ESTIMATE: &str = "gpu.memory.bandwidth.estimated";
    let batch = run(
        vec![gauge(
            GPU_A,
            ESTIMATE,
            native::BYTES_PER_SECOND,
            900_000_000_000,
            SECOND,
        )],
        vec![],
    );
    assert!(value_of(&batch, &gpu_key(GPU_A, ESTIMATE)).is_none());
    assert!(
        batch
            .failures
            .iter()
            .any(|failure| failure.code == reasons::UNKNOWN_METRIC)
    );
}

fn slow_health_policy() -> dgxtop_core::canonical::PolicySnapshot {
    let mut policy = crate::testkit::policy();
    policy
        .effective_periods_ms
        .insert(naming::period_key("nvml", "slow_health"), 5000);
    policy
}

#[test]
fn energy_is_cumulative_joules_with_exact_intervals_and_reset_warmup() {
    let collector = GpuCollector::new().unwrap();
    let energy = |millijoules: u64, at: u64| {
        gauge(
            GPU_A,
            ids::GPU_ENERGY_TOTAL,
            native::MILLI_JOULE,
            millijoules,
            at,
        )
    };
    let first = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .policy(slow_health_policy())
            .at(SECOND + 1)
            .batch("nvml", "slow_health", vec![energy(5_000_000, SECOND)])
            .build(),
    );
    assert_eq!(
        number(&first, &gpu_key(GPU_A, ids::GPU_ENERGY_TOTAL)),
        Some(5000.0)
    );
    assert!(first.counter_intervals.is_empty(), "warmup");
    let second = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .policy(slow_health_policy())
            .after(&first)
            .at(6 * SECOND + 1)
            .batch("nvml", "slow_health", vec![energy(6_250_000, 6 * SECOND)])
            .build(),
    );
    let interval = &second.counter_intervals[0];
    assert_eq!(interval.key, gpu_key(GPU_A, ids::GPU_ENERGY_TOTAL));
    assert_eq!(interval.unit, CanonicalUnit::Joules);
    assert_close(interval.delta.to_f64_lossy(), 1250.0);
    let reset = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .policy(slow_health_policy())
            .after(&second)
            .at(11 * SECOND + 1)
            .batch("nvml", "slow_health", vec![energy(10, 11 * SECOND)])
            .build(),
    );
    assert!(
        reset.counter_intervals.is_empty(),
        "backend reset is warmup, never negative"
    );
    assert_eq!(
        number(&reset, &gpu_key(GPU_A, ids::GPU_ENERGY_TOTAL)),
        Some(0.01)
    );
}

#[test]
fn process_allocations_are_summed_per_gpu_and_deduplicated_by_pid() {
    let batch = run(
        vec![],
        vec![
            // PID 10 arrives as one merged compute+graphics association; a
            // duplicate of the same association never counts twice.
            process_memory(
                GPU_A,
                10,
                reasons::GPU_USAGE_COMPUTE_AND_GRAPHICS,
                Some(400),
                SECOND,
                1,
            ),
            process_memory(
                GPU_A,
                10,
                reasons::GPU_USAGE_COMPUTE_AND_GRAPHICS,
                Some(400),
                SECOND,
                1,
            ),
            process_memory(GPU_A, 11, reasons::GPU_USAGE_COMPUTE, Some(100), SECOND, 1),
            process_memory(GPU_B, 10, reasons::GPU_USAGE_GRAPHICS, Some(7), SECOND, 1),
        ],
    );
    let a = value_of(&batch, &gpu_key(GPU_A, ids::GPU_MEMORY_PROCESS_ALLOCATED)).unwrap();
    assert_eq!(a.value.and_then(|value| value.as_u64()), Some(500));
    assert_eq!(a.selection_state, SelectionState::Selected);
    assert_eq!(a.quality, Quality::Usable);
    assert!(a.reason_codes.is_empty());
    assert_eq!(
        a.candidate_source_ids,
        vec![dgxtop_core::testing::source_id(&format!(
            "nvml:{GPU_A}:process.memory_allocated"
        ))]
    );
    let b = value_of(&batch, &gpu_key(GPU_B, ids::GPU_MEMORY_PROCESS_ALLOCATED)).unwrap();
    assert_eq!(b.value.and_then(|value| value.as_u64()), Some(7));
}

#[test]
fn incomplete_process_lists_flag_the_sum_as_partial() {
    let collector = GpuCollector::new().unwrap();
    let records = || {
        vec![process_memory(
            GPU_A,
            10,
            reasons::GPU_USAGE_COMPUTE,
            Some(400),
            SECOND,
            1,
        )]
    };
    let partial = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .catalog(nvml_catalog())
            .at(SECOND + 1)
            .batch_with_status(
                "nvml",
                "process",
                records(),
                BatchStatus::Partial,
                Some("NVML_PROCESS_LIST_PARTIAL"),
            )
            .build(),
    );
    let sum = value_of(&partial, &gpu_key(GPU_A, ids::GPU_MEMORY_PROCESS_ALLOCATED)).unwrap();
    assert_eq!(sum.value.and_then(|value| value.as_u64()), Some(400));
    assert!(
        sum.reason_codes
            .contains(&reasons::PARTIAL_PROCESS_VISIBILITY.to_owned())
    );
    assert!(
        partial
            .failures
            .iter()
            .any(|failure| failure.code == reasons::BATCH_PARTIAL)
    );

    // A continuing page without its final page is incomplete too ...
    let first_page_only = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .catalog(nvml_catalog())
            .at(SECOND + 1)
            .batch_with_status(
                "nvml",
                "process",
                records(),
                BatchStatus::Partial,
                Some(dgxtop_core::observation::PAGE_CONTINUES),
            )
            .build(),
    );
    let sum = value_of(
        &first_page_only,
        &gpu_key(GPU_A, ids::GPU_MEMORY_PROCESS_ALLOCATED),
    )
    .unwrap();
    assert!(
        sum.reason_codes
            .contains(&reasons::PARTIAL_PROCESS_VISIBILITY.to_owned())
    );

    // ... but not when the final page is part of the same plan.
    let both_pages = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .catalog(nvml_catalog())
            .at(SECOND + 1)
            .batch_with_status(
                "nvml",
                "process",
                records(),
                BatchStatus::Partial,
                Some(dgxtop_core::observation::PAGE_CONTINUES),
            )
            .batch(
                "nvml",
                "process:1",
                vec![process_memory(
                    GPU_A,
                    11,
                    reasons::GPU_USAGE_COMPUTE,
                    Some(100),
                    SECOND,
                    1,
                )],
            )
            .build(),
    );
    let sum = value_of(
        &both_pages,
        &gpu_key(GPU_A, ids::GPU_MEMORY_PROCESS_ALLOCATED),
    )
    .unwrap();
    assert_eq!(sum.value.and_then(|value| value.as_u64()), Some(500));
    assert!(sum.reason_codes.is_empty());
}

#[test]
fn failed_process_list_is_never_a_zero_sum() {
    let collector = GpuCollector::new().unwrap();
    let batch = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .catalog(nvml_catalog())
            .at(SECOND + 1)
            .batch_with_status(
                "nvml",
                "process",
                Vec::new(),
                BatchStatus::Failed,
                Some("NVML_SESSION_LOST"),
            )
            .build(),
    );
    assert!(batch.canonical.is_empty());
    assert_eq!(batch.failures[0].code, reasons::BATCH_FAILED);
}

#[test]
fn unknown_process_allocations_are_flagged_never_counted_as_zero() {
    let batch = run(
        vec![],
        vec![
            process_memory(GPU_A, 10, reasons::GPU_USAGE_COMPUTE, Some(400), SECOND, 1),
            process_memory(GPU_A, 12, reasons::GPU_USAGE_COMPUTE, None, SECOND, 1),
        ],
    );
    let partial = value_of(&batch, &gpu_key(GPU_A, ids::GPU_MEMORY_PROCESS_ALLOCATED)).unwrap();
    assert_eq!(partial.value.and_then(|value| value.as_u64()), Some(400));
    assert!(
        partial
            .reason_codes
            .contains(&reasons::PARTIAL_PROCESS_VISIBILITY.to_owned())
    );

    let none_known = run(
        vec![],
        vec![process_memory(
            GPU_A,
            12,
            reasons::GPU_USAGE_COMPUTE,
            None,
            SECOND,
            1,
        )],
    );
    let unavailable = value_of(
        &none_known,
        &gpu_key(GPU_A, ids::GPU_MEMORY_PROCESS_ALLOCATED),
    )
    .unwrap();
    assert_eq!(unavailable.value, None);
    assert_eq!(unavailable.selection_state, SelectionState::Unavailable);
    assert_eq!(
        unavailable.reason_codes,
        vec![reasons::NO_PROCESS_MEMORY_VISIBLE.to_owned()]
    );
}

#[test]
fn exited_processes_from_an_older_list_are_not_summed() {
    let collector = GpuCollector::new().unwrap();
    let first = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .at(SECOND + 1)
            .batch(
                "nvml",
                "process",
                vec![
                    process_memory(GPU_A, 10, reasons::GPU_USAGE_COMPUTE, Some(400), SECOND, 1),
                    process_memory(GPU_A, 11, reasons::GPU_USAGE_COMPUTE, Some(100), SECOND, 1),
                ],
            )
            .build(),
    );
    // PID 11 exited: the next list only has PID 10.
    let second = collect(
        &collector,
        &PlanBuilder::new("gpu")
            .after(&first)
            .cycle(2)
            .at(2 * SECOND + 1)
            .batch(
                "nvml",
                "process",
                vec![process_memory(
                    GPU_A,
                    10,
                    reasons::GPU_USAGE_COMPUTE,
                    Some(450),
                    2 * SECOND,
                    2,
                )],
            )
            .build(),
    );
    let value = value_of(&second, &gpu_key(GPU_A, ids::GPU_MEMORY_PROCESS_ALLOCATED)).unwrap();
    assert_eq!(value.value.and_then(|value| value.as_u64()), Some(450));
}
