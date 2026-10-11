//! Disk module projection: block-device I/O (busiest first, whose rate
//! trends feed the panel chart) with each drive's temperature, and mounted
//! filesystems (by mount point). A mount is never presented as a device.

use dgxtop_core::canonical::{CatalogSnapshot, SnapshotEntry};
use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::ids;
use dgxtop_core::naming::{attributes, entity};
use dgxtop_core::observation::EntityRecord;
use dgxtop_core::view::{DiskDeviceView, DiskView, FilesystemView, Trend};

use super::labels::label_text;
use super::ordering::{descending_present, sum_present};
use super::reader::MetricReader;

/// Device and filesystem rows kept per projection.
pub(crate) const MAX_DISK_ROWS: usize = 32;
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
    let devices = device_views(reader);
    let trend = |metric: &str| {
        devices.first().map_or_else(Trend::default, |busiest| {
            reader.trend_preferring(
                metric,
                busiest.entity_id.as_str(),
                entity::SCOPE_DEVICE,
                trend_width,
            )
        })
    };
    DiskView {
        read_trend: trend(ids::DISK_READ_BPS),
        write_trend: trend(ids::DISK_WRITE_BPS),
        devices,
        filesystems: filesystem_views(reader),
    }
}

/// Devices with samples, by total throughput (missing last), then name;
/// the first is the busiest.
pub(crate) fn device_views(reader: &MetricReader<'_>) -> Vec<DiskDeviceView> {
    let mut ranked: Vec<(Option<f64>, DiskDeviceView)> = reader
        .index()
        .entities_reporting(&DEVICE_METRICS, BLOCK_DEVICE_PREFIX)
        .into_iter()
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
    ranked
        .into_iter()
        .map(|(_, view)| view)
        .take(MAX_DISK_ROWS)
        .collect()
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

/// The temperature of the drive a block device is (D-92): its own, or for
/// a partition the disk's the catalog names as its parent.
fn drive_temperature<'a>(
    reader: &MetricReader<'a>,
    entity_id: &EntityId,
) -> Option<&'a SnapshotEntry> {
    let own = |id: &str| {
        reader
            .index()
            .find(ids::DISK_TEMPERATURE, id, entity::SCOPE_DEVICE)
    };
    own(entity_id.as_str()).or_else(|| {
        let parent = reader.catalog().entity(entity_id)?.parent_id.as_ref()?;
        parent
            .as_str()
            .starts_with(BLOCK_DEVICE_PREFIX)
            .then(|| own(parent.as_str()))
            .flatten()
    })
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
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, &fixture.trends, NOW);
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

    /// A drive's temperature shows on its disk and on the partitions the
    /// catalog places on that disk (D-92); a device without one, or below
    /// a parent that is no block device, shows none (never 0).
    #[test]
    fn devices_show_their_drives_temperature() {
        let mut fixture = Fixture::new();
        block_device(&mut fixture, "blockdev:259:0", "nvme0n1", Some("host"));
        block_device(
            &mut fixture,
            "blockdev:259:1",
            "nvme0n1p1",
            Some("blockdev:259:0"),
        );
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
        assert_eq!(temperature("sda"), (None, CellState::Unavailable));
        assert_eq!(temperature("sda1"), (None, CellState::Unavailable));
        assert_eq!(temperature("dm-0"), (None, CellState::Unavailable));
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
            fixture.add_entity(entity_record(
                id,
                EntityKind::Filesystem,
                "",
                &[
                    (attributes::MOUNT_POINT, mount),
                    (attributes::MAJOR_MINOR, major_minor),
                ],
            ));
            fixture.add(
                "disk",
                sample("filesystem.total", id, "filesystem").u64(1000),
            );
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
