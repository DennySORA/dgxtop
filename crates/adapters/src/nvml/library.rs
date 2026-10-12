//! Real backend on top of `nvml-wrapper` (system `libnvidia-ml.so.1`).
//!
//! `Nvml::init()` asks the dynamic loader for the bare soname
//! `libnvidia-ml.so.1`, so only the system library search path is used: no
//! explicit path, no current-directory lookup and no CUDA stub library.
//! Device handles are re-acquired by UUID per query instead of being cached,
//! because a cached raw handle would need `unsafe` and could outlive a driver
//! reload; the lookup is an in-process table scan.

use dgxtop_core::gpu_state::{GpuLink, PeerAccess};
use nvml_wrapper::enum_wrappers::device::{
    Brand, Clock, ComputeMode, EccCounter, MemoryError, P2pCapabilitiesIndex, P2pStatus,
    PcieUtilCounter, PerformancePolicy, PerformanceState, RetirementCause, TemperatureSensor,
    TemperatureThreshold, TopologyLevel,
};
use nvml_wrapper::enums::device::{DeviceArchitecture, SampleValue, UsedGpuMemory};
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::struct_wrappers::device::PciInfo;
use nvml_wrapper::structs::device::FieldId;
use nvml_wrapper::sys_exports::field_id::{
    NVML_FI_DEV_C2C_LINK_COUNT, NVML_FI_DEV_C2C_LINK_GET_MAX_BW, NVML_FI_DEV_POWER_INSTANT,
};
use nvml_wrapper::{Nvml, cuda_driver_version_major, cuda_driver_version_minor};

use super::backend::{
    BackendConnector, BackendError, BackendResult, C2cLinks, ClockDomain, ConnectError,
    DeviceDetails, EccCounterScope, EccErrorKind, FramebufferMemory, GpuBackend, GpuInventory,
    GpuProcessEntry, GpuProcessQueries, GpuTelemetry, HealthValue, PcieDirection, PcieLink,
    ProcessUtilizationSample, RetiredPageCause, StaticFact, ThermalThreshold, ThrottleCause,
    UtilizationRates,
};
use super::linux_only;

/// Words of the CPU affinity mask asked for: 1024 CPUs.
const CPU_AFFINITY_WORDS: usize = 16;
/// Words of the NUMA affinity mask asked for: 128 nodes.
const NUMA_AFFINITY_WORDS: usize = 2;
/// `NVML_AFFINITY_SCOPE_NODE`: the memory affinity by NUMA node.
const NUMA_NODE_SCOPE: u32 = 0;

/// Opens NVML sessions with `Nvml::init()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NvmlLibraryConnector;

impl BackendConnector for NvmlLibraryConnector {
    fn connect(&mut self) -> Result<Box<dyn GpuBackend>, ConnectError> {
        let nvml = Nvml::init().map_err(|error| connect_error_of(&error))?;
        Ok(Box::new(NvmlLibraryBackend { nvml }))
    }
}

/// One open NVML session; dropping it calls `nvmlShutdown`.
pub struct NvmlLibraryBackend {
    nvml: Nvml,
}

impl std::fmt::Debug for NvmlLibraryBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NvmlLibraryBackend")
    }
}

impl NvmlLibraryBackend {
    fn device(&self, uuid: &str) -> BackendResult<nvml_wrapper::Device<'_>> {
        self.nvml.device_by_uuid(uuid).map_err(|error| match error {
            // A UUID that is no longer enumerated means the device is gone.
            NvmlError::NotFound | NvmlError::InvalidArg => BackendError::GpuLost,
            other => backend_error_of(&other),
        })
    }
}

fn connect_error_of(error: &NvmlError) -> ConnectError {
    match error {
        NvmlError::LibloadingError(_)
        | NvmlError::LibraryNotFound
        | NvmlError::DriverNotLoaded
        | NvmlError::FailedToLoadSymbol(_)
        | NvmlError::FunctionNotFound => ConnectError::DriverMissing(error_name(error).to_owned()),
        NvmlError::NoPermission => ConnectError::NoPermission,
        NvmlError::NotFound => ConnectError::NoGpu,
        other => ConnectError::Other(error_name(other).to_owned()),
    }
}

