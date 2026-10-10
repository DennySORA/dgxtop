//! Device inventory: enumeration by UUID, catalog generation and the
//! per-generation metadata cache (name, PCI id, serial, maxima, limits,
//! framebuffer size, fan count). Metadata is read once per generation; a new
//! UUID set, index order or driver version starts a new generation and drops
//! every cached value (driver reload must not reuse stale facts).

use std::collections::BTreeMap;

use super::backend::{
    BackendError, BackendResult, ClockDomain, DeviceDetails, FramebufferMemory, GpuBackend,
    GpuInventory, GpuTelemetry, PcieLink, StaticFact, ThermalThreshold,
};
use super::capabilities::CapabilityTracker;
use super::channels::Channel;
use super::stack::NvidiaStack;
use super::topology::DeviceTopology;

/// Upper bound on enumerated devices (bounded catalog and metadata cache).
pub const MAX_GPUS: u32 = 64;
/// Generations stay representable in schema `integer` fields.
const MAX_GENERATION: u32 = i32::MAX.unsigned_abs();

/// How device memory is organised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryModel {
    /// Dedicated framebuffer reported by the driver.
    Framebuffer,
    /// Framebuffer query unsupported while the device is present (e.g. GB10
    /// unified LPDDR); system RAM is never substituted.
    Unified,
    /// The framebuffer query failed for another reason.
    Unknown,
}

impl MemoryModel {
    pub const fn attribute(self) -> Option<&'static str> {
        match self {
            Self::Framebuffer => Some("framebuffer"),
            Self::Unified => Some("unified"),
            Self::Unknown => None,
        }
    }
}

/// Static framebuffer sizes in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramebufferSize {
    pub total: u64,
    pub reserved: u64,
}

/// Metadata of one device, valid for one generation.
#[derive(Debug, Clone)]
pub struct DeviceMetadata {
    pub uuid: String,
    pub index: u32,
    pub details: DeviceDetails,
    pub memory_model: MemoryModel,
    pub framebuffer: BackendResult<FramebufferSize>,
    pub power_limit_mw: BackendResult<u32>,
    pub max_clocks: BTreeMap<ClockDomain, BackendResult<u32>>,
    pub thresholds: BTreeMap<ThermalThreshold, BackendResult<u32>>,
    pub max_pcie_link: BTreeMap<PcieLink, BackendResult<u32>>,
    /// Cores, bus width, BAR1 size and power-limit bounds (D-48).
    pub facts: BTreeMap<StaticFact, BackendResult<u64>>,
    pub fan_count: BackendResult<u32>,
    pub compute_mode: Option<String>,
    pub persistence_mode: Option<bool>,
    /// The nearest CPUs and NUMA nodes, the paths to the other GPUs (D-54).
    pub topology: DeviceTopology,
    pub discovered_boot_ns: u64,
}

