//! Telemetry naming contract shared by adapters, collectors, the store and the
//! presentation layer: adapter/collector ids, read groups, entity id and scope
//! spellings, source id format and the routing of read groups to collectors.
//!
//! Adapters emit raw records keyed by these names; collectors only accept
//! records whose `(adapter, read_group)` they consume and whose metric ids
//! appear in [`crate::metric::METRIC_CATALOG`].
//!
//! | Adapter | Read group | Period | Emits (metric id, native unit) |
//! |---|---|---|---|
//! | procfs | `cpu` | 1 s | `cpu.time.*` (ticks, counter; entity `cpu`, scope `host` for the aggregate line and entity `cpu.core.N`, scope `core:N` per logical CPU; guest fields are NOT emitted because they are already inside user/nice), `cpu.load.{1m,5m,15m}` and `cpu.tasks.{running,total}` (count; entity `host`, scope `host`), `cpu.frequency.current` (kHz, gauge, per core: `cpuinfo_avg_freq` where the kernel offers it, else `scaling_cur_freq`, D-67) |
//! | procfs | `memory` | 1 s | `memory.{total,available,buffers,cached,swap_total}`, `memory.free`, `memory.swap_free` (KiB; entity `memory`, scope `system`) |
//! | procfs | `disk` | 1 s | `disk.{reads.completed,sectors.read,read.time,writes.completed,sectors.written,write.time}` (count / sectors_512 / milliseconds counters; entity `blockdev:MAJ:MIN`, scope `device` or `partition`) |
//! | procfs | `process` | 1 s, paged | `process.cpu.ticks` (ticks counter) and `process.rss` (bytes) per `ProcessKey` entity (`process:NS:PID:START`, scope `process`) |
//! | hwmon | `fast` | 1 s | temperatures (mdegC), power (uW), fans (count) mapped by profile: `cpu.temperature.package`/`core`, `cpu.power.package`, `cpu.power.cluster`, `cpu.power.platform`, else `sensor.*` (diagnostics) |
//! | thermal | `fast` | 1 s | zone temperatures (mdegC): `cpu.temperature.package` only when the profile proves the package, `soc.temperature` for SoC zones, GB10's `acpitz` zones by ACPI path (`cpu.temperature.cluster` scopes `cluster:0e`…`cluster:1p`, `soc.temperature`, D-68), else `sensor.temperature` |
//! | powercap | `fast` | 1 s | `cpu.energy.package` / `cpu.energy.platform` (uJ counters with declared modulus), else `sensor.energy` |
//! | nvml | `fast` | 1 s | GPU utilization/temperature/power/clocks/fan/framebuffer/PCIe/encoder/decoder/process_allocated (entity `gpu:UUID`) |
//! | nvml | `slow_health` | 5 s | `gpu.energy.total` (mJ counter), ECC, retired pages, `gpu.nvlink.active`, `gpu.power.limit`, `gpu.clock.maximum`, framebuffer total/reserved |
//! | nvml | `process` | 1 s | `process.gpu.memory_allocated` (bytes) and `process.gpu.utilization` (percent): entity `gpupid:UUID:PID`, scope `gpu:UUID` (the PID is joined to a ProcessKey by the process collector) |
//! | network | `fast` | 1 s | `network.{rx,tx}.bytes` (bytes counters), `network.{rx,tx}.{packets,errors,drops}` (count counters): entity `netif:NETNS:IFINDEX`, scope `interface` |
//! | filesystem | `capacity` | 5 s | `filesystem.{total,free,available}` (bytes): entity `fs:MNTNS:MOUNT_ID`, scope `filesystem` |

use crate::identity::{AdapterId, CollectorId, EntityId, IdentityError, Scope};

/// Adapter ids.
pub mod adapters {
    pub const PROCFS: &str = "procfs";
    pub const HWMON: &str = "hwmon";
    pub const THERMAL: &str = "thermal";
    pub const POWERCAP: &str = "powercap";
    pub const NVML: &str = "nvml";
    pub const NETWORK: &str = "network";
    pub const FILESYSTEM: &str = "filesystem";
    pub const REPLAY: &str = "replay";

    /// The seven live adapters, in host start order.
    pub const LIVE: [&str; 7] = [PROCFS, HWMON, THERMAL, POWERCAP, NVML, NETWORK, FILESYSTEM];
}

