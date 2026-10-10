//! Driver-neutral GPU backend contract.
//!
//! Every NVML call the adapter makes goes through these small traits so the
//! whole adapter (discovery, caching, throttling, paging, process merging) is
//! testable with a fake backend. Only `library.rs` talks to `nvml-wrapper`.
//! Devices are addressed by UUID, never by enumeration index, because the
//! index may change across driver reloads while the UUID does not.

use std::fmt;

use dgxtop_core::gpu_state::{GpuLink, PeerAccess};
use dgxtop_core::naming::entity;
use dgxtop_core::quality::ReadStatus;

/// Classified backend failure. Nothing here ever becomes a numeric value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// The device or driver cannot provide this value (never "0").
    NotSupported,
    /// The caller lacks permission for this query.
    NoPermission,
    /// The device fell off the bus, needs a reset or is no longer enumerated.
    GpuLost,
    /// The library session is gone (driver unloaded/reloaded); reconnect needed.
    Uninitialized,
    /// Any other failure, with a bounded diagnostic code.
    Other(String),
}

impl BackendError {
    /// Read status recorded on the observation for this failure.
    pub fn read_status(&self) -> ReadStatus {
        match self {
            Self::NotSupported => ReadStatus::Unsupported,
            Self::NoPermission => ReadStatus::PermissionDenied,
            Self::GpuLost => ReadStatus::Removed,
            Self::Uninitialized | Self::Other(_) => ReadStatus::Error,
        }
    }

    /// Bounded reason code (no paths, no driver free text beyond 80 bytes).
    pub fn reason_code(&self) -> String {
        match self {
            Self::NotSupported => "NVML_NOT_SUPPORTED".to_owned(),
            Self::NoPermission => "NVML_NO_PERMISSION".to_owned(),
            Self::GpuLost => "NVML_GPU_LOST".to_owned(),
            Self::Uninitialized => "NVML_UNINITIALIZED".to_owned(),
            Self::Other(code) => {
                let code = dgxtop_core::text::sanitize_display(code, 64);
                format!("NVML_ERROR_{code}")
            }
        }
    }

    /// Failures that describe a capability rather than a transient fault; they
    /// are cached and re-probed at most once per probe interval.
    pub fn is_capability_failure(&self) -> bool {
        matches!(self, Self::NotSupported | Self::NoPermission)
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason_code())
    }
}

/// Result of one backend query.
pub type BackendResult<T> = Result<T, BackendError>;

/// Why the backend library could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// Library missing or the kernel driver is not loaded.
    DriverMissing(String),
    /// The driver refused this user.
    NoPermission,
    /// The library loaded but no GPU is present.
    NoGpu,
    /// Anything else (library/driver mismatch, unexpected failures).
    Other(String),
}

impl ConnectError {
    pub fn reason_code(&self) -> String {
        match self {
            Self::DriverMissing(detail) => format!(
                "NVML_DRIVER_MISSING:{}",
                dgxtop_core::text::sanitize_display(detail, 48)
            ),
            Self::NoPermission => "NVML_NO_PERMISSION".to_owned(),
            Self::NoGpu => "NVML_NO_GPU".to_owned(),
            Self::Other(detail) => format!(
                "NVML_INIT_FAILED:{}",
                dgxtop_core::text::sanitize_display(detail, 48)
            ),
        }
    }
}

/// Descriptive, slowly changing details of one device (cached per
/// generation); a value the driver does not report stays `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceDetails {
    pub name: Option<String>,
    pub pci_bus_id: Option<String>,
    /// Board serial; `None` when the driver does not expose it to this user.
    pub serial: Option<String>,
    /// `Blackwell`, `Ampere`, ...
    pub architecture: Option<String>,
    /// `GeForce`, `Tesla`, `NVIDIA RTX`, ...
    pub brand: Option<String>,
    /// CUDA compute capability, `major.minor`.
    pub compute_capability: Option<String>,
    pub vbios_version: Option<String>,
    pub inforom_image: Option<String>,
    pub gsp_firmware: Option<String>,
    pub board_part_number: Option<String>,
    /// `N` of `/dev/nvidiaN`.
    pub minor_number: Option<u32>,
    /// `vendor:device` (`10de:2c02`).
    pub pci_device_id: Option<String>,
    /// `vendor:device` of the board (`10de:2095`).
    pub pci_subsystem_id: Option<String>,
}

