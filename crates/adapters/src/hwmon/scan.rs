//! hwmon enumeration: `/sys/class/hwmon/hwmonN` devices, their value
//! channels (`temp*_input`, `power*_input`, `power*_average`, `fan*_input`,
//! `energy*_input`) with fault/alarm companions, and profile classification.
//!
//! Static metadata (`name`, `device` link, `update_interval`, `*_label`) is
//! read once per enumeration instance (canonical hwmon path) and cached until
//! that instance changes or the device is suspected to be re-created, so
//! repeated discoveries do not re-read unchanged metadata (AT34). Values are
//! read through the canonical path: a re-numbered `hwmonN` can never make a
//! source read another device's file.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io;

use dgxtop_core::identity::{EntityId, ProfileId};
use dgxtop_core::metric::{NativeUnit, native};
use dgxtop_core::naming;
use dgxtop_core::observation::EntityKind;
use dgxtop_core::quality::RawKind;

use super::engine::{ScanContext, Scanner, codes};
use super::mapping::{Mapping, resolve};
use super::profile::{
    ChannelFacts, ChannelKind, DeviceFacts, HostFacts, classify, is_amd_temperature_driver,
    label_is, package_label,
};
use super::reader::{AlarmPath, ReadSpec, ValueFormat, reasons};
use super::sysfs::{
    attribute, canonical, canonical_link, device_key, disambiguate, is_missing, list_names,
    numbered, read_label, read_optional,
};
use super::table::{EntityDraft, ScanOutput, ScannedDevice, SourceDraft};
use crate::common::{SystemFiles, reason_of};

const CLASS: &str = "/sys/class/hwmon";
const ADAPTER: &str = naming::adapters::HWMON;
/// Longest accepted driver update interval (schema bound of the observation).
const MAX_UPDATE_INTERVAL_MS: u32 = 3_600_000;

/// One value channel and its companion attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChannelFiles {
    kind: ChannelKind,
    index: u32,
    has_label: bool,
    has_fault: bool,
    alarms: Vec<(String, &'static str)>,
}

impl ChannelFiles {
    /// `temp1`, `power2`: the label/fault/alarm prefix and diagnostics channel id.
    fn base(&self) -> String {
        format!("{}{}", self.kind.prefix(), self.index)
    }

    /// Source id channel: `temp1`, `power1`, `power1_average`, ...
    fn source_channel(&self) -> String {
        match self.kind {
            ChannelKind::PowerAverage => format!("{}_average", self.base()),
            _ => self.base(),
        }
    }

    /// Value attribute (`temp1_input`, `power1_average`).
    fn attribute(&self) -> String {
        format!("{}_{}", self.base(), self.kind.suffix())
    }
}

/// Cached static metadata of one enumeration instance.
#[derive(Debug, Clone, Default)]
struct DeviceMetadata {
    name: String,
    device_path: Option<String>,
    update_interval_ms: Option<u32>,
    /// `temp1` -> label (`None` = no label file).
    labels: BTreeMap<String, Option<String>>,
    /// Final device key of the last scan (for re-creation invalidation).
    device_key: Option<String>,
}

#[derive(Debug)]
struct FoundDevice {
    index: u32,
    instance: String,
    channels: Vec<ChannelFiles>,
}

enum DeviceScan {
    Found(FoundDevice),
    /// The kernel's hwmon mirror of a thermal zone: kept in the cache so its
    /// metadata is not re-read, never catalogued.
    ThermalZoneMirror {
        instance: String,
    },
    Vanished,
    Failed,
}

/// Whether a device is the kernel's hwmon view of a thermal zone: its
/// `device` is a `thermal_zoneN`. The thermal adapter reads every zone and
/// alone knows a zone's ACPI path (D-68), so hwmon leaves the mirror out
/// instead of reading the same sensors a second time (D-81).
fn is_thermal_zone_mirror(metadata: &DeviceMetadata) -> bool {
    metadata
        .device_path
        .as_deref()
        .and_then(|path| path.rsplit('/').next())
        .is_some_and(|leaf| numbered(leaf, "thermal_zone").is_some())
}

/// Scanner of the hwmon class with its metadata cache.
#[derive(Debug, Default)]
pub(crate) struct HwmonScanner {
    cache: BTreeMap<String, DeviceMetadata>,
}

