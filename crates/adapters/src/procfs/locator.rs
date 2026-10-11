//! How each catalogued procfs source is read, plus the catalog families and
//! admission ranks shared by the procfs group modules.

use dgxtop_core::metric::{ids, native};

use super::parse::{CpuLabel, DiskField, MemField, StatColumn};

/// Driver name in provenance.
pub(crate) const DRIVER: &str = "procfs";

/// Catalog families: the unit that is carried over when an enumeration
/// file cannot be read.
pub(crate) const FAMILY_HOST: &str = "host";
pub(crate) const FAMILY_CPU: &str = "cpu";
pub(crate) const FAMILY_MEMORY: &str = "memory";
pub(crate) const FAMILY_DISK: &str = "disk";

/// Admission ranks under the source limit (lower is kept first): host-wide
/// figures, then block devices, then per-core counters, then per-core
/// frequencies.
pub(crate) const RANK_HOST_WIDE: u8 = 0;
pub(crate) const RANK_DEVICE: u8 = 1;
pub(crate) const RANK_CORE: u8 = 2;
pub(crate) const RANK_CORE_FREQUENCY: u8 = 3;

/// `/proc/loadavg` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadField {
    One,
    Five,
    Fifteen,
    Running,
    Total,
}

pub(crate) const LOAD_FIELDS: [LoadField; 5] = [
    LoadField::One,
    LoadField::Five,
    LoadField::Fifteen,
    LoadField::Running,
    LoadField::Total,
];

/// The kernel recomputes load averages every LOAD_FREQ (5 s); reading more
/// often does not produce a new measurement.
const LOAD_AVERAGE_UPDATE_MS: u32 = 5_000;

impl LoadField {
    pub(crate) fn channel(self) -> &'static str {
        match self {
            Self::One => "1m",
            Self::Five => "5m",
            Self::Fifteen => "15m",
            Self::Running => "running",
            Self::Total => "total",
        }
    }

    pub(crate) fn metric_id(self) -> &'static str {
        match self {
            Self::One => ids::CPU_LOAD_1M,
            Self::Five => ids::CPU_LOAD_5M,
            Self::Fifteen => ids::CPU_LOAD_15M,
            Self::Running => ids::CPU_TASKS_RUNNING,
            Self::Total => ids::CPU_TASKS_TOTAL,
        }
    }

    pub(crate) fn native_unit(self) -> &'static str {
        native::COUNT
    }

    pub(crate) fn update_interval_ms(self) -> Option<u32> {
        match self {
            Self::One | Self::Five | Self::Fifteen => Some(LOAD_AVERAGE_UPDATE_MS),
            Self::Running | Self::Total => None,
        }
    }
}

/// The cpufreq file a CPU's current frequency is read from (D-67).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CurrentFrequency {
    /// `cpuinfo_avg_freq`: what the CPU ran at over its last moments, from
    /// its hardware counters, where the kernel offers it.
    Average,
    /// `scaling_cur_freq`: what the cpufreq driver reports; for some
    /// drivers the requested frequency, not the clock.
    Scaling,
}

impl CurrentFrequency {
    pub(crate) const fn file(self) -> &'static str {
        match self {
            Self::Average => "cpuinfo_avg_freq",
            Self::Scaling => "scaling_cur_freq",
        }
    }
}

/// How the read path obtains one catalogued source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ProcfsLocator {
    /// A time column of a `/proc/stat` CPU line.
    Stat { cpu: CpuLabel, column: StatColumn },
    /// A `/proc/loadavg` field.
    Load(LoadField),
    /// The current frequency of logical CPU `cpu`, from `current`.
    FrequencyCurrent { cpu: u32, current: CurrentFrequency },
    /// `cpuinfo_max_freq` read at discovery (cached per generation), in kHz.
    FrequencyMaximum { khz: u64 },
    /// A `/proc/meminfo` field.
    Memory(MemField),
    /// A `/proc/diskstats` field of the device `major:minor` named `name`.
    Disk {
        major: u32,
        minor: u32,
        name: String,
        field: DiskField,
    },
}