/// Framebuffer memory as reported by the driver (bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramebufferMemory {
    pub total: u64,
    pub reserved: u64,
    pub used: u64,
    pub free: u64,
}

/// Busy-time ratios over the driver's last sample period (percent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtilizationRates {
    pub compute_percent: u32,
    /// Fraction of time device memory was read or written; NOT bandwidth.
    pub memory_busy_percent: u32,
}

/// Clock domains reported by NVML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ClockDomain {
    Graphics,
    Memory,
    Sm,
    Video,
}

impl ClockDomain {
    pub const ALL: [Self; 4] = [Self::Graphics, Self::Memory, Self::Sm, Self::Video];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Graphics => "graphics",
            Self::Memory => "memory",
            Self::Sm => "sm",
            Self::Video => "video",
        }
    }
}

/// A temperature at which the driver protects or steers the device
/// (static).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ThermalThreshold {
    /// The GPU begins hardware slowdown.
    Slowdown,
    /// The GPU shuts down.
    Shutdown,
    /// Hottest the GPU runs before it may drop below its base clock.
    MaxOperating,
    /// The temperature the fan control aims for.
    Target,
}

impl ThermalThreshold {
    pub const ALL: [Self; 4] = [
        Self::Slowdown,
        Self::Shutdown,
        Self::MaxOperating,
        Self::Target,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Slowdown => entity::SCOPE_SLOWDOWN,
            Self::Shutdown => entity::SCOPE_SHUTDOWN,
            Self::MaxOperating => entity::SCOPE_MAX_OPERATING,
            Self::Target => entity::SCOPE_TARGET,
        }
    }
}

/// A capability of the device read once per generation (D-48), in the
/// unit of its query: cores, bits, bytes or milliwatts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StaticFact {
    Cores,
    MemoryBusWidth,
    Bar1Total,
    PowerLimitDefault,
    PowerLimitMinimum,
    PowerLimitMaximum,
    /// The limit set for the device, before other limiters (D-54).
    PowerLimitRequested,
}

impl StaticFact {
    pub const ALL: [Self; 7] = [
        Self::Cores,
        Self::MemoryBusWidth,
        Self::Bar1Total,
        Self::PowerLimitDefault,
        Self::PowerLimitMinimum,
        Self::PowerLimitMaximum,
        Self::PowerLimitRequested,
    ];
}

/// The NVLink-C2C links between a device and the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct C2cLinks {
    pub count: u32,
    /// The highest bandwidth of the first link, in MB/s (10^6 bytes).
    pub link_max_mb_per_s: Option<u32>,
}

/// Why the driver held the clocks back, as NVML accounts the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ThrottleCause {
    Power,
    Thermal,
    SyncBoost,
    BoardLimit,
    Reliability,
}

impl ThrottleCause {
    pub const ALL: [Self; 5] = [
        Self::Power,
        Self::Thermal,
        Self::SyncBoost,
        Self::BoardLimit,
        Self::Reliability,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Power => entity::SCOPE_THROTTLE_POWER,
            Self::Thermal => entity::SCOPE_THROTTLE_THERMAL,
            Self::SyncBoost => entity::SCOPE_THROTTLE_SYNC_BOOST,
            Self::BoardLimit => entity::SCOPE_THROTTLE_BOARD_LIMIT,
            Self::Reliability => entity::SCOPE_THROTTLE_RELIABILITY,
        }
    }
}

/// A slowly changing live value (D-48), in the unit of its query: bytes,
/// sessions, frames per second, microseconds, replays or nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HealthValue {
    Bar1Used,
    EncoderSessions,
    EncoderFps,
    EncoderLatency,
    PcieReplays,
    /// Time held back by a cause since the driver loaded.
    ThrottleTime(ThrottleCause),
}

impl HealthValue {
    /// Every value, the throttle time of each cause included.
    pub fn all() -> Vec<Self> {
        let mut values = vec![
            Self::Bar1Used,
            Self::EncoderSessions,
            Self::EncoderFps,
            Self::EncoderLatency,
            Self::PcieReplays,
        ];
        values.extend(ThrottleCause::ALL.map(Self::ThrottleTime));
        values
    }
}

