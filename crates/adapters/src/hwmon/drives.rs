//! Drive temperatures (D-92): which block devices an hwmon drive sensor
//! measures, proven through sysfs links and never guessed from names.
//!
//! * NVMe (`nvme` driver): the hwmon `device` is the controller
//!   (`.../nvme/nvmeN`) and its `Composite` channel is the drive's
//!   temperature (the NVMe composite temperature). Every namespace block
//!   device below the controller (`.../nvme/nvmeN/nvmeNnM`) is that drive.
//! * SATA/SAS (`drivetemp`): the hwmon `device` is the SCSI device
//!   (`.../targetH:C:T/H:C:T:L`) and `temp1` is the drive's temperature; the
//!   disk below it (`.../H:C:T:L/block/sdX`) is that drive.
//! * Native NVMe multipath: a namespace head (`.../nvme-subsysK/nvmeKnM`)
//!   lies below no controller; its `multipath/` entries link its paths. The
//!   head is the drive of a sensor only when every path lies below that
//!   sensor's controller.
//!
//! Partitions are left out (the disk holding them is the drive, and procfs
//! names that disk as their parent), as are hidden block devices (multipath
//! path devices) and disks of drives without a sensor. A disk two sensors
//! would claim is claimed by neither.

use std::collections::BTreeMap;
use std::io;

use super::profile::{ChannelKind, label_is};
use super::sysfs::{canonical, is_missing, list_names, read_optional};
use crate::common::SystemFiles;

/// The block device class: one entry per disk and partition.
const BLOCK_CLASS: &str = "/sys/class/block";

/// hwmon drivers whose device link names a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriveDriver {
    Nvme,
    Drivetemp,
}

impl DriveDriver {
    /// The drive driver of an hwmon device `name`, if it is one.
    pub(crate) fn of(name: &str) -> Option<Self> {
        match name {
            "nvme" => Some(Self::Nvme),
            "drivetemp" => Some(Self::Drivetemp),
            _ => None,
        }
    }

    /// Whether a channel is the drive's own temperature: NVMe's
    /// `Composite` (its `Sensor N` channels are single points inside the
    /// drive), or drivetemp's only channel.
    pub(crate) fn is_drive_temperature(
        self,
        kind: ChannelKind,
        index: u32,
        label: Option<&str>,
    ) -> bool {
        kind == ChannelKind::Temperature
            && match self {
                Self::Nvme => label_is(label, "Composite"),
                Self::Drivetemp => index == 1,
            }
    }
}

/// A whole-disk block device (`blockdev:MAJ:MIN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BlockDisk {
    pub major: u32,
    pub minor: u32,
}

/// Static facts of one block device instance (its canonical sysfs path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockFacts {
    /// `None` for partitions, hidden devices and unreadable numbers.
    disk: Option<BlockDisk>,
}

/// One whole disk and the canonical paths placing it in the device tree:
/// its own directory, or the paths of a multipath head.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacedDisk {
    disk: BlockDisk,
    placements: Vec<String>,
}

/// Block device facts cached by canonical path; a device is re-read only
/// when its instance changes.
#[derive(Debug, Default)]
pub(crate) struct BlockCache {
    facts: BTreeMap<String, BlockFacts>,
}

impl BlockCache {
    /// The disks each drive sensor measures, by the sensor's canonical
    /// device path. A missing block class means no disks.
    pub(crate) fn assign<'a>(
        &mut self,
        files: &dyn SystemFiles,
        sensors: impl IntoIterator<Item = &'a str>,
    ) -> io::Result<BTreeMap<String, Vec<BlockDisk>>> {
        let disks = self.placed_disks(files)?;
        let sensors: Vec<&str> = sensors.into_iter().collect();
        let mut assigned: BTreeMap<String, Vec<BlockDisk>> = BTreeMap::new();
        for placed in &disks {
            let mut owners = sensors.iter().filter(|sensor| placed_below(placed, sensor));
            if let (Some(owner), None) = (owners.next(), owners.next()) {
                assigned
                    .entry((*owner).to_owned())
                    .or_default()
                    .push(placed.disk);
            }
        }
        for disks in assigned.values_mut() {
            disks.sort();
        }
        Ok(assigned)
    }

    fn placed_disks(&mut self, files: &dyn SystemFiles) -> io::Result<Vec<PlacedDisk>> {
        let names = match list_names(files, BLOCK_CLASS) {
            Ok(names) => names,
            Err(error) if is_missing(&error) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut live = BTreeMap::new();
        let mut placed = Vec::new();
        for name in names {
            let Ok(instance) = canonical(files, &format!("{BLOCK_CLASS}/{name}")) else {
                continue;
            };
            let facts = match self.facts.get(&instance) {
                Some(facts) => *facts,
                None => read_facts(files, &instance),
            };
            live.insert(instance.clone(), facts);
            if let Some(disk) = facts.disk {
                placed.push(PlacedDisk {
                    disk,
                    placements: placements(files, &instance),
                });
            }
        }
        self.facts = live;
        Ok(placed)
    }
}

