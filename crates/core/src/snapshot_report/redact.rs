//! A snapshot document made fit to share (`--snapshot-json --redact`,
//! D-145): what names the host, one unit of its hardware, a place on it or
//! one of its users is taken out, and every figure stays.
//!
//! * The host's name and id, also as the host entity's name.
//! * Each GPU's UUID, wherever it is written (entity ids, parents,
//!   attributes, a metric's entity, scope and source, a process's GPUs):
//!   it becomes `GPU-redacted-0`, `GPU-redacted-1`, … in the entities'
//!   order, so every row still names the same GPU.
//! * Every entity attribute not on [`KEPT_ATTRIBUTES`]: serial numbers,
//!   mount points, device paths, the CUDA toolkit's path, interface names,
//!   and whatever an adapter adds later, until it is put on that list.
//! * The names of filesystems and network interfaces, which are their
//!   mount point and their interface name: `filesystem-0`, `interface-1`.
//!   A source's id that names an interface or the host as one of its
//!   parts (`network:eno1:rx_bytes`) names the stand-in instead.
//! * Each GPU process's name and its owner's login name. The PID and the
//!   numeric uid stay.
//!
//! Model names, versions, modes, the kernel's own device names and
//! numbers, and the rest of the sources' ids (which name drivers and bus
//! addresses, not a unit) stay: they are what a reader needs to follow a
//! reading.

use super::{ReportEntity, SnapshotReport};
use crate::naming::{attributes, entity};
use crate::observation::EntityKind;

/// What stands where something was taken out.
pub const REDACTED: &str = "redacted";

/// The entity attributes a redacted document keeps as they are: a model,
/// a version, a mode, a count, or a place in the kernel's own numbering.
/// Any other attribute keeps its key and loses its value.
const KEPT_ATTRIBUTES: &[&str] = &[
    // Rewritten to the GPU's stand-in before it is kept.
    attributes::UUID,
    attributes::PCI_BUS_ID,
    attributes::DRIVER_VERSION,
    attributes::NVML_INDEX,
    attributes::MEMORY_MODEL,
    attributes::COMPUTE_MODE,
    attributes::PERSISTENCE_MODE,
    attributes::ARCHITECTURE,
    attributes::BRAND,
    attributes::COMPUTE_CAPABILITY,
    attributes::VBIOS_VERSION,
    attributes::INFOROM_IMAGE,
    attributes::GSP_FIRMWARE,
    attributes::BOARD_PART_NUMBER,
    attributes::MINOR_NUMBER,
    attributes::PCI_DEVICE_ID,
    attributes::PCI_SUBSYSTEM_ID,
    attributes::NVML_VERSION,
    attributes::CUDA_DRIVER_VERSION,
    attributes::KERNEL_MODULE,
    attributes::CUDA_TOOLKIT,
    attributes::NVCC_VERSION,
    attributes::CUDA_TOOLKITS,
    attributes::NUMA_NODES,
    attributes::CPU_AFFINITY,
    attributes::GPU_PEERS,
    attributes::C2C_LINKS,
    attributes::C2C_LINK_BANDWIDTH,
    attributes::FS_TYPE,
    attributes::MAJOR_MINOR,
    attributes::BLOCK_LAYER,
    attributes::OPERSTATE,
    attributes::CLK_TCK,
    attributes::CPU_MODEL,
    attributes::PHYSICAL_PACKAGE,
    // What the thermal and network adapters say of a sensor's zone and of
    // an interface: the firmware's zone names and the kernel's policies.
    "acpi_path",
    "policy",
    "type",
    "zone",
    "loopback",
];
/// Leads the attributes of a thermal zone's trip points (`trip_0`).
const TRIP_POINT_PREFIX: &str = "trip_";

