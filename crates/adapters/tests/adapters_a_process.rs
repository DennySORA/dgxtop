//! Procfs `process` group: identity, values, paging and bounds.

#[path = "fixtures/adapters_a/support.rs"]
mod support;

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dgxtop_adapters::procfs::{HostConstants, ProcfsAdapter};
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{BootId, ProcessKey};
use dgxtop_core::metric::ids;
use dgxtop_core::observation::{BatchStatus, Observation, PAGE_CONTINUES};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Quality, ReadStatus};
use dgxtop_core::testing::{FakeClock, boot_id, host_id};
use support::*;

const ADAPTER: &str = "procfs";

fn adapter(tree: &FakeTree, page_size: usize) -> ProcfsAdapter {
    ProcfsAdapter::with_host_constants(
        FakeClock::new(1_000_000_000),
        host_id("test-host"),
        tree.files(),
        AdapterOptions {
            page_size,
            ..AdapterOptions::default()
        },
        HostConstants {
            clk_tck: Some(100),
            page_size: Some(4096),
        },
    )
}

fn process_tree(label: &str) -> FakeTree {
    let tree = FakeTree::new(label);
    write_namespaces(&tree);
    write_process(&tree, 1, "systemd", 10, 5, 1, "100");
    write_process(&tree, 7, "kworker/0:1H", 0, 3, 2, "0");
    write_process(&tree, 42, "my (weird) prog", 250, 50, 90_100, "789");
    tree
}

fn by_metric<'a>(records: &'a [Observation], metric: &str, pid: u32) -> &'a Observation {
    records
        .iter()
        .find(|record| {
            record.key.metric_id.as_str() == metric
                && record.provenance.device_path == format!("/proc/{pid}")
        })
        .unwrap_or_else(|| panic!("no {metric} for pid {pid}"))
}

#[test]
fn process_records_carry_identity_basename_and_values() {
    let tree = process_tree("process-identity");
    let mut adapter = adapter(&tree, 256);
    let batches = read_all(&mut adapter, ADAPTER, "process", 1);
    assert_eq!(batches[0].status, BatchStatus::Ok);
    let records = records(&batches);
    assert_eq!(records.len(), 9);
    let pids: Vec<&str> = records
        .iter()
        .map(|record| record.provenance.device_path.as_str())
        .collect();
    assert_eq!(pids[0], "/proc/1");
    assert_eq!(pids[3], "/proc/7");
    assert_eq!(pids[6], "/proc/42");

    let ticks = by_metric(&records, ids::PROCESS_CPU_TICKS, 42);
    assert_eq!(u64_value(ticks), 300, "utime + stime");
    assert_eq!(ticks.native_unit.as_str(), "ticks");
    assert_eq!(ticks.quality, Quality::Verified);
    assert_eq!(ticks.key.scope.as_str(), "process");
    assert_eq!(
        ticks.key.entity_id.as_str(),
        "process:pid:[4026531836]:42:90100"
    );
    assert_eq!(ticks.provenance.channel, "my (weird) prog");
    assert_eq!(
        ticks.provenance.source_id.as_str(),
        "procfs:pid42.90100:cpu.ticks"
    );
    let key = ProcessKey::from_entity_id(
        &host_id("test-host"),
        &boot_id("fake-boot"),
        ticks.key.entity_id.as_str(),
    )
    .unwrap();
    assert_eq!(key.pid.get(), 42);
    assert_eq!(key.start_time_ticks, 90_100);
    assert_eq!(key.pid_namespace.as_str(), PID_NAMESPACE);
    assert_eq!(key.entity_id().unwrap(), ticks.key.entity_id);

    let rss = by_metric(&records, ids::PROCESS_RSS, 42);
    assert_eq!(u64_value(rss), 789 * 4096);
    assert_eq!(rss.native_unit.as_str(), "bytes");
    assert_eq!(rss.provenance.source_id.as_str(), "procfs:pid42.90100:rss");

    let uid = by_metric(&records, ids::PROCESS_UID, 42);
    let owner = std::fs::metadata(tree.path("/proc/42")).unwrap().uid();
    assert_eq!(u64_value(uid), u64::from(owner));
    assert_eq!(uid.native_unit.as_str(), "count");

    let kernel_thread = by_metric(&records, ids::PROCESS_RSS, 7);
    assert_eq!(u64_value(kernel_thread), 0, "a real zero stays zero");
    // Convention: the sanitized command name travels in the channel. `comm`
    // is not a path, so a kernel thread name keeps its `/`.
    assert_eq!(kernel_thread.provenance.channel, "kworker/0:1H");
    for record in &records {
        record.validate().unwrap();
    }
}

