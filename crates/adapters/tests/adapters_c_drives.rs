//! hwmon drive temperatures (D-92) over fake sysfs trees: which block
//! devices an NVMe controller's or a drivetemp disk's sensor measures.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests may unwrap

#[path = "fixtures/adapters_c/support.rs"]
mod support;

use std::io;
use std::sync::Arc;

use dgxtop_adapters::common::SystemFiles;
use dgxtop_adapters::hwmon::HwmonAdapter;
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_core::identity::OriginRelation;
use dgxtop_core::metric::ids;
use dgxtop_core::observation::{BatchStatus, SourceDescriptor};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Quality, ReadStatus};
use support::*;

const ADAPTER: &str = "hwmon";
const NVME0: &str = "pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0";
const NVME0_KEY: &str = "nvme@pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0";

/// A block device directory below `parent` (a `/sys/devices` path) with its
/// `dev` and class link; `partition` marks a partition.
fn block(sysfs: &FakeSysfs, parent: &str, name: &str, dev: &str, partition: bool) -> String {
    let directory = format!("{parent}/{name}");
    sysfs.write(&format!("{directory}/dev"), &format!("{dev}\n"));
    if partition {
        sysfs.write(&format!("{directory}/partition"), "1\n");
    }
    sysfs.symlink(&format!("/sys/class/block/{name}"), &directory);
    directory
}

/// An NVMe controller's hwmon (`Composite`, `Sensor 1`, `Sensor 2`).
fn nvme_sensor(sysfs: &FakeSysfs, index: u32, controller: &str, composite: &str) -> String {
    let hwmon = sysfs.hwmon(index, controller, "nvme");
    for (channel, label, value) in [
        (1, "Composite", composite),
        (2, "Sensor 1", "39850"),
        (3, "Sensor 2", "52850"),
    ] {
        sysfs.write(
            &format!("{hwmon}/temp{channel}_label"),
            &format!("{label}\n"),
        );
        sysfs.write(
            &format!("{hwmon}/temp{channel}_input"),
            &format!("{value}\n"),
        );
    }
    sysfs.write(&format!("{hwmon}/temp1_alarm"), "0\n");
    hwmon
}

fn adapter_over(
    files: Arc<dyn SystemFiles>,
) -> (HwmonAdapter, Arc<dgxtop_core::testing::FakeClock>) {
    let clock = clock();
    let hwmon = HwmonAdapter::new(time(&clock), host(), files, AdapterOptions::default());
    (hwmon, clock)
}

fn catalog_of(
    sysfs: &FakeSysfs,
) -> (
    HwmonAdapter,
    Arc<dgxtop_core::testing::FakeClock>,
    Vec<SourceDescriptor>,
) {
    let (mut hwmon, clock) = adapter_over(sysfs.files());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    (hwmon, clock, sources(&pages))
}

/// `(source id, entity, scope)` of every drive temperature source.
fn drive_keys(catalog: &[SourceDescriptor]) -> Vec<(String, String, String)> {
    catalog
        .iter()
        .map(|descriptor| {
            let key = descriptor.metric.as_ref().unwrap();
            assert_eq!(key.metric_id.as_str(), ids::DISK_TEMPERATURE);
            (
                descriptor.source_id.as_str().to_owned(),
                key.entity_id.as_str().to_owned(),
                key.scope.as_str().to_owned(),
            )
        })
        .collect()
}

fn disk_records(
    hwmon: &mut HwmonAdapter,
    clock: &Arc<dgxtop_core::testing::FakeClock>,
) -> Vec<dgxtop_core::observation::Observation> {
    records(&read_all(
        hwmon,
        ADAPTER,
        "disk",
        1,
        deadline(clock),
        Vec::new(),
    ))
}

