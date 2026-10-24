//! Procfs adapter: `cpu`, `memory` and `disk` groups against fake trees.

#[path = "fixtures/adapters_a/support.rs"]
mod support;

use std::sync::Arc;

use dgxtop_adapters::common::SystemFiles;
use dgxtop_adapters::procfs::{HostConstants, ProcfsAdapter, manifest_with};
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::OriginRelation;
use dgxtop_core::metric::ids;
use dgxtop_core::observation::{BatchStatus, Isolation, ObservationBatch};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{
    Availability, Namespace, Quality, RawKind, ReadStatus, SampleKind, Support,
};
use dgxtop_core::testing::{FakeClock, host_id};
use support::*;

const ADAPTER: &str = "procfs";
const DISCOVERY_NS: u64 = 1_000_000_000;

fn procfs_with(clock: &Arc<FakeClock>, files: Arc<dyn SystemFiles>) -> ProcfsAdapter {
    ProcfsAdapter::with_host_constants(
        clock.clone(),
        host_id("test-host"),
        files,
        AdapterOptions::default(),
        HostConstants {
            clk_tck: Some(100),
            page_size: Some(4096),
        },
    )
}

fn standard() -> (FakeTree, Arc<FakeClock>, ProcfsAdapter, ParentCatalog) {
    let tree = FakeTree::new("procfs");
    standard_procfs(&tree);
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, tree.files());
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, ADAPTER);
    clock.advance_ms(1_000);
    (tree, clock, adapter, parent)
}

fn read_group(
    adapter: &mut ProcfsAdapter,
    group: &str,
    cycle: u64,
) -> Vec<dgxtop_core::observation::Observation> {
    records(&read_all(adapter, ADAPTER, group, cycle))
}

#[test]
fn stat_sources_follow_the_naming_contract() {
    let (_tree, _clock, _adapter, parent) = standard();
    let source = parent.source("procfs:cpu3:user");
    let key = source.metric.as_ref().unwrap();
    assert_eq!(key.entity_id.as_str(), "cpu.core.3");
    assert_eq!(key.scope.as_str(), "core:3");
    assert_eq!(key.metric_id.as_str(), ids::CPU_TIME_USER);
    assert_eq!(key.host_id.as_str(), "test-host");
    assert_eq!(key.boot_id.as_str(), "fake-boot");
    assert_eq!(source.native_unit.as_str(), "ticks");
    assert_eq!(source.raw_kind, RawKind::Counter);
    assert_eq!(source.sample_kind, SampleKind::Measured);
    assert_eq!(source.read_group.as_str(), "cpu");
    assert_eq!(source.origin.relation, OriginRelation::Confirmed);
    assert_eq!(
        source.origin.origin_id.as_ref().unwrap().as_str(),
        "procfs:/proc/stat:cpu3.user"
    );
    assert_eq!(source.profile_id.as_str(), "linux-procfs");
    assert_eq!(source.profile_revision, 1);
    assert_eq!(source.profile_rank, 0);
    let aggregate = parent.source("procfs:cpu:steal");
    assert_eq!(aggregate.metric.as_ref().unwrap().scope.as_str(), "host");
    assert!(
        parent.sources.values().all(|source| !source
            .metric
            .as_ref()
            .unwrap()
            .metric_id
            .as_str()
            .contains("guest")),
        "guest time is already inside user/nice"
    );
    let load = parent.source("procfs:loadavg:1m");
    assert_eq!(load.update_interval_ms, Some(5_000));
    assert_eq!(load.metric.as_ref().unwrap().entity_id.as_str(), "host");
    assert_eq!(
        parent.source("procfs:loadavg:running").update_interval_ms,
        None
    );
}

