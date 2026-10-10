//! Versioned hwmon profiles: which channels a profile proves to be a CPU
//! metric, with what origin, quality and rank. Everything a profile does not
//! prove stays a `sensor.*` diagnostics channel with unknown origin — `temp1`
//! is not necessarily the CPU.
//!
//! Pure functions over driver facts, so every rule is unit-tested without a
//! filesystem.

use dgxtop_core::metric::ids;
use dgxtop_core::quality::Quality;

use super::mapping::{Mapping, ProfileSpec, Target};

/// Kind of an hwmon value channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ChannelKind {
    /// `tempN_input` (millidegree Celsius).
    Temperature,
    /// `powerN_input` (microwatt, instantaneous).
    PowerInput,
    /// `powerN_average` (microwatt, averaged by the driver).
    PowerAverage,
    /// `fanN_input` (RPM).
    Fan,
    /// `energyN_input` (microjoule counter).
    Energy,
}

impl ChannelKind {
    /// sysfs attribute type prefix (`temp`, `power`, ...).
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Temperature => "temp",
            Self::PowerInput | Self::PowerAverage => "power",
            Self::Fan => "fan",
            Self::Energy => "energy",
        }
    }

    /// Value attribute suffix (`input` or `average`).
    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Self::PowerAverage => "average",
            _ => "input",
        }
    }

    /// Diagnostics metric used when no profile proves the purpose.
    fn diagnostics_metric(self) -> &'static str {
        match self {
            Self::Temperature => ids::SENSOR_TEMPERATURE,
            Self::PowerInput | Self::PowerAverage => ids::SENSOR_POWER,
            Self::Fan => ids::SENSOR_FAN,
            Self::Energy => ids::SENSOR_ENERGY,
        }
    }

    fn is_power(self) -> bool {
        matches!(self, Self::PowerInput | Self::PowerAverage)
    }

    /// Input before average: different semantics, never merged into one source.
    fn power_rank(self) -> u16 {
        match self {
            Self::PowerAverage => 1,
            _ => 0,
        }
    }
}

pub(crate) const CORETEMP: ProfileSpec = ProfileSpec {
    id: "hwmon-coretemp",
    revision: 1,
};
pub(crate) const AMD_K10TEMP: ProfileSpec = ProfileSpec {
    id: "hwmon-amd-k10temp",
    revision: 1,
};
pub(crate) const NVIDIA_GB10: ProfileSpec = ProfileSpec {
    id: "nvidia-gb10-hwmon",
    revision: 1,
};
pub(crate) const GENERIC: ProfileSpec = ProfileSpec {
    id: "hwmon-generic",
    revision: 1,
};

/// Facts about one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChannelFacts<'a> {
    pub kind: ChannelKind,
    pub index: u32,
    pub label: Option<&'a str>,
}

/// Facts about one hwmon device, derived from its name and labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceFacts<'a> {
    pub name: &'a str,
    pub device_key: &'a str,
    /// Package of a coretemp device: its single `Package id N` label.
    pub coretemp_package: Option<u32>,
    /// AMD device exposing `Tdie` (then `Tctl`, which may carry an offset, is not used).
    pub has_tdie: bool,
    /// Device exposing both `cpu_p` and `cpu_e` power labels.
    pub gb10_labels: bool,
}

/// Facts about the whole host that gate multi-device profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct HostFacts {
    /// k10temp/zenpower devices; mapping is only proven for exactly one.
    pub amd_devices: usize,
    /// Devices with GB10 cluster labels; mapping is only proven for exactly one.
    pub gb10_devices: usize,
}

/// Parses `Package id N` (or the older `Physical id N`).
pub(crate) fn package_label(label: &str) -> Option<u32> {
    let rest = label
        .strip_prefix("Package id ")
        .or_else(|| label.strip_prefix("Physical id "))?;
    rest.trim().parse().ok()
}

/// Parses `Core M`.
fn core_label(label: &str) -> Option<u32> {
    label.strip_prefix("Core ")?.trim().parse().ok()
}

/// Whether a device name belongs to the AMD k10temp/zenpower family.
pub(crate) fn is_amd_temperature_driver(name: &str) -> bool {
    name == "k10temp" || name == "zenpower"
}

/// GB10 power labels compare case-insensitively after trimming.
pub(crate) fn label_is(label: Option<&str>, expected: &str) -> bool {
    label.is_some_and(|text| text.trim().eq_ignore_ascii_case(expected))
}

/// Classifies one channel of one device.
pub(crate) fn classify(
    host: &HostFacts,
    device: &DeviceFacts<'_>,
    channel: &ChannelFacts<'_>,
) -> Mapping {
    let proven = if device.name == "coretemp" {
        coretemp(device, channel)
    } else if is_amd_temperature_driver(device.name) && host.amd_devices == 1 {
        amd(device, channel)
    } else if device.gb10_labels && host.gb10_devices == 1 {
        gb10(device, channel)
    } else {
        None
    };
    proven.unwrap_or_else(|| diagnostic(device, channel))
}