/// One NVMe drive (the GB10's layout): `Composite` is the temperature of
/// its namespace disk, in the `disk` read group; its other sensors and its
/// partitions are not sources.
#[test]
fn an_nvme_composite_is_the_temperature_of_its_namespace() {
    let sysfs = FakeSysfs::new("drives-nvme");
    nvme_sensor(&sysfs, 1, NVME0, "41850");
    let disk = block(
        &sysfs,
        &format!("/sys/devices/{NVME0}"),
        "nvme0n1",
        "259:0",
        false,
    );
    block(&sysfs, &disk, "nvme0n1p1", "259:1", true);
    block(&sysfs, &disk, "nvme0n1p2", "259:2", true);
    let (mut hwmon, clock, catalog) = catalog_of(&sysfs);
    assert_eq!(
        drive_keys(&catalog),
        vec![(
            format!("hwmon:{NVME0_KEY}:temp1@259:0"),
            "blockdev:259:0".to_owned(),
            "device".to_owned()
        )]
    );
    let source = &catalog[0];
    assert_eq!(source.read_group.as_str(), "disk");
    assert_eq!(source.channel, "temp1_input");
    assert_eq!(source.native_unit.as_str(), "mdegC");
    assert_eq!(source.label.as_deref(), Some("Composite"));
    assert_eq!(source.profile_id.as_str(), "hwmon-drive");
    assert_eq!(source.origin.relation, OriginRelation::Confirmed);
    assert_eq!(
        source.origin.origin_id.as_ref().unwrap().as_str(),
        format!("hwmon:{NVME0_KEY}:temp1")
    );

    let readings = disk_records(&mut hwmon, &clock);
    assert_eq!(readings.len(), 1);
    assert_eq!(readings[0].read_status, ReadStatus::Ok);
    assert_eq!(value_of(&readings[0]), 41_850.0);
    assert_eq!(readings[0].quality, Quality::Verified);
    let fast = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert!(
        fast.is_empty(),
        "drive temperatures are read in the disk group only"
    );
}

/// Several drives: each controller's sensor maps only the namespaces below
/// it (`nvme1` is not a prefix of `nvme10`).
#[test]
fn several_nvme_drives_keep_their_own_disks() {
    let sysfs = FakeSysfs::new("drives-several");
    let nvme1 = "pci0000:00/0000:00:1c.0/0000:03:00.0/nvme/nvme1";
    let nvme10 = "pci0000:00/0000:00:1c.0/0000:03:00.0/nvme/nvme10";
    nvme_sensor(&sysfs, 1, nvme1, "35000");
    nvme_sensor(&sysfs, 2, nvme10, "45000");
    block(
        &sysfs,
        &format!("/sys/devices/{nvme1}"),
        "nvme1n1",
        "259:4",
        false,
    );
    block(
        &sysfs,
        &format!("/sys/devices/{nvme10}"),
        "nvme10n1",
        "259:8",
        false,
    );
    let (mut hwmon, clock, catalog) = catalog_of(&sysfs);
    let mut entities: Vec<String> = drive_keys(&catalog)
        .into_iter()
        .map(|(_, entity, _)| entity)
        .collect();
    entities.sort();
    assert_eq!(entities, vec!["blockdev:259:4", "blockdev:259:8"]);
    let readings = disk_records(&mut hwmon, &clock);
    let celsius = |entity: &str| {
        value_of(
            readings
                .iter()
                .find(|reading| reading.key.entity_id.as_str() == entity)
                .unwrap(),
        )
    };
    assert_eq!(celsius("blockdev:259:4"), 35_000.0);
    assert_eq!(celsius("blockdev:259:8"), 45_000.0);
}

/// A controller with two namespaces: one sensor, two disks, one origin.
#[test]
fn every_namespace_of_a_controller_shares_its_composite() {
    let sysfs = FakeSysfs::new("drives-namespaces");
    nvme_sensor(&sysfs, 1, NVME0, "41850");
    let controller = format!("/sys/devices/{NVME0}");
    block(&sysfs, &controller, "nvme0n1", "259:0", false);
    block(&sysfs, &controller, "nvme0n2", "259:3", false);
    let (_, _, catalog) = catalog_of(&sysfs);
    let entities: Vec<String> = drive_keys(&catalog)
        .into_iter()
        .map(|(_, entity, _)| entity)
        .collect();
    assert_eq!(entities, vec!["blockdev:259:0", "blockdev:259:3"]);
    assert_eq!(catalog[0].origin, catalog[1].origin, "one physical sensor");
    assert_ne!(catalog[0].source_id, catalog[1].source_id);
}