impl Scanner for HwmonScanner {
    fn scan(&mut self, context: &ScanContext<'_>) -> ScanOutput {
        self.invalidate(context.invalidated);
        let names = match list_names(context.files, CLASS) {
            Ok(names) => names,
            Err(error) if is_missing(&error) => return ScanOutput::missing(),
            Err(error) => return ScanOutput::failed(format!("HWMON_CLASS_{}", reason_of(&error))),
        };
        let mut found = Vec::new();
        let mut mirrors = Vec::new();
        let mut partial_failures = 0u32;
        for name in &names {
            let Some(index) = numbered(name, "hwmon") else {
                continue;
            };
            if context.expired() {
                return ScanOutput::failed(codes::DEADLINE_EXPIRED);
            }
            match self.scan_device(context.files, index, name) {
                DeviceScan::Found(device) => found.push(device),
                DeviceScan::ThermalZoneMirror { instance } => mirrors.push(instance),
                DeviceScan::Vanished => {}
                DeviceScan::Failed => partial_failures = partial_failures.saturating_add(1),
            }
        }
        let live: BTreeSet<&str> = found
            .iter()
            .map(|device| device.instance.as_str())
            .chain(mirrors.iter().map(String::as_str))
            .collect();
        self.cache
            .retain(|instance, _| live.contains(instance.as_str()));
        let mut output = self.build_output(context, &found);
        output.partial_failures = partial_failures;
        output
    }
}

impl HwmonScanner {
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

    fn scan_device(&mut self, files: &dyn SystemFiles, index: u32, name: &str) -> DeviceScan {
        let class_path = format!("{CLASS}/{name}");
        let instance = match canonical(files, &class_path) {
            Ok(instance) => instance,
            Err(error) if is_missing(&error) => return DeviceScan::Vanished,
            Err(_) => return DeviceScan::Failed,
        };
        match self.cached_metadata(files, &instance) {
            Ok(metadata) if is_thermal_zone_mirror(metadata) => {
                return DeviceScan::ThermalZoneMirror { instance };
            }
            Ok(_) => {}
            Err(error) if is_missing(&error) => return DeviceScan::Vanished,
            Err(_) => return DeviceScan::Failed,
        }
        let attributes = match list_names(files, &instance) {
            Ok(attributes) => attributes,
            Err(error) if is_missing(&error) => return DeviceScan::Vanished,
            Err(_) => return DeviceScan::Failed,
        };
        let channels = parse_channels(&attributes);
        self.refresh_labels(files, &instance, &channels);
        DeviceScan::Found(FoundDevice {
            index,
            instance,
            channels,
        })
    }

    /// Static metadata of an instance, read only when not cached yet.
    fn cached_metadata(
        &mut self,
        files: &dyn SystemFiles,
        instance: &str,
    ) -> io::Result<&DeviceMetadata> {
        Ok(match self.cache.entry(instance.to_owned()) {
            Entry::Occupied(cached) => cached.into_mut(),
            Entry::Vacant(slot) => slot.insert(read_device_metadata(files, instance)?),
        })
    }

    /// Reads the labels not cached yet for this instance's channels.
    fn refresh_labels(
        &mut self,
        files: &dyn SystemFiles,
        instance: &str,
        channels: &[ChannelFiles],
    ) {
        let Some(metadata) = self.cache.get_mut(instance) else {
            return;
        };
        for channel in channels.iter().filter(|channel| channel.has_label) {
            let base = channel.base();
            if metadata.labels.contains_key(&base) {
                continue;
            }
            // A transient label failure is not cached and is retried next scan.
            if let Ok(label) = read_label(files, &format!("{instance}/{base}_label")) {
                metadata.labels.insert(base, label);
            }
        }
    }

    fn build_output(&mut self, context: &ScanContext<'_>, found: &[FoundDevice]) -> ScanOutput {
        let mut keys: Vec<(String, u32)> = found
            .iter()
            .map(|device| {
                let metadata = self.cache.get(&device.instance);
                let name = metadata.map_or("unnamed", |metadata| metadata.name.as_str());
                let path = metadata.and_then(|metadata| metadata.device_path.as_deref());
                (device_key(name, path), device.index)
            })
            .collect();
        disambiguate(&mut keys);
        let host = host_facts(found, &self.cache);
        let mut output = ScanOutput::complete();
        for (device, (key, _)) in found.iter().zip(keys) {
            let Some(metadata) = self.cache.get_mut(&device.instance) else {
                continue;
            };
            metadata.device_key = Some(key.clone());
            let metadata = metadata.clone();
            output.devices.push(ScannedDevice {
                device_key: key.clone(),
                instance: device.instance.clone(),
            });
            let builder = DeviceDrafts {
                context,
                device,
                metadata: &metadata,
                device_key: &key,
                host: &host,
            };
            builder.append_to(&mut output);
        }
        output
    }
}

