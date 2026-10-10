//! Filesystem adapter against fake mountinfo and a fake statvfs probe.

#[path = "fixtures/adapters_a/support.rs"]
mod support;

use std::sync::Arc;

use dgxtop_adapters::filesystem::{
    CapacityFigures, FilesystemAdapter, NETWORK_FS_SKIPPED, manifest,
};
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_core::metric::ids;
use dgxtop_core::observation::{BatchStatus, EntityKind};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Availability, RawKind, ReadStatus, Support};
use dgxtop_core::testing::{FakeClock, host_id};
use support::*;

const ADAPTER: &str = "filesystem";

const MOUNTINFO: &str = "\
69 1 254:2 / / rw,relatime shared:1 - ext4 /dev/mapper/vg-root rw,errors=remount-ro
36 69 0:6 / /dev rw,nosuid shared:2 - devtmpfs devtmpfs rw,size=4096k
80 69 259:5 / /mnt/my\\040disk rw,relatime shared:30 master:1 - ext4 /dev/nvme0n1p5 rw
81 69 0:50 / /data rw,relatime - nfs4 server:/export rw,vers=4.2
82 69 7:0 / /snap/core/1 ro - squashfs /dev/loop0 ro
83 69 0:31 /@home /home rw - btrfs /dev/nvme1n1p2 rw
84 69 259:5 /sub /srv/bind rw - ext4 /dev/nvme0n1p5 rw
";

fn figures(blocks: u64, free: u64, available: u64) -> CapacityFigures {
    CapacityFigures {
        blocks,
        blocks_free: free,
        blocks_available: available,
        fragment_size: 4096,
    }
}

fn fs_tree(label: &str) -> FakeTree {
    let tree = FakeTree::new(label);
    write_namespaces(&tree);
    tree.write("/proc/self/mountinfo", MOUNTINFO);
    tree
}

fn standard_probe() -> FakeCapacityProbe {
    let probe = FakeCapacityProbe::default();
    probe.set("/", figures(1000, 400, 300));
    probe.set("/mnt/my disk", figures(2000, 1000, 900));
    probe.set("/home", figures(500, 250, 250));
    probe.set("/srv/bind", figures(2000, 1000, 900));
    probe
}

fn adapter(
    tree: &FakeTree,
    clock: &Arc<FakeClock>,
    probe: &FakeCapacityProbe,
) -> FilesystemAdapter {
    FilesystemAdapter::with_capacity_probe(
        clock.clone(),
        host_id("test-host"),
        tree.files(),
        AdapterOptions::default(),
        Box::new(probe.clone()),
    )
}

fn entity_of(mount_id: u64) -> String {
    format!("fs:{MNT_NAMESPACE}:{mount_id}")
}

fn setup(
    label: &str,
) -> (
    FakeTree,
    Arc<FakeClock>,
    FakeCapacityProbe,
    FilesystemAdapter,
    ParentCatalog,
) {
    let tree = fs_tree(label);
    let clock = FakeClock::new(1_000_000_000);
    let probe = standard_probe();
    let mut adapter = adapter(&tree, &clock, &probe);
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, ADAPTER);
    (tree, clock, probe, adapter, parent)
}