/// Maps a per-query failure. Missing symbols mean an older driver library
/// without that query, which is a capability gap, not a fault.
fn backend_error_of(error: &NvmlError) -> BackendError {
    match error {
        NvmlError::NotSupported
        | NvmlError::FunctionNotFound
        | NvmlError::FailedToLoadSymbol(_) => BackendError::NotSupported,
        NvmlError::NoPermission => BackendError::NoPermission,
        NvmlError::GpuLost | NvmlError::ResetRequired => BackendError::GpuLost,
        NvmlError::Uninitialized | NvmlError::DriverNotLoaded | NvmlError::LibRmVersionMismatch => {
            BackendError::Uninitialized
        }
        other => BackendError::Other(error_name(other).to_owned()),
    }
}

#[allow(deprecated)]
fn error_name(error: &NvmlError) -> &'static str {
    match error {
        NvmlError::Utf8Error(_) => "UTF8",
        NvmlError::NulError(_) => "NUL_BYTE",
        NvmlError::LibloadingError(_) => "LIBRARY_LOAD",
        NvmlError::FailedToLoadSymbol(_) => "SYMBOL_MISSING",
        NvmlError::StringTooLong { .. } => "STRING_TOO_LONG",
        NvmlError::IncorrectBits(_) => "INCORRECT_BITS",
        NvmlError::UnexpectedVariant(_) => "UNEXPECTED_VARIANT",
        NvmlError::SetReleaseFailed => "SET_RELEASE_FAILED",
        NvmlError::GetPciInfoFailed => "PCI_INFO_FAILED",
        NvmlError::PciInfoToCFailed => "PCI_INFO_CONVERSION",
        NvmlError::Uninitialized => "UNINITIALIZED",
        NvmlError::InvalidArg => "INVALID_ARGUMENT",
        NvmlError::NotSupported => "NOT_SUPPORTED",
        NvmlError::NoPermission => "NO_PERMISSION",
        NvmlError::AlreadyInitialized => "ALREADY_INITIALIZED",
        NvmlError::NotFound => "NOT_FOUND",
        NvmlError::InsufficientSize(_) => "INSUFFICIENT_SIZE",
        NvmlError::InsufficientPower => "INSUFFICIENT_POWER",
        NvmlError::DriverNotLoaded => "DRIVER_NOT_LOADED",
        NvmlError::Timeout => "TIMEOUT",
        NvmlError::IrqIssue => "IRQ_ISSUE",
        NvmlError::LibraryNotFound => "LIBRARY_NOT_FOUND",
        NvmlError::FunctionNotFound => "FUNCTION_NOT_FOUND",
        NvmlError::CorruptedInfoROM => "CORRUPTED_INFOROM",
        NvmlError::GpuLost => "GPU_LOST",
        NvmlError::ResetRequired => "RESET_REQUIRED",
        NvmlError::OperatingSystem => "OPERATING_SYSTEM",
        NvmlError::LibRmVersionMismatch => "LIB_RM_VERSION_MISMATCH",
        NvmlError::InUse => "IN_USE",
        NvmlError::InsufficientMemory => "INSUFFICIENT_MEMORY",
        NvmlError::NoData => "NO_DATA",
        NvmlError::VgpuEccNotSupported => "VGPU_ECC_NOT_SUPPORTED",
        NvmlError::Unknown => "UNKNOWN",
    }
}

fn query<T>(result: Result<T, NvmlError>) -> BackendResult<T> {
    result.map_err(|error| backend_error_of(&error))
}

const fn clock_of(domain: ClockDomain) -> Clock {
    match domain {
        ClockDomain::Graphics => Clock::Graphics,
        ClockDomain::Memory => Clock::Memory,
        ClockDomain::Sm => Clock::SM,
        ClockDomain::Video => Clock::Video,
    }
}