/// An attribute of the PCIe link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PcieLink {
    Generation,
    Width,
}

impl PcieLink {
    pub const ALL: [Self; 2] = [Self::Generation, Self::Width];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Generation => entity::SCOPE_LINK_GENERATION,
            Self::Width => entity::SCOPE_LINK_WIDTH,
        }
    }
}

/// PCIe throughput direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PcieDirection {
    Transmit,
    Receive,
}

/// ECC error severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EccErrorKind {
    Corrected,
    Uncorrected,
}

/// ECC counter lifetime: since driver load, or for the device lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EccCounterScope {
    Volatile,
    Aggregate,
}

impl EccCounterScope {
    pub const ALL: [Self; 2] = [Self::Volatile, Self::Aggregate];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Volatile => "volatile",
            Self::Aggregate => "aggregate",
        }
    }
}

/// Why memory pages were retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetiredPageCause {
    MultipleSingleBitErrors,
    DoubleBitError,
}

/// One process with a context on a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuProcessEntry {
    pub pid: u32,
    /// `None` when the driver reports the value as not available.
    pub used_memory_bytes: Option<u64>,
}

/// One per-process utilization sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessUtilizationSample {
    pub pid: u32,
    /// Driver CPU timestamp in microseconds.
    pub timestamp_us: u64,
    pub sm_percent: u32,
}

/// Device enumeration and slowly changing metadata (discovery only).
pub trait GpuInventory {
    fn driver_version(&self) -> BackendResult<String>;
    /// Version of the NVML library itself.
    fn nvml_version(&self) -> BackendResult<String>;
    /// Newest CUDA version the driver supports, `major.minor`.
    fn cuda_driver_version(&self) -> BackendResult<String>;
    fn device_count(&self) -> BackendResult<u32>;
    /// UUID of the device currently enumerated at `index`.
    fn device_uuid(&self, index: u32) -> BackendResult<String>;
    fn device_details(&self, uuid: &str) -> BackendResult<DeviceDetails>;
    /// Human readable compute mode (`default`, `exclusive_process`, ...).
    fn compute_mode(&self, uuid: &str) -> BackendResult<String>;
    fn persistence_mode(&self, uuid: &str) -> BackendResult<bool>;
    fn fan_count(&self, uuid: &str) -> BackendResult<u32>;
    /// Enforced power limit in milliwatts.
    fn power_limit_mw(&self, uuid: &str) -> BackendResult<u32>;
    fn max_clock_mhz(&self, uuid: &str, domain: ClockDomain) -> BackendResult<u32>;
    fn temperature_threshold_c(
        &self,
        uuid: &str,
        threshold: ThermalThreshold,
    ) -> BackendResult<u32>;
    /// Highest generation or width the device and its slot support.
    fn max_pcie_link(&self, uuid: &str, link: PcieLink) -> BackendResult<u32>;
    fn static_fact(&self, uuid: &str, fact: StaticFact) -> BackendResult<u64>;
    /// CPUs nearest the device: bit `n` of the words is CPU `n` (D-54).
    fn cpu_affinity(&self, uuid: &str) -> BackendResult<Vec<u64>>;
    /// NUMA nodes nearest the device's memory: bit `n` is node `n`.
    fn numa_affinity(&self, uuid: &str) -> BackendResult<Vec<u64>>;
    /// The device's NVLink-C2C links to the CPU (GB10, Grace).
    fn c2c_links(&self, uuid: &str) -> BackendResult<C2cLinks>;
    /// The PCIe path between two devices.
    fn peer_link(&self, uuid: &str, peer: &str) -> BackendResult<GpuLink>;
    /// Whether `uuid` may read and write `peer`'s memory directly.
    fn peer_access(&self, uuid: &str, peer: &str) -> BackendResult<PeerAccess>;
}