impl DeviceMetadata {
    /// Reads every cached fact of one device among `devices` (the
    /// enumeration's `(index, uuid)` pairs). Only a lost library session
    /// aborts; per-item failures stay attached to the item.
    fn load(
        backend: &dyn GpuBackend,
        (uuid, index): (&str, u32),
        devices: &[(u32, String)],
        now_boot_ns: u64,
        tracker: &mut CapabilityTracker,
    ) -> Result<Self, BackendError> {
        let details = abort_on_session_loss(backend.device_details(uuid))?.unwrap_or_default();
        let framebuffer = abort_on_lost(framebuffer_size(backend, uuid))?;
        tracker.record(
            uuid,
            &Channel::FramebufferTotal.query_key(),
            &framebuffer,
            now_boot_ns,
        );
        let power_limit_mw = abort_on_lost(backend.power_limit_mw(uuid))?;
        tracker.record(
            uuid,
            &Channel::PowerLimit.query_key(),
            &power_limit_mw,
            now_boot_ns,
        );
        let mut max_clocks = BTreeMap::new();
        for domain in ClockDomain::ALL {
            let value = abort_on_lost(backend.max_clock_mhz(uuid, domain))?;
            tracker.record(
                uuid,
                &Channel::MaxClock(domain).query_key(),
                &value,
                now_boot_ns,
            );
            max_clocks.insert(domain, value);
        }
        let mut thresholds = BTreeMap::new();
        for threshold in ThermalThreshold::ALL {
            let value = abort_on_lost(backend.temperature_threshold_c(uuid, threshold))?;
            let channel = Channel::TemperatureThreshold(threshold);
            tracker.record(uuid, &channel.query_key(), &value, now_boot_ns);
            thresholds.insert(threshold, value);
        }
        let mut max_pcie_link = BTreeMap::new();
        for link in PcieLink::ALL {
            let value = abort_on_lost(backend.max_pcie_link(uuid, link))?;
            let channel = Channel::MaxPcieLink(link);
            tracker.record(uuid, &channel.query_key(), &value, now_boot_ns);
            max_pcie_link.insert(link, value);
        }
        let mut facts = BTreeMap::new();
        for fact in StaticFact::ALL {
            let value = abort_on_lost(backend.static_fact(uuid, fact))?;
            let channel = Channel::Static(fact);
            tracker.record(uuid, &channel.query_key(), &value, now_boot_ns);
            facts.insert(fact, value);
        }
        Ok(Self {
            uuid: uuid.to_owned(),
            index,
            details,
            memory_model: memory_model_of(&framebuffer),
            framebuffer,
            power_limit_mw,
            max_clocks,
            thresholds,
            max_pcie_link,
            facts,
            fan_count: abort_on_lost(backend.fan_count(uuid))?,
            compute_mode: abort_on_session_loss(backend.compute_mode(uuid))?,
            persistence_mode: abort_on_session_loss(backend.persistence_mode(uuid))?,
            topology: DeviceTopology::read(backend, uuid, devices)?,
            discovered_boot_ns: now_boot_ns,
        })
    }

    /// Cached static value for a static channel (`None` for other channels).
    pub fn static_value(&self, channel: Channel) -> Option<BackendResult<u64>> {
        let value = match channel {
            Channel::PowerLimit => self.power_limit_mw.clone().map(u64::from),
            Channel::MaxClock(domain) => self
                .max_clocks
                .get(&domain)
                .cloned()
                .unwrap_or(Err(BackendError::NotSupported))
                .map(u64::from),
            Channel::TemperatureThreshold(threshold) => cached(&self.thresholds, threshold),
            Channel::MaxPcieLink(link) => cached(&self.max_pcie_link, link),
            Channel::Static(fact) => self
                .facts
                .get(&fact)
                .cloned()
                .unwrap_or(Err(BackendError::NotSupported)),
            Channel::FramebufferTotal => self.framebuffer.clone().map(|size| size.total),
            Channel::FramebufferReserved => self.framebuffer.clone().map(|size| size.reserved),
            _ => return None,
        };
        Some(value)
    }

    /// Re-reads one static value after a failure (throttled by the caller).
    pub fn refresh_static(&mut self, backend: &dyn GpuInventory, channel: Channel) {
        match channel {
            Channel::PowerLimit => self.power_limit_mw = backend.power_limit_mw(&self.uuid),
            Channel::MaxClock(domain) => {
                self.max_clocks
                    .insert(domain, backend.max_clock_mhz(&self.uuid, domain));
            }
            Channel::TemperatureThreshold(threshold) => {
                let value = backend.temperature_threshold_c(&self.uuid, threshold);
                self.thresholds.insert(threshold, value);
            }
            Channel::MaxPcieLink(link) => {
                let value = backend.max_pcie_link(&self.uuid, link);
                self.max_pcie_link.insert(link, value);
            }
            Channel::Static(fact) => {
                self.facts
                    .insert(fact, backend.static_fact(&self.uuid, fact));
            }
            _ => {}
        }
    }

    /// Replaces the framebuffer facts after a re-probe through telemetry.
    pub fn update_framebuffer(&mut self, framebuffer: BackendResult<FramebufferSize>) {
        self.memory_model = memory_model_of(&framebuffer);
        self.framebuffer = framebuffer;
    }

    /// Number of catalogued fans (0 when none or unsupported).
    pub fn catalogued_fans(&self) -> u32 {
        self.fan_count.clone().unwrap_or(0)
    }
}

/// The cached value of `key`, or unsupported when it was never read.
fn cached<K: Ord>(values: &BTreeMap<K, BackendResult<u32>>, key: K) -> BackendResult<u64> {
    values
        .get(&key)
        .cloned()
        .unwrap_or(Err(BackendError::NotSupported))
        .map(u64::from)
}