/// P-state number; `None` for a state NVML could not name.
const fn performance_state_number(state: PerformanceState) -> Option<u32> {
    Some(match state {
        PerformanceState::Zero => 0,
        PerformanceState::One => 1,
        PerformanceState::Two => 2,
        PerformanceState::Three => 3,
        PerformanceState::Four => 4,
        PerformanceState::Five => 5,
        PerformanceState::Six => 6,
        PerformanceState::Seven => 7,
        PerformanceState::Eight => 8,
        PerformanceState::Nine => 9,
        PerformanceState::Ten => 10,
        PerformanceState::Eleven => 11,
        PerformanceState::Twelve => 12,
        PerformanceState::Thirteen => 13,
        PerformanceState::Fourteen => 14,
        PerformanceState::Fifteen => 15,
        PerformanceState::Unknown => return None,
    })
}

/// Readable architecture name; `None` for one this library cannot name.
const fn architecture_name(architecture: DeviceArchitecture) -> Option<&'static str> {
    Some(match architecture {
        DeviceArchitecture::Kepler => "Kepler",
        DeviceArchitecture::Maxwell => "Maxwell",
        DeviceArchitecture::Pascal => "Pascal",
        DeviceArchitecture::Volta => "Volta",
        DeviceArchitecture::Turing => "Turing",
        DeviceArchitecture::Ampere => "Ampere",
        DeviceArchitecture::Ada => "Ada Lovelace",
        DeviceArchitecture::Hopper => "Hopper",
        DeviceArchitecture::Blackwell => "Blackwell",
        DeviceArchitecture::Unknown => return None,
    })
}

/// Product line as NVIDIA names it; `None` when the driver does not know.
const fn brand_name(brand: Brand) -> Option<&'static str> {
    Some(match brand {
        Brand::Quadro => "Quadro",
        Brand::Tesla => "Tesla",
        Brand::NVS => "NVS",
        Brand::GRID => "GRID",
        Brand::GeForce => "GeForce",
        Brand::Titan => "Titan",
        Brand::VApps => "NVIDIA vApps",
        Brand::VPC => "NVIDIA vPC",
        Brand::VCS => "NVIDIA vCS",
        Brand::VWS => "NVIDIA vWS",
        Brand::CloudGaming => "NVIDIA Cloud Gaming",
        Brand::VGaming => "NVIDIA vGaming",
        Brand::QuadroRTX => "Quadro RTX",
        Brand::NvidiaRTX => "NVIDIA RTX",
        Brand::Nvidia => "NVIDIA",
        Brand::GeForceRTX => "GeForce RTX",
        Brand::TitanRTX => "Titan RTX",
        Brand::Unknown => return None,
    })
}

/// `vendor:device` of a combined PCI id (device in the high 16 bits).
fn pci_id(combined: u32) -> String {
    format!("{:04x}:{:04x}", combined & 0xffff, combined >> 16)
}

const fn policy_of(cause: ThrottleCause) -> PerformancePolicy {
    match cause {
        ThrottleCause::Power => PerformancePolicy::Power,
        ThrottleCause::Thermal => PerformancePolicy::Thermal,
        ThrottleCause::SyncBoost => PerformancePolicy::SyncBoost,
        ThrottleCause::BoardLimit => PerformancePolicy::BoardLimit,
        ThrottleCause::Reliability => PerformancePolicy::Reliability,
    }
}

const fn link_of(level: TopologyLevel) -> GpuLink {
    match level {
        TopologyLevel::Internal => GpuLink::SameBoard,
        TopologyLevel::Single => GpuLink::OneSwitch,
        TopologyLevel::Multiple => GpuLink::Switches,
        TopologyLevel::HostBridge => GpuLink::HostBridge,
        TopologyLevel::Node => GpuLink::NumaNode,
        TopologyLevel::System => GpuLink::System,
    }
}

/// A field value in the driver's unsigned unit.
fn unsigned_field(value: SampleValue) -> BackendResult<u32> {
    match value {
        SampleValue::U32(value) => Ok(value),
        SampleValue::U64(value) => {
            u32::try_from(value).map_err(|_| BackendError::Other("FIELD_RANGE".to_owned()))
        }
        SampleValue::F64(_) | SampleValue::I64(_) => {
            Err(BackendError::Other("FIELD_TYPE".to_owned()))
        }
    }
}

