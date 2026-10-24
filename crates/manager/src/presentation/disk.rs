//! Disk module projection: the I/O of the block devices that hold a mounted
//! filesystem (busiest first) with each drive's temperature, mounted
//! filesystems (by mount point), and the busiest whole disk, whose rate
//! trends feed the panel chart. A mount is never presented as a device.

use std::collections::BTreeSet;

use dgxtop_core::canonical::{CatalogSnapshot, SnapshotEntry};
use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::ids;
use dgxtop_core::naming::{attributes, block_layer, entity};
use dgxtop_core::observation::EntityRecord;
use dgxtop_core::view::{DiskDeviceView, DiskView, FilesystemView, Trend};

use super::labels::label_text;
use super::ordering::{descending_present, sum_present};
use super::reader::MetricReader;

/// Device and filesystem rows kept per projection.
pub(crate) const MAX_DISK_ROWS: usize = 32;
/// The most devices a drive's temperature is looked for through: a volume
/// on an encrypted mapping on a partition of a disk is four.
const MAX_STACK_DEPTH: usize = 8;
const BLOCK_DEVICE_PREFIX: &str = "blockdev:";
const FILESYSTEM_PREFIX: &str = "fs:";
const UNKNOWN_TEXT: &str = "unknown";
const DEVICE_METRICS: [&str; 5] = [
    ids::DISK_READ_BPS,
    ids::DISK_WRITE_BPS,
    ids::DISK_READ_IOPS,
    ids::DISK_WRITE_IOPS,
    ids::DISK_AWAIT,
];
const FILESYSTEM_METRICS: [&str; 5] = [
    ids::FS_TOTAL,
    ids::FS_USED,
    ids::FS_FREE,
    ids::FS_AVAILABLE,
    ids::FS_USE_PERCENT,
];

pub(crate) fn build_disk_view(reader: &MetricReader<'_>, trend_width: usize) -> DiskView {
    let filesystems = filesystem_views(reader);
    let devices = listed_devices(reader, &filesystems);
    let charted = charted_of(reader, &devices);
    let trend = |metric: &str| {
        charted.as_ref().map_or_else(Trend::default, |device| {
            reader.trend_preferring(
                metric,
                device.entity_id.as_str(),
                entity::SCOPE_DEVICE,
                trend_width,
            )
        })
    };
    DiskView {
        read_trend: trend(ids::DISK_READ_BPS),
        write_trend: trend(ids::DISK_WRITE_BPS),
        devices,
        filesystems,
        charted,
    }
}

/// The device the Disk panel charts ([`charted_of`]).
pub(crate) fn charted_device(reader: &MetricReader<'_>) -> Option<DiskDeviceView> {
    charted_of(reader, &listed_devices(reader, &filesystem_views(reader)))
}

/// The busiest whole disk (D-124), the bottom of its stack, so its rates
/// count every partition and volume on it; where no whole disk reports
/// (sysfs unreadable), the busiest of the `listed` devices.
fn charted_of(reader: &MetricReader<'_>, listed: &[DiskDeviceView]) -> Option<DiskDeviceView> {
    let catalog = reader.catalog();
    ranked_devices(reader, |entity_id| is_whole_disk(catalog, entity_id))
        .into_iter()
        .next()
        .or_else(|| listed.first().cloned())
}

fn is_whole_disk(catalog: &CatalogSnapshot, entity_id: &EntityId) -> bool {
    catalog
        .entity(entity_id)
        .and_then(|record| record.attribute(attributes::BLOCK_LAYER))
        == Some(block_layer::DISK)
}

/// The devices the panel lists ([`ListedDevices`]), busiest first.
fn listed_devices(
    reader: &MetricReader<'_>,
    filesystems: &[FilesystemView],
) -> Vec<DiskDeviceView> {
    let listed = ListedDevices::of(reader.catalog(), filesystems);
    let mut devices = ranked_devices(reader, |entity_id| listed.includes(entity_id));
    devices.truncate(MAX_DISK_ROWS);
    devices
}