/// Framebuffer facts read through telemetry.
pub fn framebuffer_size(backend: &dyn GpuTelemetry, uuid: &str) -> BackendResult<FramebufferSize> {
    framebuffer_size_from(backend.framebuffer_memory(uuid)?)
}

/// A zero-sized framebuffer is reported as unsupported because it describes
/// a device without dedicated memory, not "0 bytes free".
pub fn framebuffer_size_from(memory: FramebufferMemory) -> BackendResult<FramebufferSize> {
    if memory.total == 0 {
        return Err(BackendError::NotSupported);
    }
    Ok(FramebufferSize {
        total: memory.total,
        reserved: memory.reserved,
    })
}

fn memory_model_of(framebuffer: &BackendResult<FramebufferSize>) -> MemoryModel {
    match framebuffer {
        Ok(_) => MemoryModel::Framebuffer,
        Err(BackendError::NotSupported) => MemoryModel::Unified,
        Err(_) => MemoryModel::Unknown,
    }
}

/// Keeps per-item failures but aborts when the library session is gone.
fn abort_on_lost<T>(result: BackendResult<T>) -> Result<BackendResult<T>, BackendError> {
    match result {
        Err(BackendError::Uninitialized) => Err(BackendError::Uninitialized),
        other => Ok(other),
    }
}

fn abort_on_session_loss<T>(result: BackendResult<T>) -> Result<Option<T>, BackendError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(BackendError::Uninitialized) => Err(BackendError::Uninitialized),
        Err(_) => Ok(None),
    }
}

/// Driver version, `(uuid, index)` pairs and the software stack; any change
/// is a new generation.
type Fingerprint = (Option<String>, Vec<(String, u32)>, NvidiaStack);

/// The driver and CUDA stack every GPU of the host runs on (D-48).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostSoftware {
    pub driver_version: Option<String>,
    pub nvml_version: Option<String>,
    /// Newest CUDA version the driver supports.
    pub cuda_driver_version: Option<String>,
    pub stack: NvidiaStack,
}

/// One enumeration pass: `(index, uuid)` pairs plus the driver's versions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Enumeration {
    pub driver_version: Option<String>,
    pub nvml_version: Option<String>,
    pub cuda_driver_version: Option<String>,
    pub devices: Vec<(u32, String)>,
    /// Indices whose UUID could not be read (they cannot have an identity).
    pub unreadable_indices: u32,
    /// The driver reported more devices than [`MAX_GPUS`].
    pub truncated: bool,
}

impl Enumeration {
    /// Enumerates devices; only a failing device count or a lost session fails.
    pub fn read(backend: &dyn GpuInventory) -> Result<Self, BackendError> {
        let optional = |result: BackendResult<String>| match result {
            Ok(version) => Ok(Some(version)),
            Err(BackendError::Uninitialized) => Err(BackendError::Uninitialized),
            Err(_) => Ok(None),
        };
        let driver_version = optional(backend.driver_version())?;
        let nvml_version = optional(backend.nvml_version())?;
        let cuda_driver_version = optional(backend.cuda_driver_version())?;
        let count = backend.device_count()?;
        let mut enumeration = Self {
            driver_version,
            nvml_version,
            cuda_driver_version,
            truncated: count > MAX_GPUS,
            ..Self::default()
        };
        for index in 0..count.min(MAX_GPUS) {
            match backend.device_uuid(index) {
                Ok(uuid) if valid_uuid(&uuid) => enumeration.devices.push((index, uuid)),
                Err(BackendError::Uninitialized) => return Err(BackendError::Uninitialized),
                Ok(_) | Err(_) => {
                    enumeration.unreadable_indices =
                        enumeration.unreadable_indices.saturating_add(1);
                }
            }
        }
        enumeration
            .devices
            .sort_by(|left, right| left.1.cmp(&right.1));
        enumeration
            .devices
            .dedup_by(|left, right| left.1 == right.1);
        Ok(enumeration)
    }

    fn fingerprint(&self, stack: &NvidiaStack) -> Fingerprint {
        (
            self.driver_version.clone(),
            self.devices
                .iter()
                .map(|(index, uuid)| (uuid.clone(), *index))
                .collect(),
            stack.clone(),
        )
    }
}

