//! Process collector tests (AT09).

use dgxtop_core::canonical::{CollectionBatch, CollectionLimits, GpuUsageKind, ProcessRow};
use dgxtop_core::metric::{ids, native};
use dgxtop_core::observation::BatchStatus;
use dgxtop_core::observation::Observation;
use dgxtop_core::quality::{Quality, ReadStatus};
use dgxtop_core::testing::metric_key;

use super::*;
use crate::testkit::{PlanBuilder, SECOND, catalog, collect, host_entity, limits, reading};

const NS: &str = "pid:[4026531836]";
const CLK_TCK: u64 = 100;

fn entity_of(pid: i32, start: u64) -> String {
    format!("process:{NS}:{pid}:{start}")
}

/// procfs records of one process: ticks, RSS (KiB) and uid.
fn host(pid: i32, start: u64, ticks: u64, rss_kib: u64, at: u64) -> Vec<Observation> {
    let entity = entity_of(pid, start);
    let record = |metric: &str, unit: &str, value: u64| {
        let mut observation = reading(
            &metric_key(&entity, metric, "process"),
            "procfs",
            &format!("procfs:{entity}:{metric}"),
            at,
        )
        .unit(unit)
        .u64(value)
        .build();
        observation.provenance.channel = format!("worker-{pid}").into();
        observation
    };
    vec![
        record(ids::PROCESS_CPU_TICKS, native::TICKS, ticks),
        record(ids::PROCESS_RSS, native::KIB, rss_kib),
        record(ids::PROCESS_UID, native::COUNT, 1000),
    ]
}

/// NVML adapter convention: one source per GPU and metric (records carry
/// their own `gpupid:<UUID>:<PID>` key), usage as a `gpu_usage:*` reason
/// code, `usable` quality.
fn gpu_memory(uuid: &str, pid: i32, usage: &str, bytes: u64, at: u64) -> Observation {
    let key = metric_key(
        &format!("gpupid:{uuid}:{pid}"),
        ids::PROCESS_GPU_MEMORY,
        &format!("gpu:{uuid}"),
    );
    reading(
        &key,
        "nvml",
        &format!("nvml:{uuid}:process.memory_allocated"),
        at,
    )
    .unit(native::BYTES)
    .u64(bytes)
    .quality(Quality::Usable)
    .reason(usage)
    .build()
}

fn gpu_utilization(uuid: &str, pid: i32, percent: u64, at: u64) -> Observation {
    let key = metric_key(
        &format!("gpupid:{uuid}:{pid}"),
        ids::PROCESS_GPU_UTILIZATION,
        &format!("gpu:{uuid}"),
    );
    reading(
        &key,
        "nvml",
        &format!("nvml:{uuid}:process.utilization"),
        at,
    )
    .unit(native::PERCENT)
    .u64(percent)
    .quality(Quality::Usable)
    .build()
}

/// Per-GPU NVML process sources are catalogued without a fixed key.
fn nvml_process_sources(uuids: &[&str]) -> Vec<String> {
    uuids
        .iter()
        .flat_map(|uuid| {
            [
                format!("nvml:{uuid}:process.memory_allocated"),
                format!("nvml:{uuid}:process.utilization"),
            ]
        })
        .collect()
}

fn process_catalog() -> dgxtop_core::canonical::CatalogSnapshot {
    let names = nvml_process_sources(&["GPU-a", "GPU-b"]);
    let placeholder = metric_key("gpu:GPU-a", ids::PROCESS_GPU_MEMORY, "gpu:GPU-a");
    let specs: Vec<crate::testkit::SourceSpec<'_>> = names
        .iter()
        .map(|name| {
            let mut spec = crate::testkit::SourceSpec::new(name, "nvml", &placeholder, "process");
            spec.key = None;
            spec
        })
        .collect();
    catalog(&specs, vec![host_entity(&CLK_TCK.to_string())])
}

fn plan(procfs: Vec<Observation>, nvml: Vec<Observation>, now: u64) -> PlanBuilder {
    let mut builder = PlanBuilder::new("process")
        .catalog(process_catalog())
        .at(now);
    if !procfs.is_empty() {
        builder = builder.batch("procfs", "process", procfs);
    }
    if !nvml.is_empty() {
        builder = builder.batch("nvml", "process", nvml);
    }
    builder
}