impl SnapshotReport {
    /// The document with what names the host, its hardware and its users
    /// taken out (module docs); every figure and every link between its
    /// rows stays.
    #[must_use]
    pub fn redacted(mut self) -> Self {
        let gpus = GpuStandIns::of(&self.entities);
        let mut names = NamedParts::default();
        names.take(&self.host.name, REDACTED);
        self.host.name = REDACTED.to_owned();
        self.host.id = None;
        let mut numbers = KindNumbers::default();
        for entity in &mut self.entities {
            redact_entity(entity, &gpus, (&mut numbers, &mut names));
        }
        for metric in &mut self.metrics {
            gpus.rewrite(&mut metric.entity);
            gpus.rewrite(&mut metric.scope);
            if let Some(source) = &mut metric.source {
                gpus.rewrite(source);
                names.rewrite(source);
            }
        }
        for process in &mut self.processes {
            REDACTED.clone_into(&mut process.name);
            process.user = None;
            for held in &mut process.gpus {
                gpus.rewrite(&mut held.gpu);
            }
        }
        self.redacted = true;
        self
    }
}

/// Each GPU's UUID and what stands for it.
#[derive(Debug, Default)]
struct GpuStandIns(Vec<(String, String)>);

impl GpuStandIns {
    /// A stand-in for the UUID of each GPU among `entities`, numbered in
    /// their order. A GPU without a `uuid` attribute is known by what
    /// follows `gpu:` in its id.
    fn of(entities: &[ReportEntity]) -> Self {
        let uuids = entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Gpu)
            .filter_map(|entity| {
                entity
                    .attributes
                    .get(attributes::UUID)
                    .map(String::as_str)
                    .or_else(|| entity.id.split_once(':').map(|(_, rest)| rest))
                    .filter(|uuid| !uuid.is_empty())
                    .map(str::to_owned)
            });
        Self(
            uuids
                .enumerate()
                .map(|(number, uuid)| (uuid, format!("GPU-{REDACTED}-{number}")))
                .collect(),
        )
    }

    /// Writes each GPU's stand-in where `text` has its UUID.
    fn rewrite(&self, text: &mut String) {
        for (uuid, stand_in) in &self.0 {
            if text.contains(uuid.as_str()) {
                *text = text.replace(uuid.as_str(), stand_in);
            }
        }
    }
}

/// Between the parts of a source's id (`adapter:device:channel`).
const PART_SEPARATOR: char = ':';

/// The names taken out of the entities, with what stands for each: a
/// source's id may hold one as a part.
#[derive(Debug, Default)]
struct NamedParts(Vec<(String, String)>);

impl NamedParts {
    /// `name` stands as `stand_in` from now on; an empty name is none.
    fn take(&mut self, name: &str, stand_in: &str) {
        let known = self.0.iter().any(|(taken, _)| taken == name);
        if !name.is_empty() && !known {
            self.0.push((name.to_owned(), stand_in.to_owned()));
        }
    }

    /// Writes the stand-in for each part of `id` that is a taken name. A
    /// whole part only: `lo` is in many words.
    fn rewrite(&self, id: &mut String) {
        if !id
            .split(PART_SEPARATOR)
            .any(|part| self.stand_in(part).is_some())
        {
            return;
        }
        let parts: Vec<&str> = id
            .split(PART_SEPARATOR)
            .map(|part| self.stand_in(part).unwrap_or(part))
            .collect();
        *id = parts.join(&PART_SEPARATOR.to_string());
    }

    fn stand_in(&self, part: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(taken, _)| taken == part)
            .map(|(_, stand_in)| stand_in.as_str())
    }
}

/// How many entities of the kinds named by number came before.
#[derive(Debug, Default)]
struct KindNumbers {
    filesystems: usize,
    interfaces: usize,
}

impl KindNumbers {
    /// `filesystem-0`, `interface-1`: the name of the next entity of
    /// `kind`, for the kinds whose name is a place or a name the owner
    /// chose; `None` for a kind that keeps its name.
    fn next_name(&mut self, kind: EntityKind) -> Option<String> {
        let (label, number) = match kind {
            EntityKind::Filesystem => ("filesystem", &mut self.filesystems),
            EntityKind::NetworkInterface => ("interface", &mut self.interfaces),
            _ => return None,
        };
        let name = format!("{label}-{number}");
        *number += 1;
        Some(name)
    }
}