/// Collector ids.
pub mod collectors {
    pub const CPU: &str = "cpu";
    pub const GPU: &str = "gpu";
    pub const MEMORY: &str = "memory";
    pub const DISK: &str = "disk";
    pub const NETWORK: &str = "network";
    pub const PROCESS: &str = "process";

    pub const ALL: [&str; 6] = [CPU, GPU, MEMORY, DISK, NETWORK, PROCESS];
}

/// Read group names.
pub mod groups {
    pub const CPU: &str = "cpu";
    pub const MEMORY: &str = "memory";
    pub const DISK: &str = "disk";
    pub const PROCESS: &str = "process";
    pub const FAST: &str = "fast";
    pub const SLOW_HEALTH: &str = "slow_health";
    pub const CAPACITY: &str = "capacity";
}

/// Which period class a read group follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeriodClass {
    /// `sampling.interval_ms`.
    Fast,
    /// `sampling.capacity_interval_ms`.
    Capacity,
    /// `sampling.slow_health_interval_ms`.
    SlowHealth,
}

/// One schedulable read group of one adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadGroupSpec {
    pub adapter: &'static str,
    pub group: &'static str,
    pub period: PeriodClass,
    /// Whether the group may return several pages per cycle.
    pub paged: bool,
    /// Collectors fed by this group.
    pub collectors: &'static [&'static str],
}

/// Every scheduled read group of the live adapters.
pub const READ_GROUPS: &[ReadGroupSpec] = &[
    ReadGroupSpec {
        adapter: adapters::PROCFS,
        group: groups::CPU,
        period: PeriodClass::Fast,
        paged: true,
        collectors: &[collectors::CPU],
    },
    ReadGroupSpec {
        adapter: adapters::PROCFS,
        group: groups::MEMORY,
        period: PeriodClass::Fast,
        paged: false,
        collectors: &[collectors::MEMORY],
    },
    ReadGroupSpec {
        adapter: adapters::PROCFS,
        group: groups::DISK,
        period: PeriodClass::Fast,
        paged: true,
        collectors: &[collectors::DISK],
    },
    ReadGroupSpec {
        adapter: adapters::PROCFS,
        group: groups::PROCESS,
        period: PeriodClass::Fast,
        paged: true,
        collectors: &[collectors::PROCESS],
    },
    ReadGroupSpec {
        adapter: adapters::HWMON,
        group: groups::FAST,
        period: PeriodClass::Fast,
        paged: true,
        collectors: &[collectors::CPU],
    },
    ReadGroupSpec {
        adapter: adapters::THERMAL,
        group: groups::FAST,
        period: PeriodClass::Fast,
        paged: false,
        collectors: &[collectors::CPU],
    },
    ReadGroupSpec {
        adapter: adapters::POWERCAP,
        group: groups::FAST,
        period: PeriodClass::Fast,
        paged: false,
        collectors: &[collectors::CPU],
    },
    ReadGroupSpec {
        adapter: adapters::NVML,
        group: groups::FAST,
        period: PeriodClass::Fast,
        paged: true,
        collectors: &[collectors::GPU],
    },
    ReadGroupSpec {
        adapter: adapters::NVML,
        group: groups::SLOW_HEALTH,
        period: PeriodClass::SlowHealth,
        paged: true,
        collectors: &[collectors::GPU],
    },
    ReadGroupSpec {
        adapter: adapters::NVML,
        group: groups::PROCESS,
        period: PeriodClass::Fast,
        paged: true,
        collectors: &[collectors::PROCESS, collectors::GPU],
    },
    ReadGroupSpec {
        adapter: adapters::NETWORK,
        group: groups::FAST,
        period: PeriodClass::Fast,
        paged: true,
        collectors: &[collectors::NETWORK],
    },
    ReadGroupSpec {
        adapter: adapters::FILESYSTEM,
        group: groups::CAPACITY,
        period: PeriodClass::Capacity,
        paged: true,
        collectors: &[collectors::DISK],
    },
];

/// Read groups of one adapter.
pub fn read_groups_of(adapter: &str) -> impl Iterator<Item = &'static ReadGroupSpec> + '_ {
    READ_GROUPS
        .iter()
        .filter(move |spec| spec.adapter == adapter)
}

