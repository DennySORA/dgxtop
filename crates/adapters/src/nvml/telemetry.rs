//! Reads the `fast` and `slow_health` groups of every device.
//!
//! Each channel yields exactly one record: a native value, or `None` with a
//! status and reason. A lost GPU only turns its own remaining channels into
//! `removed` records (no further driver calls for it this cycle); a lost
//! library session stops the read and marks every remaining record failed.

use std::collections::BTreeSet;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::metric::Scalar;
use dgxtop_core::observation::{Observation, ReadPlan, SourceDescriptor};
use dgxtop_core::quality::{Quality, ReadStatus};

use crate::common::RecordFactory;

use super::backend::{
    BackendError, BackendResult, FramebufferMemory, GpuDeviceQueries, GpuTelemetry,
    UtilizationRates,
};
use super::capabilities::{Admission, CapabilityTracker};
use super::catalog::PublishedSources;
use super::channels::{Channel, ChannelGroup, MAX_NVLINK_LINKS};
use super::inventory::{
    DeviceMetadata, Inventory, MemoryModel, framebuffer_size, framebuffer_size_from,
};

/// Reason attached to PCIe records: NVML's "KB" may be 1000 or 1024 bytes.
pub const PCIE_UNVERIFIED_REASON: &str = "NVML_KB_DEFINITION_UNVERIFIED";
const UNIFIED_MEMORY_REASON: &str = "NVML_UNIFIED_MEMORY_NO_FRAMEBUFFER";
const DEADLINE_REASON: &str = "NVML_DEADLINE_EXPIRED";

/// Outcome of reading one group across all devices.
#[derive(Debug, Default)]
pub struct GroupOutcome {
    pub records: Vec<Observation>,
    /// The library session died (driver unloaded); the adapter must reconnect.
    pub session_lost: bool,
}

/// Everything a group read needs besides the backend.
pub struct TelemetryRead<'a> {
    pub plan: &'a ReadPlan,
    pub clock: &'a dyn TimeProvider,
    pub published: &'a PublishedSources,
    pub tracker: &'a mut CapabilityTracker,
    pub lost_devices: &'a mut BTreeSet<String>,
    /// Requested slots; empty means every source of the group.
    pub requested_slots: &'a BTreeSet<u32>,
}

/// Per-device memo so one driver call serves channels that share it.
#[derive(Default)]
struct DeviceQueries {
    utilization: Option<BackendResult<UtilizationRates>>,
    framebuffer: Option<BackendResult<FramebufferMemory>>,
}