#[test]
fn hostile_command_names_are_sanitized() {
    let tree = FakeTree::new("process-hostile");
    write_namespaces(&tree);
    write_process(&tree, 9, "evil\u{1b}[2Jname", 1, 1, 5, "1");
    let mut adapter = adapter(&tree, 256);
    let records = records(&read_all(&mut adapter, ADAPTER, "process", 1));
    assert_eq!(records[0].provenance.channel, "evilname");
}

#[test]
fn paging_is_consistent_within_a_cycle_and_resnapshots_the_next_cycle() {
    let tree = process_tree("process-paging");
    let mut adapter = adapter(&tree, 4);
    let first = adapter.read(&read_plan(ADAPTER, "process", 1, 0, Vec::new()));
    assert_eq!(first.records.len(), 4);
    assert_eq!(first.status, BatchStatus::Partial);
    assert_eq!(first.error_code.as_deref(), Some(PAGE_CONTINUES));
    write_process(&tree, 50, "late", 1, 1, 7, "1");
    let second = adapter.read(&read_plan(ADAPTER, "process", 1, 1, Vec::new()));
    let third = adapter.read(&read_plan(ADAPTER, "process", 1, 2, Vec::new()));
    assert_eq!(second.records.len(), 4);
    assert!(second.continues());
    assert_eq!(third.records.len(), 1);
    assert!(!third.continues());
    assert_eq!(third.status, BatchStatus::Ok);
    let cycle: Vec<&Observation> = first
        .records
        .iter()
        .chain(&second.records)
        .chain(&third.records)
        .collect();
    assert!(
        cycle
            .iter()
            .all(|record| record.provenance.device_path != "/proc/50"),
        "later pages slice the page-0 snapshot"
    );
    let sources: std::collections::BTreeSet<&str> = cycle
        .iter()
        .map(|record| record.provenance.source_id.as_str())
        .collect();
    assert_eq!(sources.len(), 9, "no duplicates across pages");
    assert!(first.started_boot_ns == third.started_boot_ns);

    let next = records(&read_all(&mut adapter, ADAPTER, "process", 2));
    assert_eq!(next.len(), 12);
    assert!(
        next.iter()
            .any(|record| record.provenance.device_path == "/proc/50")
    );
}

#[test]
fn vanished_and_unparsable_processes_are_not_error_records() {
    let tree = process_tree("process-vanished");
    tree.mkdir("/proc/60");
    tree.write("/proc/61/stat", "garbage");
    tree.mkdir("/proc/self");
    tree.mkdir("/proc/sys");
    let mut adapter = adapter(&tree, 256);
    let batches = read_all(&mut adapter, ADAPTER, "process", 1);
    let records = records(&batches);
    assert_eq!(records.len(), 9);
    assert!(
        records
            .iter()
            .all(|record| record.read_status == ReadStatus::Ok)
    );
    assert_eq!(
        batches[0].error_code.as_deref(),
        Some("PROCESS_STAT_UNPARSABLE")
    );
}

#[test]
fn overflowing_values_become_error_records() {
    let tree = FakeTree::new("process-overflow");
    write_namespaces(&tree);
    write_process(&tree, 70, "big", u64::MAX, 1, 3, "18446744073709551615");
    let mut adapter = adapter(&tree, 256);
    let records = records(&read_all(&mut adapter, ADAPTER, "process", 1));
    let rss = by_metric(&records, ids::PROCESS_RSS, 70);
    assert_eq!(rss.read_status, ReadStatus::Error);
    assert_eq!(rss.value, None);
    assert_eq!(rss.reason_codes, vec!["RSS_BYTES_OVERFLOW".to_owned()]);
    let ticks = by_metric(&records, ids::PROCESS_CPU_TICKS, 70);
    assert_eq!(ticks.read_status, ReadStatus::Error);
    assert_eq!(ticks.reason_codes, vec!["CPU_TICKS_OVERFLOW".to_owned()]);
}