#[test]
fn cpufreq_sources_exist_only_where_supported() {
    let tree = FakeTree::new("procfs-cpufreq");
    standard_procfs(&tree);
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, tree.files());
    let mut parent = ParentCatalog::default();
    let pages = parent.sync(&mut adapter, ADAPTER);
    let current = parent.source("procfs:cpu0:scaling_cur_freq");
    assert_eq!(current.native_unit.as_str(), "kHz");
    assert_eq!(current.raw_kind, RawKind::Gauge);
    let maximum = parent.source("procfs:cpu0:cpuinfo_max_freq");
    assert_eq!(maximum.raw_kind, RawKind::Static);
    assert!(!parent.sources.contains_key("procfs:cpu3:scaling_cur_freq"));
    let unsupported = pages[0]
        .capabilities
        .iter()
        .find(|capability| {
            capability.capability_id == ids::CPU_FREQUENCY_CURRENT
                && capability.entity_id.as_ref().map(|id| id.as_str()) == Some("cpu.core.3")
        })
        .unwrap();
    assert_eq!(unsupported.support, Support::Unsupported);
    assert_eq!(unsupported.reason.as_deref(), Some("CPUFREQ_NOT_PRESENT"));
}

/// Where the kernel offers `cpuinfo_avg_freq` (D-67), a CPU's current
/// frequency is what it ran at, from its hardware counters; the cppc
/// driver's `scaling_cur_freq` under the performance governor is the
/// requested maximum (the DGX Spark showed 3900 MHz at any load). An idle
/// CPU has no recent counter sample (`EAGAIN`): it reports no current
/// frequency that cycle, and the batch stays whole.
#[test]
fn the_current_frequency_is_the_averaged_one_where_the_kernel_offers_it() {
    let tree = FakeTree::new("procfs-avgfreq");
    standard_procfs(&tree);
    let average = |cpu: u32| format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_avg_freq");
    tree.write(&average(0), "2563945\n");
    tree.write(&average(1), "3842871\n");
    let files = tree.faulty();
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, files.clone());
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, ADAPTER);
    assert!(parent.sources.contains_key("procfs:cpu0:cpuinfo_avg_freq"));
    assert!(!parent.sources.contains_key("procfs:cpu0:scaling_cur_freq"));
    clock.advance_ms(1_000);
    let first = read_group(&mut adapter, "cpu", 1);
    let current = find(&first, ids::CPU_FREQUENCY_CURRENT, "cpu.core.0");
    assert_eq!(u64_value(current), 2_563_945);
    assert_eq!(current.quality, Quality::Usable);
    assert!(
        current
            .reason_codes
            .contains(&"CPUINFO_AVG_FREQ_COUNTERS".to_owned()),
        "{:?}",
        current.reason_codes
    );
    files.inject(&average(1), 11);
    clock.advance_ms(1_000);
    let batches = read_all(&mut adapter, ADAPTER, "cpu", 2);
    assert_eq!(batches[0].status, BatchStatus::Ok);
    let idle = |records: &[dgxtop_core::observation::Observation]| {
        !records.iter().any(|record| {
            record.key.metric_id.as_str() == ids::CPU_FREQUENCY_CURRENT
                && record.key.entity_id.as_str() == "cpu.core.1"
        })
    };
    assert!(idle(&records(&batches)), "an idle CPU reports none");
    // Running again, it reports again: an idle spell is no failure that
    // defers the next read.
    files.clear(&average(1));
    clock.advance_ms(1_000);
    let running = read_group(&mut adapter, "cpu", 3);
    assert!(!idle(&running));
    assert_eq!(
        u64_value(find(&running, ids::CPU_FREQUENCY_CURRENT, "cpu.core.1")),
        3_842_871
    );
}