impl TelemetryRead<'_> {
    /// Reads `group` for every device of the inventory.
    pub fn read_group(
        &mut self,
        backend: &dyn GpuDeviceQueries,
        inventory: &mut Inventory,
        group: ChannelGroup,
    ) -> GroupOutcome {
        let mut outcome = GroupOutcome::default();
        let uuids: Vec<String> = inventory
            .devices()
            .map(|device| device.uuid.clone())
            .collect();
        for uuid in uuids {
            let Some(device) = inventory.device_mut(&uuid) else {
                continue;
            };
            let channels = self.channels_of(device, group);
            if outcome.session_lost {
                self.fail_all(&mut outcome, &uuid, &channels, &BackendError::Uninitialized);
                continue;
            }
            if self.clock.now_boot_ns() >= self.plan.deadline_boot_ns {
                self.time_out_all(&mut outcome, &uuid, &channels);
                continue;
            }
            self.read_device(backend, device, &channels, &mut outcome);
        }
        outcome
    }

    fn channels_of(&self, device: &DeviceMetadata, group: ChannelGroup) -> Vec<Channel> {
        Channel::device_channels(device.catalogued_fans())
            .into_iter()
            .filter(|channel| channel.group() == group)
            .filter(|channel| {
                self.published
                    .get(&device.uuid, *channel)
                    .is_some_and(|descriptor| self.is_requested(descriptor))
            })
            .collect()
    }

    fn is_requested(&self, descriptor: &SourceDescriptor) -> bool {
        self.requested_slots.is_empty() || self.requested_slots.contains(&descriptor.slot)
    }

    fn read_device(
        &mut self,
        backend: &dyn GpuDeviceQueries,
        device: &mut DeviceMetadata,
        channels: &[Channel],
        outcome: &mut GroupOutcome,
    ) {
        let mut queries = DeviceQueries::default();
        for channel in channels {
            let started = self.clock.now_boot_ns();
            let result = if self.lost_devices.contains(&device.uuid) {
                Err(BackendError::GpuLost)
            } else if outcome.session_lost {
                Err(BackendError::Uninitialized)
            } else {
                self.channel_value(backend, device, *channel, &mut queries, started)
            };
            match &result {
                Err(BackendError::GpuLost) => {
                    self.lost_devices.insert(device.uuid.clone());
                }
                Err(BackendError::Uninitialized) => outcome.session_lost = true,
                _ => {}
            }
            let finished = self.clock.now_boot_ns();
            if let Some(record) = self.record(device, *channel, &result, started, finished) {
                outcome.records.push(record);
            }
        }
    }

    fn channel_value(
        &mut self,
        backend: &dyn GpuDeviceQueries,
        device: &mut DeviceMetadata,
        channel: Channel,
        queries: &mut DeviceQueries,
        now_boot_ns: u64,
    ) -> BackendResult<u64> {
        if channel.is_static_metadata() {
            return self.static_value(backend, device, channel, now_boot_ns);
        }
        let query_key = channel.query_key();
        if let Admission::Skip(failure) = self.tracker.admit(&device.uuid, &query_key, now_boot_ns)
        {
            return Err(failure);
        }
        let result = live_value(backend, &device.uuid, channel, queries);
        self.tracker
            .record(&device.uuid, &query_key, &result, now_boot_ns);
        if channel.is_framebuffer() {
            refresh_memory_model(device, queries);
        }
        result
    }

    /// Static values come from the generation cache; failed ones are re-read
    /// (capability failures at most once per probe interval).
    fn static_value(
        &mut self,
        backend: &dyn GpuDeviceQueries,
        device: &mut DeviceMetadata,
        channel: Channel,
        now_boot_ns: u64,
    ) -> BackendResult<u64> {
        if let Some(Ok(value)) = device.static_value(channel) {
            return Ok(value);
        }
        let query_key = channel.query_key();
        if let Admission::Skip(failure) = self.tracker.admit(&device.uuid, &query_key, now_boot_ns)
        {
            return Err(failure);
        }
        if channel.is_framebuffer() {
            let size = framebuffer_size(backend, &device.uuid);
            self.tracker
                .record(&device.uuid, &query_key, &size, now_boot_ns);
            device.update_framebuffer(size);
        } else {
            device.refresh_static(backend, channel);
        }
        let value = device
            .static_value(channel)
            .unwrap_or(Err(BackendError::NotSupported));
        self.tracker
            .record(&device.uuid, &query_key, &value, now_boot_ns);
        value
    }

    fn record(
        &self,
        device: &DeviceMetadata,
        channel: Channel,
        result: &BackendResult<u64>,
        started: u64,
        finished: u64,
    ) -> Option<Observation> {
        let descriptor = self.published.get(&device.uuid, channel)?;
        let key = descriptor.metric.clone()?;
        Some(match result {
            Ok(value) => {
                let (quality, reasons) = value_quality(channel);
                RecordFactory::ok(
                    self.plan,
                    descriptor,
                    key,
                    Scalar::from_u64(*value),
                    quality,
                    started,
                    finished,
                    reasons,
                )
            }
            Err(error) => RecordFactory::failed(
                self.plan,
                descriptor,
                key,
                error.read_status(),
                started,
                finished,
                failure_reason(device, channel, error),
            ),
        })
    }

    fn fail_all(
        &self,
        outcome: &mut GroupOutcome,
        uuid: &str,
        channels: &[Channel],
        error: &BackendError,
    ) {
        let now = self.clock.now_boot_ns();
        for channel in channels {
            if let Some(record) = self.failed_record(
                uuid,
                *channel,
                error.read_status(),
                now,
                error.reason_code(),
            ) {
                outcome.records.push(record);
            }
        }
    }

    fn time_out_all(&self, outcome: &mut GroupOutcome, uuid: &str, channels: &[Channel]) {
        let now = self.clock.now_boot_ns();
        for channel in channels {
            if let Some(record) = self.failed_record(
                uuid,
                *channel,
                ReadStatus::Timeout,
                now,
                DEADLINE_REASON.to_owned(),
            ) {
                outcome.records.push(record);
            }
        }
    }

    fn failed_record(
        &self,
        uuid: &str,
        channel: Channel,
        status: ReadStatus,
        now: u64,
        reason: String,
    ) -> Option<Observation> {
        let descriptor = self.published.get(uuid, channel)?;
        let key = descriptor.metric.clone()?;
        Some(RecordFactory::failed(
            self.plan, descriptor, key, status, now, now, reason,
        ))
    }
}

fn value_quality(channel: Channel) -> (Quality, Vec<String>) {
    if matches!(channel, Channel::PcieThroughput(_)) {
        (Quality::Suspect, vec![PCIE_UNVERIFIED_REASON.to_owned()])
    } else {
        (Quality::Usable, Vec::new())
    }
}

