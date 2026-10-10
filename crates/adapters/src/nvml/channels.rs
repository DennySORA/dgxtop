//! Static description of every NVML channel: which backend query produces it,
//! its metric id, native unit, raw kind, read group and scope. Units are the
//! driver's native units; conversion is the collector's job.

use dgxtop_core::metric::{ids, native};
use dgxtop_core::naming::{entity, groups};
use dgxtop_core::quality::RawKind;

use super::backend::{
    ClockDomain, EccCounterScope, EccErrorKind, HealthValue, PcieDirection, PcieLink,
    RetiredPageCause, StaticFact, ThermalThreshold,
};

/// Highest NVLink index probed (`NVML_NVLINK_MAX_LINKS`).
pub const MAX_NVLINK_LINKS: u32 = 18;
/// Fans per GPU beyond this are not catalogued (bounded catalog).
pub const MAX_FANS_PER_GPU: u32 = 16;

/// Read group a channel belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChannelGroup {
    Fast,
    SlowHealth,
    Process,
}

impl ChannelGroup {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fast => groups::FAST,
            Self::SlowHealth => groups::SLOW_HEALTH,
            Self::Process => groups::PROCESS,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            groups::FAST => Some(Self::Fast),
            groups::SLOW_HEALTH => Some(Self::SlowHealth),
            groups::PROCESS => Some(Self::Process),
            _ => None,
        }
    }
}

/// One readable NVML channel of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Channel {
    UtilizationCompute,
    UtilizationMemoryBusy,
    Temperature,
    PowerDraw,
    /// The instantaneous power beside the averaged draw (D-54).
    PowerInstant,
    Clock(ClockDomain),
    FanSpeed(u32),
    FramebufferUsed,
    FramebufferFree,
    PcieThroughput(PcieDirectionKey),
    EncoderUtilization,
    DecoderUtilization,
    EnergyTotal,
    Ecc(EccKindKey, EccScopeKey),
    RetiredPages(RetiredCauseKey),
    NvlinkActive,
    PowerLimit,
    MaxClock(ClockDomain),
    FramebufferTotal,
    FramebufferReserved,
    ProcessMemory,
    ProcessUtilization,
    PerformanceState,
    ThrottleReasons,
    /// Current PCIe link generation or width.
    PcieLink(PcieLink),
    MaxPcieLink(PcieLink),
    TemperatureThreshold(ThermalThreshold),
    /// A capability read once per generation (D-48).
    Static(StaticFact),
    /// A slowly changing live value (D-48).
    Health(HealthValue),
}