fn read_device_metadata(files: &dyn SystemFiles, instance: &str) -> io::Result<DeviceMetadata> {
    let name =
        read_label(files, &format!("{instance}/name"))?.unwrap_or_else(|| "unnamed".to_owned());
    let device_path = canonical_link(files, &format!("{instance}/device"))?;
    let update_interval_ms = read_optional(files, &format!("{instance}/update_interval"))
        .ok()
        .flatten()
        .and_then(|text| text.parse::<u32>().ok())
        .filter(|interval| (1..=MAX_UPDATE_INTERVAL_MS).contains(interval));
    Ok(DeviceMetadata {
        name,
        device_path,
        update_interval_ms,
        labels: BTreeMap::new(),
        device_key: None,
    })
}

/// Host-level facts gating multi-device profiles.
fn host_facts(found: &[FoundDevice], cache: &BTreeMap<String, DeviceMetadata>) -> HostFacts {
    let mut host = HostFacts::default();
    for device in found {
        let Some(metadata) = cache.get(&device.instance) else {
            continue;
        };
        if is_amd_temperature_driver(&metadata.name) {
            host.amd_devices += 1;
        }
        if has_gb10_labels(device, metadata) {
            host.gb10_devices += 1;
        }
    }
    host
}

fn label_of<'a>(metadata: &'a DeviceMetadata, channel: &ChannelFiles) -> Option<&'a str> {
    metadata
        .labels
        .get(&channel.base())
        .and_then(|label| label.as_deref())
}

fn has_gb10_labels(device: &FoundDevice, metadata: &DeviceMetadata) -> bool {
    let power_labels: Vec<Option<&str>> = device
        .channels
        .iter()
        .filter(|channel| {
            matches!(
                channel.kind,
                ChannelKind::PowerInput | ChannelKind::PowerAverage
            )
        })
        .map(|channel| label_of(metadata, channel))
        .collect();
    let has = |expected| power_labels.iter().any(|label| label_is(*label, expected));
    has("cpu_p") && has("cpu_e")
}

fn coretemp_package(device: &FoundDevice, metadata: &DeviceMetadata) -> Option<u32> {
    let packages: BTreeSet<u32> = device
        .channels
        .iter()
        .filter(|channel| channel.kind == ChannelKind::Temperature)
        .filter_map(|channel| label_of(metadata, channel).and_then(package_label))
        .collect();
    match packages.len() {
        1 => packages.into_iter().next(),
        _ => None,
    }
}

/// Builds the drafts of one device.
struct DeviceDrafts<'a> {
    context: &'a ScanContext<'a>,
    device: &'a FoundDevice,
    metadata: &'a DeviceMetadata,
    device_key: &'a str,
    host: &'a HostFacts,
}