#[test]
fn cpu_read_emits_counters_load_and_frequencies() {
    let (_tree, clock, mut adapter, _parent) = standard();
    let read_started = clock.now_boot_ns();
    let batches = read_all(&mut adapter, ADAPTER, "cpu", 1);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].status, BatchStatus::Ok);
    let records = records(&batches);
    assert_eq!(records.len(), 41);
    assert_eq!(u64_value(find(&records, ids::CPU_TIME_USER, "cpu")), 1000);
    assert_eq!(
        u64_value(find(&records, ids::CPU_TIME_USER, "cpu.core.3")),
        300
    );
    assert_eq!(
        u64_value(find(&records, ids::CPU_TIME_STEAL, "cpu.core.0")),
        1
    );
    assert_eq!(
        find(&records, ids::CPU_LOAD_1M, "host")
            .value
            .unwrap()
            .to_f64_lossy(),
        0.5
    );
    assert_eq!(
        find(&records, ids::CPU_LOAD_15M, "host")
            .value
            .unwrap()
            .to_f64_lossy(),
        1.25
    );
    assert_eq!(u64_value(find(&records, ids::CPU_TASKS_RUNNING, "host")), 3);
    assert_eq!(
        u64_value(find(&records, ids::CPU_TASKS_TOTAL, "host")),
        1591
    );
    let current = find(&records, ids::CPU_FREQUENCY_CURRENT, "cpu.core.0");
    assert_eq!(u64_value(current), 2_400_000);
    assert_eq!(current.quality, Quality::Usable);
    assert!(
        current
            .reason_codes
            .contains(&"SCALING_CUR_FREQ_DRIVER_DEFINED".to_owned())
    );
    let maximum = find(&records, ids::CPU_FREQUENCY_MAXIMUM, "cpu.core.1");
    assert_eq!(u64_value(maximum), 3_500_000);
    for record in &records {
        record.validate().unwrap();
        assert_eq!(record.device_sample_boot_ns, None);
        assert_eq!(record.sample_kind, SampleKind::Measured);
        assert_eq!(record.read_status, ReadStatus::Ok);
        if record.key.metric_id.as_str().starts_with("cpu.time") {
            assert_eq!(record.quality, Quality::Verified);
            assert!(record.read_started_boot_ns >= read_started);
        }
    }
}

#[test]
fn the_cached_maximum_is_served_with_each_reads_time_and_never_reread() {
    // A live overview showed `max stale` a minute after start: the cached
    // maximum kept its discovery read time and outlived the collector's
    // static deadline (three discovery periods). Like NVML's metadata cache,
    // every read serves the cached value with its own time, and the file is
    // not read again while the CPU's generation stays.
    let (tree, clock, mut adapter, _parent) = standard();
    tree.write(
        "/sys/devices/system/cpu/cpu1/cpufreq/cpuinfo_max_freq",
        "9999999\n",
    );
    clock.advance_ms(60_000);
    let read_started = clock.now_boot_ns();
    let records = read_group(&mut adapter, "cpu", 1);
    let maximum = find(&records, ids::CPU_FREQUENCY_MAXIMUM, "cpu.core.1");
    assert_eq!(
        u64_value(maximum),
        3_500_000,
        "served from the generation cache, not re-read"
    );
    assert!(
        maximum.read_started_boot_ns >= read_started,
        "read {} before this read started at {read_started}",
        maximum.read_started_boot_ns
    );
    assert!(maximum.read_finished_boot_ns >= maximum.read_started_boot_ns);
    assert_eq!(maximum.quality, Quality::Verified);
}

#[test]
fn missing_columns_are_unsupported_and_garbage_is_an_error() {
    let tree = FakeTree::new("procfs-columns");
    standard_procfs(&tree);
    tree.write(
        "/proc/stat",
        "cpu 1 2 3 4\ncpu0 1 2 x 4 5 6 7 8\nnot-a-cpu 1 2\n",
    );
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, tree.files());
    ParentCatalog::default().sync(&mut adapter, ADAPTER);
    let records = read_group(&mut adapter, "cpu", 1);
    let iowait = find(&records, ids::CPU_TIME_IOWAIT, "cpu");
    assert_eq!(iowait.read_status, ReadStatus::Unsupported);
    assert_eq!(iowait.value, None);
    assert_eq!(iowait.reason_codes, vec!["FIELD_ABSENT".to_owned()]);
    let system = find(&records, ids::CPU_TIME_SYSTEM, "cpu.core.0");
    assert_eq!(system.read_status, ReadStatus::Error);
    assert_eq!(system.value, None);
    assert_eq!(system.reason_codes, vec!["PARSE_ERROR".to_owned()]);
    assert_eq!(u64_value(find(&records, ids::CPU_TIME_IDLE, "cpu")), 4);
}

#[test]
fn u64_max_counter_survives_the_wire() {
    let tree = FakeTree::new("procfs-u64");
    standard_procfs(&tree);
    tree.write("/proc/stat", "cpu 18446744073709551615 0 0 0 0 0 0 0\n");
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, tree.files());
    ParentCatalog::default().sync(&mut adapter, ADAPTER);
    let batches = read_all(&mut adapter, ADAPTER, "cpu", 1);
    let json = serde_json::to_string(&batches[0]).unwrap();
    let back: ObservationBatch = serde_json::from_str(&json).unwrap();
    assert_eq!(
        u64_value(find(&back.records, ids::CPU_TIME_USER, "cpu")),
        u64::MAX
    );
}