/// Per-device gauges and counters (fast and slow_health groups).
pub trait GpuTelemetry {
    fn utilization(&self, uuid: &str) -> BackendResult<UtilizationRates>;
    fn temperature_c(&self, uuid: &str) -> BackendResult<u32>;
    /// Power the driver reports by default (a one-second average on Ampere
    /// and newer GPUs).
    fn power_draw_mw(&self, uuid: &str) -> BackendResult<u32>;
    /// The driver's instantaneous power reading (D-54).
    fn power_instant_mw(&self, uuid: &str) -> BackendResult<u32>;
    fn clock_mhz(&self, uuid: &str, domain: ClockDomain) -> BackendResult<u32>;
    fn fan_speed_percent(&self, uuid: &str, fan: u32) -> BackendResult<u32>;
    fn framebuffer_memory(&self, uuid: &str) -> BackendResult<FramebufferMemory>;
    /// PCIe throughput in the driver's "KB/s" (definition unverified).
    fn pcie_throughput_kb(&self, uuid: &str, direction: PcieDirection) -> BackendResult<u32>;
    fn encoder_utilization(&self, uuid: &str) -> BackendResult<u32>;
    fn decoder_utilization(&self, uuid: &str) -> BackendResult<u32>;
    /// Energy since the last driver reload in millijoules.
    fn total_energy_mj(&self, uuid: &str) -> BackendResult<u64>;
    fn ecc_errors(
        &self,
        uuid: &str,
        kind: EccErrorKind,
        scope: EccCounterScope,
    ) -> BackendResult<u64>;
    fn retired_page_count(&self, uuid: &str, cause: RetiredPageCause) -> BackendResult<u64>;
    fn nvlink_active(&self, uuid: &str, link: u32) -> BackendResult<bool>;
    /// Performance state, 0 (maximum performance) to 15.
    fn performance_state(&self, uuid: &str) -> BackendResult<u32>;
    /// NVML clocks-event ("throttle") reason bits.
    fn throttle_reasons(&self, uuid: &str) -> BackendResult<u64>;
    /// Current generation or width of the PCIe link (it may train down at
    /// idle).
    fn pcie_link(&self, uuid: &str, link: PcieLink) -> BackendResult<u32>;
    fn health_value(&self, uuid: &str, value: HealthValue) -> BackendResult<u64>;
}

/// Per-device process associations (process group).
pub trait GpuProcessQueries {
    fn compute_processes(&self, uuid: &str) -> BackendResult<Vec<GpuProcessEntry>>;
    fn graphics_processes(&self, uuid: &str) -> BackendResult<Vec<GpuProcessEntry>>;
    /// Samples newer than `since_timestamp_us` (0 = everything buffered); an
    /// empty list means the driver had no new sample.
    fn process_utilization(
        &self,
        uuid: &str,
        since_timestamp_us: u64,
    ) -> BackendResult<Vec<ProcessUtilizationSample>>;
}

/// Device-level queries (metadata plus telemetry) used by group reads.
pub trait GpuDeviceQueries: GpuInventory + GpuTelemetry {}

impl<T: GpuInventory + GpuTelemetry + ?Sized> GpuDeviceQueries for T {}

/// One open backend session.
pub trait GpuBackend: GpuDeviceQueries + GpuProcessQueries + Send {}

impl<T: GpuInventory + GpuTelemetry + GpuProcessQueries + Send> GpuBackend for T {}

/// Opens backend sessions; called lazily and throttled by the adapter.
pub trait BackendConnector: Send {
    fn connect(&mut self) -> Result<Box<dyn GpuBackend>, ConnectError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_map_to_distinct_statuses() {
        assert_eq!(
            BackendError::NotSupported.read_status(),
            ReadStatus::Unsupported
        );
        assert_eq!(
            BackendError::NoPermission.read_status(),
            ReadStatus::PermissionDenied
        );
        assert_eq!(BackendError::GpuLost.read_status(), ReadStatus::Removed);
        assert_eq!(BackendError::Uninitialized.read_status(), ReadStatus::Error);
        assert!(BackendError::NotSupported.is_capability_failure());
        assert!(!BackendError::GpuLost.is_capability_failure());
    }

    #[test]
    fn reason_codes_are_bounded_and_clean() {
        let hostile = BackendError::Other(format!("\u{1b}[31m{}", "x".repeat(500)));
        let code = hostile.reason_code();
        assert!(code.len() <= 80, "{code}");
        assert!(!code.contains('\u{1b}'));
        assert!(
            ConnectError::DriverMissing("y".repeat(300))
                .reason_code()
                .len()
                <= 80
        );
    }
}