const fn compute_mode_name(mode: ComputeMode) -> &'static str {
    match mode {
        ComputeMode::Default => "default",
        ComputeMode::ExclusiveThread => "exclusive_thread",
        ComputeMode::Prohibited => "prohibited",
        ComputeMode::ExclusiveProcess => "exclusive_process",
    }
}

fn process_entry_of(info: nvml_wrapper::struct_wrappers::device::ProcessInfo) -> GpuProcessEntry {
    GpuProcessEntry {
        pid: info.pid,
        used_memory_bytes: match info.used_gpu_memory {
            UsedGpuMemory::Used(bytes) => Some(bytes),
            UsedGpuMemory::Unavailable => None,
        },
    }
}

impl GpuInventory for NvmlLibraryBackend {
    fn driver_version(&self) -> BackendResult<String> {
        query(self.nvml.sys_driver_version())
    }

    fn nvml_version(&self) -> BackendResult<String> {
        query(self.nvml.sys_nvml_version())
    }

    fn cuda_driver_version(&self) -> BackendResult<String> {
        let version = query(self.nvml.sys_cuda_driver_version())?;
        Ok(format!(
            "{}.{}",
            cuda_driver_version_major(version),
            cuda_driver_version_minor(version)
        ))
    }

    fn device_count(&self) -> BackendResult<u32> {
        query(self.nvml.device_count())
    }

    fn device_uuid(&self, index: u32) -> BackendResult<String> {
        let device = query(self.nvml.device_by_index(index))?;
        query(device.uuid())
    }

    fn device_details(&self, uuid: &str) -> BackendResult<DeviceDetails> {
        let device = self.device(uuid)?;
        let pci: Option<PciInfo> = device.pci_info().ok();
        Ok(DeviceDetails {
            name: device.name().ok(),
            pci_bus_id: pci.as_ref().map(|info| info.bus_id.clone()),
            serial: device.serial().ok(),
            architecture: device
                .architecture()
                .ok()
                .and_then(architecture_name)
                .map(str::to_owned),
            brand: device.brand().ok().and_then(brand_name).map(str::to_owned),
            compute_capability: device
                .cuda_compute_capability()
                .ok()
                .map(|capability| format!("{}.{}", capability.major, capability.minor)),
            vbios_version: device.vbios_version().ok(),
            inforom_image: device.info_rom_image_version().ok(),
            gsp_firmware: device.gsp_firmware_version().ok(),
            board_part_number: device
                .board_part_number()
                .ok()
                .filter(|part| !part.trim().is_empty()),
            minor_number: linux_only::minor_number(&device).ok(),
            pci_device_id: pci.as_ref().map(|info| pci_id(info.pci_device_id)),
            pci_subsystem_id: pci
                .as_ref()
                .and_then(|info| info.pci_sub_system_id)
                .map(pci_id),
        })
    }

    fn compute_mode(&self, uuid: &str) -> BackendResult<String> {
        let mode = query(self.device(uuid)?.compute_mode())?;
        Ok(compute_mode_name(mode).to_owned())
    }

    fn persistence_mode(&self, uuid: &str) -> BackendResult<bool> {
        query(linux_only::persistence_mode(&self.device(uuid)?))
    }

    fn fan_count(&self, uuid: &str) -> BackendResult<u32> {
        query(self.device(uuid)?.num_fans())
    }

    fn power_limit_mw(&self, uuid: &str) -> BackendResult<u32> {
        query(self.device(uuid)?.enforced_power_limit())
    }

    fn max_clock_mhz(&self, uuid: &str, domain: ClockDomain) -> BackendResult<u32> {
        query(self.device(uuid)?.max_clock_info(clock_of(domain)))
    }

    fn temperature_threshold_c(
        &self,
        uuid: &str,
        threshold: ThermalThreshold,
    ) -> BackendResult<u32> {
        let threshold = match threshold {
            ThermalThreshold::Slowdown => TemperatureThreshold::Slowdown,
            ThermalThreshold::Shutdown => TemperatureThreshold::Shutdown,
            ThermalThreshold::MaxOperating => TemperatureThreshold::GpuMax,
            ThermalThreshold::Target => TemperatureThreshold::AcousticCurr,
        };
        query(self.device(uuid)?.temperature_threshold(threshold))
    }