#[test]
fn unknown_page_size_is_unsupported_not_zero() {
    let tree = process_tree("process-pagesize");
    let mut adapter = ProcfsAdapter::with_host_constants(
        FakeClock::new(1),
        host_id("test-host"),
        tree.files(),
        AdapterOptions::default(),
        HostConstants {
            clk_tck: None,
            page_size: None,
        },
    );
    let records = records(&read_all(&mut adapter, ADAPTER, "process", 1));
    let rss = by_metric(&records, ids::PROCESS_RSS, 42);
    assert_eq!(rss.read_status, ReadStatus::Unsupported);
    assert_eq!(rss.value, None);
    assert_eq!(rss.reason_codes, vec!["PAGE_SIZE_UNKNOWN".to_owned()]);
}

#[test]
fn missing_pid_namespace_fails_the_process_group() {
    let tree = FakeTree::new("process-nons");
    write_process(&tree, 42, "x", 1, 1, 1, "1");
    let mut adapter = adapter(&tree, 256);
    let batch = adapter.read(&read_plan(ADAPTER, "process", 1, 0, Vec::new()));
    assert_eq!(batch.status, BatchStatus::Failed);
    assert_eq!(batch.error_code.as_deref(), Some("PID_NAMESPACE_UNKNOWN"));
}

#[test]
fn process_scan_is_bounded() {
    let tree = FakeTree::new("process-bounded");
    write_namespaces(&tree);
    for pid in 1..=4097u32 {
        write_process(&tree, pid, "worker", 1, 1, u64::from(pid), "1");
    }
    let mut adapter = adapter(&tree, 256);
    let batches = read_all(&mut adapter, ADAPTER, "process", 1);
    assert_eq!(batches.len(), 48);
    let records = records(&batches);
    assert_eq!(records.len(), 4096 * 3);
    assert!(
        records
            .iter()
            .all(|record| record.provenance.device_path != "/proc/4097")
    );
    let last = batches.last().unwrap();
    assert_eq!(last.error_code.as_deref(), Some("PROCESS_SCAN_TRUNCATED"));
    assert_eq!(last.status, BatchStatus::Partial);
    assert!(batches[..47].iter().all(|batch| batch.continues()));
}

/// Clock that advances 1 ms on every reading, so long scans hit deadlines.
struct SteppingClock {
    now: AtomicU64,
}

impl TimeProvider for SteppingClock {
    fn boot_id(&self) -> BootId {
        boot_id("fake-boot")
    }

    fn now_boot_ns(&self) -> u64 {
        self.now.fetch_add(1_000_000, Ordering::SeqCst)
    }

    fn now_utc_ns(&self) -> i128 {
        0
    }
}

#[test]
fn process_scan_stops_at_the_deadline() {
    let tree = FakeTree::new("process-deadline");
    write_namespaces(&tree);
    for pid in 1..=300u32 {
        write_process(&tree, pid, "worker", 1, 1, u64::from(pid), "1");
    }
    let clock = Arc::new(SteppingClock {
        now: AtomicU64::new(1_000_000_000),
    });
    let mut adapter = ProcfsAdapter::with_host_constants(
        clock,
        host_id("test-host"),
        tree.files(),
        AdapterOptions::default(),
        HostConstants {
            clk_tck: Some(100),
            page_size: Some(4096),
        },
    );
    let mut plan = read_plan(ADAPTER, "process", 1, 0, Vec::new());
    plan.deadline_boot_ns = 1_000_000_000 + 100 * 1_000_000;
    let batch = adapter.read(&plan);
    let mut batches = vec![batch];
    let mut page = 1;
    while batches.last().unwrap().continues() {
        let mut next = read_plan(ADAPTER, "process", 1, page, Vec::new());
        next.deadline_boot_ns = u64::MAX;
        batches.push(adapter.read(&next));
        page += 1;
    }
    let last = batches.last().unwrap();
    assert_eq!(last.error_code.as_deref(), Some("PROCESS_SCAN_DEADLINE"));
    let scanned = records(&batches).len() / 3;
    assert!(scanned > 0 && scanned < 300, "scanned {scanned}");
}