/// The reporting devices `include` keeps, by total throughput (missing
/// last), then name; the first is the busiest.
fn ranked_devices(
    reader: &MetricReader<'_>,
    include: impl Fn(&EntityId) -> bool,
) -> Vec<DiskDeviceView> {
    let mut ranked: Vec<(Option<f64>, DiskDeviceView)> = reader
        .index()
        .entities_reporting(&DEVICE_METRICS, BLOCK_DEVICE_PREFIX)
        .into_iter()
        .filter(|entity_id| include(entity_id))
        .map(|entity_id| {
            let view = device_view(reader, entity_id);
            let throughput = sum_present([view.read_bps.value_f64(), view.write_bps.value_f64()]);
            (throughput, view)
        })
        .collect();
    ranked.sort_by(|(left_rate, left), (right_rate, right)| {
        descending_present(*left_rate, *right_rate)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.entity_id.cmp(&right.entity_id))
    });
    ranked.into_iter().map(|(_, view)| view).collect()
}

/// The block devices the panel lists (D-124): those holding a filesystem.
/// Any other device either repeats I/O already listed (a whole disk counts
/// its partitions', an encrypted or LVM container the volumes' on it) or
/// has no capacity to show (swap, an unused partition). Every device is
/// listed while no filesystem is placed on one (a container's overlay
/// root, an unreadable mount table) or while a filesystem's device number
/// is anonymous (btrfs, ZFS, bcachefs), so its disk is unknown and could
/// otherwise be hidden.
enum ListedDevices<'a> {
    Holding(BTreeSet<&'a EntityId>),
    Every,
}

impl<'a> ListedDevices<'a> {
    fn of(catalog: &CatalogSnapshot, filesystems: &'a [FilesystemView]) -> Self {
        let holding: BTreeSet<&EntityId> = filesystems
            .iter()
            .filter_map(|filesystem| filesystem.block_device.as_ref())
            .collect();
        let unplaced = filesystems.iter().any(|filesystem| {
            filesystem.block_device.is_none()
                && has_anonymous_device(catalog, &filesystem.entity_id)
        });
        if holding.is_empty() || unplaced {
            Self::Every
        } else {
            Self::Holding(holding)
        }
    }

    fn includes(&self, device: &EntityId) -> bool {
        match self {
            Self::Holding(holding) => holding.contains(device),
            Self::Every => true,
        }
    }
}

/// Whether a filesystem's mount reports an anonymous device number (major
/// 0), which names none of the disks it lives on.
fn has_anonymous_device(catalog: &CatalogSnapshot, filesystem: &EntityId) -> bool {
    catalog
        .entity(filesystem)
        .and_then(|record| record.attribute(attributes::MAJOR_MINOR))
        .and_then(|major_minor| major_minor.split_once(':'))
        .is_some_and(|(major, _)| major == "0")
}

fn device_view(reader: &MetricReader<'_>, entity_id: &EntityId) -> DiskDeviceView {
    let id = entity_id.as_str();
    let cell = |metric: &str| reader.cell_preferring(metric, id, entity::SCOPE_DEVICE);
    DiskDeviceView {
        entity_id: entity_id.clone(),
        name: device_name(reader.catalog(), entity_id),
        read_bps: cell(ids::DISK_READ_BPS),
        write_bps: cell(ids::DISK_WRITE_BPS),
        read_iops: cell(ids::DISK_READ_IOPS),
        write_iops: cell(ids::DISK_WRITE_IOPS),
        await_ms: cell(ids::DISK_AWAIT),
        temperature: reader
            .cell_or_missing(drive_temperature(reader, entity_id), ids::DISK_TEMPERATURE),
    }
}