#[test]
fn offline_cpu_only_affects_itself() {
    let (tree, _clock, mut adapter, mut parent) = standard();
    tree.write(
        "/proc/stat",
        &STAT.replace("cpu3 300 3 150 15000 10 0 2 1 0 0\n", ""),
    );
    let records = read_group(&mut adapter, "cpu", 1);
    for record in records
        .iter()
        .filter(|record| record.key.entity_id.as_str() == "cpu.core.3")
    {
        assert_eq!(record.read_status, ReadStatus::Removed);
        assert_eq!(record.value, None);
        assert_eq!(record.reason_codes, vec!["CPU_NOT_ONLINE".to_owned()]);
    }
    assert!(
        records
            .iter()
            .filter(|record| record.key.entity_id.as_str() != "cpu.core.3")
            .all(|record| record.read_status == ReadStatus::Ok)
    );
    let pages = parent.sync(&mut adapter, ADAPTER);
    let last = pages.last().unwrap();
    let removed_entities: Vec<&str> = last.removed_entities.iter().map(|id| id.as_str()).collect();
    assert_eq!(removed_entities, vec!["cpu.core.3"]);
    assert_eq!(last.removed_sources.len(), 8);
    assert!(
        last.removed_sources
            .iter()
            .all(|id| id.as_str().starts_with("procfs:cpu3:"))
    );
    assert_eq!(last.generation, 2);
}

#[test]
fn memory_fields_are_native_kib_and_missing_available_is_unsupported() {
    let (tree, _clock, mut adapter, mut parent) = standard();
    let records = read_group(&mut adapter, "memory", 1);
    assert_eq!(records.len(), 6);
    let total = find(&records, ids::MEMORY_TOTAL, "memory");
    assert_eq!(u64_value(total), 32_654_612);
    assert_eq!(total.native_unit.as_str(), "KiB");
    assert_eq!(total.key.scope.as_str(), "system");
    assert_eq!(
        u64_value(find(&records, ids::MEMORY_AVAILABLE, "memory")),
        20_000_000
    );
    assert_eq!(
        u64_value(find(&records, ids::MEMORY_SWAP_FREE_RAW, "memory")),
        7_000_000
    );

    tree.write(
        "/proc/meminfo",
        &MEMINFO.replace("MemAvailable:   20000000 kB\n", ""),
    );
    let pages = parent.sync(&mut adapter, ADAPTER);
    assert!(pages[0].capabilities.iter().any(|capability| {
        capability.capability_id == ids::MEMORY_AVAILABLE
            && capability.support == Support::Unsupported
            && capability.reason.as_deref() == Some("FIELD_ABSENT")
    }));
    let records = read_group(&mut adapter, "memory", 2);
    let available = find(&records, ids::MEMORY_AVAILABLE, "memory");
    assert_eq!(available.read_status, ReadStatus::Unsupported);
    assert_eq!(available.value, None, "never computed from MemFree");
    assert!(
        records
            .iter()
            .all(|record| record.key.metric_id.as_str() != "memory.free"),
        "MemFree is not read"
    );
}

#[test]
fn unreadable_file_permission_bits_yield_permission_denied() {
    if running_as_root() {
        eprintln!("skipped: permission bits are not enforced for euid 0");
        return;
    }
    let (tree, _clock, mut adapter, _parent) = standard();
    tree.chmod("/proc/meminfo", 0o000);
    let records = read_group(&mut adapter, "memory", 1);
    assert!(
        records
            .iter()
            .all(|record| record.read_status == ReadStatus::PermissionDenied
                && record.value.is_none())
    );
    tree.chmod("/proc/meminfo", 0o644);
}