    fn max_pcie_link(&self, uuid: &str, link: PcieLink) -> BackendResult<u32> {
        let device = self.device(uuid)?;
        query(match link {
            PcieLink::Generation => device.max_pcie_link_gen(),
            PcieLink::Width => device.max_pcie_link_width(),
        })
    }

    fn static_fact(&self, uuid: &str, fact: StaticFact) -> BackendResult<u64> {
        let device = self.device(uuid)?;
        match fact {
            StaticFact::Cores => query(device.num_cores()).map(u64::from),
            StaticFact::MemoryBusWidth => query(device.memory_bus_width()).map(u64::from),
            StaticFact::Bar1Total => query(device.bar1_memory_info()).map(|info| info.total),
            StaticFact::PowerLimitDefault => {
                query(device.power_management_limit_default()).map(u64::from)
            }
            StaticFact::PowerLimitMinimum => query(device.power_management_limit_constraints())
                .map(|limits| u64::from(limits.min_limit)),
            StaticFact::PowerLimitMaximum => query(device.power_management_limit_constraints())
                .map(|limits| u64::from(limits.max_limit)),
            StaticFact::PowerLimitRequested => {
                query(device.power_management_limit()).map(u64::from)
            }
        }
    }

    fn cpu_affinity(&self, uuid: &str) -> BackendResult<Vec<u64>> {
        query(linux_only::cpu_affinity(
            &self.device(uuid)?,
            CPU_AFFINITY_WORDS,
        ))
    }

    /// The link count and the first link's bandwidth: the binding reads
    /// field values without a scope, which selects link 0.
    fn c2c_links(&self, uuid: &str) -> BackendResult<C2cLinks> {
        let fields = [
            FieldId(NVML_FI_DEV_C2C_LINK_COUNT),
            FieldId(NVML_FI_DEV_C2C_LINK_GET_MAX_BW),
        ];
        let mut samples = query(self.device(uuid)?.field_values_for(&fields))?.into_iter();
        let mut next_field = || {
            let sample = samples.next().ok_or(BackendError::NotSupported)?;
            unsigned_field(query(query(sample)?.value)?)
        };
        let count = next_field()?;
        Ok(C2cLinks {
            count,
            link_max_mb_per_s: next_field().ok().filter(|bandwidth| *bandwidth > 0),
        })
    }

    fn numa_affinity(&self, uuid: &str) -> BackendResult<Vec<u64>> {
        query(linux_only::memory_affinity(
            &self.device(uuid)?,
            NUMA_AFFINITY_WORDS,
            NUMA_NODE_SCOPE,
        ))
    }

    fn peer_link(&self, uuid: &str, peer: &str) -> BackendResult<GpuLink> {
        let other = self.device(peer)?;
        query(linux_only::common_ancestor(&self.device(uuid)?, other)).map(link_of)
    }

    fn peer_access(&self, uuid: &str, peer: &str) -> BackendResult<PeerAccess> {
        let device = self.device(uuid)?;
        let other = self.device(peer)?;
        let allowed = |capability| {
            query(device.p2p_status(&other, capability)).map(|status| status == P2pStatus::Ok)
        };
        Ok(PeerAccess {
            read: allowed(P2pCapabilitiesIndex::Read)?,
            write: allowed(P2pCapabilitiesIndex::Write)?,
        })
    }
}

impl GpuTelemetry for NvmlLibraryBackend {
    fn utilization(&self, uuid: &str) -> BackendResult<UtilizationRates> {
        let rates = query(self.device(uuid)?.utilization_rates())?;
        Ok(UtilizationRates {
            compute_percent: rates.gpu,
            memory_busy_percent: rates.memory,
        })
    }

    fn temperature_c(&self, uuid: &str) -> BackendResult<u32> {
        query(self.device(uuid)?.temperature(TemperatureSensor::Gpu))
    }