/// The temperature of the drive a block device is on (D-92, D-124): its
/// own, or the first along the block devices the catalog names as parents
/// (a partition's disk, the one device a mapping is built on).
fn drive_temperature<'a>(
    reader: &MetricReader<'a>,
    entity_id: &EntityId,
) -> Option<&'a SnapshotEntry> {
    let mut device = entity_id;
    for _ in 0..MAX_STACK_DEPTH {
        let own = reader
            .index()
            .find(ids::DISK_TEMPERATURE, device.as_str(), entity::SCOPE_DEVICE);
        if own.is_some() {
            return own;
        }
        let parent = reader.catalog().entity(device)?.parent_id.as_ref()?;
        if !parent.as_str().starts_with(BLOCK_DEVICE_PREFIX) {
            return None;
        }
        device = parent;
    }
    None
}

fn device_name(catalog: &CatalogSnapshot, entity_id: &EntityId) -> String {
    let record = catalog.entity(entity_id);
    let named = record
        .map(|record| record.display_name.as_str())
        .filter(|name| !name.is_empty())
        .or_else(|| record.and_then(|record| record.attribute(attributes::DEVICE)))
        .or_else(|| record.and_then(|record| record.attribute(attributes::MAJOR_MINOR)));
    label_text(named.unwrap_or(entity_id.as_str()))
}

fn filesystem_views(reader: &MetricReader<'_>) -> Vec<FilesystemView> {
    let mut views: Vec<FilesystemView> = reader
        .index()
        .entities_reporting(&FILESYSTEM_METRICS, FILESYSTEM_PREFIX)
        .into_iter()
        .map(|entity_id| filesystem_view(reader, entity_id))
        .collect();
    views.sort_by(|left, right| {
        left.mount_point
            .cmp(&right.mount_point)
            .then_with(|| left.entity_id.cmp(&right.entity_id))
    });
    views.truncate(MAX_DISK_ROWS);
    views
}

fn filesystem_view(reader: &MetricReader<'_>, entity_id: &EntityId) -> FilesystemView {
    let id = entity_id.as_str();
    let cell = |metric: &str| reader.cell_preferring(metric, id, entity::SCOPE_FILESYSTEM);
    let record = reader.catalog().entity(entity_id).map(AsRef::as_ref);
    FilesystemView {
        entity_id: entity_id.clone(),
        mount_point: attribute_text(record, attributes::MOUNT_POINT),
        device: attribute_text(record, attributes::DEVICE),
        block_device: block_device_of(reader.catalog(), record),
        fs_type: attribute_text(record, attributes::FS_TYPE),
        total: cell(ids::FS_TOTAL),
        used: cell(ids::FS_USED),
        free: cell(ids::FS_FREE),
        available: cell(ids::FS_AVAILABLE),
        use_percent: cell(ids::FS_USE_PERCENT),
    }
}

/// The block device a filesystem lives on: `blockdev:MAJ:MIN` from the
/// mount's device number, when the catalog lists that device.
fn block_device_of(catalog: &CatalogSnapshot, record: Option<&EntityRecord>) -> Option<EntityId> {
    let major_minor = record?.attribute(attributes::MAJOR_MINOR)?;
    let device = EntityId::new(format!("{BLOCK_DEVICE_PREFIX}{major_minor}")).ok()?;
    catalog.entity(&device).is_some().then_some(device)
}