fn rows(batch: &CollectionBatch) -> &[ProcessRow] {
    &batch.process_table.as_ref().unwrap().rows
}

fn row(batch: &CollectionBatch, pid: i32) -> &ProcessRow {
    rows(batch)
        .iter()
        .find(|row| row.key.pid.get() == pid)
        .unwrap_or_else(|| panic!("pid {pid} missing"))
}

/// Two cycles one second apart; returns the second batch.
fn two_cycles(
    first: (Vec<Observation>, Vec<Observation>),
    second: (Vec<Observation>, Vec<Observation>),
) -> CollectionBatch {
    let collector = ProcessCollector::new().unwrap();
    let warmup = collect(&collector, &plan(first.0, first.1, SECOND + 1).build());
    collect(
        &collector,
        &plan(second.0, second.1, 2 * SECOND + 1)
            .after(&warmup)
            .cycle(2)
            .build(),
    )
}

#[test]
fn cpu_percent_counts_cores_and_may_exceed_100() {
    let collector = ProcessCollector::new().unwrap();
    let warmup = collect(
        &collector,
        &plan(host(42, 500, 1_000, 2048, SECOND), vec![], SECOND + 1).build(),
    );
    let first = row(&warmup, 42);
    assert_eq!(first.cpu_percent, None);
    assert_eq!(first.missing_reason.as_deref(), Some(reasons::WARMUP));
    assert_eq!(first.rss_bytes, Some(2048 * 1024));
    assert_eq!(first.uid, Some(1000));
    assert_eq!(first.name, "worker-42");
    // 250 ticks in 1 s at CLK_TCK 100: two and a half cores busy.
    let measured = collect(
        &collector,
        &plan(
            host(42, 500, 1_250, 2048, 2 * SECOND),
            vec![],
            2 * SECOND + 1,
        )
        .after(&warmup)
        .build(),
    );
    assert_eq!(row(&measured, 42).cpu_percent, Some(250.0));
    assert!(
        measured.canonical.is_empty(),
        "process metrics never become canonical series"
    );
}

/// The adapter's display name may name a folder (`ComfyUI/main`, D-65) and
/// a kernel thread's `comm` holds a slash (`kworker/0:1H`): a row keeps the
/// whole name, cleaned of control sequences.
#[test]
fn a_process_name_keeps_its_folder() {
    let named = |pid: i32, name: &str| -> Vec<Observation> {
        host(pid, 100, 0, 10, SECOND)
            .into_iter()
            .map(|mut observation| {
                observation.provenance.channel = name.to_owned().into();
                observation
            })
            .collect()
    };
    let mut procfs = named(1, "ComfyUI/main");
    procfs.extend(named(2, "kworker/0:1H"));
    procfs.extend(named(3, "evil\u{1b}[2J/name"));
    let collector = ProcessCollector::new().unwrap();
    let batch = collect(&collector, &plan(procfs, vec![], SECOND + 1).build());
    assert_eq!(row(&batch, 1).name, "ComfyUI/main");
    assert_eq!(row(&batch, 2).name, "kworker/0:1H");
    assert_eq!(row(&batch, 3).name, "evil/name");
}

/// The uid record's channel carries the command line after the name
/// (D-73): the row keeps both apart, the command cleaned.
#[test]
fn a_process_row_carries_its_command_line() {
    let separator = dgxtop_core::naming::PROCESS_COMMAND_SEPARATOR;
    let procfs: Vec<Observation> = host(7, 100, 0, 10, SECOND)
        .into_iter()
        .map(|mut observation| {
            if observation.key.metric_id.as_str() == ids::PROCESS_UID {
                observation.provenance.channel =
                    format!("worker-7{separator}python train.py --lr 3e-4\u{1b}[2J").into();
            }
            observation
        })
        .collect();
    let collector = ProcessCollector::new().unwrap();
    let batch = collect(&collector, &plan(procfs, vec![], SECOND + 1).build());
    assert_eq!(row(&batch, 7).name, "worker-7");
    assert_eq!(row(&batch, 7).command, "python train.py --lr 3e-4");
    let plain = collect(
        &collector,
        &plan(host(8, 100, 0, 10, SECOND), vec![], SECOND + 1).build(),
    );
    assert_eq!(row(&plain, 8).command, "", "no command line, nothing");
}

