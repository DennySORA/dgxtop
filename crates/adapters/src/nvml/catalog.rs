//! Builds catalog records (entities, source descriptors, capabilities) from
//! the inventory. Entities are `gpu:<UUID>`; the enumeration index is only the
//! `nvml_index` display attribute. Source ids are
//! `nvml:<UUID>:<channel>` and keep their interned slot for the catalog's
//! lifetime.

use std::collections::BTreeMap;

use dgxtop_core::gpu_state::format_peers;
use dgxtop_core::identity::{
    AdapterId, BootId, EntityId, HostId, IdentityError, MetricId, MetricKey, OriginId,
    OriginIdentity, ProfileId, ReadGroup, SourceId,
};
use dgxtop_core::metric::NativeUnit;
use dgxtop_core::naming::{self, attributes};
use dgxtop_core::observation::{
    Attribute, Capability, EntityKind, EntityRecord, MAX_ATTRIBUTES, SourceDescriptor,
};
use dgxtop_core::quality::{SampleKind, Support};
use dgxtop_core::text::sanitize_display;

use crate::common::SlotInterner;

use super::capabilities::CapabilityTracker;
use super::channels::{Channel, all_metric_ids};
use super::inventory::{DeviceMetadata, HostSoftware, Inventory, MemoryModel};
use super::stack::NvidiaStack;

/// Manifest `source_limit`: sources beyond it are not catalogued.
pub const SOURCE_LIMIT: usize = 2048;
/// Profile used for NVML provenance (driver-reported native values).
pub const PROFILE_ID: &str = "nvml.native";
pub const PROFILE_REVISION: u32 = 1;
/// Fastest cadence the adapter accepts (manifest `minimum_sample_ms`).
pub const MIN_INTERVAL_MS: u32 = 1000;
const DRIVER_NAME: &str = "nvidia";
/// NVML reports C2C bandwidth in MB/s of 10^6 bytes.
const BYTES_PER_MB: u64 = 1_000_000;