    fn power_draw_mw(&self, uuid: &str) -> BackendResult<u32> {
        query(self.device(uuid)?.power_usage())
    }

    fn power_instant_mw(&self, uuid: &str) -> BackendResult<u32> {
        let fields = [FieldId(NVML_FI_DEV_POWER_INSTANT)];
        let samples = query(self.device(uuid)?.field_values_for(&fields))?;
        // The call succeeds as a whole while each field carries its own result.
        let sample = samples
            .into_iter()
            .next()
            .ok_or(BackendError::NotSupported)?;
        unsigned_field(query(query(sample)?.value)?)
    }

    fn clock_mhz(&self, uuid: &str, domain: ClockDomain) -> BackendResult<u32> {
        query(self.device(uuid)?.clock_info(clock_of(domain)))
    }

    fn fan_speed_percent(&self, uuid: &str, fan: u32) -> BackendResult<u32> {
        query(self.device(uuid)?.fan_speed(fan))
    }

    fn framebuffer_memory(&self, uuid: &str) -> BackendResult<FramebufferMemory> {
        let info = query(self.device(uuid)?.memory_info())?;
        Ok(FramebufferMemory {
            total: info.total,
            reserved: info.reserved,
            used: info.used,
            free: info.free,
        })
    }

    fn pcie_throughput_kb(&self, uuid: &str, direction: PcieDirection) -> BackendResult<u32> {
        let counter = match direction {
            PcieDirection::Transmit => PcieUtilCounter::Send,
            PcieDirection::Receive => PcieUtilCounter::Receive,
        };
        query(self.device(uuid)?.pcie_throughput(counter))
    }

    fn encoder_utilization(&self, uuid: &str) -> BackendResult<u32> {
        Ok(query(self.device(uuid)?.encoder_utilization())?.utilization)
    }

    fn decoder_utilization(&self, uuid: &str) -> BackendResult<u32> {
        Ok(query(self.device(uuid)?.decoder_utilization())?.utilization)
    }

    fn total_energy_mj(&self, uuid: &str) -> BackendResult<u64> {
        query(self.device(uuid)?.total_energy_consumption())
    }

    fn ecc_errors(
        &self,
        uuid: &str,
        kind: EccErrorKind,
        scope: EccCounterScope,
    ) -> BackendResult<u64> {
        let error_type = match kind {
            EccErrorKind::Corrected => MemoryError::Corrected,
            EccErrorKind::Uncorrected => MemoryError::Uncorrected,
        };
        let counter = match scope {
            EccCounterScope::Volatile => EccCounter::Volatile,
            EccCounterScope::Aggregate => EccCounter::Aggregate,
        };
        query(self.device(uuid)?.total_ecc_errors(error_type, counter))
    }

    fn retired_page_count(&self, uuid: &str, cause: RetiredPageCause) -> BackendResult<u64> {
        let cause = match cause {
            RetiredPageCause::MultipleSingleBitErrors => {
                RetirementCause::MultipleSingleBitEccErrors
            }
            RetiredPageCause::DoubleBitError => RetirementCause::DoubleBitEccError,
        };
        let pages = query(self.device(uuid)?.retired_pages(cause))?;
        u64::try_from(pages.len()).map_err(|_| BackendError::Other("PAGE_COUNT".to_owned()))
    }

    fn nvlink_active(&self, uuid: &str, link: u32) -> BackendResult<bool> {
        query(self.device(uuid)?.link_wrapper_for(link).is_active())
    }

    fn performance_state(&self, uuid: &str) -> BackendResult<u32> {
        let state = query(self.device(uuid)?.performance_state())?;
        performance_state_number(state).ok_or(BackendError::NotSupported)
    }

    fn throttle_reasons(&self, uuid: &str) -> BackendResult<u64> {
        Ok(query(self.device(uuid)?.current_throttle_reasons())?.bits())
    }

    fn pcie_link(&self, uuid: &str, link: PcieLink) -> BackendResult<u32> {
        let device = self.device(uuid)?;
        query(match link {
            PcieLink::Generation => device.current_pcie_link_gen(),
            PcieLink::Width => device.current_pcie_link_width(),
        })
    }

