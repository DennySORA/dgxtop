//! Procfs `process` group: identity, values, paging and bounds. One record
//! per process (D-79): its CPU ticks, the rest of its `stat` as tags.

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
use dgxtop_core::process_stat::ProcessStat;
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

/// The one record of process `pid`.
fn by_pid(records: &[Observation], pid: u32) -> &Observation {
    let path = format!("/proc/{pid}");
    let mut found = records
        .iter()
        .filter(|record| record.provenance.device_path == path);
    let record = found
        .next()
        .unwrap_or_else(|| panic!("no record for pid {pid}"));
    assert!(found.next().is_none(), "one record per process");
    record
}

fn stat_of(record: &Observation) -> ProcessStat {
    ProcessStat::from_reason_codes(&record.reason_codes).unwrap_or_else(|| panic!("no stat tags"))
}

/// The display name before the command line in a record's channel.
fn name_of(record: &Observation) -> String {
    record
        .provenance
        .channel
        .split(dgxtop_core::naming::PROCESS_COMMAND_SEPARATOR)
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn process_records_carry_identity_basename_and_values() {
    let tree = process_tree("process-identity");
    let mut adapter = adapter(&tree, 256);
    let batches = read_all(&mut adapter, ADAPTER, "process", 1);
    assert_eq!(batches[0].status, BatchStatus::Ok);
    let records = records(&batches);
    assert_eq!(records.len(), 3, "one record per process");
    let pids: Vec<&str> = records
        .iter()
        .map(|record| record.provenance.device_path.as_str())
        .collect();
    assert_eq!(pids, ["/proc/1", "/proc/7", "/proc/42"]);

    let ticks = by_pid(&records, 42);
    assert_eq!(ticks.key.metric_id.as_str(), ids::PROCESS_CPU_TICKS);
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

    let owner = std::fs::metadata(tree.path("/proc/42")).unwrap().uid();
    assert_eq!(
        stat_of(ticks),
        ProcessStat {
            rss_bytes: Some(789 * 4096),
            uid: Some(owner),
            ppid: Some(1),
            state: Some('S'),
            threads: Some(1),
            reaped_ticks: Some(0),
        }
    );

    let kernel_thread = by_pid(&records, 7);
    assert_eq!(
        stat_of(kernel_thread).rss_bytes,
        Some(0),
        "a real zero stays zero"
    );
    // Convention: the sanitized command name travels in the channel. `comm`
    // is not a path, so a kernel thread name keeps its `/`.
    assert_eq!(kernel_thread.provenance.channel, "kworker/0:1H");
    for record in &records {
        record.validate().unwrap();
    }
}

/// The owner of `/proc/<pid>` is read once per process instance, not
/// every cycle; a new instance of the PID is read again.
#[test]
fn owners_are_read_once_per_process() {
    let tree = process_tree("process-owners");
    let files = tree.faulty();
    let mut adapter = ProcfsAdapter::with_host_constants(
        FakeClock::new(1_000_000_000),
        host_id("test-host"),
        files.clone(),
        AdapterOptions::default(),
        HostConstants {
            clk_tck: Some(100),
            page_size: Some(4096),
        },
    );
    let owner = std::fs::metadata(tree.path("/proc/42")).unwrap().uid();
    for cycle in 1..=3 {
        let records = records(&read_all(&mut adapter, ADAPTER, "process", cycle));
        assert_eq!(stat_of(by_pid(&records, 42)).uid, Some(owner));
    }
    assert_eq!(files.reads_of("/proc/42"), 1);
    assert_eq!(files.reads_of("/proc/42/stat"), 3, "the stat every cycle");
    write_process(&tree, 42, "reused", 1, 1, 95_000, "1");
    let records = records(&read_all(&mut adapter, ADAPTER, "process", 4));
    assert_eq!(stat_of(by_pid(&records, 42)).uid, Some(owner));
    assert_eq!(
        files.reads_of("/proc/42"),
        2,
        "another instance, read again"
    );
}

/// Parent, threads and the reaped children's time come from the same
/// `stat` read.
#[test]
fn a_record_carries_parent_threads_and_reaped_time() {
    let tree = FakeTree::new("process-tree-fields");
    write_namespaces(&tree);
    tree.write(
        "/proc/9/stat",
        "9 (trainer) R 4 9 9 0 -1 4194560 100 0 0 0 30 20 7 5 20 0 12 0 900 1234567 64 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0\n",
    );
    let mut adapter = adapter(&tree, 256);
    let records = records(&read_all(&mut adapter, ADAPTER, "process", 1));
    let record = by_pid(&records, 9);
    assert_eq!(u64_value(record), 50);
    let stat = stat_of(record);
    assert_eq!(stat.ppid, Some(4));
    assert_eq!(stat.state, Some('R'));
    assert_eq!(stat.threads, Some(12));
    assert_eq!(stat.reaped_ticks, Some(12), "cutime + cstime");
}

/// A reaped children's time the kernel line does not hold as a number is
/// left out of the tags, never sent as zero.
#[test]
fn an_unparsable_reaped_time_is_left_out_not_zero() {
    let tree = FakeTree::new("process-reaped-unparsable");
    write_namespaces(&tree);
    tree.write(
        "/proc/9/stat",
        "9 (trainer) R 4 9 9 0 -1 4194560 100 0 0 0 30 20 x 5 20 0 12 0 900 1234567 64 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0\n",
    );
    let mut adapter = adapter(&tree, 256);
    let records = records(&read_all(&mut adapter, ADAPTER, "process", 1));
    let record = by_pid(&records, 9);
    assert_eq!(u64_value(record), 50, "the ticks stay");
    let stat = stat_of(record);
    assert_eq!(stat.reaped_ticks, None);
    assert_eq!(stat.ppid, Some(4));
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

/// A stat line of process `pid` whose parent is `parent`.
fn write_child(tree: &FakeTree, pid: u32, comm: &str, parent: u32, start: u64) {
    let stat = format!(
        "{pid} ({comm}) S {parent} {pid} {pid} 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 {start} 1234567 10 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0\n"
    );
    tree.write(&format!("/proc/{pid}/stat"), &stat);
}

/// D-65: an interpreter's process is named by what it runs, never by an
/// option value; a child whose command line names no program takes its
/// parent's name; a generic name takes its project folder. D-73: the
/// record's channel also carries the command line after the separator;
/// the command line is read once per process.
#[test]
fn processes_are_named_by_what_they_run_and_command_lines_are_read_once() {
    let tree = FakeTree::new("process-names");
    write_namespaces(&tree);
    write_process(&tree, 1, "systemd", 10, 5, 1, "100");
    write_process(&tree, 50, "python", 1, 1, 500, "10");
    tree.write(
        "/proc/50/cmdline",
        "/venv/bin/python\0-u\0-m\0acme_llm.training.train\0--token\0SECRET\0",
    );
    write_child(&tree, 51, "python", 50, 600);
    tree.write(
        "/proc/51/cmdline",
        "python\0-c\0from multiprocessing.spawn import spawn_main\0--multiprocessing-fork\0",
    );
    write_process(&tree, 60, "pt_main_thread", 1, 1, 700, "10");
    tree.write("/proc/60/cmdline", "python3\0main.py\0--listen\0");
    tree.symlink("/proc/60/cwd", "/home/u/ComfyUI");
    let files = tree.faulty();
    let mut adapter = ProcfsAdapter::with_host_constants(
        FakeClock::new(1_000_000_000),
        host_id("test-host"),
        files.clone(),
        AdapterOptions::default(),
        HostConstants {
            clk_tck: Some(100),
            page_size: Some(4096),
        },
    );
    for cycle in 1..=2 {
        let records = records(&read_all(&mut adapter, ADAPTER, "process", cycle));
        let name = |pid: u32| name_of(by_pid(&records, pid));
        assert_eq!(
            [name(1), name(50), name(51), name(60)],
            [
                "systemd",
                "acme_llm.train",
                "acme_llm.train",
                "ComfyUI/main"
            ]
        );
        // No name holds an option value; the command line is shown as it is.
        assert!(!name(50).contains("SECRET"));
        assert_eq!(
            by_pid(&records, 50).provenance.channel,
            format!(
                "acme_llm.train{}/venv/bin/python -u -m acme_llm.training.train --token SECRET",
                dgxtop_core::naming::PROCESS_COMMAND_SEPARATOR
            )
        );
        assert_eq!(
            by_pid(&records, 1).provenance.channel,
            "systemd",
            "no command line, no separator"
        );
    }
    assert_eq!(
        files.reads_of("/proc/50/cmdline"),
        1,
        "read once per process"
    );
    assert_eq!(files.reads_of("/proc/60/cwd"), 1);
    assert_eq!(
        files.reads_of("/proc/1/cmdline"),
        1,
        "every command line once"
    );
    // A program that sets its title changes its comm: its name is read again.
    write_process(&tree, 50, "VLLM::Worker", 1, 1, 500, "10");
    tree.write("/proc/50/cmdline", "VLLM::Worker\0");
    let records = records(&read_all(&mut adapter, ADAPTER, "process", 3));
    assert_eq!(name_of(by_pid(&records, 50)), "VLLM::Worker");
}

#[test]
fn paging_is_consistent_within_a_cycle_and_resnapshots_the_next_cycle() {
    let tree = process_tree("process-paging");
    write_process(&tree, 2, "a", 1, 1, 3, "1");
    write_process(&tree, 3, "b", 1, 1, 4, "1");
    write_process(&tree, 4, "c", 1, 1, 5, "1");
    write_process(&tree, 5, "d", 1, 1, 6, "1");
    write_process(&tree, 6, "e", 1, 1, 7, "1");
    write_process(&tree, 8, "f", 1, 1, 8, "1");
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
    assert_eq!(next.len(), 10);
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
    assert_eq!(records.len(), 3);
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
    let ticks = by_pid(&records, 70);
    assert_eq!(ticks.read_status, ReadStatus::Error);
    assert_eq!(ticks.value, None);
    assert_eq!(ticks.reason_codes[0], "CPU_TICKS_OVERFLOW");
    assert!(
        ticks
            .reason_codes
            .contains(&"RSS_BYTES_OVERFLOW".to_owned())
    );
    let stat = stat_of(ticks);
    assert_eq!(stat.rss_bytes, None, "an overflow is no value");
    assert_eq!(stat.ppid, Some(1), "the rest of the stat stays");
    ticks.validate().unwrap();
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
    let record = by_pid(&records, 42);
    assert_eq!(record.read_status, ReadStatus::Ok, "the ticks still count");
    assert_eq!(stat_of(record).rss_bytes, None);
    assert!(
        record
            .reason_codes
            .contains(&"PAGE_SIZE_UNKNOWN".to_owned())
    );
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
    assert_eq!(batches.len(), 16);
    let records = records(&batches);
    assert_eq!(records.len(), 4096);
    assert!(
        records
            .iter()
            .all(|record| record.provenance.device_path != "/proc/4097")
    );
    let last = batches.last().unwrap();
    assert_eq!(last.error_code.as_deref(), Some("PROCESS_SCAN_TRUNCATED"));
    assert_eq!(last.status, BatchStatus::Partial);
    assert!(batches[..15].iter().all(|batch| batch.continues()));
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
    let scanned = records(&batches).len();
    assert!(scanned > 0 && scanned < 300, "scanned {scanned}");
}