#[test]
fn discovery_lists_disk_mounts_and_flags_network_mounts() {
    let tree = fs_tree("fs-discovery");
    let clock = FakeClock::new(1_000_000_000);
    let probe = standard_probe();
    let mut adapter = adapter(&tree, &clock, &probe);
    let mut parent = ParentCatalog::default();
    let pages = parent.sync(&mut adapter, ADAPTER);
    assert_eq!(pages[0].availability, Some(Availability::Available));
    let entities: Vec<&str> = parent.entities.keys().map(String::as_str).collect();
    let expected: Vec<String> = [69, 80, 81, 83, 84].into_iter().map(entity_of).collect();
    assert_eq!(
        entities,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let disk = parent.entity(&entity_of(80));
    assert_eq!(disk.kind, EntityKind::Filesystem);
    assert_eq!(
        disk.display_name, "/mnt/my disk",
        "octal escapes are decoded"
    );
    assert_eq!(disk.attribute("mount_point"), Some("/mnt/my disk"));
    assert_eq!(disk.attribute("device"), Some("/dev/nvme0n1p5"));
    assert_eq!(disk.attribute("fs_type"), Some("ext4"));
    assert_eq!(disk.attribute("major_minor"), Some("259:5"));
    assert_eq!(disk.parent_id.as_ref().unwrap().as_str(), "blockdev:259:5");
    assert_eq!(
        parent.entity(&entity_of(84)).attribute("mount_root"),
        Some("/sub")
    );
    assert!(
        parent.entity(&entity_of(83)).parent_id.is_none(),
        "btrfs reports an anonymous device"
    );
    assert_eq!(parent.sources.len(), 12);
    let total = parent.source("filesystem:mount.80:total");
    let key = total.metric.as_ref().unwrap();
    assert_eq!(key.metric_id.as_str(), ids::FS_TOTAL);
    assert_eq!(key.scope.as_str(), "filesystem");
    assert_eq!(total.native_unit.as_str(), "bytes");
    assert_eq!(total.raw_kind, RawKind::Gauge);
    assert_eq!(total.read_group.as_str(), "capacity");
    let skipped = pages[0]
        .capabilities
        .iter()
        .find(|capability| {
            capability.entity_id.as_ref().map(|id| id.to_string()) == Some(entity_of(81))
        })
        .unwrap();
    assert_eq!(skipped.support, Support::Unsupported);
    assert_eq!(skipped.reason.as_deref(), Some(NETWORK_FS_SKIPPED));
    assert!(probe.calls().is_empty(), "discovery never calls statvfs");
}

#[test]
fn capacities_are_fragments_times_fragment_size() {
    let (_tree, _clock, probe, mut adapter, _parent) = setup("fs-read");
    let batches = read_all(&mut adapter, ADAPTER, "capacity", 1);
    assert_eq!(batches[0].status, BatchStatus::Ok);
    let records = records(&batches);
    assert_eq!(records.len(), 12);
    assert_eq!(
        u64_value(find(&records, ids::FS_TOTAL, &entity_of(69))),
        4_096_000
    );
    assert_eq!(
        u64_value(find(&records, ids::FS_FREE, &entity_of(69))),
        1_638_400
    );
    assert_eq!(
        u64_value(find(&records, ids::FS_AVAILABLE, &entity_of(69))),
        1_228_800
    );
    // Two mounts of one device are distinct entities with the same figures;
    // summing them would double count.
    assert_eq!(
        u64_value(find(&records, ids::FS_TOTAL, &entity_of(80))),
        u64_value(find(&records, ids::FS_TOTAL, &entity_of(84)))
    );
    assert!(
        !probe.calls().contains(&"/data".to_owned()),
        "network filesystems are never probed"
    );
    assert_eq!(probe.calls().len(), 4);
    for record in &records {
        record.validate().unwrap();
    }
}

#[test]
fn overflowing_capacity_is_an_error_record() {
    let (_tree, _clock, probe, mut adapter, _parent) = setup("fs-overflow");
    probe.set("/home", figures(u64::MAX, 1, 1));
    let records = records(&read_all(&mut adapter, ADAPTER, "capacity", 1));
    let total = find(&records, ids::FS_TOTAL, &entity_of(83));
    assert_eq!(total.read_status, ReadStatus::Error);
    assert_eq!(total.value, None);
    assert_eq!(total.reason_codes, vec!["CAPACITY_OVERFLOW".to_owned()]);
    assert_eq!(
        u64_value(find(&records, ids::FS_FREE, &entity_of(83))),
        4096
    );
}

#[test]
fn denied_statvfs_is_reprobed_at_most_once_per_interval() {
    let (_tree, clock, probe, mut adapter, _parent) = setup("fs-denied");
    probe.fail("/home", 13);
    for cycle in 0..=24u64 {
        let records = records(&read_all(&mut adapter, ADAPTER, "capacity", cycle + 1));
        let total = find(&records, ids::FS_TOTAL, &entity_of(83));
        assert_eq!(total.read_status, ReadStatus::PermissionDenied);
        assert_eq!(total.value, None);
        clock.advance_ms(5_000);
    }
    let home_calls = probe
        .calls()
        .iter()
        .filter(|call| call.as_str() == "/home")
        .count();
    assert_eq!(home_calls, 3, "probes at 0 s, 60 s and 120 s");
}

#[test]
fn changed_mount_is_not_probed_and_becomes_a_new_generation() {
    let (tree, _clock, probe, mut adapter, mut parent) = setup("fs-changed");
    let old_generation = parent.entity(&entity_of(80)).generation;
    tree.write(
        "/proc/self/mountinfo",
        &MOUNTINFO.replace(
            "80 69 259:5 / /mnt/my\\040disk",
            "80 69 8:1 / /mnt/my\\040disk",
        ),
    );
    let records = records(&read_all(&mut adapter, ADAPTER, "capacity", 1));
    let changed = find(&records, ids::FS_TOTAL, &entity_of(80));
    assert_eq!(changed.read_status, ReadStatus::Removed);
    assert_eq!(changed.reason_codes, vec!["MOUNT_CHANGED".to_owned()]);
    assert_eq!(
        probe
            .calls()
            .iter()
            .filter(|call| call.as_str() == "/mnt/my disk")
            .count(),
        0
    );
    parent.sync(&mut adapter, ADAPTER);
    let renewed = parent.entity(&entity_of(80));
    assert!(renewed.generation > old_generation);
    assert_eq!(renewed.attribute("major_minor"), Some("8:1"));
}

#[test]
fn shadowed_mounts_and_mounts_below_network_filesystems_are_not_probed() {
    let (tree, _clock, probe, mut adapter, mut parent) = setup("fs-shadow");
    let extra = "90 80 0:60 / /mnt/my\\040disk rw - tmpfs tmpfs rw\n\
                 91 81 8:17 / /data/local rw - xfs /dev/sdb1 rw\n";
    tree.write("/proc/self/mountinfo", &format!("{MOUNTINFO}{extra}"));
    parent.sync(&mut adapter, ADAPTER);
    assert!(parent.entities.contains_key(&entity_of(91)));
    let records = records(&read_all(&mut adapter, ADAPTER, "capacity", 1));
    let shadowed = find(&records, ids::FS_TOTAL, &entity_of(80));
    assert_eq!(shadowed.reason_codes, vec!["MOUNT_SHADOWED".to_owned()]);
    assert_eq!(shadowed.value, None);
    let below_network = find(&records, ids::FS_TOTAL, &entity_of(91));
    assert_eq!(below_network.read_status, ReadStatus::Unsupported);
    assert_eq!(
        below_network.reason_codes,
        vec!["NETWORK_FS_IN_PATH".to_owned()]
    );
    let calls = probe.calls();
    assert!(!calls.contains(&"/mnt/my disk".to_owned()));
    assert!(!calls.contains(&"/data/local".to_owned()));
    assert!(calls.contains(&"/srv/bind".to_owned()));
}

#[test]
fn unmounted_filesystem_only_removes_itself() {
    let (tree, _clock, _probe, mut adapter, mut parent) = setup("fs-unmount");
    tree.write(
        "/proc/self/mountinfo",
        &MOUNTINFO.replace("83 69 0:31 /@home /home rw - btrfs /dev/nvme1n1p2 rw\n", ""),
    );
    let records = records(&read_all(&mut adapter, ADAPTER, "capacity", 1));
    let gone = find(&records, ids::FS_TOTAL, &entity_of(83));
    assert_eq!(gone.reason_codes, vec!["MOUNT_REMOVED".to_owned()]);
    let pages = parent.sync(&mut adapter, ADAPTER);
    let last = pages.last().unwrap();
    let removed: Vec<&str> = last.removed_entities.iter().map(|id| id.as_str()).collect();
    assert_eq!(removed, vec![entity_of(83).as_str()]);
    assert_eq!(last.removed_sources.len(), 3);
    assert_eq!(parent.sources.len(), 9);
}

#[test]
fn unreadable_mountinfo_keeps_the_catalog_and_fails_reads_without_values() {
    let (tree, _clock, probe, mut adapter, mut parent) = setup("fs-unreadable");
    tree.remove("/proc/self/mountinfo");
    let pages = parent.sync(&mut adapter, ADAPTER);
    assert!(pages.last().unwrap().removed_entities.is_empty());
    assert!(
        pages[0]
            .error_code
            .as_deref()
            .unwrap()
            .starts_with("MOUNTINFO_UNREADABLE")
    );
    assert_eq!(parent.sources.len(), 12);
    let records = records(&read_all(&mut adapter, ADAPTER, "capacity", 1));
    assert!(records.iter().all(|record| record.value.is_none()));
    assert!(
        probe.calls().is_empty(),
        "no statvfs without a verified mount table"
    );
}

#[test]
fn missing_mount_namespace_is_unavailable() {
    let tree = FakeTree::new("fs-nons");
    tree.write("/proc/self/mountinfo", MOUNTINFO);
    let clock = FakeClock::new(1);
    let probe = standard_probe();
    let mut adapter = adapter(&tree, &clock, &probe);
    let pages = discover_all(&mut adapter, ADAPTER, 0);
    assert_eq!(pages[0].availability, Some(Availability::Unavailable));
    assert_eq!(
        pages[0].error_code.as_deref(),
        Some("MOUNT_NAMESPACE_UNKNOWN")
    );
    assert_eq!(adapter.health().availability, Availability::Unavailable);
}

#[test]
fn manifest_uses_the_capacity_cadence() {
    let manifest = manifest();
    manifest.validate().unwrap();
    assert_eq!(manifest.adapter_id.as_str(), "filesystem");
    assert_eq!(manifest.minimum_sample_ms, 1000);
    assert_eq!(manifest.runtime_library, None);
}