#[test]
fn diskstats_scopes_partitions_and_skips_loop_and_ram() {
    let (_tree, _clock, mut adapter, parent) = standard();
    assert!(!parent.entities.contains_key("blockdev:7:0"));
    assert!(!parent.entities.contains_key("blockdev:1:0"));
    let partition = parent.entity("blockdev:259:1");
    assert_eq!(partition.display_name, "nvme0n1p1");
    assert_eq!(partition.attribute("major_minor"), Some("259:1"));
    assert_eq!(partition.attribute("device"), Some("nvme0n1p1"));
    let source = parent.source("procfs:blockdev.259.1:sectors_read");
    assert_eq!(source.metric.as_ref().unwrap().scope.as_str(), "partition");
    let device = parent.source("procfs:blockdev.259.0:sectors_read");
    assert_eq!(device.metric.as_ref().unwrap().scope.as_str(), "device");
    assert_eq!(device.native_unit.as_str(), "sectors_512");
    let records = read_group(&mut adapter, "disk", 1);
    assert_eq!(records.len(), 18);
    assert_eq!(
        u64_value(find(&records, ids::DISK_READS_COMPLETED, "blockdev:259:0")),
        347_806
    );
    assert_eq!(
        u64_value(find(&records, ids::DISK_SECTORS_READ, "blockdev:259:0")),
        11_509_086
    );
    let read_time = find(&records, ids::DISK_READ_TIME, "blockdev:259:0");
    assert_eq!(u64_value(read_time), 31_169);
    assert_eq!(read_time.native_unit.as_str(), "milliseconds");
    assert_eq!(
        u64_value(find(&records, ids::DISK_WRITES_COMPLETED, "blockdev:259:0")),
        554_347
    );
    assert_eq!(
        u64_value(find(&records, ids::DISK_SECTORS_WRITTEN, "blockdev:259:0")),
        82_536_402
    );
    assert_eq!(
        u64_value(find(&records, ids::DISK_WRITE_TIME, "blockdev:259:0")),
        2_694_532
    );
}

/// A partition hangs below the disk holding it (the `dev` of its sysfs
/// parent directory), so the disk's own facts such as its temperature reach
/// it (D-92); a disk, or a partition whose disk cannot be read, hangs below
/// the host.
#[test]
fn a_partition_hangs_below_the_disk_holding_it() {
    let tree = FakeTree::new("procfs-partition-parent");
    standard_procfs(&tree);
    let disk = "/sys/devices/pci0000:00/0000:00:1d.0/0000:02:00.0/nvme/nvme0/nvme0n1";
    tree.write(&format!("{disk}/dev"), "259:0\n");
    tree.write(&format!("{disk}/nvme0n1p1/dev"), "259:1\n");
    tree.write(&format!("{disk}/nvme0n1p1/partition"), "1\n");
    for (name, target) in [
        ("nvme0n1", disk.to_owned()),
        ("nvme0n1p1", format!("{disk}/nvme0n1p1")),
    ] {
        let link = format!("/sys/class/block/{name}");
        tree.remove(&link);
        tree.symlink(&link, tree.path(&target).to_str().unwrap());
    }
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, tree.files());
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, ADAPTER);
    let parent_of = |entity: &str| {
        parent
            .entity(entity)
            .parent_id
            .as_ref()
            .map(|id| id.as_str().to_owned())
    };
    assert_eq!(
        parent_of("blockdev:259:1").as_deref(),
        Some("blockdev:259:0")
    );
    assert_eq!(parent_of("blockdev:259:0").as_deref(), Some("host"));
    assert_eq!(parent_of("blockdev:8:0").as_deref(), Some("host"));
    assert_eq!(
        parent
            .source("procfs:blockdev.259.1:sectors_read")
            .metric
            .as_ref()
            .unwrap()
            .scope
            .as_str(),
        "partition"
    );

    // The standard tree's partition is no link into a disk: its disk is
    // unknown and it stays below the host.
    let (_tree, _clock, _adapter, standard) = standard();
    assert_eq!(
        standard
            .entity("blockdev:259:1")
            .parent_id
            .as_ref()
            .map(|id| id.as_str()),
        Some("host")
    );
}