impl DeviceDrafts<'_> {
    fn append_to(&self, output: &mut ScanOutput) {
        let facts = DeviceFacts {
            name: &self.metadata.name,
            device_key: self.device_key,
            coretemp_package: (self.metadata.name == "coretemp")
                .then(|| coretemp_package(self.device, self.metadata))
                .flatten(),
            has_tdie: self
                .device
                .channels
                .iter()
                .any(|channel| label_of(self.metadata, channel) == Some("Tdie")),
            gb10_labels: has_gb10_labels(self.device, self.metadata),
        };
        for channel in &self.device.channels {
            let label = label_of(self.metadata, channel);
            let mapping = classify(
                self.host,
                &facts,
                &ChannelFacts {
                    kind: channel.kind,
                    index: channel.index,
                    label,
                },
            );
            match self.source_draft(channel, label, &mapping) {
                Some((source, entity)) => {
                    output.sources.push(source);
                    output.entities.extend(entity);
                }
                None => output.partial_failures = output.partial_failures.saturating_add(1),
            }
        }
    }

    fn source_draft(
        &self,
        channel: &ChannelFiles,
        label: Option<&str>,
        mapping: &Mapping,
    ) -> Option<(SourceDraft, Option<EntityDraft>)> {
        let source_id =
            naming::source_id(ADAPTER, self.device_key, &channel.source_channel()).ok()?;
        let resolved = resolve(
            self.context,
            mapping,
            ADAPTER,
            self.device_key,
            &channel.base(),
        )?;
        let entity_draft = if resolved.proven {
            resolved.shared_entity.clone()
        } else {
            Some(self.sensor_entity(&resolved.entity_id, channel, label))
        };
        let (unit, raw_kind, format) = channel_units(channel.kind);
        let source = SourceDraft {
            source_id,
            device_key: self.device_key.to_owned(),
            entity_id: resolved.entity_id,
            record_key: resolved.record_key,
            proven: resolved.proven,
            origin: resolved.origin,
            driver: self.metadata.name.clone(),
            device_path: self.device.instance.clone(),
            channel: channel.attribute(),
            native_unit: NativeUnit::new(unit).ok()?,
            raw_kind,
            profile_id: ProfileId::new(mapping.profile.id).ok()?,
            profile_revision: mapping.profile.revision,
            profile_rank: mapping.rank,
            quality: mapping.quality,
            label: label.map(str::to_owned),
            counter_modulus: None,
            update_interval_ms: self.metadata.update_interval_ms,
            read: self.read_spec(channel, format),
        };
        Some((source, entity_draft))
    }

    fn read_spec(&self, channel: &ChannelFiles, format: ValueFormat) -> ReadSpec {
        let instance = &self.device.instance;
        let base = channel.base();
        ReadSpec {
            value_path: format!("{instance}/{}", channel.attribute()),
            format,
            fault_path: channel
                .has_fault
                .then(|| format!("{instance}/{base}_fault")),
            alarm_paths: channel
                .alarms
                .iter()
                .map(|(suffix, reason)| AlarmPath {
                    path: format!("{instance}/{base}_{suffix}"),
                    reason,
                })
                .collect(),
            gate_path: None,
        }
    }

    fn sensor_entity(
        &self,
        entity_id: &EntityId,
        channel: &ChannelFiles,
        label: Option<&str>,
    ) -> EntityDraft {
        let base = channel.base();
        let mut attributes = vec![
            attribute("driver", &self.metadata.name),
            attribute("channel", &base),
            attribute(naming::attributes::DEVICE, &self.device.instance),
        ];
        if let Some(label) = label {
            attributes.push(attribute("label", label));
        }
        EntityDraft {
            entity_id: entity_id.clone(),
            kind: EntityKind::Sensor,
            parent_id: None,
            display_name: match label {
                Some(label) => format!("{} {label}", self.metadata.name),
                None => format!("{} {base}", self.metadata.name),
            },
            attributes,
            device_key: Some(self.device_key.to_owned()),
            owned: true,
        }
    }
}

/// Native unit, raw kind and value format of a channel kind.
fn channel_units(kind: ChannelKind) -> (&'static str, RawKind, ValueFormat) {
    match kind {
        ChannelKind::Temperature => (
            native::MILLI_DEG_C,
            RawKind::Gauge,
            ValueFormat::SignedGauge,
        ),
        ChannelKind::PowerInput | ChannelKind::PowerAverage => {
            (native::MICRO_WATT, RawKind::Gauge, ValueFormat::SignedGauge)
        }
        // RPM is reported as a plain count (catalog unit of `sensor.fan`).
        ChannelKind::Fan => (native::COUNT, RawKind::Gauge, ValueFormat::SignedGauge),
        ChannelKind::Energy => (
            native::MICRO_JOULE,
            RawKind::Counter,
            ValueFormat::UnsignedCounter,
        ),
    }
}

/// Splits `temp1_crit_alarm` into (`temp`, 1, `crit_alarm`).
fn split_attribute(name: &str) -> Option<(&str, u32, &str)> {
    let digits_start = name.find(|character: char| character.is_ascii_digit())?;
    let (prefix, rest) = name.split_at(digits_start);
    let digits_end = rest.find(|character: char| !character.is_ascii_digit())?;
    let (digits, suffix) = rest.split_at(digits_end);
    Some((prefix, digits.parse().ok()?, suffix.strip_prefix('_')?))
}

/// Specific reason of an alarm attribute suffix (`None` = not an alarm).
fn alarm_reason(suffix: &str) -> Option<&'static str> {
    Some(match suffix {
        "alarm" => reasons::ALARM,
        "min_alarm" => "ALARM_MIN",
        "max_alarm" => "ALARM_MAX",
        "lcrit_alarm" => "ALARM_LCRIT",
        "crit_alarm" => "ALARM_CRIT",
        "emergency_alarm" => "ALARM_EMERGENCY",
        "cap_alarm" => "ALARM_CAP",
        other if other.ends_with("_alarm") => reasons::ALARM,
        _ => return None,
    })
}

