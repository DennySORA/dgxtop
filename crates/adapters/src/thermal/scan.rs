//! Thermal-zone enumeration: `/sys/class/thermal/thermal_zoneN` with `type`,
//! `temp` (mdegC), optional `mode` (a `disabled` zone is unsupported),
//! optional `device` link and trip points (bounded entity attributes, never
//! series). Zone identity is the type plus the device path when present; the
//! zone index is only a last-resort disambiguator.
//!
//! Static metadata and the physical package count are cached: zone metadata
//! per enumeration instance, the package count per set of `cpuN` entries.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use dgxtop_core::identity::ProfileId;
use dgxtop_core::metric::{NativeUnit, native};
use dgxtop_core::naming;
use dgxtop_core::observation::{Attribute, EntityKind};
use dgxtop_core::quality::RawKind;

use super::profile::{ThermalHostFacts, X86_PKG_TEMP_TYPE, classify, is_arm_cpu_zone};
use crate::common::{SystemFiles, reason_of};
use crate::hwmon::engine::{ScanContext, Scanner, codes};
use crate::hwmon::mapping::resolve;
use crate::hwmon::reader::{ReadSpec, ValueFormat};
use crate::hwmon::sysfs::{
    attribute, canonical, canonical_link, device_key, disambiguate, is_missing, list_names,
    numbered, read_label, read_optional,
};
use crate::hwmon::table::{EntityDraft, ScanOutput, ScannedDevice, SourceDraft};

const CLASS: &str = "/sys/class/thermal";
const CPU_ROOT: &str = "/sys/devices/system/cpu";
const ADAPTER: &str = naming::adapters::THERMAL;
/// Channel name of the zone temperature.
const CHANNEL: &str = "temp";
/// Trip points kept as attributes (16 attributes minus the fixed ones).
const MAX_TRIP_POINTS: u32 = 12;

/// Cached static metadata of one zone instance.
#[derive(Debug, Clone, Default)]
struct ZoneMetadata {
    zone_type: String,
    device_path: Option<String>,
    policy: Option<String>,
    trips: Vec<Attribute>,
    has_mode: bool,
    has_temp: bool,
    device_key: Option<String>,
}

#[derive(Debug)]
struct FoundZone {
    index: u32,
    instance: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TopologyCache {
    cpus: Vec<String>,
    packages: Option<usize>,
}

enum ZoneScan {
    Found(FoundZone),
    Vanished,
    Failed,
}

/// Scanner of the thermal class with its metadata caches.
#[derive(Debug, Default)]
pub(crate) struct ThermalScanner {
    cache: BTreeMap<String, ZoneMetadata>,
    topology: Option<TopologyCache>,
}

impl Scanner for ThermalScanner {
    fn scan(&mut self, context: &ScanContext<'_>) -> ScanOutput {
        self.invalidate(context.invalidated);
        let names = match list_names(context.files, CLASS) {
            Ok(names) => names,
            Err(error) if is_missing(&error) => return ScanOutput::missing(),
            Err(error) => {
                return ScanOutput::failed(format!("THERMAL_CLASS_{}", reason_of(&error)));
            }
        };
        let mut found = Vec::new();
        let mut partial_failures = 0u32;
        for name in &names {
            let Some(index) = numbered(name, "thermal_zone") else {
                continue;
            };
            if context.expired() {
                return ScanOutput::failed(codes::DEADLINE_EXPIRED);
            }
            match self.scan_zone(context.files, index, name) {
                ZoneScan::Found(zone) => found.push(zone),
                ZoneScan::Vanished => {}
                ZoneScan::Failed => partial_failures = partial_failures.saturating_add(1),
            }
        }
        let live: BTreeSet<&str> = found.iter().map(|zone| zone.instance.as_str()).collect();
        self.cache
            .retain(|instance, _| live.contains(instance.as_str()));
        let packages = self.physical_packages(context.files);
        let mut output = self.build_output(context, &found, packages);
        output.partial_failures = output.partial_failures.saturating_add(partial_failures);
        output
    }
}

impl ThermalScanner {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn invalidate(&mut self, device_keys: &BTreeSet<String>) {
        if device_keys.is_empty() {
            return;
        }
        self.cache.retain(|_, metadata| {
            metadata
                .device_key
                .as_ref()
                .is_none_or(|key| !device_keys.contains(key))
        });
    }