/// Each block device names its layer (D-124), and a device stacked on
/// exactly one other hangs below it: `dm-1` on `dm-0` on the partition
/// `nvme0n1p1`, so the drive's facts reach the volume. A RAID on two disks
/// hangs below the host, zram is virtual, and a device sysfs does not list
/// names no layer rather than passing for a disk.
#[test]
fn block_devices_name_their_layer_and_hang_below_what_they_stack_on() {
    let tree = FakeTree::new("procfs-block-layers");
    standard_procfs(&tree);
    let stacked = "\
 254       0 dm-0 10 0 80 1 20 0 160 2 0 3 3 0 0 0 0 0 0
 254       1 dm-1 10 0 80 1 20 0 160 2 0 3 3 0 0 0 0 0 0
   9       0 md0 10 0 80 1 20 0 160 2 0 3 3 0 0 0 0 0 0
 252       0 zram0 10 0 80 1 20 0 160 2 0 3 3 0 0 0 0 0 0
  65     144 sdz 10 0 80 1 20 0 160 2 0 3 3 0 0 0 0 0 0
";
    tree.write("/proc/diskstats", &format!("{DISKSTATS}{stacked}"));
    let disk = "/sys/devices/pci0000:00/0000:00:1d.0/0000:02:00.0/nvme/nvme0/nvme0n1";
    let partition = format!("{disk}/nvme0n1p1");
    let virtual_block = "/sys/devices/virtual/block";
    tree.write(&format!("{disk}/dev"), "259:0\n");
    tree.write(&format!("{partition}/dev"), "259:1\n");
    tree.write(&format!("{partition}/partition"), "1\n");
    for (name, dev) in [
        ("dm-0", "254:0"),
        ("dm-1", "254:1"),
        ("md0", "9:0"),
        ("zram0", "252:0"),
    ] {
        tree.write(&format!("{virtual_block}/{name}/dev"), &format!("{dev}\n"));
    }
    let slave = |device: &str, name: &str, target: &str| {
        let link = format!("{virtual_block}/{device}/slaves/{name}");
        tree.symlink(&link, tree.path(target).to_str().unwrap());
    };
    tree.mkdir(&format!("{virtual_block}/dm-0/slaves"));
    tree.mkdir(&format!("{virtual_block}/dm-1/slaves"));
    tree.mkdir(&format!("{virtual_block}/md0/slaves"));
    slave("dm-0", "nvme0n1p1", &partition);
    slave("dm-1", "dm-0", &format!("{virtual_block}/dm-0"));
    slave("md0", "nvme0n1", disk);
    slave("md0", "sda", "/sys/class/block/sda");
    let class_links = [
        ("nvme0n1", disk.to_owned()),
        ("nvme0n1p1", partition.clone()),
        ("dm-0", format!("{virtual_block}/dm-0")),
        ("dm-1", format!("{virtual_block}/dm-1")),
        ("md0", format!("{virtual_block}/md0")),
        ("zram0", format!("{virtual_block}/zram0")),
    ];
    for (name, target) in class_links {
        let link = format!("/sys/class/block/{name}");
        tree.remove(&link);
        tree.symlink(&link, tree.path(&target).to_str().unwrap());
    }
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, tree.files());
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, ADAPTER);
    let placement = |entity: &str| {
        let record = parent.entity(entity);
        (
            record.attribute("block_layer").map(str::to_owned),
            record.parent_id.as_ref().map(|id| id.as_str().to_owned()),
        )
    };
    let expected = [
        ("blockdev:259:0", Some("disk"), "host"),
        ("blockdev:259:1", Some("partition"), "blockdev:259:0"),
        ("blockdev:8:0", Some("disk"), "host"),
        ("blockdev:254:0", Some("stacked"), "blockdev:259:1"),
        ("blockdev:254:1", Some("stacked"), "blockdev:254:0"),
        ("blockdev:9:0", Some("stacked"), "host"),
        ("blockdev:252:0", Some("virtual"), "host"),
        ("blockdev:65:144", None, "host"),
    ];
    for (entity, layer, parent_id) in expected {
        assert_eq!(
            placement(entity),
            (layer.map(str::to_owned), Some(parent_id.to_owned())),
            "{entity}"
        );
    }
    assert_eq!(
        parent
            .source("procfs:blockdev.254.1:sectors_read")
            .metric
            .as_ref()
            .unwrap()
            .scope
            .as_str(),
        "device",
        "a stacked device keeps the device scope"
    );
}

