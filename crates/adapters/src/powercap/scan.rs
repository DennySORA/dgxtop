//! Powercap zone enumeration: `/sys/class/powercap/<zone>` (`intel-rapl:N`,
//! `intel-rapl:N:M`, ...) with `name`, `energy_uj` (u64 counter, often
//! root-only) and `max_energy_range_uj` (declared counter modulus). Zone
//! directory names are stable identities; `name` and the modulus are static
//! metadata cached per enumeration instance.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use dgxtop_core::identity::{EntityId, ProfileId};
use dgxtop_core::metric::{NativeUnit, native};
use dgxtop_core::naming::{self, entity};
use dgxtop_core::observation::EntityKind;
use dgxtop_core::quality::RawKind;

use crate::hwmon::mapping::Mapping;

use super::profile::{classify, control_type, parent_zone};
use crate::common::{SystemFiles, parse_u64, reason_of};
use crate::hwmon::engine::{ScanContext, Scanner, codes};
use crate::hwmon::mapping::resolve;
use crate::hwmon::reader::{ReadSpec, ValueFormat};
use crate::hwmon::sysfs::{
    attribute, canonical, device_key, is_missing, list_names, read_label, read_optional,
    static_group,
};
use crate::hwmon::table::{EntityDraft, ScanOutput, ScannedDevice, SourceDraft};

const CLASS: &str = "/sys/class/powercap";
const ADAPTER: &str = naming::adapters::POWERCAP;
const CHANNEL: &str = "energy_uj";

/// Cached static metadata of one zone instance.
#[derive(Debug, Clone)]
struct ZoneMetadata {
    name: String,
    max_energy_range_uj: Option<u64>,
    has_energy: bool,
}

#[derive(Debug)]
struct FoundZone {
    directory: String,
    instance: String,
}

enum ZoneScan {
    Found(FoundZone),
    Vanished,
    Failed,
}

/// Scanner of the powercap class with its metadata cache.
#[derive(Debug, Default)]
pub(crate) struct PowercapScanner {
    cache: BTreeMap<String, ZoneMetadata>,
}

impl Scanner for PowercapScanner {
    fn scan(&mut self, context: &ScanContext<'_>) -> ScanOutput {
        self.invalidate(context.invalidated);
        let names = match list_names(context.files, CLASS) {
            Ok(names) => names,
            Err(error) if is_missing(&error) => return ScanOutput::missing(),
            Err(error) => {
                return ScanOutput::failed(format!("POWERCAP_CLASS_{}", reason_of(&error)));
            }
        };
        let mut found = Vec::new();
        let mut partial_failures = 0u32;
        // Control-type directories (`intel-rapl`) hold no counter; zones contain ':'.
        for directory in names.iter().filter(|name| name.contains(':')) {
            if context.expired() {
                return ScanOutput::failed(codes::DEADLINE_EXPIRED);
            }
            match self.scan_zone(context.files, directory) {
                ZoneScan::Found(zone) => found.push(zone),
                ZoneScan::Vanished => {}
                ZoneScan::Failed => partial_failures = partial_failures.saturating_add(1),
            }
        }
        let live: BTreeSet<&str> = found.iter().map(|zone| zone.instance.as_str()).collect();
        self.cache
            .retain(|instance, _| live.contains(instance.as_str()));
        let mut output = ScanOutput::complete();
        output.partial_failures = partial_failures;
        for zone in &found {
            let Some(metadata) = self.cache.get(&zone.instance) else {
                continue;
            };
            if !metadata.has_energy {
                continue;
            }
            // Subdomains, MMIO duplicates and unknown names are not
            // catalogued (D-92).
            let Some(mapping) = classify(&zone.directory, &metadata.name) else {
                continue;
            };
            let key = device_key(&zone.directory, None);
            output.devices.push(ScannedDevice {
                device_key: key.clone(),
                instance: zone.instance.clone(),
            });
            match zone_drafts(context, &mapping, zone, metadata, &key) {
                Some((source, entities)) => {
                    output.sources.push(source);
                    output.entities.extend(entities);
                }
                None => output.partial_failures = output.partial_failures.saturating_add(1),
            }
        }
        output
    }
}