    fn health_value(&self, uuid: &str, value: HealthValue) -> BackendResult<u64> {
        let device = self.device(uuid)?;
        match value {
            HealthValue::Bar1Used => query(device.bar1_memory_info()).map(|info| info.used),
            HealthValue::EncoderSessions => {
                query(device.encoder_stats()).map(|stats| u64::from(stats.session_count))
            }
            HealthValue::EncoderFps => {
                query(device.encoder_stats()).map(|stats| u64::from(stats.average_fps))
            }
            HealthValue::EncoderLatency => {
                query(device.encoder_stats()).map(|stats| u64::from(stats.average_latency))
            }
            HealthValue::PcieReplays => query(device.pcie_replay_counter()).map(u64::from),
            HealthValue::ThrottleTime(cause) => {
                query(device.violation_status(policy_of(cause))).map(|time| time.violation_time)
            }
        }
    }
}

impl GpuProcessQueries for NvmlLibraryBackend {
    fn compute_processes(&self, uuid: &str) -> BackendResult<Vec<GpuProcessEntry>> {
        let processes = query(self.device(uuid)?.running_compute_processes())?;
        Ok(processes.into_iter().map(process_entry_of).collect())
    }

    fn graphics_processes(&self, uuid: &str) -> BackendResult<Vec<GpuProcessEntry>> {
        let processes = query(self.device(uuid)?.running_graphics_processes())?;
        Ok(processes.into_iter().map(process_entry_of).collect())
    }

    fn process_utilization(
        &self,
        uuid: &str,
        since_timestamp_us: u64,
    ) -> BackendResult<Vec<ProcessUtilizationSample>> {
        let device = self.device(uuid)?;
        match device.process_utilization_stats(since_timestamp_us) {
            Ok(samples) => Ok(samples
                .into_iter()
                .map(|sample| ProcessUtilizationSample {
                    pid: sample.pid,
                    timestamp_us: sample.timestamp,
                    sm_percent: sample.sm_util,
                })
                .collect()),
            // NVML reports "no sample newer than the timestamp" as NOT_FOUND.
            Err(NvmlError::NotFound) => Ok(Vec::new()),
            Err(error) => Err(backend_error_of(&error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_gaps_are_not_faults() {
        assert_eq!(
            backend_error_of(&NvmlError::NotSupported),
            BackendError::NotSupported
        );
        assert_eq!(
            backend_error_of(&NvmlError::FunctionNotFound),
            BackendError::NotSupported
        );
        assert_eq!(
            backend_error_of(&NvmlError::FailedToLoadSymbol("x".into())),
            BackendError::NotSupported
        );
        assert_eq!(
            backend_error_of(&NvmlError::NoPermission),
            BackendError::NoPermission
        );
    }

    #[test]
    fn lost_and_reloaded_driver_are_classified() {
        assert_eq!(backend_error_of(&NvmlError::GpuLost), BackendError::GpuLost);
        assert_eq!(
            backend_error_of(&NvmlError::ResetRequired),
            BackendError::GpuLost
        );
        assert_eq!(
            backend_error_of(&NvmlError::DriverNotLoaded),
            BackendError::Uninitialized
        );
        assert_eq!(
            backend_error_of(&NvmlError::Timeout),
            BackendError::Other("TIMEOUT".into())
        );
    }

    #[test]
    fn init_failures_are_classified() {
        assert!(matches!(
            connect_error_of(&NvmlError::LibraryNotFound),
            ConnectError::DriverMissing(_)
        ));
        assert!(matches!(
            connect_error_of(&NvmlError::DriverNotLoaded),
            ConnectError::DriverMissing(_)
        ));
        assert_eq!(
            connect_error_of(&NvmlError::NoPermission),
            ConnectError::NoPermission
        );
        assert_eq!(connect_error_of(&NvmlError::NotFound), ConnectError::NoGpu);
        assert!(matches!(
            connect_error_of(&NvmlError::Unknown),
            ConnectError::Other(_)
        ));
    }
}