/// Collectors fed by `(adapter, read_group)`; `read_group` without page suffix.
///
/// Replay adapters (ids starting with `replay`) mirror live groups with the
/// read group spelling `<live adapter>.<group>` (e.g. `hwmon.fast`) and are
/// routed exactly like the live group they imitate.
pub fn collectors_for(adapter: &str, read_group: &str) -> &'static [&'static str] {
    let (adapter, read_group) = if is_replay_adapter(adapter) {
        match read_group.split_once('.') {
            Some((live_adapter, live_group)) => (live_adapter, live_group),
            None => return &[],
        }
    } else {
        (adapter, read_group)
    };
    READ_GROUPS
        .iter()
        .find(|spec| spec.adapter == adapter && spec.group == read_group)
        .map_or(&[], |spec| spec.collectors)
}

/// Whether an adapter id denotes a replay adapter (simulated/replay namespace).
pub fn is_replay_adapter(adapter: &str) -> bool {
    adapter == adapters::REPLAY || adapter.starts_with("replay-")
}

/// Replay read group mirroring a live group (`<live adapter>.<group>`).
pub fn replay_group(live_adapter: &str, live_group: &str) -> String {
    format!("{live_adapter}.{live_group}")
}

/// Key of `PolicySnapshot::effective_periods_ms` for a read group.
pub fn period_key(adapter: &str, read_group: &str) -> String {
    format!("{adapter}/{read_group}")
}

/// Key of `PolicySnapshot::effective_periods_ms` used for `RawKind::Static`
/// sources, whose values are refreshed by discovery (static metadata).
pub fn static_period_key(adapter: &str) -> String {
    format!("{adapter}/discovery")
}

/// Builds a source id `adapter:device:channel` (bounded, control-free).
pub fn source_id(
    adapter: &str,
    device: &str,
    channel: &str,
) -> Result<crate::identity::SourceId, IdentityError> {
    crate::identity::SourceId::new(format!("{adapter}:{device}:{channel}"))
}

/// Entity and scope spellings.
pub mod entity {
    use super::{EntityId, IdentityError, Scope};

    pub const HOST: &str = "host";
    pub const CPU_AGGREGATE: &str = "cpu";
    pub const MEMORY: &str = "memory";
    pub const SOC: &str = "soc";

    pub const SCOPE_HOST: &str = "host";
    pub const SCOPE_SYSTEM: &str = "system";
    pub const SCOPE_SOC: &str = "soc";
    pub const SCOPE_GPU: &str = "gpu";
    pub const SCOPE_DEVICE: &str = "device";
    pub const SCOPE_PARTITION: &str = "partition";
    pub const SCOPE_FILESYSTEM: &str = "filesystem";
    pub const SCOPE_INTERFACE: &str = "interface";
    pub const SCOPE_PROCESS: &str = "process";
    pub const SCOPE_SENSOR: &str = "sensor";
    pub const SCOPE_PLATFORM: &str = "platform";
    /// Scopes of `gpu.temperature.threshold` and `gpu.pcie.link.maximum`.
    pub const SCOPE_SLOWDOWN: &str = "slowdown";
    pub const SCOPE_SHUTDOWN: &str = "shutdown";
    /// Hottest the GPU runs before it slows below its base clock.
    pub const SCOPE_MAX_OPERATING: &str = "max_operating";
    /// The temperature the fan control aims for.
    pub const SCOPE_TARGET: &str = "target";
    pub const SCOPE_LINK_GENERATION: &str = "generation";
    pub const SCOPE_LINK_WIDTH: &str = "width";
    /// Scopes of `gpu.throttle.time`: why the clocks were held back.
    pub const SCOPE_THROTTLE_POWER: &str = "power";
    pub const SCOPE_THROTTLE_THERMAL: &str = "thermal";
    pub const SCOPE_THROTTLE_SYNC_BOOST: &str = "sync_boost";
    pub const SCOPE_THROTTLE_BOARD_LIMIT: &str = "board_limit";
    pub const SCOPE_THROTTLE_RELIABILITY: &str = "reliability";

    fn entity(value: String) -> Result<EntityId, IdentityError> {
        EntityId::new(value)
    }

    fn scope(value: String) -> Result<Scope, IdentityError> {
        Scope::new(value)
    }

    /// Logical CPU `N` (the kernel `cpuN`, not the read order).
    pub fn cpu_core(cpu: u32) -> Result<EntityId, IdentityError> {
        entity(format!("cpu.core.{cpu}"))
    }

    pub fn scope_core(cpu: u32) -> Result<Scope, IdentityError> {
        scope(format!("core:{cpu}"))
    }

    /// Physical package `P`.
    pub fn cpu_package(package: u32) -> Result<EntityId, IdentityError> {
        entity(format!("cpu.package.{package}"))
    }