#[test]
fn reused_pid_with_new_start_time_never_inherits_the_old_base() {
    let batch = two_cycles(
        (host(42, 500, 9_000, 10, SECOND), vec![]),
        (host(42, 777, 50, 10, 2 * SECOND), vec![]),
    );
    let reused = row(&batch, 42);
    assert_eq!(reused.key.start_time_ticks, 777);
    assert_eq!(reused.cpu_percent, None);
    assert_eq!(reused.missing_reason.as_deref(), Some(reasons::WARMUP));
    assert_eq!(
        rows(&batch).len(),
        1,
        "the exited instance is dropped from the table"
    );
}

#[test]
fn one_process_on_two_gpus_is_one_host_row_with_two_links() {
    let at = 2 * SECOND;
    let batch = two_cycles(
        (host(42, 100, 1_000, 4096, SECOND), vec![]),
        (
            host(42, 100, 1_100, 4096, at),
            vec![
                gpu_memory("GPU-a", 42, reasons::GPU_USAGE_COMPUTE, 1 << 30, at),
                gpu_utilization("GPU-a", 42, 60, at),
                gpu_memory("GPU-b", 42, reasons::GPU_USAGE_COMPUTE, 1 << 29, at),
            ],
        ),
    );
    assert_eq!(rows(&batch).len(), 1);
    let process = row(&batch, 42);
    assert_eq!(process.cpu_percent, Some(100.0));
    assert_eq!(process.rss_bytes, Some(4096 * 1024));
    assert_eq!(process.gpu.len(), 2);
    assert_eq!(process.gpu[0].gpu_entity_id.as_str(), "gpu:GPU-a");
    assert_eq!(process.gpu[0].memory_bytes, Some(1 << 30));
    assert_eq!(process.gpu[0].utilization_percent, Some(60.0));
    assert_eq!(process.gpu[0].usage, GpuUsageKind::Compute);
    assert_eq!(process.gpu[1].gpu_entity_id.as_str(), "gpu:GPU-b");
    assert_eq!(
        process.gpu[1].utilization_percent, None,
        "unknown utilization is not 0"
    );
}

#[test]
fn merged_compute_and_graphics_association_is_one_link() {
    let at = 2 * SECOND;
    let batch = two_cycles(
        (host(7, 100, 0, 10, SECOND), vec![]),
        (
            host(7, 100, 10, 10, at),
            vec![
                gpu_memory("GPU-a", 7, reasons::GPU_USAGE_COMPUTE_AND_GRAPHICS, 300, at),
                gpu_memory("GPU-b", 7, reasons::GPU_USAGE_GRAPHICS, 5, at),
            ],
        ),
    );
    let process = row(&batch, 7);
    assert_eq!(process.gpu.len(), 2);
    assert_eq!(process.gpu[0].usage, GpuUsageKind::ComputeAndGraphics);
    assert_eq!(
        process.gpu[0].memory_bytes,
        Some(300),
        "never double counted"
    );
    assert_eq!(process.gpu[1].usage, GpuUsageKind::Graphics);
    assert_eq!(
        process.rss_bytes,
        Some(10 * 1024),
        "host resources are not repeated per GPU"
    );
}

#[test]
fn association_without_usage_marker_is_unknown_usage() {
    let at = 2 * SECOND;
    let mut unmarked = gpu_memory("GPU-a", 8, reasons::GPU_USAGE_COMPUTE, 1, at);
    unmarked.reason_codes.clear();
    let batch = two_cycles(
        (host(8, 100, 0, 10, SECOND), vec![]),
        (host(8, 100, 10, 10, at), vec![unmarked]),
    );
    assert_eq!(row(&batch, 8).gpu[0].usage, GpuUsageKind::Unknown);
}

#[test]
fn unavailable_gpu_memory_is_none_not_zero() {
    let at = 2 * SECOND;
    let unavailable = {
        let key = metric_key("gpupid:GPU-a:9", ids::PROCESS_GPU_MEMORY, "gpu:GPU-a");
        reading(&key, "nvml", "nvml:GPU-a:process.memory_allocated", at)
            .unit(native::BYTES)
            .failed(ReadStatus::Unsupported)
            .reason(reasons::GPU_USAGE_COMPUTE)
            .build()
    };
    let batch = two_cycles(
        (host(9, 100, 0, 10, SECOND), vec![]),
        (host(9, 100, 10, 10, at), vec![unavailable]),
    );
    let link = &row(&batch, 9).gpu[0];
    assert_eq!(link.memory_bytes, None);
    assert_eq!(link.utilization_percent, None);
    assert_eq!(link.usage, GpuUsageKind::Compute);
}