impl PowercapScanner {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Zone directory names are the device keys; drop their cached metadata.
    fn invalidate(&mut self, device_keys: &BTreeSet<String>) {
        if device_keys.is_empty() {
            return;
        }
        self.cache.retain(|instance, _| {
            let directory = instance.rsplit('/').next().unwrap_or(instance);
            !device_keys.contains(&device_key(directory, None))
        });
    }

    fn scan_zone(&mut self, files: &dyn SystemFiles, directory: &str) -> ZoneScan {
        let instance = match canonical(files, &format!("{CLASS}/{directory}")) {
            Ok(instance) => instance,
            Err(error) if is_missing(&error) => return ZoneScan::Vanished,
            Err(_) => return ZoneScan::Failed,
        };
        if !self.cache.contains_key(&instance) {
            match read_zone_metadata(files, &instance) {
                Ok(metadata) => {
                    self.cache.insert(instance.clone(), metadata);
                }
                Err(error) if is_missing(&error) => return ZoneScan::Vanished,
                Err(_) => return ZoneScan::Failed,
            }
        }
        ZoneScan::Found(FoundZone {
            directory: directory.to_owned(),
            instance,
        })
    }
}

fn read_zone_metadata(files: &dyn SystemFiles, instance: &str) -> io::Result<ZoneMetadata> {
    let entries = list_names(files, instance)?;
    let name =
        read_label(files, &format!("{instance}/name"))?.unwrap_or_else(|| "unnamed".to_owned());
    let max_energy_range_uj = read_optional(files, &format!("{instance}/max_energy_range_uj"))
        .ok()
        .flatten()
        .and_then(|text| parse_u64(&text))
        .filter(|range| *range > 0);
    Ok(ZoneMetadata {
        name,
        max_energy_range_uj,
        has_energy: entries.iter().any(|entry| entry == CHANNEL),
    })
}

/// Source and entity drafts of one zone.
fn zone_drafts(
    context: &ScanContext<'_>,
    mapping: &Mapping,
    zone: &FoundZone,
    metadata: &ZoneMetadata,
    key: &str,
) -> Option<(SourceDraft, Vec<EntityDraft>)> {
    let resolved = resolve(context, mapping)?;
    let domain_id = entity::power_domain(key).ok()?;
    let mut entities = vec![domain_entity(zone, metadata, key, &domain_id)];
    entities.extend(resolved.shared_entity.clone());
    let source = SourceDraft {
        source_id: naming::source_id(ADAPTER, key, CHANNEL).ok()?,
        device_key: key.to_owned(),
        entity_id: resolved.entity_id,
        record_key: resolved.record_key,
        origin: resolved.origin,
        driver: control_type(&zone.directory).to_owned(),
        device_path: zone.instance.clone(),
        channel: CHANNEL.to_owned(),
        native_unit: NativeUnit::new(native::MICRO_JOULE).ok()?,
        raw_kind: RawKind::Counter,
        profile_id: ProfileId::new(mapping.profile.id).ok()?,
        profile_revision: mapping.profile.revision,
        profile_rank: mapping.rank,
        quality: mapping.quality,
        label: Some(metadata.name.clone()),
        counter_modulus: metadata.max_energy_range_uj,
        update_interval_ms: None,
        read: ReadSpec::value(
            format!("{}/{CHANNEL}", zone.instance),
            ValueFormat::UnsignedCounter,
        ),
        read_group: static_group(naming::groups::FAST),
    };
    Some((source, entities))
}

/// Power-domain metadata entity (`power:<zone>`), parented to its zone.
fn domain_entity(
    zone: &FoundZone,
    metadata: &ZoneMetadata,
    key: &str,
    domain_id: &EntityId,
) -> EntityDraft {
    let mut attributes = vec![
        attribute("name", &metadata.name),
        attribute("control_type", control_type(&zone.directory)),
        attribute(naming::attributes::DEVICE, &zone.instance),
    ];
    if let Some(range) = metadata.max_energy_range_uj {
        attributes.push(attribute("max_energy_range_uj", &range.to_string()));
    }
    EntityDraft {
        entity_id: domain_id.clone(),
        kind: EntityKind::PowerDomain,
        parent_id: parent_zone(&zone.directory)
            .and_then(|parent| entity::power_domain(&device_key(parent, None)).ok()),
        display_name: metadata.name.clone(),
        attributes,
        device_key: Some(key.to_owned()),
        owned: true,
    }
}