    pub fn scope_package(package: u32) -> Result<Scope, IdentityError> {
        scope(format!("package:{package}"))
    }

    /// Per-physical-core temperature scope inside a package.
    pub fn scope_physical_core(core: u32) -> Result<Scope, IdentityError> {
        scope(format!("core:{core}"))
    }

    /// CPU cluster power component (e.g. GB10 `cpu_p`/`cpu_e`).
    pub fn scope_cluster(name: &str) -> Result<Scope, IdentityError> {
        scope(format!("cluster:{name}"))
    }

    /// GPU identified by UUID (never by enumeration index).
    pub fn gpu(uuid: &str) -> Result<EntityId, IdentityError> {
        entity(format!("gpu:{uuid}"))
    }

    pub fn scope_gpu_fan(fan: u32) -> Result<Scope, IdentityError> {
        scope(format!("fan:{fan}"))
    }

    /// GPU-side process association before joining to a ProcessKey.
    pub fn gpu_pid(uuid: &str, pid: i32) -> Result<EntityId, IdentityError> {
        entity(format!("gpupid:{uuid}:{pid}"))
    }

    pub fn scope_gpu_of(uuid: &str) -> Result<Scope, IdentityError> {
        scope(format!("gpu:{uuid}"))
    }

    /// Block device by `major:minor` (stable within a boot).
    pub fn block_device(major: u32, minor: u32) -> Result<EntityId, IdentityError> {
        entity(format!("blockdev:{major}:{minor}"))
    }

    /// Mounted filesystem by mount namespace and mount id.
    pub fn filesystem(mount_namespace: &str, mount_id: u64) -> Result<EntityId, IdentityError> {
        entity(format!("fs:{mount_namespace}:{mount_id}"))
    }

    /// Network interface by network namespace and ifindex.
    pub fn network_interface(net_namespace: &str, ifindex: u32) -> Result<EntityId, IdentityError> {
        entity(format!("netif:{net_namespace}:{ifindex}"))
    }

    /// Unclassified sensor channel (diagnostics only).
    pub fn sensor(adapter: &str, device: &str, channel: &str) -> Result<EntityId, IdentityError> {
        entity(format!("sensor:{adapter}:{device}:{channel}"))
    }

    /// Powercap/RAPL domain.
    pub fn power_domain(domain: &str) -> Result<EntityId, IdentityError> {
        entity(format!("power:{domain}"))
    }

    /// Parses `gpupid:UUID:PID` back into `(uuid, pid)`.
    pub fn parse_gpu_pid(entity_id: &str) -> Option<(&str, i32)> {
        let rest = entity_id.strip_prefix("gpupid:")?;
        let (uuid, pid) = rest.rsplit_once(':')?;
        Some((uuid, pid.parse().ok()?))
    }

    /// Parses `gpu:UUID` back into the UUID.
    pub fn parse_gpu(entity_id: &str) -> Option<&str> {
        entity_id.strip_prefix("gpu:")
    }
}