/// Orderable mirror of [`PcieDirection`] (channel ordering keeps paging stable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PcieDirectionKey {
    Transmit,
    Receive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EccKindKey {
    Corrected,
    Uncorrected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EccScopeKey {
    Volatile,
    Aggregate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RetiredCauseKey {
    MultipleSingleBit,
    DoubleBit,
}

impl PcieDirectionKey {
    pub const fn direction(self) -> PcieDirection {
        match self {
            Self::Transmit => PcieDirection::Transmit,
            Self::Receive => PcieDirection::Receive,
        }
    }
}

impl EccKindKey {
    pub const fn kind(self) -> EccErrorKind {
        match self {
            Self::Corrected => EccErrorKind::Corrected,
            Self::Uncorrected => EccErrorKind::Uncorrected,
        }
    }
}

impl EccScopeKey {
    pub const fn scope(self) -> EccCounterScope {
        match self {
            Self::Volatile => EccCounterScope::Volatile,
            Self::Aggregate => EccCounterScope::Aggregate,
        }
    }
}

impl RetiredCauseKey {
    pub const fn cause(self) -> RetiredPageCause {
        match self {
            Self::MultipleSingleBit => RetiredPageCause::MultipleSingleBitErrors,
            Self::DoubleBit => RetiredPageCause::DoubleBitError,
        }
    }
}

impl Channel {
    /// Every per-device channel for a device with `fan_count` fans.
    pub fn device_channels(fan_count: u32) -> Vec<Self> {
        let mut channels = vec![
            Self::UtilizationCompute,
            Self::UtilizationMemoryBusy,
            Self::Temperature,
            Self::PowerDraw,
            Self::PowerInstant,
            Self::PerformanceState,
            Self::ThrottleReasons,
        ];
        channels.extend(ClockDomain::ALL.map(Self::Clock));
        channels.extend((0..fan_count.min(MAX_FANS_PER_GPU)).map(Self::FanSpeed));
        channels.extend([
            Self::FramebufferUsed,
            Self::FramebufferFree,
            Self::PcieThroughput(PcieDirectionKey::Transmit),
            Self::PcieThroughput(PcieDirectionKey::Receive),
            Self::EncoderUtilization,
            Self::DecoderUtilization,
            Self::EnergyTotal,
        ]);
        for kind in [EccKindKey::Corrected, EccKindKey::Uncorrected] {
            for scope in [EccScopeKey::Volatile, EccScopeKey::Aggregate] {
                channels.push(Self::Ecc(kind, scope));
            }
        }
        channels.extend([
            Self::RetiredPages(RetiredCauseKey::MultipleSingleBit),
            Self::RetiredPages(RetiredCauseKey::DoubleBit),
            Self::NvlinkActive,
            Self::PowerLimit,
        ]);
        channels.extend(PcieLink::ALL.map(Self::PcieLink));
        channels.extend(PcieLink::ALL.map(Self::MaxPcieLink));
        channels.extend(ThermalThreshold::ALL.map(Self::TemperatureThreshold));
        channels.extend(ClockDomain::ALL.map(Self::MaxClock));
        channels.extend(StaticFact::ALL.map(Self::Static));
        channels.extend(HealthValue::all().into_iter().map(Self::Health));
        channels.extend([
            Self::FramebufferTotal,
            Self::FramebufferReserved,
            Self::ProcessMemory,
            Self::ProcessUtilization,
        ]);
        channels
    }

    /// Source channel name (`nvml:<UUID>:<channel>`); stable per device.
    pub fn name(self) -> String {
        match self {
            Self::UtilizationCompute => "utilization.compute".to_owned(),
            Self::UtilizationMemoryBusy => "utilization.memory_busy".to_owned(),
            Self::Temperature => "temperature".to_owned(),
            Self::PowerDraw => "power.draw".to_owned(),
            Self::PowerInstant => "power.instant".to_owned(),
            Self::Clock(domain) => format!("clock.{}", domain.name()),
            Self::FanSpeed(fan) => format!("fan.{fan}"),
            Self::FramebufferUsed => "memory.framebuffer.used".to_owned(),
            Self::FramebufferFree => "memory.framebuffer.free".to_owned(),
            Self::PcieThroughput(PcieDirectionKey::Transmit) => "pcie.tx".to_owned(),
            Self::PcieThroughput(PcieDirectionKey::Receive) => "pcie.rx".to_owned(),
            Self::EncoderUtilization => "encoder.utilization".to_owned(),
            Self::DecoderUtilization => "decoder.utilization".to_owned(),
            Self::EnergyTotal => "energy.total".to_owned(),
            Self::Ecc(kind, scope) => {
                format!("ecc.{}.{}", ecc_kind_name(kind), scope.scope().name())
            }
            Self::RetiredPages(RetiredCauseKey::MultipleSingleBit) => {
                "retired_pages.single".to_owned()
            }
            Self::RetiredPages(RetiredCauseKey::DoubleBit) => "retired_pages.double".to_owned(),
            Self::NvlinkActive => "nvlink.active".to_owned(),
            Self::PowerLimit => "power.limit".to_owned(),
            Self::MaxClock(domain) => format!("clock.maximum.{}", domain.name()),
            Self::FramebufferTotal => "memory.framebuffer.total".to_owned(),
            Self::FramebufferReserved => "memory.framebuffer.reserved".to_owned(),
            Self::ProcessMemory => "process.memory_allocated".to_owned(),
            Self::ProcessUtilization => "process.utilization".to_owned(),
            Self::PerformanceState => "performance_state".to_owned(),
            Self::ThrottleReasons => "throttle_reasons".to_owned(),
            Self::PcieLink(link) => format!("pcie.link.{}", link.name()),
            Self::MaxPcieLink(link) => format!("pcie.link.maximum.{}", link.name()),
            Self::TemperatureThreshold(threshold) => {
                format!("temperature.threshold.{}", threshold.name())
            }
            Self::Static(fact) => static_name(fact).to_owned(),
            Self::Health(HealthValue::ThrottleTime(cause)) => {
                format!("throttle.time.{}", cause.name())
            }
            Self::Health(value) => health_name(value).to_owned(),
        }
    }

    /// Key of the backend query behind this channel; channels served by one
    /// call (utilization pair, framebuffer quadruple) share capability state.
    pub fn query_key(self) -> String {
        match self {
            Self::UtilizationCompute | Self::UtilizationMemoryBusy => "utilization".to_owned(),
            Self::FramebufferUsed
            | Self::FramebufferFree
            | Self::FramebufferTotal
            | Self::FramebufferReserved => "framebuffer".to_owned(),
            Self::Static(StaticFact::PowerLimitMinimum | StaticFact::PowerLimitMaximum) => {
                "power.limit.constraints".to_owned()
            }
            Self::Health(
                HealthValue::EncoderSessions
                | HealthValue::EncoderFps
                | HealthValue::EncoderLatency,
            ) => "encoder.stats".to_owned(),
            other => other.name(),
        }
    }

    pub const fn metric_id(self) -> &'static str {
        match self {
            Self::UtilizationCompute => ids::GPU_UTILIZATION_COMPUTE,
            Self::UtilizationMemoryBusy => ids::GPU_UTILIZATION_MEMORY_BUSY,
            Self::Temperature => ids::GPU_TEMPERATURE,
            Self::PowerDraw => ids::GPU_POWER_DRAW,
            Self::PowerInstant => ids::GPU_POWER_INSTANT,
            Self::Clock(ClockDomain::Graphics) => ids::GPU_CLOCK_GRAPHICS,
            Self::Clock(ClockDomain::Memory) => ids::GPU_CLOCK_MEMORY,
            Self::Clock(ClockDomain::Sm) => ids::GPU_CLOCK_SM,
            Self::Clock(ClockDomain::Video) => ids::GPU_CLOCK_VIDEO,
            Self::FanSpeed(_) => ids::GPU_FAN_SPEED,
            Self::FramebufferUsed => ids::GPU_MEMORY_FB_USED,
            Self::FramebufferFree => ids::GPU_MEMORY_FB_FREE,
            Self::PcieThroughput(PcieDirectionKey::Transmit) => ids::GPU_PCIE_TX,
            Self::PcieThroughput(PcieDirectionKey::Receive) => ids::GPU_PCIE_RX,
            Self::EncoderUtilization => ids::GPU_ENCODER_UTILIZATION,
            Self::DecoderUtilization => ids::GPU_DECODER_UTILIZATION,
            Self::EnergyTotal => ids::GPU_ENERGY_TOTAL,
            Self::Ecc(EccKindKey::Corrected, _) => ids::GPU_ECC_CORRECTED,
            Self::Ecc(EccKindKey::Uncorrected, _) => ids::GPU_ECC_UNCORRECTED,
            Self::RetiredPages(RetiredCauseKey::MultipleSingleBit) => ids::GPU_RETIRED_PAGES_SINGLE,
            Self::RetiredPages(RetiredCauseKey::DoubleBit) => ids::GPU_RETIRED_PAGES_DOUBLE,
            Self::NvlinkActive => ids::GPU_NVLINK_ACTIVE,
            Self::PowerLimit => ids::GPU_POWER_LIMIT,
            Self::MaxClock(_) => ids::GPU_CLOCK_MAXIMUM,
            Self::FramebufferTotal => ids::GPU_MEMORY_FB_TOTAL,
            Self::FramebufferReserved => ids::GPU_MEMORY_FB_RESERVED,
            Self::ProcessMemory => ids::PROCESS_GPU_MEMORY,
            Self::ProcessUtilization => ids::PROCESS_GPU_UTILIZATION,
            Self::PerformanceState => ids::GPU_PERFORMANCE_STATE,
            Self::ThrottleReasons => ids::GPU_THROTTLE_REASONS,
            Self::PcieLink(PcieLink::Generation) => ids::GPU_PCIE_LINK_GENERATION,
            Self::PcieLink(PcieLink::Width) => ids::GPU_PCIE_LINK_WIDTH,
            Self::MaxPcieLink(_) => ids::GPU_PCIE_LINK_MAXIMUM,
            Self::TemperatureThreshold(_) => ids::GPU_TEMPERATURE_THRESHOLD,
            Self::Static(StaticFact::Cores) => ids::GPU_CORES,
            Self::Static(StaticFact::MemoryBusWidth) => ids::GPU_MEMORY_BUS_WIDTH,
            Self::Static(StaticFact::Bar1Total) => ids::GPU_MEMORY_BAR1_TOTAL,
            Self::Static(StaticFact::PowerLimitDefault) => ids::GPU_POWER_LIMIT_DEFAULT,
            Self::Static(StaticFact::PowerLimitMinimum) => ids::GPU_POWER_LIMIT_MINIMUM,
            Self::Static(StaticFact::PowerLimitMaximum) => ids::GPU_POWER_LIMIT_MAXIMUM,
            Self::Static(StaticFact::PowerLimitRequested) => ids::GPU_POWER_LIMIT_REQUESTED,
            Self::Health(HealthValue::Bar1Used) => ids::GPU_MEMORY_BAR1_USED,
            Self::Health(HealthValue::EncoderSessions) => ids::GPU_ENCODER_SESSIONS,
            Self::Health(HealthValue::EncoderFps) => ids::GPU_ENCODER_FPS,
            Self::Health(HealthValue::EncoderLatency) => ids::GPU_ENCODER_LATENCY,
            Self::Health(HealthValue::PcieReplays) => ids::GPU_PCIE_REPLAYS,
            Self::Health(HealthValue::ThrottleTime(_)) => ids::GPU_THROTTLE_TIME,
        }
    }

    /// Scope text of the metric key (process channels use `gpu:<UUID>`).
    pub fn scope(self, uuid: &str) -> String {
        match self {
            Self::FanSpeed(fan) => format!("fan:{fan}"),
            Self::Ecc(_, scope) => scope.scope().name().to_owned(),
            Self::MaxClock(domain) => domain.name().to_owned(),
            Self::MaxPcieLink(link) => link.name().to_owned(),
            Self::TemperatureThreshold(threshold) => threshold.name().to_owned(),
            Self::Health(HealthValue::ThrottleTime(cause)) => cause.name().to_owned(),
            Self::ProcessMemory | Self::ProcessUtilization => format!("gpu:{uuid}"),
            _ => entity::SCOPE_GPU.to_owned(),
        }
    }

    pub const fn native_unit(self) -> &'static str {
        match self {
            Self::UtilizationCompute
            | Self::UtilizationMemoryBusy
            | Self::FanSpeed(_)
            | Self::EncoderUtilization
            | Self::DecoderUtilization
            | Self::ProcessUtilization => native::PERCENT,
            Self::Temperature | Self::TemperatureThreshold(_) => native::DEG_C,
            Self::PowerDraw
            | Self::PowerInstant
            | Self::PowerLimit
            | Self::Static(
                StaticFact::PowerLimitDefault
                | StaticFact::PowerLimitMinimum
                | StaticFact::PowerLimitMaximum
                | StaticFact::PowerLimitRequested,
            ) => native::MILLI_WATT,
            Self::Clock(_) | Self::MaxClock(_) => native::MHZ,
            Self::FramebufferUsed
            | Self::FramebufferFree
            | Self::FramebufferTotal
            | Self::FramebufferReserved
            | Self::ProcessMemory
            | Self::Static(StaticFact::Bar1Total)
            | Self::Health(HealthValue::Bar1Used) => native::BYTES,
            Self::Health(HealthValue::EncoderFps) => native::COUNT_PER_SECOND,
            Self::Health(HealthValue::EncoderLatency) => native::MICROSECONDS,
            Self::Health(HealthValue::ThrottleTime(_)) => native::NANOSECONDS,
            // NVML documents "KB/s" without proving 1000 or 1024; the collector
            // has no conversion for this unit and therefore rejects it.
            Self::PcieThroughput(_) => native::KB_UNVERIFIED_PER_SECOND,
            Self::EnergyTotal => native::MILLI_JOULE,
            Self::Ecc(_, _)
            | Self::RetiredPages(_)
            | Self::NvlinkActive
            | Self::PerformanceState
            | Self::ThrottleReasons
            | Self::PcieLink(_)
            | Self::MaxPcieLink(_)
            | Self::Static(StaticFact::Cores | StaticFact::MemoryBusWidth)
            | Self::Health(HealthValue::EncoderSessions | HealthValue::PcieReplays) => {
                native::COUNT
            }
        }
    }

    pub const fn raw_kind(self) -> RawKind {
        match self {
            Self::EnergyTotal | Self::Ecc(_, _) => RawKind::Counter,
            Self::PowerLimit
            | Self::MaxClock(_)
            | Self::MaxPcieLink(_)
            | Self::TemperatureThreshold(_)
            | Self::FramebufferTotal
            | Self::FramebufferReserved
            | Self::Static(_) => RawKind::Static,
            _ => RawKind::Gauge,
        }
    }

    pub const fn group(self) -> ChannelGroup {
        match self {
            Self::EnergyTotal
            | Self::Ecc(_, _)
            | Self::RetiredPages(_)
            | Self::NvlinkActive
            | Self::PowerLimit
            | Self::MaxClock(_)
            | Self::PcieLink(_)
            | Self::MaxPcieLink(_)
            | Self::TemperatureThreshold(_)
            | Self::FramebufferTotal
            | Self::FramebufferReserved
            | Self::Static(_)
            | Self::Health(_) => ChannelGroup::SlowHealth,
            Self::ProcessMemory | Self::ProcessUtilization => ChannelGroup::Process,
            _ => ChannelGroup::Fast,
        }
    }

    /// Static values come from the per-generation metadata cache.
    pub const fn is_static_metadata(self) -> bool {
        matches!(self.raw_kind(), RawKind::Static)
    }

    /// Framebuffer channels are meaningless on unified-memory devices.
    pub const fn is_framebuffer(self) -> bool {
        matches!(
            self,
            Self::FramebufferUsed
                | Self::FramebufferFree
                | Self::FramebufferTotal
                | Self::FramebufferReserved
        )
    }
}