/// Catalog construction failures (infrastructure-side, never values).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("invalid identity: {0}")]
    Identity(#[from] IdentityError),
    #[error("source slot space exhausted")]
    SlotsExhausted,
    #[error("invalid native unit {0}")]
    NativeUnit(&'static str),
}

/// Identity context shared by every record of this adapter.
#[derive(Debug, Clone)]
pub struct CatalogIdentity {
    pub host_id: HostId,
    pub boot_id: BootId,
    pub adapter_id: AdapterId,
    pub profile_id: ProfileId,
}

/// Published sources of the current generation, addressable by channel.
#[derive(Debug, Clone, Default)]
pub struct PublishedSources {
    by_channel: BTreeMap<(String, Channel), SourceDescriptor>,
    /// More sources existed than [`SOURCE_LIMIT`].
    pub truncated: bool,
}

impl PublishedSources {
    pub fn get(&self, uuid: &str, channel: Channel) -> Option<&SourceDescriptor> {
        self.by_channel.get(&(uuid.to_owned(), channel))
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &SourceDescriptor> {
        self.by_channel.values()
    }

    pub fn source_ids_of(&self, uuid: &str) -> Vec<SourceId> {
        self.by_channel
            .iter()
            .filter(|((device, _), _)| device == uuid)
            .map(|(_, descriptor)| descriptor.source_id.clone())
            .collect()
    }
}

/// Builds the published sources for the inventory's current generation.
pub fn publish_sources(
    inventory: &Inventory,
    identity: &CatalogIdentity,
    interner: &mut SlotInterner,
) -> Result<PublishedSources, CatalogError> {
    let mut published = PublishedSources::default();
    for device in inventory.devices() {
        for channel in Channel::device_channels(device.catalogued_fans()) {
            if published.by_channel.len() >= SOURCE_LIMIT {
                published.truncated = true;
                return Ok(published);
            }
            let descriptor = source_descriptor(
                device,
                channel,
                inventory.generation(),
                inventory.driver_version(),
                identity,
                interner,
            )?;
            published
                .by_channel
                .insert((device.uuid.clone(), channel), descriptor);
        }
    }
    Ok(published)
}

fn source_descriptor(
    device: &DeviceMetadata,
    channel: Channel,
    generation: u32,
    driver_version: Option<&str>,
    identity: &CatalogIdentity,
    interner: &mut SlotInterner,
) -> Result<SourceDescriptor, CatalogError> {
    let channel_name = channel.name();
    let source_id = naming::source_id(naming::adapters::NVML, &device.uuid, &channel_name)?;
    let slot = interner
        .slot_for(&source_id)
        .ok_or(CatalogError::SlotsExhausted)?;
    let entity_id = naming::entity::gpu(&device.uuid)?;
    Ok(SourceDescriptor {
        source_id: source_id.clone(),
        adapter_id: identity.adapter_id.clone(),
        slot,
        generation,
        entity_id: entity_id.clone(),
        device_id: device.uuid.clone(),
        channel: channel_name.clone(),
        metric: metric_key_of(device, channel, &entity_id, identity)?,
        // The UUID pins the physical device and NVML is its only reader.
        origin: OriginIdentity::confirmed(OriginId::new(source_id.as_str())?),
        driver: DRIVER_NAME.to_owned(),
        api_version: driver_version.map(|version| sanitize_display(version, 64)),
        device_path: device_path(device),
        native_unit: NativeUnit::new(channel.native_unit())
            .map_err(|_| CatalogError::NativeUnit(channel.native_unit()))?,
        raw_kind: channel.raw_kind(),
        sample_kind: SampleKind::Measured,
        read_group: ReadGroup::new(channel.group().name())?,
        update_interval_ms: None,
        profile_id: identity.profile_id.clone(),
        profile_revision: PROFILE_REVISION,
        profile_rank: 0,
        label: Some(channel_name),
        counter_modulus: None,
    })
}

/// Process channels carry per-PID keys in each observation
/// (`gpupid:<UUID>:<PID>`), so their catalog descriptor has no fixed key.
fn metric_key_of(
    device: &DeviceMetadata,
    channel: Channel,
    entity_id: &EntityId,
    identity: &CatalogIdentity,
) -> Result<Option<MetricKey>, IdentityError> {
    if matches!(
        channel,
        Channel::ProcessMemory | Channel::ProcessUtilization
    ) {
        return Ok(None);
    }
    Ok(Some(MetricKey::new(
        identity.host_id.clone(),
        identity.boot_id.clone(),
        entity_id.clone(),
        MetricId::new(channel.metric_id())?,
        channel.scope(&device.uuid)?,
    )))
}

pub fn device_path(device: &DeviceMetadata) -> String {
    let location = device
        .details
        .pci_bus_id
        .as_deref()
        .unwrap_or(device.uuid.as_str());
    format!("nvml://{}", sanitize_display(location, 200))
}

/// Entity record of one GPU (bounded, sanitized metadata).
pub fn entity_record(
    device: &DeviceMetadata,
    software: &HostSoftware,
    generation: u32,
) -> Result<EntityRecord, IdentityError> {
    let display_name = device
        .details
        .name
        .as_deref()
        .map(|name| sanitize_display(name, 256))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| device.uuid.clone());
    Ok(EntityRecord {
        entity_id: naming::entity::gpu(&device.uuid)?,
        kind: EntityKind::Gpu,
        parent_id: None,
        generation,
        discovered_boot_ns: device.discovered_boot_ns,
        removed_boot_ns: None,
        display_name,
        attributes: entity_attributes(device, software),
    })
}

/// The GPU's identity, firmware and modes, where it sits (D-54), then the
/// driver and CUDA stack of the host (D-48).
fn entity_attributes(device: &DeviceMetadata, software: &HostSoftware) -> Vec<Attribute> {
    let details = &device.details;
    let topology = &device.topology;
    let persistence = device
        .persistence_mode
        .map(|enabled| if enabled { "enabled" } else { "disabled" });
    let mut candidates: Vec<(&str, Option<String>)> = vec![
        (attributes::UUID, Some(device.uuid.clone())),
        (attributes::PCI_BUS_ID, details.pci_bus_id.clone()),
        (attributes::DRIVER_VERSION, software.driver_version.clone()),
        (attributes::NVML_INDEX, Some(device.index.to_string())),
        (attributes::SERIAL, details.serial.clone()),
        (
            attributes::MEMORY_MODEL,
            device.memory_model.attribute().map(str::to_owned),
        ),
        (attributes::COMPUTE_MODE, device.compute_mode.clone()),
        (attributes::PERSISTENCE_MODE, persistence.map(str::to_owned)),
        (attributes::ARCHITECTURE, details.architecture.clone()),
        (attributes::BRAND, details.brand.clone()),
        (
            attributes::COMPUTE_CAPABILITY,
            details.compute_capability.clone(),
        ),
        (attributes::VBIOS_VERSION, details.vbios_version.clone()),
        (attributes::INFOROM_IMAGE, details.inforom_image.clone()),
        (attributes::GSP_FIRMWARE, details.gsp_firmware.clone()),
        (
            attributes::BOARD_PART_NUMBER,
            details.board_part_number.clone(),
        ),
        (
            attributes::MINOR_NUMBER,
            details.minor_number.map(|minor| minor.to_string()),
        ),
        (attributes::PCI_DEVICE_ID, details.pci_device_id.clone()),
        (
            attributes::PCI_SUBSYSTEM_ID,
            details.pci_subsystem_id.clone(),
        ),
        (attributes::NVML_VERSION, software.nvml_version.clone()),
        (
            attributes::CUDA_DRIVER_VERSION,
            software.cuda_driver_version.clone(),
        ),
        (attributes::NUMA_NODES, topology.numa_nodes.clone()),
        (attributes::CPU_AFFINITY, topology.cpus.clone()),
        (
            attributes::GPU_PEERS,
            (!topology.peers.is_empty()).then(|| format_peers(&topology.peers)),
        ),
        (
            attributes::C2C_LINKS,
            topology.c2c.map(|links| links.count.to_string()),
        ),
        (
            attributes::C2C_LINK_BANDWIDTH,
            topology
                .c2c
                .and_then(|links| links.link_max_mb_per_s)
                .map(|mb_per_s| (u64::from(mb_per_s) * BYTES_PER_MB).to_string()),
        ),
    ];
    candidates.extend(stack_attributes(&software.stack));
    candidates
        .into_iter()
        .filter_map(|(key, value)| {
            let value = sanitize_display(&value?, 256);
            (!value.is_empty()).then(|| Attribute {
                key: key.to_owned(),
                value,
            })
        })
        .take(MAX_ATTRIBUTES)
        .collect()
}

/// The kernel module and the default CUDA toolkit with its nvcc; every
/// toolkit when there are several.
fn stack_attributes(stack: &NvidiaStack) -> Vec<(&'static str, Option<String>)> {
    let default = stack.toolkits.first();
    vec![
        (attributes::KERNEL_MODULE, stack.kernel_module.clone()),
        (
            attributes::CUDA_TOOLKIT,
            default.map(|toolkit| toolkit.version.clone()),
        ),
        (
            attributes::CUDA_TOOLKIT_PATH,
            default.map(|toolkit| toolkit.path.clone()),
        ),
        (
            attributes::NVCC_VERSION,
            default.and_then(|toolkit| toolkit.nvcc.clone()),
        ),
        (
            attributes::CUDA_TOOLKITS,
            (stack.toolkits.len() > 1).then(|| {
                stack
                    .toolkits
                    .iter()
                    .map(|toolkit| toolkit.version.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
        ),
    ]
}

/// Per-GPU, per-metric capabilities from what reads have proven so far.
pub fn device_capabilities(
    inventory: &Inventory,
    tracker: &CapabilityTracker,
) -> Result<Vec<Capability>, IdentityError> {
    let mut capabilities = Vec::new();
    for device in inventory.devices() {
        let entity_id = naming::entity::gpu(&device.uuid)?;
        let channels = Channel::device_channels(device.catalogued_fans());
        for metric_id in all_metric_ids() {
            let (support, reason) = metric_support(device, &channels, metric_id, tracker);
            capabilities.push(Capability {
                capability_id: metric_id.to_owned(),
                entity_id: Some(entity_id.clone()),
                support,
                reason,
                min_interval_ms: Some(MIN_INTERVAL_MS),
            });
        }
    }
    Ok(capabilities)
}

fn metric_support(
    device: &DeviceMetadata,
    channels: &[Channel],
    metric_id: &str,
    tracker: &CapabilityTracker,
) -> (Support, Option<String>) {
    let metric_channels: Vec<Channel> = channels
        .iter()
        .copied()
        .filter(|channel| channel.metric_id() == metric_id)
        .collect();
    if metric_channels.is_empty() {
        // Only fans can be absent: no fan means unsupported, never 0 %.
        let reason = match &device.fan_count {
            Ok(_) => "NVML_NO_FAN".to_owned(),
            Err(error) => error.reason_code(),
        };
        return (Support::Unsupported, Some(reason));
    }
    if metric_channels[0].is_framebuffer() && device.memory_model == MemoryModel::Unified {
        return (
            Support::Unsupported,
            Some("NVML_UNIFIED_MEMORY_NO_FRAMEBUFFER".to_owned()),
        );
    }
    let states: Vec<(Support, Option<String>)> = metric_channels
        .iter()
        .map(|channel| tracker.support_of(&device.uuid, &channel.query_key()))
        .collect();
    if states
        .iter()
        .any(|(support, _)| *support == Support::Supported)
    {
        return (Support::Supported, None);
    }
    if states
        .iter()
        .all(|(support, _)| *support == Support::Unsupported)
    {
        return (Support::Unsupported, states[0].1.clone());
    }
    (Support::Unknown, None)
}

/// Every NVML capability marked unsupported with one reason (NVML unavailable).
pub fn unavailable_capabilities(reason: &str) -> Vec<Capability> {
    all_metric_ids()
        .into_iter()
        .map(|metric_id| Capability {
            capability_id: metric_id.to_owned(),
            entity_id: None,
            support: Support::Unsupported,
            reason: Some(reason.to_owned()),
            min_interval_ms: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvml::backend::{BackendError, ClockDomain, DeviceDetails};
    use crate::nvml::inventory::FramebufferSize;
    use crate::nvml::topology::DeviceTopology;
    use dgxtop_core::gpu_state::{GpuLink, GpuPeer, PeerAccess};
    use std::collections::BTreeMap;

    fn device(memory_model: MemoryModel, fans: Result<u32, BackendError>) -> DeviceMetadata {
        DeviceMetadata {
            uuid: "GPU-abc".to_owned(),
            index: 3,
            details: DeviceDetails {
                name: Some("Test\u{1b}[31m GPU".to_owned()),
                pci_bus_id: Some("00000000:01:00.0".to_owned()),
                serial: None,
                ..DeviceDetails::default()
            },
            memory_model,
            framebuffer: Ok(FramebufferSize {
                total: 8,
                reserved: 1,
            }),
            power_limit_mw: Ok(300_000),
            max_clocks: ClockDomain::ALL
                .into_iter()
                .map(|domain| (domain, Ok(2000)))
                .collect(),
            thresholds: BTreeMap::new(),
            max_pcie_link: BTreeMap::new(),
            facts: BTreeMap::new(),
            fan_count: fans,
            compute_mode: Some("default".to_owned()),
            persistence_mode: Some(false),
            topology: DeviceTopology {
                cpus: Some("0-31".to_owned()),
                numa_nodes: Some("0".to_owned()),
                peers: vec![GpuPeer {
                    index: 0,
                    link: Some(GpuLink::HostBridge),
                    access: Some(PeerAccess {
                        read: false,
                        write: false,
                    }),
                }],
                c2c: None,
            },
            discovered_boot_ns: 5,
        }
    }

    #[test]
    fn entity_uses_uuid_and_sanitized_attributes() {
        let software = HostSoftware {
            driver_version: Some("595.1".to_owned()),
            ..HostSoftware::default()
        };
        let record = entity_record(&device(MemoryModel::Framebuffer, Ok(1)), &software, 2).unwrap();
        assert_eq!(record.entity_id.as_str(), "gpu:GPU-abc");
        assert_eq!(record.display_name, "Test GPU");
        assert_eq!(record.attribute(attributes::NVML_INDEX), Some("3"));
        assert_eq!(record.attribute(attributes::SERIAL), None);
        assert_eq!(
            record.attribute(attributes::MEMORY_MODEL),
            Some("framebuffer")
        );
        assert_eq!(
            record.attribute(attributes::PERSISTENCE_MODE),
            Some("disabled")
        );
        assert_eq!(record.attribute(attributes::CPU_AFFINITY), Some("0-31"));
        assert_eq!(record.attribute(attributes::NUMA_NODES), Some("0"));
        assert_eq!(record.attribute(attributes::GPU_PEERS), Some("0:PHB:none"));
        record.validate().unwrap();
        // A lone GPU has no peers, and unknown affinities no attribute.
        let mut alone = device(MemoryModel::Framebuffer, Ok(1));
        alone.topology = DeviceTopology::default();
        let record = entity_record(&alone, &software, 2).unwrap();
        for key in [
            attributes::GPU_PEERS,
            attributes::CPU_AFFINITY,
            attributes::NUMA_NODES,
        ] {
            assert_eq!(record.attribute(key), None, "{key}");
        }
    }

    #[test]
    fn missing_fans_and_unified_framebuffer_are_unsupported() {
        let tracker = CapabilityTracker::new(60_000);
        let unified = device(MemoryModel::Unified, Ok(0));
        let channels = Channel::device_channels(unified.catalogued_fans());
        let (fan, fan_reason) = metric_support(
            &unified,
            &channels,
            dgxtop_core::metric::ids::GPU_FAN_SPEED,
            &tracker,
        );
        assert_eq!(fan, Support::Unsupported);
        assert_eq!(fan_reason.as_deref(), Some("NVML_NO_FAN"));
        let (framebuffer, _) = metric_support(
            &unified,
            &channels,
            dgxtop_core::metric::ids::GPU_MEMORY_FB_USED,
            &tracker,
        );
        assert_eq!(framebuffer, Support::Unsupported);
        let (temperature, _) = metric_support(
            &unified,
            &channels,
            dgxtop_core::metric::ids::GPU_TEMPERATURE,
            &tracker,
        );
        assert_eq!(temperature, Support::Unknown);
    }

    #[test]
    fn unavailable_capabilities_cover_every_metric() {
        let capabilities = unavailable_capabilities("NVML_NO_GPU");
        assert_eq!(capabilities.len(), all_metric_ids().len());
        assert!(
            capabilities
                .iter()
                .all(|capability| capability.support == Support::Unsupported)
        );
    }
}