fn redact_entity(
    entity: &mut ReportEntity,
    gpus: &GpuStandIns,
    (numbers, names): (&mut KindNumbers, &mut NamedParts),
) {
    if entity.id == entity::HOST {
        names.take(&entity.name, REDACTED);
        REDACTED.clone_into(&mut entity.name);
    } else if let Some(name) = numbers.next_name(entity.kind) {
        names.take(&entity.name, &name);
        if let Some(interface) = entity.attributes.get(attributes::IFNAME) {
            names.take(interface, &name);
        }
        entity.name = name;
    }
    gpus.rewrite(&mut entity.id);
    if let Some(parent) = &mut entity.parent {
        gpus.rewrite(parent);
    }
    let kind = entity.kind;
    for (key, value) in &mut entity.attributes {
        if is_kept(kind, key) {
            gpus.rewrite(value);
        } else {
            REDACTED.clone_into(value);
        }
    }
}

/// Whether an entity of `kind` keeps its `attribute`: one on the list, a
/// thermal zone's trip point, or a block device's `device`, which is its
/// kernel name where a filesystem's is a path.
fn is_kept(kind: EntityKind, attribute: &str) -> bool {
    KEPT_ATTRIBUTES.contains(&attribute)
        || attribute.starts_with(TRIP_POINT_PREFIX)
        || (kind == EntityKind::BlockDevice && attribute == attributes::DEVICE)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::canonical::GpuUsageKind;
    use crate::metric::CanonicalUnit;
    use crate::quality::{Namespace, Quality, SampleKind, SelectionState};
    use crate::snapshot_report::{
        ReportHost, ReportMetric, ReportProcess, ReportProcessGpu, SNAPSHOT_FORMAT,
    };

    const UUID_A: &str = "GPU-aaaa1111-2222-3333-4444-555566667777";
    const UUID_B: &str = "GPU-bbbb1111-2222-3333-4444-555566667777";

    fn entity(
        id: &str,
        kind: EntityKind,
        name: &str,
        parent: Option<&str>,
        attributes: &[(&str, &str)],
    ) -> ReportEntity {
        ReportEntity {
            id: id.to_owned(),
            kind,
            name: name.to_owned(),
            parent: parent.map(str::to_owned),
            attributes: attributes
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn metric(entity: &str, scope: &str, source: Option<&str>) -> ReportMetric {
        ReportMetric {
            metric: "gpu.temperature".to_owned(),
            entity: entity.to_owned(),
            scope: scope.to_owned(),
            label: None,
            value: None,
            unit: CanonicalUnit::DegC,
            state: SelectionState::Selected,
            quality: Quality::Verified,
            kind: SampleKind::Measured,
            stale: false,
            age_ms: 10,
            source: source.map(str::to_owned),
            reasons: Vec::new(),
        }
    }

    /// A document of an invented host with everything that names it.
    fn document() -> SnapshotReport {
        SnapshotReport {
            format: SNAPSHOT_FORMAT,
            dgxtop_version: "9.9.9".to_owned(),
            host: ReportHost {
                id: Some("host-hash-1".to_owned()),
                name: "acme-node".to_owned(),
                kernel: Some("6.8.0-acme".to_owned()),
            },
            namespace: Namespace::Live,
            taken_unix_ms: 1,
            uptime_ms: 2,
            interval_ms: 1000,
            snapshot_revision: 3,
            adapters: Vec::new(),
            entities: vec![
                entity(
                    "host",
                    EntityKind::Host,
                    "acme-node",
                    None,
                    &[("clk_tck", "100")],
                ),
                entity("cpu", EntityKind::Host, "All CPUs", Some("host"), &[]),
                entity(
                    &format!("gpu:{UUID_A}"),
                    EntityKind::Gpu,
                    "Acme GPU 9000",
                    Some("host"),
                    &[
                        ("uuid", UUID_A),
                        ("serial", "1320000000001"),
                        ("nvml_index", "0"),
                        ("driver_version", "1.2.3"),
                        ("cuda_toolkit_path", "/home/alice/cuda"),
                        ("made_up_later", "alice's card"),
                    ],
                ),
                entity(
                    &format!("gpu:{UUID_B}"),
                    EntityKind::Gpu,
                    "Acme GPU 9000",
                    None,
                    &[],
                ),
                entity(
                    "blockdev:259:1",
                    EntityKind::BlockDevice,
                    "nvme0n1p1",
                    Some("host"),
                    &[("device", "nvme0n1p1"), ("major_minor", "259:1")],
                ),
                entity(
                    "fs:mnt:[1]:28",
                    EntityKind::Filesystem,
                    "/mnt/alice_ssd",
                    Some("blockdev:259:1"),
                    &[
                        ("mount_point", "/mnt/alice_ssd"),
                        ("mount_root", "/"),
                        ("device", "/dev/mapper/alice-home"),
                        ("fs_type", "ext4"),
                    ],
                ),
                entity("fs:mnt:[1]:29", EntityKind::Filesystem, "/", None, &[]),
                entity(
                    "netif:net:[1]:2",
                    EntityKind::NetworkInterface,
                    "enx001122334455",
                    Some("host"),
                    &[("ifname", "enx001122334455"), ("operstate", "up")],
                ),
                entity(
                    "sensor:thermal:acpitz:temp",
                    EntityKind::Sensor,
                    "acpitz",
                    Some("soc"),
                    &[
                        ("acpi_path", "\\_TZ_.TSOC"),
                        ("device", "/sys/devices/virtual/thermal/thermal_zone0"),
                        ("trip_0", "critical 104800 mdegC"),
                    ],
                ),
            ],
            metrics: vec![
                metric(
                    &format!("gpu:{UUID_A}"),
                    "gpu",
                    Some(&format!("nvml:{UUID_A}:temperature")),
                ),
                metric(
                    &format!("gpupid:{UUID_B}:4242"),
                    &format!("gpu:{UUID_B}"),
                    None,
                ),
                metric("memory", "system", Some("procfs:meminfo:MemTotal")),
                metric(
                    "netif:net:[1]:2",
                    "interface",
                    Some("network:enx001122334455:rx_bytes"),
                ),
                metric("host", "host", Some("probe:acme-node:lo:uptime")),
            ],
            processes: vec![ReportProcess {
                pid: 4242,
                start_time_ticks: 90_100,
                name: "alice_experiment".to_owned(),
                uid: Some(1000),
                user: Some("alice".to_owned()),
                state: Some('R'),
                cpu_percent: Some(103.0),
                rss_bytes: Some(1 << 30),
                gpus: vec![ReportProcessGpu {
                    gpu: format!("gpu:{UUID_B}"),
                    usage: GpuUsageKind::Compute,
                    utilization_percent: Some(96.0),
                    encoder_percent: None,
                    decoder_percent: None,
                    memory_bytes: Some(8 << 30),
                }],
            }],
            processes_truncated: false,
            redacted: false,
        }
    }

    /// Nothing that names the host, a unit of its hardware, a place on it
    /// or one of its users is left anywhere in the document, whatever
    /// field held it.
    #[test]
    fn nothing_that_names_the_host_or_its_users_is_left() {
        let plain = serde_json::to_string(&document()).unwrap();
        let redacted = document().redacted();
        let text = serde_json::to_string(&redacted).unwrap();
        for secret in [
            "acme-node",
            "host-hash-1",
            "aaaa1111",
            "bbbb1111",
            "1320000000001",
            "alice",
            "enx001122334455",
            "/mnt/",
            "/dev/",
            "/sys/",
            "/home/",
        ] {
            assert!(plain.contains(secret), "the fixture holds {secret}");
            assert!(!text.contains(secret), "{secret} is left: {text}");
        }
        assert!(redacted.redacted && !document().redacted);
        assert_eq!(redacted.host.name, REDACTED);
        assert_eq!(redacted.host.id, None);
        assert_eq!(redacted.host.kernel.as_deref(), Some("6.8.0-acme"));
    }

    /// Each GPU keeps one stand-in wherever it was named, so a metric and
    /// a process still point at their GPU's entity.
    #[test]
    fn every_row_still_names_its_gpu() {
        let redacted = document().redacted();
        let ids: Vec<&str> = redacted
            .entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Gpu)
            .map(|entity| entity.id.as_str())
            .collect();
        assert_eq!(ids, vec!["gpu:GPU-redacted-0", "gpu:GPU-redacted-1"]);
        let first = &redacted.entities[2];
        assert_eq!(first.attributes["uuid"], "GPU-redacted-0");
        assert_eq!(redacted.metrics[0].entity, "gpu:GPU-redacted-0");
        assert_eq!(
            redacted.metrics[0].source.as_deref(),
            Some("nvml:GPU-redacted-0:temperature")
        );
        assert_eq!(redacted.metrics[1].entity, "gpupid:GPU-redacted-1:4242");
        assert_eq!(redacted.metrics[1].scope, "gpu:GPU-redacted-1");
        assert_eq!(redacted.processes[0].gpus[0].gpu, "gpu:GPU-redacted-1");
        // A metric of no GPU is as it was.
        assert_eq!(redacted.metrics[2], document().metrics[2]);
    }

    /// A source's id that names an interface or the host as one of its
    /// parts names the stand-in; a part that only holds such a name, or
    /// another interface's, is as it was.
    #[test]
    fn a_source_names_the_stand_in_of_an_interface_or_the_host() {
        let redacted = document().redacted();
        assert_eq!(
            redacted.metrics[3].source.as_deref(),
            Some("network:interface-0:rx_bytes")
        );
        assert_eq!(
            redacted.metrics[4].source.as_deref(),
            Some("probe:redacted:lo:uptime")
        );
        let mut names = NamedParts::default();
        names.take("lo", "interface-0");
        names.take("lo", "interface-9");
        names.take("", "interface-1");
        let mut id = "procfs:loadavg:lo:overflow".to_owned();
        names.rewrite(&mut id);
        assert_eq!(id, "procfs:loadavg:interface-0:overflow");
    }

    /// An attribute on the list stays; any other keeps its key and loses
    /// its value, one an adapter adds later too. Filesystems and
    /// interfaces are named by number; models and the kernel's device
    /// names stay.
    #[test]
    fn attributes_stay_only_when_listed_and_places_are_numbered() {
        let redacted = document().redacted();
        let by_id = |id: &str| -> &ReportEntity {
            redacted
                .entities
                .iter()
                .find(|entity| entity.id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
        };
        let gpu = by_id("gpu:GPU-redacted-0");
        assert_eq!(gpu.name, "Acme GPU 9000");
        assert_eq!(gpu.attributes["driver_version"], "1.2.3");
        assert_eq!(gpu.attributes["nvml_index"], "0");
        for taken in ["serial", "cuda_toolkit_path", "made_up_later"] {
            assert_eq!(gpu.attributes[taken], REDACTED, "{taken}");
        }
        assert_eq!(by_id("host").name, REDACTED);
        assert_eq!(by_id("host").attributes["clk_tck"], "100");
        assert_eq!(by_id("cpu").name, "All CPUs");
        let disk = by_id("blockdev:259:1");
        assert_eq!(disk.name, "nvme0n1p1");
        assert_eq!(disk.attributes["major_minor"], "259:1");
        assert_eq!(disk.attributes["device"], "nvme0n1p1", "its kernel name");
        let home = by_id("fs:mnt:[1]:28");
        assert_eq!(home.name, "filesystem-0");
        assert_eq!(home.parent.as_deref(), Some("blockdev:259:1"));
        assert_eq!(home.attributes["fs_type"], "ext4");
        for taken in ["mount_point", "mount_root", "device"] {
            assert_eq!(home.attributes[taken], REDACTED, "{taken}");
        }
        assert_eq!(by_id("fs:mnt:[1]:29").name, "filesystem-1");
        let interface = by_id("netif:net:[1]:2");
        assert_eq!(interface.name, "interface-0");
        assert_eq!(interface.attributes["ifname"], REDACTED);
        assert_eq!(interface.attributes["operstate"], "up");
        let sensor = by_id("sensor:thermal:acpitz:temp");
        assert_eq!(sensor.name, "acpitz");
        assert_eq!(sensor.attributes["acpi_path"], "\\_TZ_.TSOC");
        assert_eq!(sensor.attributes["trip_0"], "critical 104800 mdegC");
        assert_eq!(sensor.attributes["device"], REDACTED);
    }

    /// A process keeps its PID, its numeric owner and every figure; its
    /// name and its owner's login name go.
    #[test]
    fn a_process_keeps_its_figures_and_loses_its_names() {
        let redacted = document().redacted();
        let [process] = redacted.processes.as_slice() else {
            panic!("one process");
        };
        assert_eq!(process.name, REDACTED);
        assert_eq!(process.user, None);
        assert_eq!((process.pid, process.uid), (4242, Some(1000)));
        assert_eq!(process.start_time_ticks, 90_100);
        assert_eq!(process.cpu_percent, Some(103.0));
        assert_eq!(process.gpus[0].memory_bytes, Some(8 << 30));
    }
}