const fn static_name(fact: StaticFact) -> &'static str {
    match fact {
        StaticFact::Cores => "cores",
        StaticFact::MemoryBusWidth => "memory.bus_width",
        StaticFact::Bar1Total => "memory.bar1.total",
        StaticFact::PowerLimitDefault => "power.limit.default",
        StaticFact::PowerLimitMinimum => "power.limit.minimum",
        StaticFact::PowerLimitMaximum => "power.limit.maximum",
        StaticFact::PowerLimitRequested => "power.limit.requested",
    }
}

const fn health_name(value: HealthValue) -> &'static str {
    match value {
        HealthValue::Bar1Used => "memory.bar1.used",
        HealthValue::EncoderSessions => "encoder.sessions",
        HealthValue::EncoderFps => "encoder.fps",
        HealthValue::EncoderLatency => "encoder.latency",
        HealthValue::PcieReplays => "pcie.replays",
        HealthValue::ThrottleTime(_) => "throttle.time",
    }
}

const fn ecc_kind_name(kind: EccKindKey) -> &'static str {
    match kind {
        EccKindKey::Corrected => "corrected",
        EccKindKey::Uncorrected => "uncorrected",
    }
}

/// Every NVML metric id (used for capabilities when NVML is unavailable).
pub fn all_metric_ids() -> Vec<&'static str> {
    let mut metric_ids: Vec<&'static str> = Channel::device_channels(1)
        .into_iter()
        .map(Channel::metric_id)
        .collect();
    metric_ids.sort_unstable();
    metric_ids.dedup();
    metric_ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn channel_names_are_unique_per_device() {
        let channels = Channel::device_channels(3);
        let names: BTreeSet<String> = channels.iter().map(|channel| channel.name()).collect();
        assert_eq!(names.len(), channels.len());
    }

    #[test]
    fn every_metric_id_is_catalogued_with_matching_kind() {
        for channel in Channel::device_channels(2) {
            let descriptor = dgxtop_core::metric::descriptor(channel.metric_id())
                .unwrap_or_else(|| panic!("{} missing from catalog", channel.metric_id()));
            assert_eq!(descriptor.kind, channel.raw_kind(), "{channel:?}");
            assert!(dgxtop_core::metric::NativeUnit::new(channel.native_unit()).is_ok());
        }
    }

    #[test]
    fn scopes_follow_naming_contract() {
        assert_eq!(Channel::FanSpeed(2).scope("GPU-1"), "fan:2");
        assert_eq!(
            Channel::Ecc(EccKindKey::Corrected, EccScopeKey::Aggregate).scope("GPU-1"),
            "aggregate"
        );
        assert_eq!(Channel::MaxClock(ClockDomain::Sm).scope("GPU-1"), "sm");
        assert_eq!(Channel::ProcessMemory.scope("GPU-1"), "gpu:GPU-1");
        assert_eq!(Channel::Temperature.scope("GPU-1"), "gpu");
    }

    #[test]
    fn fans_are_bounded_and_groups_assigned() {
        let channels = Channel::device_channels(1000);
        let fans = channels
            .iter()
            .filter(|channel| matches!(channel, Channel::FanSpeed(_)))
            .count();
        assert_eq!(fans, usize::try_from(MAX_FANS_PER_GPU).unwrap());
        assert_eq!(Channel::EnergyTotal.group(), ChannelGroup::SlowHealth);
        assert_eq!(Channel::ProcessUtilization.group(), ChannelGroup::Process);
        assert_eq!(Channel::PowerDraw.group(), ChannelGroup::Fast);
        assert_eq!(
            ChannelGroup::from_name("slow_health"),
            Some(ChannelGroup::SlowHealth)
        );
        assert!(all_metric_ids().contains(&ids::PROCESS_GPU_MEMORY));
    }
}