#[derive(Debug, Default)]
struct AttributeGroup {
    input: bool,
    average: bool,
    label: bool,
    fault: bool,
    alarms: Vec<(String, &'static str)>,
}

/// Groups attribute names into value channels with their companions.
fn parse_channels(attributes: &[String]) -> Vec<ChannelFiles> {
    let mut grouped: BTreeMap<(String, u32), AttributeGroup> = BTreeMap::new();
    for name in attributes {
        let Some((prefix, index, suffix)) = split_attribute(name) else {
            continue;
        };
        if !matches!(prefix, "temp" | "power" | "fan" | "energy") {
            continue;
        }
        let group = grouped.entry((prefix.to_owned(), index)).or_default();
        match suffix {
            "input" => group.input = true,
            "average" => group.average = true,
            "label" => group.label = true,
            "fault" => group.fault = true,
            other => {
                if let Some(reason) = alarm_reason(other) {
                    group.alarms.push((other.to_owned(), reason));
                }
            }
        }
    }
    let mut channels = Vec::new();
    for ((prefix, index), group) in grouped {
        let kinds: &[(bool, ChannelKind)] = match prefix.as_str() {
            "temp" => &[(group.input, ChannelKind::Temperature)],
            "power" => &[
                (group.input, ChannelKind::PowerInput),
                (group.average, ChannelKind::PowerAverage),
            ],
            "fan" => &[(group.input, ChannelKind::Fan)],
            _ => &[(group.input, ChannelKind::Energy)],
        };
        for (present, kind) in kinds {
            if *present {
                channels.push(ChannelFiles {
                    kind: *kind,
                    index,
                    has_label: group.label,
                    has_fault: group.fault,
                    alarms: group.alarms.clone(),
                });
            }
        }
    }
    channels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn attributes_split_into_type_index_suffix() {
        assert_eq!(split_attribute("temp12_input"), Some(("temp", 12, "input")));
        assert_eq!(
            split_attribute("temp1_crit_alarm"),
            Some(("temp", 1, "crit_alarm"))
        );
        assert_eq!(split_attribute("name"), None);
        assert_eq!(split_attribute("temp1"), None);
    }

    #[test]
    fn channels_group_companions_and_split_power_semantics() {
        let channels = parse_channels(&names(&[
            "name",
            "temp1_input",
            "temp1_label",
            "temp1_fault",
            "temp1_crit_alarm",
            "temp1_alarm",
            "power1_input",
            "power1_average",
            "power1_label",
            "fan2_input",
            "in0_input",
            "curr1_input",
            "energy1_input",
            "temp3_max",
        ]));
        let kinds: Vec<(ChannelKind, u32)> = channels
            .iter()
            .map(|channel| (channel.kind, channel.index))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (ChannelKind::Energy, 1),
                (ChannelKind::Fan, 2),
                (ChannelKind::PowerInput, 1),
                (ChannelKind::PowerAverage, 1),
                (ChannelKind::Temperature, 1),
            ]
        );
        let temp = &channels[4];
        assert!(temp.has_label && temp.has_fault);
        assert_eq!(temp.alarms.len(), 2);
        assert_eq!(channels[3].source_channel(), "power1_average");
        assert_eq!(channels[3].attribute(), "power1_average");
        assert_eq!(channels[2].source_channel(), "power1");
        assert_eq!(temp.attribute(), "temp1_input");
    }

    fn device_at(path: Option<&str>) -> DeviceMetadata {
        DeviceMetadata {
            name: "acpitz".to_owned(),
            device_path: path.map(str::to_owned),
            ..DeviceMetadata::default()
        }
    }

    #[test]
    fn only_devices_of_a_thermal_zone_are_mirrors() {
        let mirror = device_at(Some("/sys/devices/virtual/thermal/thermal_zone0"));
        assert!(is_thermal_zone_mirror(&mirror));
        let cooling = device_at(Some("/sys/devices/virtual/thermal/cooling_device0"));
        assert!(!is_thermal_zone_mirror(&cooling));
        let unnumbered = device_at(Some("/sys/devices/virtual/thermal/thermal_zone"));
        assert!(!is_thermal_zone_mirror(&unnumbered));
        let platform = device_at(Some("/sys/devices/platform/coretemp.0"));
        assert!(!is_thermal_zone_mirror(&platform));
        assert!(!is_thermal_zone_mirror(&device_at(None)));
    }

    #[test]
    fn alarm_suffixes_map_to_reasons() {
        assert_eq!(alarm_reason("alarm"), Some("ALARM"));
        assert_eq!(alarm_reason("crit_alarm"), Some("ALARM_CRIT"));
        assert_eq!(alarm_reason("rated_alarm"), Some("ALARM"));
        assert_eq!(alarm_reason("max"), None);
    }
}