#[test]
fn renamed_block_device_is_a_new_generation() {
    let (tree, _clock, mut adapter, mut parent) = standard();
    let old_generation = parent.entity("blockdev:8:0").generation;
    tree.write("/proc/diskstats", &DISKSTATS.replace(" sda ", " sdb "));
    let records = read_group(&mut adapter, "disk", 1);
    let stale = find(&records, ids::DISK_READS_COMPLETED, "blockdev:8:0");
    assert_eq!(stale.read_status, ReadStatus::Removed);
    assert_eq!(stale.reason_codes, vec!["DEVICE_RECREATED".to_owned()]);
    parent.sync(&mut adapter, ADAPTER);
    let renamed = parent.entity("blockdev:8:0");
    assert_eq!(renamed.display_name, "sdb");
    assert!(renamed.generation > old_generation);
    assert_eq!(
        parent
            .source("procfs:blockdev.8.0:reads_completed")
            .generation,
        renamed.generation
    );
    let records = read_group(&mut adapter, "disk", 2);
    let fresh = find(&records, ids::DISK_READS_COMPLETED, "blockdev:8:0");
    assert_eq!(fresh.provenance.generation, renamed.generation);
    assert_eq!(u64_value(fresh), 59_889);
}

#[test]
fn removed_disk_only_removes_its_own_sources() {
    let (tree, _clock, mut adapter, mut parent) = standard();
    tree.write(
        "/proc/diskstats",
        &DISKSTATS.replace(
            "   8       0 sda 59889 0 483728 2907 0 0 0 0 0 2816 2907 0 0 0 0 0 0\n",
            "",
        ),
    );
    let records = read_group(&mut adapter, "disk", 1);
    let gone = records
        .iter()
        .filter(|record| record.key.entity_id.as_str() == "blockdev:8:0")
        .collect::<Vec<_>>();
    assert_eq!(gone.len(), 6);
    assert!(
        gone.iter()
            .all(|record| record.read_status == ReadStatus::Removed)
    );
    let pages = parent.sync(&mut adapter, ADAPTER);
    let last = pages.last().unwrap();
    assert_eq!(last.removed_entities.len(), 1);
    assert_eq!(last.removed_entities[0].as_str(), "blockdev:8:0");
    assert_eq!(last.removed_sources.len(), 6);
    assert!(parent.entities.contains_key("blockdev:259:0"));
}

#[test]
fn unreadable_enumeration_file_keeps_previous_devices() {
    let (tree, _clock, mut adapter, mut parent) = standard();
    tree.remove("/proc/diskstats");
    tree.remove("/proc/meminfo");
    let pages = parent.sync(&mut adapter, ADAPTER);
    let last = pages.last().unwrap();
    assert!(
        last.removed_entities.is_empty(),
        "a failed read is not a disappearance"
    );
    assert!(last.removed_sources.is_empty());
    let code = last.error_code.as_deref().unwrap();
    assert!(
        code.contains("DISKSTATS_UNREADABLE:SOURCE_REMOVED"),
        "{code}"
    );
    assert!(code.contains("MEMINFO_UNREADABLE"), "{code}");
    assert!(parent.entities.contains_key("blockdev:8:0"));
    let records = read_group(&mut adapter, "disk", 1);
    assert!(
        records
            .iter()
            .all(|record| record.read_status == ReadStatus::Removed && record.value.is_none())
    );
}

#[test]
fn missing_proc_stat_makes_the_adapter_unavailable_but_keeps_other_groups() {
    let tree = FakeTree::new("procfs-nostat");
    standard_procfs(&tree);
    tree.remove("/proc/stat");
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, tree.files());
    let mut parent = ParentCatalog::default();
    let pages = parent.sync(&mut adapter, ADAPTER);
    assert_eq!(pages[0].availability, Some(Availability::Unavailable));
    assert!(
        pages[0]
            .error_code
            .as_deref()
            .unwrap()
            .contains("PROC_STAT_UNREADABLE")
    );
    assert!(parent.entities.contains_key("memory"));
    assert!(!parent.entities.contains_key("cpu"));
}