/// Whether every placement of a disk lies below the sensor's device.
fn placed_below(placed: &PlacedDisk, sensor_device: &str) -> bool {
    let prefix = format!("{}/", sensor_device.trim_end_matches('/'));
    !placed.placements.is_empty()
        && placed
            .placements
            .iter()
            .all(|placement| placement.starts_with(&prefix))
}

/// A multipath head is placed by its paths; any other disk by itself.
fn placements(files: &dyn SystemFiles, instance: &str) -> Vec<String> {
    let multipath = format!("{instance}/multipath");
    match list_names(files, &multipath) {
        Ok(paths) => paths
            .iter()
            .filter_map(|path| canonical(files, &format!("{multipath}/{path}")).ok())
            .collect(),
        Err(_) => vec![instance.to_owned()],
    }
}

fn read_facts(files: &dyn SystemFiles, instance: &str) -> BlockFacts {
    let partition = files.exists(&format!("{instance}/partition"));
    let hidden = matches!(
        read_optional(files, &format!("{instance}/hidden")),
        Ok(Some(flag)) if flag == "1"
    );
    let disk = if partition || hidden {
        None
    } else {
        read_optional(files, &format!("{instance}/dev"))
            .ok()
            .flatten()
            .and_then(|dev| parse_dev(&dev))
    };
    BlockFacts { disk }
}

/// `MAJ:MIN` of a block device's `dev` file.
pub(crate) fn parse_dev(text: &str) -> Option<BlockDisk> {
    let (major, minor) = text.trim().split_once(':')?;
    Some(BlockDisk {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_nvme_and_drivetemp_are_drive_drivers() {
        assert_eq!(DriveDriver::of("nvme"), Some(DriveDriver::Nvme));
        assert_eq!(DriveDriver::of("drivetemp"), Some(DriveDriver::Drivetemp));
        assert_eq!(DriveDriver::of("acpitz"), None);
        assert_eq!(DriveDriver::of("mt7925_phy0"), None);
    }

    #[test]
    fn the_drive_temperature_is_the_composite_or_drivetemps_only_channel() {
        let temperature = ChannelKind::Temperature;
        let nvme = DriveDriver::Nvme;
        assert!(nvme.is_drive_temperature(temperature, 1, Some("Composite")));
        assert!(!nvme.is_drive_temperature(temperature, 2, Some("Sensor 1")));
        assert!(!nvme.is_drive_temperature(temperature, 1, None));
        assert!(!nvme.is_drive_temperature(ChannelKind::PowerInput, 1, Some("Composite")));
        let sata = DriveDriver::Drivetemp;
        assert!(sata.is_drive_temperature(temperature, 1, None));
        assert!(!sata.is_drive_temperature(temperature, 2, None));
    }

    #[test]
    fn device_numbers_parse_or_are_refused() {
        assert_eq!(
            parse_dev("259:0\n"),
            Some(BlockDisk {
                major: 259,
                minor: 0
            })
        );
        assert_eq!(parse_dev("8"), None);
        assert_eq!(parse_dev("8:x"), None);
        assert_eq!(parse_dev(""), None);
    }

    fn placed(placements: &[&str]) -> PlacedDisk {
        PlacedDisk {
            disk: BlockDisk { major: 8, minor: 0 },
            placements: placements.iter().map(|path| (*path).to_owned()).collect(),
        }
    }

    /// `nvme1` is not below `nvme10`, and a head with no paths is nowhere.
    #[test]
    fn placement_is_a_whole_path_component_prefix() {
        let controller = "/sys/devices/pci0000:00/0000:01:00.0/nvme/nvme1";
        assert!(placed_below(
            &placed(&[&format!("{controller}/nvme1n1")]),
            controller
        ));
        assert!(!placed_below(
            &placed(&["/sys/devices/pci0000:00/0000:01:00.0/nvme/nvme10/nvme10n1"]),
            controller
        ));
        assert!(!placed_below(&placed(&[]), controller));
        assert!(!placed_below(
            &placed(&[&format!("{controller}/nvme1c1n1"), "/elsewhere/nvme2c2n1"]),
            controller
        ));
    }
}