#[test]
fn gpu_entry_of_an_older_instance_is_not_linked_to_a_reused_pid() {
    // The new instance started 50 s after boot; the GPU list was read at 2 s.
    let at = 2 * SECOND;
    let started_later = 50 * CLK_TCK;
    let batch = two_cycles(
        (host(42, started_later, 0, 10, SECOND), vec![]),
        (
            host(42, started_later, 10, 10, at),
            vec![gpu_memory("GPU-a", 42, reasons::GPU_USAGE_COMPUTE, 300, at)],
        ),
    );
    assert!(row(&batch, 42).gpu.is_empty());
}

#[test]
fn missing_clk_tck_is_never_defaulted() {
    let collector = ProcessCollector::new().unwrap();
    let without = |procfs: Vec<Observation>, nvml: Vec<Observation>, now: u64| {
        let mut builder = PlanBuilder::new("process")
            .at(now)
            .batch("procfs", "process", procfs);
        if !nvml.is_empty() {
            builder = builder.batch("nvml", "process", nvml);
        }
        builder
    };
    let warmup = collect(
        &collector,
        &without(host(42, 1, 0, 1, SECOND), vec![], SECOND + 1).build(),
    );
    let batch = collect(
        &collector,
        &without(
            host(42, 1, 100, 1, 2 * SECOND),
            vec![gpu_memory(
                "GPU-a",
                42,
                reasons::GPU_USAGE_COMPUTE,
                300,
                2 * SECOND,
            )],
            2 * SECOND + 1,
        )
        .after(&warmup)
        .build(),
    );
    let process = row(&batch, 42);
    assert_eq!(process.cpu_percent, None);
    assert_eq!(
        process.missing_reason.as_deref(),
        Some(reasons::CLK_TCK_UNKNOWN)
    );
    assert!(process.gpu.is_empty(), "no PID-reuse guard, no link");
    assert!(
        batch
            .failures
            .iter()
            .any(|failure| failure.code == reasons::CLK_TCK_UNKNOWN)
    );
}

#[test]
fn unreadable_rss_is_missing_not_zero() {
    let collector = ProcessCollector::new().unwrap();
    let mut records = host(9, 1, 0, 1, SECOND);
    records[1] = reading(
        &metric_key(&entity_of(9, 1), ids::PROCESS_RSS, "process"),
        "procfs",
        "procfs:9:rss",
        SECOND,
    )
    .failed(ReadStatus::PermissionDenied)
    .build();
    let batch = collect(&collector, &plan(records, vec![], SECOND + 1).build());
    let process = row(&batch, 9);
    assert_eq!(process.rss_bytes, None);
}

fn table_limits(max_processes: u32, max_links: u32) -> CollectionLimits {
    CollectionLimits {
        max_processes,
        max_process_gpu_links: max_links,
        ..limits()
    }
}

#[test]
fn truncation_keeps_gpu_processes_then_busiest() {
    let collector = ProcessCollector::new().unwrap();
    let mut first = host(1, 1, 0, 100, SECOND);
    first.extend(host(2, 1, 0, 900, SECOND));
    first.extend(host(3, 1, 0, 50, SECOND));
    first.extend(host(4, 1, 0, 10, SECOND));
    let warmup = collect(
        &collector,
        &plan(first, vec![], SECOND + 1)
            .limits(table_limits(3, 1))
            .build(),
    );
    let mut second = host(1, 1, 10, 100, 2 * SECOND); // 10 %
    second.extend(host(2, 1, 50, 900, 2 * SECOND)); // 50 %
    second.extend(host(3, 1, 90, 50, 2 * SECOND)); // 90 %
    second.extend(host(4, 1, 0, 10, 2 * SECOND)); // 0 %, but on two GPUs
    let batch = collect(
        &collector,
        &plan(
            second,
            vec![
                gpu_memory("GPU-a", 4, reasons::GPU_USAGE_COMPUTE, 1, 2 * SECOND),
                gpu_memory("GPU-b", 4, reasons::GPU_USAGE_COMPUTE, 1, 2 * SECOND),
            ],
            2 * SECOND + 1,
        )
        .limits(table_limits(3, 1))
        .after(&warmup)
        .build(),
    );
    let table = batch.process_table.as_ref().unwrap();
    let order: Vec<i32> = table.rows.iter().map(|row| row.key.pid.get()).collect();
    assert_eq!(order, vec![4, 3, 2]);
    assert!(table.truncated);
    assert_eq!(table.total_seen, 4);
    assert!(table.gpu_links_truncated);
    assert_eq!(table.rows[0].gpu.len(), 1);
}