#[test]
fn guards_reject_unservable_plans() {
    let tree = FakeTree::new("procfs-guards");
    standard_procfs(&tree);
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, tree.files());
    let failed_code = |batch: &ObservationBatch| {
        assert_eq!(batch.status, BatchStatus::Failed);
        assert!(batch.records.is_empty());
        batch.error_code.clone().unwrap()
    };
    let before = adapter.read(&read_plan(ADAPTER, "cpu", 1, 0, Vec::new()));
    assert_eq!(failed_code(&before), "CATALOG_NOT_DISCOVERED");
    ParentCatalog::default().sync(&mut adapter, ADAPTER);
    let wrong = adapter.read(&read_plan("network", "cpu", 1, 0, Vec::new()));
    assert_eq!(failed_code(&wrong), "ADAPTER_ID_MISMATCH");
    let unknown = adapter.read(&read_plan(ADAPTER, "fast", 1, 0, Vec::new()));
    assert_eq!(failed_code(&unknown), "UNKNOWN_READ_GROUP");
    let mut late = read_plan(ADAPTER, "cpu", 1, 0, Vec::new());
    late.deadline_boot_ns = DISCOVERY_NS - 1;
    assert_eq!(failed_code(&adapter.read(&late)), "DEADLINE_EXPIRED");
    let missing = adapter.read(&read_plan(ADAPTER, "cpu", 9, 1, Vec::new()));
    assert_eq!(failed_code(&missing), "PAGE_SNAPSHOT_MISSING");
    adapter.shutdown();
    let after = adapter.read(&read_plan(ADAPTER, "cpu", 2, 0, Vec::new()));
    assert_eq!(failed_code(&after), "ADAPTER_SHUT_DOWN");
    let delta = adapter.discover(&discover_request(ADAPTER, 1, 0));
    assert_eq!(delta.error_code.as_deref(), Some("ADAPTER_SHUT_DOWN"));
    assert!(!delta.complete);
    assert!(delta.removed_sources.is_empty());
}

#[test]
fn explicit_slots_read_only_those_sources_and_flag_unknown_ones() {
    let (_tree, _clock, mut adapter, parent) = standard();
    let slot = parent.source("procfs:cpu1:idle").slot;
    let plan = read_plan(ADAPTER, "cpu", 1, 0, vec![slot, 999_999]);
    let batch = adapter.read(&plan);
    batch.validate_against(&plan, 256).unwrap();
    assert_eq!(batch.records.len(), 1);
    assert_eq!(
        batch.records[0].provenance.source_id.as_str(),
        "procfs:cpu1:idle"
    );
    assert_eq!(u64_value(&batch.records[0]), 30_000);
    assert_eq!(batch.error_code.as_deref(), Some("UNKNOWN_SOURCE_SLOT"));
    assert_eq!(batch.status, BatchStatus::Partial);
}

#[test]
fn manifest_matches_the_contract() {
    let options = AdapterOptions {
        unsupported_probe_ms: 60_000,
        max_sources: 500,
        page_size: 256,
    };
    let manifest = manifest_with(&options);
    manifest.validate().unwrap();
    assert_eq!(manifest.adapter_id.as_str(), "procfs");
    assert_eq!(manifest.isolation, Isolation::PersistentProcess);
    assert_eq!(manifest.operations.len(), 3);
    assert_eq!(manifest.max_inflight, 1);
    assert_eq!(manifest.runtime_library, None);
    assert_eq!(manifest.minimum_sample_ms, 100);
    assert_eq!(manifest.source_limit, 500);
    assert_eq!(manifest.sample_namespace, Namespace::Live);
}

#[test]
fn denied_proc_stat_reports_permission_denied_availability() {
    let tree = FakeTree::new("procfs-stat-denied");
    standard_procfs(&tree);
    let faulty = tree.faulty();
    faulty.inject("/proc/stat", 13);
    let clock = FakeClock::new(DISCOVERY_NS);
    let mut adapter = procfs_with(&clock, faulty.clone());
    let pages = ParentCatalog::default().sync(&mut adapter, ADAPTER);
    assert_eq!(pages[0].availability, Some(Availability::PermissionDenied));
    assert!(
        pages[0]
            .error_code
            .as_deref()
            .unwrap()
            .contains("PROC_STAT_UNREADABLE:PERMISSION_DENIED")
    );
}