fn failure_reason(device: &DeviceMetadata, channel: Channel, error: &BackendError) -> String {
    if channel.is_framebuffer()
        && device.memory_model == MemoryModel::Unified
        && *error == BackendError::NotSupported
    {
        return UNIFIED_MEMORY_REASON.to_owned();
    }
    error.reason_code()
}

/// A successful live framebuffer read proves dedicated memory exists.
fn refresh_memory_model(device: &mut DeviceMetadata, queries: &DeviceQueries) {
    if let Some(Ok(memory)) = &queries.framebuffer
        && device.memory_model != MemoryModel::Framebuffer
    {
        device.update_framebuffer(framebuffer_size_from(*memory));
    }
}

fn live_value(
    backend: &dyn GpuDeviceQueries,
    uuid: &str,
    channel: Channel,
    queries: &mut DeviceQueries,
) -> BackendResult<u64> {
    match channel {
        Channel::UtilizationCompute => {
            utilization(backend, uuid, queries).map(|rates| u64::from(rates.compute_percent))
        }
        Channel::UtilizationMemoryBusy => {
            utilization(backend, uuid, queries).map(|rates| u64::from(rates.memory_busy_percent))
        }
        Channel::Temperature => backend.temperature_c(uuid).map(u64::from),
        Channel::PowerDraw => backend.power_draw_mw(uuid).map(u64::from),
        Channel::PowerInstant => backend.power_instant_mw(uuid).map(u64::from),
        Channel::Clock(domain) => backend.clock_mhz(uuid, domain).map(u64::from),
        Channel::FanSpeed(fan) => backend.fan_speed_percent(uuid, fan).map(u64::from),
        Channel::FramebufferUsed => framebuffer(backend, uuid, queries).map(|memory| memory.used),
        Channel::FramebufferFree => framebuffer(backend, uuid, queries).map(|memory| memory.free),
        Channel::PcieThroughput(direction) => backend
            .pcie_throughput_kb(uuid, direction.direction())
            .map(u64::from),
        Channel::EncoderUtilization => backend.encoder_utilization(uuid).map(u64::from),
        Channel::DecoderUtilization => backend.decoder_utilization(uuid).map(u64::from),
        Channel::EnergyTotal => backend.total_energy_mj(uuid),
        Channel::Ecc(kind, scope) => backend.ecc_errors(uuid, kind.kind(), scope.scope()),
        Channel::RetiredPages(cause) => backend.retired_page_count(uuid, cause.cause()),
        Channel::NvlinkActive => active_nvlink_count(backend, uuid),
        Channel::PerformanceState => backend.performance_state(uuid).map(u64::from),
        Channel::ThrottleReasons => backend.throttle_reasons(uuid),
        Channel::PcieLink(link) => backend.pcie_link(uuid, link).map(u64::from),
        Channel::Health(value) => backend.health_value(uuid, value),
        Channel::PowerLimit
        | Channel::Static(_)
        | Channel::MaxClock(_)
        | Channel::MaxPcieLink(_)
        | Channel::TemperatureThreshold(_)
        | Channel::FramebufferTotal
        | Channel::FramebufferReserved
        | Channel::ProcessMemory
        | Channel::ProcessUtilization => Err(BackendError::Other("NOT_A_LIVE_CHANNEL".to_owned())),
    }
}

fn utilization(
    backend: &dyn GpuDeviceQueries,
    uuid: &str,
    queries: &mut DeviceQueries,
) -> BackendResult<UtilizationRates> {
    queries
        .utilization
        .get_or_insert_with(|| backend.utilization(uuid))
        .clone()
}

fn framebuffer(
    backend: &dyn GpuDeviceQueries,
    uuid: &str,
    queries: &mut DeviceQueries,
) -> BackendResult<FramebufferMemory> {
    let memory = queries
        .framebuffer
        .get_or_insert_with(|| backend.framebuffer_memory(uuid))
        .clone()?;
    if memory.total == 0 {
        return Err(BackendError::NotSupported);
    }
    Ok(memory)
}

/// Counts active links. Link 0 decides support; a later link that is not
/// supported (or out of range) ends the device's link list. Presence only:
/// no throughput is inferred.
pub fn active_nvlink_count(backend: &dyn GpuTelemetry, uuid: &str) -> BackendResult<u64> {
    let mut active = 0_u64;
    for link in 0..MAX_NVLINK_LINKS {
        match backend.nvlink_active(uuid, link) {
            Ok(true) => active = active.saturating_add(1),
            Ok(false) => {}
            Err(BackendError::NotSupported | BackendError::Other(_)) if link > 0 => break,
            Err(error) => return Err(error),
        }
    }
    Ok(active)
}