fn attribute_text(record: Option<&EntityRecord>, key: &str) -> String {
    label_text(
        record
            .and_then(|record| record.attribute(key))
            .unwrap_or(UNKNOWN_TEXT),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, NOW, entity_record, sample};
    use dgxtop_core::observation::EntityKind;
    use dgxtop_core::testing::metric_key;
    use dgxtop_core::view::CellState;

    const TREND_WIDTH: usize = 60;

    fn build(fixture: &Fixture) -> DiskView {
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW)
            .with_trends(&fixture.trends);
        build_disk_view(&reader, TREND_WIDTH)
    }

    #[test]
    fn devices_sort_by_throughput_then_name_with_missing_last() {
        let mut fixture = Fixture::new();
        fixture.add_entity(entity_record(
            "blockdev:259:0",
            EntityKind::BlockDevice,
            "nvme0n1",
            &[],
        ));
        fixture.add_entity(entity_record(
            "blockdev:8:0",
            EntityKind::BlockDevice,
            "sda",
            &[],
        ));
        fixture.add_entity(entity_record(
            "blockdev:8:16",
            EntityKind::BlockDevice,
            "sdb",
            &[],
        ));
        fixture.add(
            "disk",
            sample("disk.read.bytes_per_second", "blockdev:8:0", "device").f64(10.0),
        );
        fixture.add(
            "disk",
            sample("disk.write.bytes_per_second", "blockdev:259:0", "device").f64(500.0),
        );
        fixture.add(
            "disk",
            sample("disk.read.bytes_per_second", "blockdev:8:16", "device")
                .unavailable()
                .reason("WARMUP"),
        );
        let view = build(&fixture);
        let names: Vec<&str> = view
            .devices
            .iter()
            .map(|device| device.name.as_str())
            .collect();
        assert_eq!(names, vec!["nvme0n1", "sda", "sdb"]);
        assert_eq!(view.devices[2].read_bps.state, CellState::Warmup);
        assert_eq!(view.devices[0].read_bps.state, CellState::Unavailable);
    }

    /// Where sysfs names no whole disk, the busiest listed device is
    /// charted.
    #[test]
    fn the_busiest_device_carries_its_read_and_write_trends() {
        let mut fixture = Fixture::new();
        for (entity, read, write) in [("blockdev:8:0", 10.0, 0.0), ("blockdev:259:0", 1.0, 500.0)] {
            fixture.add(
                "disk",
                sample("disk.read.bytes_per_second", entity, "device").f64(read),
            );
            fixture.add(
                "disk",
                sample("disk.write.bytes_per_second", entity, "device").f64(write),
            );
        }
        let busiest_read = metric_key("blockdev:259:0", "disk.read.bytes_per_second", "device");
        let busiest_write = metric_key("blockdev:259:0", "disk.write.bytes_per_second", "device");
        let other_read = metric_key("blockdev:8:0", "disk.read.bytes_per_second", "device");
        fixture.trends.push(&busiest_read, Some(1.0));
        fixture.trends.push(&busiest_write, None);
        fixture.trends.push(&busiest_write, Some(500.0));
        fixture.trends.push(&other_read, Some(10.0));
        let view = build(&fixture);
        assert_eq!(view.devices[0].entity_id.as_str(), "blockdev:259:0");
        assert_eq!(view.charted.as_ref(), view.devices.first());
        assert_eq!(view.read_trend.points, vec![Some(1.0)]);
        assert_eq!(view.write_trend.points, vec![None, Some(500.0)]);
    }

    #[test]
    fn idle_zero_device_ranks_above_unreadable_device() {
        let mut fixture = Fixture::new();
        fixture.add(
            "disk",
            sample("disk.read.bytes_per_second", "blockdev:8:0", "device")
                .unavailable()
                .reason("TIMEOUT"),
        );
        fixture.add(
            "disk",
            sample("disk.read.bytes_per_second", "blockdev:8:16", "device").f64(0.0),
        );
        let view = build(&fixture);
        assert_eq!(view.devices[0].entity_id.as_str(), "blockdev:8:16");
        assert_eq!(view.devices[0].read_bps.value_f64(), Some(0.0));
        assert_eq!(view.devices[1].name, "blockdev:8:0");
    }

    fn block_device(fixture: &mut Fixture, id: &str, name: &str, parent: Option<&str>) {
        let mut record = entity_record(id, EntityKind::BlockDevice, name, &[]);
        record.parent_id = parent.map(|parent| EntityId::new(parent).unwrap());
        fixture.add_entity(record);
        fixture.add(
            "disk",
            sample("disk.read.bytes_per_second", id, "device").f64(1.0),
        );
    }

    /// A drive's temperature shows on its disk and on the partitions and
    /// volumes the catalog stacks on that disk (D-92, D-124); a device
    /// without one, or below a parent that is no block device, shows none
    /// (never 0).
    #[test]
    fn devices_show_their_drives_temperature() {
        let mut fixture = Fixture::new();
        block_device(&mut fixture, "blockdev:259:0", "nvme0n1", Some("host"));
        for (id, name, parent) in [
            ("blockdev:259:1", "nvme0n1p1", "blockdev:259:0"),
            // A volume (dm-2) on an encrypted mapping (dm-1) on nvme0n1p5.
            ("blockdev:259:5", "nvme0n1p5", "blockdev:259:0"),
            ("blockdev:254:0", "dm-1", "blockdev:259:5"),
            ("blockdev:254:1", "dm-2", "blockdev:254:0"),
        ] {
            block_device(&mut fixture, id, name, Some(parent));
        }
        block_device(&mut fixture, "blockdev:8:0", "sda", Some("host"));
        block_device(&mut fixture, "blockdev:8:1", "sda1", Some("blockdev:8:0"));
        block_device(&mut fixture, "blockdev:253:0", "dm-0", Some("soc"));
        fixture.add(
            "disk",
            sample("disk.temperature", "blockdev:259:0", "device").f64(58.85),
        );
        fixture.add(
            "disk",
            sample("disk.temperature", "soc", "device").f64(90.0),
        );
        let view = build(&fixture);
        let temperature = |name: &str| {
            let device = view
                .devices
                .iter()
                .find(|device| device.name == name)
                .unwrap();
            (device.temperature.value_f64(), device.temperature.state)
        };
        assert_eq!(temperature("nvme0n1"), (Some(58.85), CellState::Ok));
        assert_eq!(temperature("nvme0n1p1"), (Some(58.85), CellState::Ok));
        assert_eq!(temperature("dm-2"), (Some(58.85), CellState::Ok));
        assert_eq!(temperature("sda"), (None, CellState::Unavailable));
        assert_eq!(temperature("sda1"), (None, CellState::Unavailable));
        assert_eq!(temperature("dm-0"), (None, CellState::Unavailable));
    }

    fn device_with_rates(
        fixture: &mut Fixture,
        (id, name, scope, layer): (&str, &str, &str, &str),
        (read, write): (f64, f64),
    ) {
        fixture.add_entity(entity_record(
            id,
            EntityKind::BlockDevice,
            name,
            &[(attributes::BLOCK_LAYER, layer)],
        ));
        fixture.add(
            "disk",
            sample("disk.read.bytes_per_second", id, scope).f64(read),
        );
        fixture.add(
            "disk",
            sample("disk.write.bytes_per_second", id, scope).f64(write),
        );
    }

    fn mounted(fixture: &mut Fixture, id: &str, mount_point: &str, major_minor: &str) {
        fixture.add_entity(entity_record(
            id,
            EntityKind::Filesystem,
            "",
            &[
                (attributes::MOUNT_POINT, mount_point),
                (attributes::MAJOR_MINOR, major_minor),
            ],
        ));
        fixture.add(
            "disk",
            sample("filesystem.total", id, "filesystem").u64(1000),
        );
    }

    /// Only devices holding a filesystem are listed (D-124): the whole
    /// disk repeats its partitions' I/O, the encrypted partition and its
    /// mapping that of the volumes on them, and swap or an unused partition
    /// has no capacity. The chart follows the busiest whole disk, which
    /// counts all of them though it is no row; a filesystem on a device
    /// never listed (a loop image) changes neither.
    #[test]
    fn only_filesystem_devices_are_listed_and_the_whole_disk_is_charted() {
        let mut fixture = Fixture::new();
        // nvme0n1 = p3 + p5; p5 = dm-0 = dm-1 (/) + dm-2 (swap).
        for (device, rates) in [
            (
                ("blockdev:259:0", "nvme0n1", "device", "disk"),
                (300.0, 80.0),
            ),
            (("blockdev:8:0", "sda", "device", "disk"), (0.0, 0.0)),
            (
                ("blockdev:259:2", "nvme0n1p2", "partition", "partition"),
                (0.0, 0.0),
            ),
            (
                ("blockdev:259:3", "nvme0n1p3", "partition", "partition"),
                (200.0, 0.0),
            ),
            (
                ("blockdev:259:5", "nvme0n1p5", "partition", "partition"),
                (100.0, 80.0),
            ),
            (
                ("blockdev:254:0", "dm-0", "device", "stacked"),
                (100.0, 80.0),
            ),
            (
                ("blockdev:254:1", "dm-1", "device", "stacked"),
                (100.0, 76.0),
            ),
            (("blockdev:254:2", "dm-2", "device", "stacked"), (0.0, 4.0)),
        ] {
            device_with_rates(&mut fixture, device, rates);
        }
        mounted(&mut fixture, "fs:mnt:1", "/", "254:1");
        mounted(&mut fixture, "fs:mnt:2", "/mnt/data", "259:3");
        mounted(&mut fixture, "fs:mnt:3", "/mnt/image", "7:0");
        let whole_disk = metric_key("blockdev:259:0", "disk.read.bytes_per_second", "device");
        let busiest_row = metric_key("blockdev:259:3", "disk.read.bytes_per_second", "partition");
        fixture.trends.push(&whole_disk, Some(300.0));
        fixture.trends.push(&busiest_row, Some(200.0));
        let view = build(&fixture);
        let names: Vec<&str> = view
            .devices
            .iter()
            .map(|device| device.name.as_str())
            .collect();
        assert_eq!(names, vec!["nvme0n1p3", "dm-1"]);
        assert_eq!(view.filesystems.len(), 3);
        let charted = view.charted.as_ref().unwrap();
        assert_eq!(charted.name, "nvme0n1");
        assert_eq!(charted.write_bps.value_f64(), Some(80.0));
        assert_eq!(view.read_trend.points, vec![Some(300.0)]);
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        assert_eq!(
            charted_device(&reader).map(|device| device.name).as_deref(),
            Some("nvme0n1")
        );
    }

    /// Every device stays listed while a filesystem cannot be placed
    /// (D-124): a btrfs root's anonymous device number names none of its
    /// disks, whose I/O would otherwise be hidden. With no filesystem at
    /// all every device is listed too (the tests above).
    #[test]
    fn every_device_is_listed_while_a_filesystem_cannot_be_placed() {
        let mut fixture = Fixture::new();
        for device in [
            ("blockdev:259:0", "nvme0n1", "device", "disk"),
            ("blockdev:259:1", "nvme0n1p1", "partition", "partition"),
            ("blockdev:259:2", "nvme0n1p2", "partition", "partition"),
        ] {
            device_with_rates(&mut fixture, device, (1.0, 0.0));
        }
        mounted(&mut fixture, "fs:mnt:1", "/boot/efi", "259:1");
        assert_eq!(build(&fixture).devices.len(), 1);
        mounted(&mut fixture, "fs:mnt:2", "/", "0:45");
        assert_eq!(build(&fixture).devices.len(), 3);
    }

    #[test]
    fn a_filesystem_links_to_the_block_device_with_its_device_number() {
        let mut fixture = Fixture::new();
        fixture.add_entity(entity_record(
            "blockdev:254:1",
            EntityKind::BlockDevice,
            "dm-1",
            &[(attributes::MAJOR_MINOR, "254:1")],
        ));
        for (id, mount, major_minor) in [
            ("fs:mnt:1", "/", "254:1"),
            ("fs:mnt:2", "/srv", "8:17"),
            ("fs:mnt:3", "/proc-like", "0:23"),
        ] {
            mounted(&mut fixture, id, mount, major_minor);
        }
        let view = build(&fixture);
        let links: Vec<Option<&str>> = view
            .filesystems
            .iter()
            .map(|fs| fs.block_device.as_ref().map(EntityId::as_str))
            .collect();
        // Only a device the catalog lists is linked.
        assert_eq!(links, vec![Some("blockdev:254:1"), None, None]);
    }
}