/// A UUID must form valid entity/source ids (`gpu:<uuid>`, no separators that
/// would make `gpupid:<uuid>:<pid>` ambiguous).
fn valid_uuid(uuid: &str) -> bool {
    !uuid.is_empty()
        && uuid.len() <= 96
        && uuid
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// What changed in one inventory refresh.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InventoryChange {
    pub generation_changed: bool,
    pub removed_uuids: Vec<String>,
}

/// Current devices and their generation.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    generation: u32,
    fingerprint: Option<Fingerprint>,
    software: HostSoftware,
    devices: BTreeMap<String, DeviceMetadata>,
}

impl Inventory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn driver_version(&self) -> Option<&str> {
        self.software.driver_version.as_deref()
    }

    pub fn software(&self) -> &HostSoftware {
        &self.software
    }

    pub fn devices(&self) -> impl Iterator<Item = &DeviceMetadata> {
        self.devices.values()
    }

    pub fn device_mut(&mut self, uuid: &str) -> Option<&mut DeviceMetadata> {
        self.devices.get_mut(uuid)
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Applies an enumeration and the software `stack` found beside it.
    /// Unchanged fingerprints keep every cached value (no metadata query); a
    /// changed one bumps the generation past both our own and the parent's
    /// generation and reloads all metadata.
    pub fn apply(
        &mut self,
        backend: &dyn GpuBackend,
        (enumeration, stack): (&Enumeration, &NvidiaStack),
        parent_generation: u32,
        now_boot_ns: u64,
        tracker: &mut CapabilityTracker,
    ) -> Result<InventoryChange, BackendError> {
        let fingerprint = enumeration.fingerprint(stack);
        if self.fingerprint.as_ref() == Some(&fingerprint) {
            return Ok(InventoryChange::default());
        }
        tracker.clear();
        let mut devices = BTreeMap::new();
        for (index, uuid) in &enumeration.devices {
            let metadata = DeviceMetadata::load(
                backend,
                (uuid, *index),
                &enumeration.devices,
                now_boot_ns,
                tracker,
            )?;
            devices.insert(uuid.clone(), metadata);
        }
        let removed_uuids = self
            .devices
            .keys()
            .filter(|uuid| !devices.contains_key(*uuid))
            .cloned()
            .collect();
        self.devices = devices;
        self.software = HostSoftware {
            driver_version: enumeration.driver_version.clone(),
            nvml_version: enumeration.nvml_version.clone(),
            cuda_driver_version: enumeration.cuda_driver_version.clone(),
            stack: stack.clone(),
        };
        self.fingerprint = Some(fingerprint);
        self.generation = self
            .generation
            .max(parent_generation)
            .saturating_add(1)
            .min(MAX_GENERATION);
        Ok(InventoryChange {
            generation_changed: true,
            removed_uuids,
        })
    }

    /// Forgets every device (library session lost); the generation is kept so
    /// the next successful enumeration moves forward.
    pub fn invalidate(&mut self) -> Vec<String> {
        self.fingerprint = None;
        self.software = HostSoftware::default();
        let removed = self.devices.keys().cloned().collect();
        self.devices.clear();
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_validation_rejects_separators_and_controls() {
        assert!(valid_uuid("GPU-0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"));
        assert!(!valid_uuid(""));
        assert!(!valid_uuid("GPU:1"));
        assert!(!valid_uuid("GPU 1"));
        assert!(!valid_uuid("GPU-\u{1b}[31m"));
        assert!(!valid_uuid(&"G".repeat(97)));
    }

    #[test]
    fn memory_model_follows_framebuffer_capability() {
        assert_eq!(
            memory_model_of(&Ok(FramebufferSize {
                total: 1,
                reserved: 0
            })),
            MemoryModel::Framebuffer
        );
        assert_eq!(
            memory_model_of(&Err(BackendError::NotSupported)),
            MemoryModel::Unified
        );
        assert_eq!(
            memory_model_of(&Err(BackendError::Other("TIMEOUT".into()))),
            MemoryModel::Unknown
        );
        assert_eq!(MemoryModel::Unified.attribute(), Some("unified"));
        assert_eq!(MemoryModel::Unknown.attribute(), None);
    }
}