    fn scan_zone(&mut self, files: &dyn SystemFiles, index: u32, name: &str) -> ZoneScan {
        let instance = match canonical(files, &format!("{CLASS}/{name}")) {
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
        ZoneScan::Found(FoundZone { index, instance })
    }

    /// Distinct physical packages, re-read only when the set of CPUs changes.
    fn physical_packages(&mut self, files: &dyn SystemFiles) -> Option<usize> {
        let cpus: Vec<String> = list_names(files, CPU_ROOT)
            .ok()?
            .into_iter()
            .filter(|name| numbered(name, "cpu").is_some())
            .collect();
        if let Some(cache) = &self.topology
            && cache.cpus == cpus
        {
            return cache.packages;
        }
        let packages = count_packages(files, &cpus);
        self.topology = Some(TopologyCache { cpus, packages });
        packages
    }

    fn build_output(
        &mut self,
        context: &ScanContext<'_>,
        found: &[FoundZone],
        packages: Option<usize>,
    ) -> ScanOutput {
        let mut keys: Vec<(String, u32)> = found
            .iter()
            .map(|zone| {
                let metadata = self.cache.get(&zone.instance);
                let zone_type = metadata.map_or("unknown", |metadata| metadata.zone_type.as_str());
                let path = metadata.and_then(|metadata| metadata.device_path.as_deref());
                (device_key(zone_type, path), zone.index)
            })
            .collect();
        disambiguate(&mut keys);
        let host = self.host_facts(found, packages);
        let mut output = ScanOutput::complete();
        for (zone, (key, _)) in found.iter().zip(keys) {
            let Some(metadata) = self.cache.get_mut(&zone.instance) else {
                continue;
            };
            metadata.device_key = Some(key.clone());
            if !metadata.has_temp {
                continue;
            }
            let metadata = metadata.clone();
            output.devices.push(ScannedDevice {
                device_key: key.clone(),
                instance: zone.instance.clone(),
            });
            match zone_drafts(context, &host, zone, &metadata, &key) {
                Some((source, entities)) => {
                    output.sources.push(source);
                    output.entities.extend(entities);
                }
                None => output.partial_failures = output.partial_failures.saturating_add(1),
            }
        }
        output
    }

    fn host_facts(&self, found: &[FoundZone], packages: Option<usize>) -> ThermalHostFacts {
        let types: Vec<&str> = found
            .iter()
            .filter_map(|zone| self.cache.get(&zone.instance))
            .map(|metadata| metadata.zone_type.as_str())
            .collect();
        ThermalHostFacts {
            physical_packages: packages,
            x86_pkg_zones: types
                .iter()
                .filter(|zone_type| **zone_type == X86_PKG_TEMP_TYPE)
                .count(),
            arm_cpu_zones: types
                .iter()
                .filter(|zone_type| is_arm_cpu_zone(zone_type))
                .count(),
        }
    }
}

/// Source and entity drafts of one zone.
fn zone_drafts(
    context: &ScanContext<'_>,
    host: &ThermalHostFacts,
    zone: &FoundZone,
    metadata: &ZoneMetadata,
    key: &str,
) -> Option<(SourceDraft, Vec<EntityDraft>)> {
    let mapping = classify(host, &metadata.zone_type);
    let resolved = resolve(context, &mapping, ADAPTER, key, CHANNEL)?;
    let zone_entity_id = naming::entity::sensor(ADAPTER, key, CHANNEL).ok()?;
    let mut entities = vec![EntityDraft {
        entity_id: zone_entity_id,
        kind: EntityKind::Sensor,
        // A mapped zone's metadata entity hangs below the semantic entity.
        parent_id: resolved.proven.then(|| resolved.entity_id.clone()),
        display_name: metadata.zone_type.clone(),
        attributes: zone_attributes(zone, metadata),
        device_key: Some(key.to_owned()),
        owned: true,
    }];
    entities.extend(resolved.shared_entity.clone());
    let instance = &zone.instance;
    let source = SourceDraft {
        source_id: naming::source_id(ADAPTER, key, CHANNEL).ok()?,
        device_key: key.to_owned(),
        entity_id: resolved.entity_id,
        record_key: resolved.record_key,
        proven: resolved.proven,
        origin: resolved.origin,
        driver: metadata.zone_type.clone(),
        device_path: instance.clone(),
        channel: CHANNEL.to_owned(),
        native_unit: NativeUnit::new(native::MILLI_DEG_C).ok()?,
        raw_kind: RawKind::Gauge,
        profile_id: ProfileId::new(mapping.profile.id).ok()?,
        profile_revision: mapping.profile.revision,
        profile_rank: mapping.rank,
        quality: mapping.quality,
        label: Some(metadata.zone_type.clone()),
        counter_modulus: None,
        update_interval_ms: None,
        read: ReadSpec {
            gate_path: metadata.has_mode.then(|| format!("{instance}/mode")),
            ..ReadSpec::value(format!("{instance}/temp"), ValueFormat::SignedGauge)
        },
    };
    Some((source, entities))
}

fn zone_attributes(zone: &FoundZone, metadata: &ZoneMetadata) -> Vec<Attribute> {
    let mut attributes = vec![
        attribute("type", &metadata.zone_type),
        attribute("zone", &format!("thermal_zone{}", zone.index)),
        attribute(naming::attributes::DEVICE, &zone.instance),
    ];
    if let Some(policy) = &metadata.policy {
        attributes.push(attribute("policy", policy));
    }
    attributes.extend(metadata.trips.iter().cloned());
    attributes
}

fn read_zone_metadata(files: &dyn SystemFiles, instance: &str) -> io::Result<ZoneMetadata> {
    let entries = list_names(files, instance)?;
    let has = |name: &str| entries.iter().any(|entry| entry == name);
    let zone_type =
        read_label(files, &format!("{instance}/type"))?.unwrap_or_else(|| "unknown".to_owned());
    Ok(ZoneMetadata {
        device_path: canonical_link(files, &format!("{instance}/device"))?,
        policy: read_label(files, &format!("{instance}/policy"))
            .ok()
            .flatten(),
        trips: read_trips(files, instance, &entries),
        has_mode: has("mode"),
        has_temp: has("temp"),
        zone_type,
        device_key: None,
    })
}

/// Trip points as bounded attributes `trip_<K>` = `<type> <temp> mdegC`.
fn read_trips(files: &dyn SystemFiles, instance: &str, entries: &[String]) -> Vec<Attribute> {
    let mut trips = Vec::new();
    for trip in 0..MAX_TRIP_POINTS {
        let temp_name = format!("trip_point_{trip}_temp");
        if !entries.contains(&temp_name) {
            break;
        }
        let kind = read_label(files, &format!("{instance}/trip_point_{trip}_type"))
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_owned());
        let Ok(Some(temp)) = read_optional(files, &format!("{instance}/{temp_name}")) else {
            continue;
        };
        if temp.parse::<i64>().is_err() {
            continue;
        }
        trips.push(attribute(
            &format!("trip_{trip}"),
            &format!("{kind} {temp} mdegC"),
        ));
    }
    trips
}

/// Distinct non-negative `physical_package_id` values; `None` when unknown.
fn count_packages(files: &dyn SystemFiles, cpus: &[String]) -> Option<usize> {
    let mut packages = BTreeSet::new();
    for cpu in cpus {
        let path = format!("{CPU_ROOT}/{cpu}/topology/physical_package_id");
        match read_optional(files, &path) {
            Ok(Some(text)) => match text.parse::<i64>() {
                Ok(package) if package >= 0 => {
                    packages.insert(package);
                }
                _ => return None,
            },
            // Offline CPUs have no topology directory.
            Ok(None) => {}
            Err(_) => return None,
        }
    }
    (!packages.is_empty()).then_some(packages.len())
}