/// Well-known entity attribute keys.
pub mod attributes {
    pub const UUID: &str = "uuid";
    pub const PCI_BUS_ID: &str = "pci_bus_id";
    pub const DRIVER_VERSION: &str = "driver_version";
    pub const NVML_INDEX: &str = "nvml_index";
    pub const SERIAL: &str = "serial";
    pub const MEMORY_MODEL: &str = "memory_model";
    pub const COMPUTE_MODE: &str = "compute_mode";
    pub const PERSISTENCE_MODE: &str = "persistence_mode";
    // GPU identity and firmware (D-48).
    pub const ARCHITECTURE: &str = "architecture";
    pub const BRAND: &str = "brand";
    /// CUDA compute capability, `major.minor` (`12.0` is `sm_120`).
    pub const COMPUTE_CAPABILITY: &str = "compute_capability";
    pub const VBIOS_VERSION: &str = "vbios_version";
    pub const INFOROM_IMAGE: &str = "inforom_image";
    pub const GSP_FIRMWARE: &str = "gsp_firmware";
    pub const BOARD_PART_NUMBER: &str = "board_part_number";
    /// `N` of `/dev/nvidiaN`.
    pub const MINOR_NUMBER: &str = "minor_number";
    /// `vendor:device` PCI ids (`10de:2c02`).
    pub const PCI_DEVICE_ID: &str = "pci_device_id";
    pub const PCI_SUBSYSTEM_ID: &str = "pci_subsystem_id";
    // The driver and CUDA stack the GPU runs on (D-48).
    pub const NVML_VERSION: &str = "nvml_version";
    /// Newest CUDA version the driver supports (`13.2`).
    pub const CUDA_DRIVER_VERSION: &str = "cuda_driver_version";
    /// `open` or `proprietary`, with the compiler that built it.
    pub const KERNEL_MODULE: &str = "kernel_module";
    /// The default CUDA toolkit (`/usr/local/cuda`), its version and path.
    pub const CUDA_TOOLKIT: &str = "cuda_toolkit";
    pub const CUDA_TOOLKIT_PATH: &str = "cuda_toolkit_path";
    /// The default toolkit's `nvcc` version.
    pub const NVCC_VERSION: &str = "nvcc_version";
    /// Every toolkit found, newest first (`13.4, 12.8`).
    pub const CUDA_TOOLKITS: &str = "cuda_toolkits";
    // Where the GPU sits (D-54).
    /// NUMA nodes nearest the GPU's memory (`0`, `0,1`).
    pub const NUMA_NODES: &str = "numa_nodes";
    /// CPUs nearest the GPU, as a Linux CPU list (`0-31`).
    pub const CPU_AFFINITY: &str = "cpu_affinity";
    /// The path to and peer access with each other GPU
    /// ([`crate::gpu_state::format_peers`]).
    pub const GPU_PEERS: &str = "gpu_peers";
    /// Active NVLink-C2C links to the CPU (`8`, D-56).
    pub const C2C_LINKS: &str = "c2c_links";
    /// The highest bandwidth of one C2C link in bytes per second.
    pub const C2C_LINK_BANDWIDTH: &str = "c2c_link_bandwidth";
    pub const MOUNT_POINT: &str = "mount_point";
    pub const DEVICE: &str = "device";
    pub const FS_TYPE: &str = "fs_type";
    pub const MAJOR_MINOR: &str = "major_minor";
    pub const IFNAME: &str = "ifname";
    pub const OPERSTATE: &str = "operstate";
    pub const UID: &str = "uid";
    pub const COMM: &str = "comm";
    pub const CLK_TCK: &str = "clk_tck";
    pub const PAGE_SIZE: &str = "page_size";
    pub const CPU_MODEL: &str = "cpu_model";
    pub const KERNEL: &str = "kernel";
    pub const HOSTNAME: &str = "hostname";
    pub const PID_NAMESPACE: &str = "pid_namespace";
    pub const PHYSICAL_PACKAGE: &str = "physical_package";
}

/// Typed ids for the built-in adapters/collectors.
pub fn adapter_id(name: &str) -> Result<AdapterId, IdentityError> {
    AdapterId::new(name)
}

pub fn collector_id(name: &str) -> Result<CollectorId, IdentityError> {
    CollectorId::new(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_covers_every_collector() {
        for collector in collectors::ALL {
            assert!(
                READ_GROUPS
                    .iter()
                    .any(|spec| spec.collectors.contains(&collector)),
                "{collector} receives no read group"
            );
        }
        assert_eq!(
            collectors_for(adapters::NVML, groups::PROCESS),
            &[collectors::PROCESS, collectors::GPU]
        );
        assert!(collectors_for("unknown", "x").is_empty());
        assert_eq!(collectors_for("replay-3", "hwmon.fast"), &[collectors::CPU]);
        assert_eq!(
            collectors_for("replay", "procfs.memory"),
            &[collectors::MEMORY]
        );
        assert!(collectors_for("replay", "fast").is_empty());
        assert!(is_replay_adapter("replay-1"));
        assert!(!is_replay_adapter("replayer"));
        assert_eq!(read_groups_of(adapters::PROCFS).count(), 4);
    }

    #[test]
    fn entity_spellings_are_valid_identities() {
        assert_eq!(entity::cpu_core(3).unwrap().as_str(), "cpu.core.3");
        assert_eq!(entity::gpu("GPU-1").unwrap().as_str(), "gpu:GPU-1");
        assert_eq!(entity::parse_gpu("gpu:GPU-1"), Some("GPU-1"));
        let pid = entity::gpu_pid("GPU-abc", 42).unwrap();
        assert_eq!(entity::parse_gpu_pid(pid.as_str()), Some(("GPU-abc", 42)));
        assert!(entity::parse_gpu_pid("gpupid:x").is_none());
        assert_eq!(
            source_id("hwmon", "coretemp.0", "temp1").unwrap().as_str(),
            "hwmon:coretemp.0:temp1"
        );
    }
}