/// SATA/SAS: drivetemp's device is the SCSI device; the disk below it is
/// the drive. A disk without a sensor gets no temperature.
#[test]
fn a_drivetemp_sensor_is_the_temperature_of_its_scsi_disk() {
    let sysfs = FakeSysfs::new("drives-drivetemp");
    let scsi = "pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0";
    let hwmon = sysfs.hwmon(3, scsi, "drivetemp");
    sysfs.write(&format!("{hwmon}/temp1_input"), "33000\n");
    sysfs.write(&format!("{hwmon}/temp1_lowest"), "20000\n");
    let disk = block(
        &sysfs,
        &format!("/sys/devices/{scsi}/block"),
        "sda",
        "8:0",
        false,
    );
    block(&sysfs, &disk, "sda1", "8:1", true);
    let other = "pci0000:00/0000:00:17.0/ata2/host1/target1:0:0/1:0:0:0";
    block(
        &sysfs,
        &format!("/sys/devices/{other}/block"),
        "sdb",
        "8:16",
        false,
    );
    let (mut hwmon_adapter, clock, catalog) = catalog_of(&sysfs);
    let keys = drive_keys(&catalog);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].1, "blockdev:8:0");
    assert_eq!(catalog[0].driver, "drivetemp");
    let readings = disk_records(&mut hwmon_adapter, &clock);
    assert_eq!(value_of(&readings[0]), 33_000.0);
}

/// No sensor, no temperature: a namespace of a controller without hwmon,
/// and an NVMe sensor without a `Composite` label (an unknown layout).
#[test]
fn drives_without_a_proven_sensor_get_no_temperature() {
    let sysfs = FakeSysfs::new("drives-none");
    sysfs.mkdir(&format!("/sys/devices/{NVME0}"));
    block(
        &sysfs,
        &format!("/sys/devices/{NVME0}"),
        "nvme0n1",
        "259:0",
        false,
    );
    let unlabeled = "pci0000:00/0000:00:1c.0/0000:04:00.0/nvme/nvme1";
    let hwmon = sysfs.hwmon(2, unlabeled, "nvme");
    sysfs.write(&format!("{hwmon}/temp1_input"), "40000\n");
    block(
        &sysfs,
        &format!("/sys/devices/{unlabeled}"),
        "nvme1n1",
        "259:5",
        false,
    );
    let (_, _, catalog) = catalog_of(&sysfs);
    assert!(catalog.is_empty());
}

/// Native multipath: the namespace head lies below the subsystem, not a
/// controller. It is the drive of a sensor when every path is below that
/// controller; the hidden path devices themselves are never sources.
/// Paths on two controllers prove no single sensor.
#[test]
fn a_multipath_head_maps_only_through_a_single_controller() {
    let sysfs = FakeSysfs::new("drives-multipath");
    nvme_sensor(&sysfs, 1, NVME0, "41850");
    let controller = format!("/sys/devices/{NVME0}");
    let path = block(&sysfs, &controller, "nvme0c0n1", "259:6", false);
    sysfs.write(&format!("{path}/hidden"), "1\n");
    let head = block(
        &sysfs,
        "/sys/devices/virtual/nvme-subsystem/nvme-subsys0",
        "nvme0n1",
        "259:0",
        false,
    );
    sysfs.symlink(&format!("{head}/multipath/nvme0c0n1"), &path);
    let (_, _, catalog) = catalog_of(&sysfs);
    let entities: Vec<String> = drive_keys(&catalog)
        .into_iter()
        .map(|(_, entity, _)| entity)
        .collect();
    assert_eq!(entities, vec!["blockdev:259:0"]);

    let nvme1 = "pci0000:00/0000:00:1c.0/0000:05:00.0/nvme/nvme1";
    nvme_sensor(&sysfs, 2, nvme1, "50000");
    let second = block(
        &sysfs,
        &format!("/sys/devices/{nvme1}"),
        "nvme0c1n1",
        "259:7",
        false,
    );
    sysfs.write(&format!("{second}/hidden"), "1\n");
    sysfs.symlink(&format!("{head}/multipath/nvme0c1n1"), &second);
    let (_, _, catalog) = catalog_of(&sysfs);
    assert!(
        catalog.is_empty(),
        "two controllers, no proven drive sensor"
    );
}