fn coretemp(device: &DeviceFacts<'_>, channel: &ChannelFacts<'_>) -> Option<Mapping> {
    if channel.kind != ChannelKind::Temperature {
        return None;
    }
    let package = device.coretemp_package?;
    let label = channel.label?;
    if package_label(label) == Some(package) {
        return Some(Mapping {
            target: Target::Proven {
                entity: format!("cpu.package.{package}"),
                metric: ids::CPU_TEMPERATURE_PACKAGE,
                scope: format!("package:{package}"),
            },
            // Same MSR as the x86_pkg_temp thermal zone: one physical sensor.
            origin: Some(format!("x86_pkg_therm:{package}")),
            quality: Quality::Verified,
            profile: CORETEMP,
            rank: 0,
        });
    }
    let core = core_label(label)?;
    Some(Mapping {
        target: Target::Proven {
            entity: format!("cpu.package.{package}"),
            metric: ids::CPU_TEMPERATURE_CORE,
            scope: format!("core:{core}"),
        },
        origin: Some(format!("coretemp:{package}:core:{core}")),
        quality: Quality::Verified,
        profile: CORETEMP,
        rank: 0,
    })
}

fn amd(device: &DeviceFacts<'_>, channel: &ChannelFacts<'_>) -> Option<Mapping> {
    if channel.kind != ChannelKind::Temperature {
        return None;
    }
    let label = channel.label?;
    let (origin, quality, rank) = match label {
        "Tdie" => ("amd_tdie:0", Quality::Verified, 0),
        // Tctl may include a vendor offset: usable fallback only without Tdie.
        "Tctl" if !device.has_tdie => ("amd_tctl:0", Quality::Usable, 1),
        _ => return None,
    };
    Some(Mapping {
        target: Target::Proven {
            entity: "cpu.package.0".to_owned(),
            metric: ids::CPU_TEMPERATURE_PACKAGE,
            scope: "package:0".to_owned(),
        },
        origin: Some(origin.to_owned()),
        quality,
        profile: AMD_K10TEMP,
        rank,
    })
}

fn gb10(device: &DeviceFacts<'_>, channel: &ChannelFacts<'_>) -> Option<Mapping> {
    if !channel.kind.is_power() {
        return None;
    }
    let (entity, metric, scope) = if label_is(channel.label, "cpu_p") {
        ("cpu.package.0", ids::CPU_POWER_CLUSTER, "cluster:p")
    } else if label_is(channel.label, "cpu_e") {
        ("cpu.package.0", ids::CPU_POWER_CLUSTER, "cluster:e")
    } else if label_is(channel.label, "soc_pkg") {
        ("host", ids::CPU_POWER_PLATFORM, "platform")
    } else {
        return None;
    };
    Some(Mapping {
        target: Target::Proven {
            entity: entity.to_owned(),
            metric,
            scope: scope.to_owned(),
        },
        origin: Some(power_origin(device, channel)),
        // Unverified hardware mapping: usable, never verified.
        quality: Quality::Usable,
        profile: NVIDIA_GB10,
        rank: channel.kind.power_rank(),
    })
}

fn diagnostic(device: &DeviceFacts<'_>, channel: &ChannelFacts<'_>) -> Mapping {
    // Input and average of one power channel read the same sensor; that much
    // is proven even when its purpose is not.
    let origin = channel
        .kind
        .is_power()
        .then(|| power_origin(device, channel));
    Mapping {
        target: Target::Diagnostic {
            metric: channel.kind.diagnostics_metric(),
        },
        origin,
        quality: Quality::Unknown,
        profile: GENERIC,
        rank: if channel.kind.is_power() {
            channel.kind.power_rank()
        } else {
            0
        },
    }
}

fn power_origin(device: &DeviceFacts<'_>, channel: &ChannelFacts<'_>) -> String {
    format!("hwmon:{}:power{}", device.device_key, channel.index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device<'a>(name: &'a str) -> DeviceFacts<'a> {
        DeviceFacts {
            name,
            device_key: "dev",
            coretemp_package: None,
            has_tdie: false,
            gb10_labels: false,
        }
    }

    fn temp(label: Option<&str>) -> ChannelFacts<'_> {
        ChannelFacts {
            kind: ChannelKind::Temperature,
            index: 1,
            label,
        }
    }

    fn proven_scope(mapping: &Mapping) -> Option<(&str, &str, &str)> {
        match &mapping.target {
            Target::Proven {
                entity,
                metric,
                scope,
            } => Some((entity.as_str(), *metric, scope.as_str())),
            Target::Diagnostic { .. } => None,
        }
    }

    #[test]
    fn coretemp_without_package_label_stays_diagnostic() {
        let coretemp = device("coretemp");
        let core = classify(&HostFacts::default(), &coretemp, &temp(Some("Core 0")));
        assert_eq!(
            core.target,
            Target::Diagnostic {
                metric: ids::SENSOR_TEMPERATURE
            }
        );
        assert_eq!(core.origin, None);
        assert_eq!(core.quality, Quality::Unknown);
    }

    #[test]
    fn amd_multi_socket_is_unproven() {
        let host = HostFacts {
            amd_devices: 2,
            gb10_devices: 0,
        };
        let mut k10 = device("k10temp");
        k10.has_tdie = true;
        assert!(proven_scope(&classify(&host, &k10, &temp(Some("Tdie")))).is_none());
    }

    #[test]
    fn package_labels_parse_both_spellings() {
        assert_eq!(package_label("Package id 3"), Some(3));
        assert_eq!(package_label("Physical id 0"), Some(0));
        assert_eq!(package_label("Package id x"), None);
        assert_eq!(package_label("Core 0"), None);
    }
}