#[test]
fn no_procfs_batch_means_no_table_update() {
    let collector = ProcessCollector::new().unwrap();
    let batch = collect(
        &collector,
        &plan(
            vec![],
            vec![gpu_memory(
                "GPU-a",
                4,
                reasons::GPU_USAGE_COMPUTE,
                1,
                SECOND,
            )],
            SECOND + 1,
        )
        .build(),
    );
    assert!(batch.process_table.is_none());
    assert_eq!(
        batch.next_state.candidates.len(),
        1,
        "the GPU list is kept for the next join"
    );
}

#[test]
fn gpu_list_from_an_earlier_job_is_joined() {
    let collector = ProcessCollector::new().unwrap();
    // Both PIDs share the per-GPU NVML source: each must be retained.
    let gpu_only = collect(
        &collector,
        &plan(
            vec![],
            vec![
                gpu_memory("GPU-a", 5, reasons::GPU_USAGE_COMPUTE, 64, SECOND),
                gpu_memory("GPU-a", 6, reasons::GPU_USAGE_COMPUTE, 32, SECOND),
                gpu_utilization("GPU-a", 6, 40, SECOND),
            ],
            SECOND + 1,
        )
        .build(),
    );
    assert_eq!(gpu_only.next_state.candidates.len(), 3);
    let mut procfs = host(5, 1, 0, 1, SECOND);
    procfs.extend(host(6, 1, 0, 1, SECOND));
    let batch = collect(
        &collector,
        &plan(procfs, vec![], SECOND + 2).after(&gpu_only).build(),
    );
    assert_eq!(row(&batch, 5).gpu[0].memory_bytes, Some(64));
    assert_eq!(row(&batch, 6).gpu[0].memory_bytes, Some(32));
    assert_eq!(row(&batch, 6).gpu[0].utilization_percent, Some(40.0));
}

#[test]
fn incomplete_gpu_process_list_is_reported() {
    let collector = ProcessCollector::new().unwrap();
    let plan = PlanBuilder::new("process")
        .catalog(process_catalog())
        .at(SECOND + 1)
        .batch("procfs", "process", host(5, 1, 0, 1, SECOND))
        .batch_with_status(
            "nvml",
            "process",
            vec![gpu_memory(
                "GPU-a",
                5,
                reasons::GPU_USAGE_COMPUTE,
                64,
                SECOND,
            )],
            BatchStatus::Partial,
            Some("NVML_PROCESS_LIST_PARTIAL"),
        )
        .build();
    let batch = collect(&collector, &plan);
    let codes: Vec<&str> = batch
        .failures
        .iter()
        .map(|failure| failure.code.as_str())
        .collect();
    assert!(codes.contains(&reasons::BATCH_PARTIAL));
    assert!(codes.contains(&reasons::GPU_PROCESS_LIST_INCOMPLETE));
    assert_eq!(
        row(&batch, 5).gpu.len(),
        1,
        "visible associations are still joined"
    );
}

#[test]
fn bases_stay_bounded_under_heavy_process_churn() {
    let collector = ProcessCollector::new().unwrap();
    let limits = table_limits(16, 64);
    let mut previous: Option<CollectionBatch> = None;
    for cycle in 0..50u64 {
        let at = (cycle + 1) * SECOND;
        let mut records = Vec::new();
        for offset in 0..200u64 {
            let pid = i32::try_from(cycle * 200 + offset + 1).unwrap();
            records.extend(host(pid, cycle, offset, 1, at));
        }
        let mut builder = plan(records, vec![], at + 1)
            .limits(limits)
            .cycle(cycle + 1);
        if let Some(previous) = &previous {
            builder = builder.after(previous);
        }
        let batch = collect(&collector, &builder.build());
        assert!(batch.next_state.processes.len() <= 16 * PROCESS_BASES_PER_ROW);
        assert!(rows(&batch).len() <= 16);
        assert!(batch.next_state.candidates.is_empty());
        previous = Some(batch);
    }
}