/// Without a block class there are no disks and nothing fails; an
/// unreadable one leaves the drives out as a partial discovery.
#[test]
fn a_missing_or_unreadable_block_class_maps_no_drive() {
    let sysfs = FakeSysfs::new("drives-no-class");
    nvme_sensor(&sysfs, 1, NVME0, "41850");
    let (_, clock, catalog) = catalog_of(&sysfs);
    assert!(catalog.is_empty());

    block(
        &sysfs,
        &format!("/sys/devices/{NVME0}"),
        "nvme0n1",
        "259:0",
        false,
    );
    let faulty = FaultyFiles::new(sysfs.rooted());
    faulty.fail(
        "/sys/class/block",
        Failure::Kind(io::ErrorKind::PermissionDenied),
    );
    let (mut hwmon, _) = adapter_over(Arc::new(faulty));
    let pages = discover_all(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let last = pages.last().unwrap();
    assert!(last.complete);
    assert_eq!(last.error_code.as_deref(), Some("PARTIAL_DISCOVERY"));
    assert!(sources(&pages).is_empty());
}

/// A drive read failure is a failed record, never a number; a removed
/// controller removes its disks' sources.
#[test]
fn a_vanished_drive_reads_removed_and_leaves_the_catalog() {
    let sysfs = FakeSysfs::new("drives-vanish");
    nvme_sensor(&sysfs, 1, NVME0, "41850");
    block(
        &sysfs,
        &format!("/sys/devices/{NVME0}"),
        "nvme0n1",
        "259:0",
        false,
    );
    let (mut hwmon, clock, catalog) = catalog_of(&sysfs);
    let id = catalog[0].source_id.clone();
    sysfs.remove_hwmon(1, NVME0);
    let readings = disk_records(&mut hwmon, &clock);
    assert_eq!(readings[0].read_status, ReadStatus::Removed);
    assert!(readings[0].value.is_none());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 1, deadline(&clock));
    assert!(pages.last().unwrap().removed_sources.contains(&id));
}

/// Only the groups the adapter serves are readable.
#[test]
fn an_unknown_read_group_fails() {
    let sysfs = FakeSysfs::new("drives-groups");
    let (mut hwmon, clock) = adapter_over(sysfs.files());
    discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let batch = hwmon.read(&read_plan(
        ADAPTER,
        "capacity",
        1,
        0,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(batch.status, BatchStatus::Failed);
    assert_eq!(batch.error_code.as_deref(), Some("UNKNOWN_READ_GROUP"));
}

/// A disk's static facts (`dev`, `partition`, `hidden`) are read once per
/// block device instance, not on every discovery.
#[test]
fn block_device_facts_are_read_once() {
    use dgxtop_adapters::common::CountingFiles;
    let sysfs = FakeSysfs::new("drives-cache");
    nvme_sensor(&sysfs, 1, NVME0, "41850");
    block(
        &sysfs,
        &format!("/sys/devices/{NVME0}"),
        "nvme0n1",
        "259:0",
        false,
    );
    let counting = Arc::new(CountingFiles::new(sysfs.rooted()));
    let files: Arc<dyn SystemFiles> = counting.clone();
    let (mut hwmon, clock) = adapter_over(files);
    for _ in 0..3 {
        let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
        assert_eq!(sources(&pages).len(), 1);
    }
    assert_eq!(counting.reads_matching("nvme0n1/dev"), 1);
}
