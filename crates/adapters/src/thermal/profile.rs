//! Versioned thermal-zone profiles. A zone type alone never proves a CPU
//! package: `x86_pkg_temp` and ARM `cpu-thermal` only map to
//! `cpu.temperature.package` when the host has exactly one physical package
//! and exactly one such zone; SoC zones map to `soc.temperature`. GB10
//! (D-68) names its `acpitz` zones by ACPI path: on one package with all
//! four of its CPU cluster zones, those map to `cpu.temperature.cluster`
//! and its SoC zone to `soc.temperature`. Everything else (acpitz, INT3400,
//! pch_*, iwlwifi_*, B0D4, TCPU, ...) is diagnostics.

use dgxtop_core::metric::ids;
use dgxtop_core::quality::Quality;

use crate::hwmon::mapping::{Mapping, ProfileSpec, Target};

pub(crate) const X86_PKG_TEMP: ProfileSpec = ProfileSpec {
    id: "thermal-x86-pkg-temp",
    revision: 1,
};
pub(crate) const SOC: ProfileSpec = ProfileSpec {
    id: "thermal-soc",
    revision: 1,
};
pub(crate) const ARM_CPU: ProfileSpec = ProfileSpec {
    id: "thermal-arm-cpu",
    revision: 1,
};
pub(crate) const GENERIC: ProfileSpec = ProfileSpec {
    id: "thermal-generic",
    revision: 1,
};
/// GB10 (DGX Spark) ACPI zones, usable: the firmware names them, no
/// datasheet does.
pub(crate) const GB10: ProfileSpec = ProfileSpec {
    id: "thermal-gb10",
    revision: 1,
};

/// The zone type of ACPI thermal zones.
pub(crate) const ACPI_ZONE_TYPE: &str = "acpitz";
/// GB10's CPU cluster zones by ACPI path: the E and P cores of each of its
/// two clusters (cluster 0 is CPUs 0-9, cluster 1 CPUs 10-19).
const GB10_CLUSTERS: [(&str, &str); 4] = [
    ("\\_TZ_.TS0E", "cluster:0e"),
    ("\\_TZ_.TS0P", "cluster:0p"),
    ("\\_TZ_.TS1E", "cluster:1e"),
    ("\\_TZ_.TS1P", "cluster:1p"),
];
/// GB10's SoC zone.
const GB10_SOC: &str = "\\_TZ_.TSOC";

/// The package-temperature zone type of Intel CPUs.
pub(crate) const X86_PKG_TEMP_TYPE: &str = "x86_pkg_temp";

/// Host facts gating package mappings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ThermalHostFacts {
    /// Distinct `physical_package_id` values; `None` when unknown.
    pub physical_packages: Option<usize>,
    pub x86_pkg_zones: usize,
    pub arm_cpu_zones: usize,
    /// `acpitz` zones on GB10 CPU cluster paths.
    pub gb10_cluster_zones: usize,
}

impl ThermalHostFacts {
    fn single_package(&self) -> bool {
        self.physical_packages == Some(1)
    }

    fn gb10(&self) -> bool {
        self.single_package() && self.gb10_cluster_zones == GB10_CLUSTERS.len()
    }
}

/// The cluster scope of a GB10 CPU cluster zone's ACPI path.
pub(crate) fn gb10_cluster(acpi_path: &str) -> Option<&'static str> {
    GB10_CLUSTERS
        .iter()
        .find(|(path, _)| *path == acpi_path)
        .map(|(_, scope)| *scope)
}

/// ARM SoC CPU zone types.
pub(crate) fn is_arm_cpu_zone(zone_type: &str) -> bool {
    zone_type == "cpu-thermal" || zone_type == "cpu_thermal"
}

/// SoC zone rank: whole-SoC zones before `soc_max`.
fn soc_rank(zone_type: &str) -> Option<u16> {
    let lower = zone_type.to_ascii_lowercase();
    match lower.as_str() {
        "soc_thermal" | "soc-thermal" | "soc" => Some(0),
        "soc_max" => Some(1),
        _ => None,
    }
}

/// Classifies one zone by its type and, for an ACPI zone, its ACPI path.
pub(crate) fn classify(
    host: &ThermalHostFacts,
    zone_type: &str,
    acpi_path: Option<&str>,
) -> Mapping {
    if zone_type == ACPI_ZONE_TYPE
        && host.gb10()
        && let Some(mapping) = acpi_path.and_then(gb10_zone)
    {
        return mapping;
    }
    if zone_type == X86_PKG_TEMP_TYPE && host.single_package() && host.x86_pkg_zones == 1 {
        return package(
            // Same MSR as coretemp `Package id 0`: the collector treats them as aliases.
            Some("x86_pkg_therm:0".to_owned()),
            Quality::Verified,
            X86_PKG_TEMP,
            1,
        );
    }
    if let Some(rank) = soc_rank(zone_type) {
        return Mapping {
            target: Target::Proven {
                entity: "soc".to_owned(),
                metric: ids::SOC_TEMPERATURE,
                scope: "soc".to_owned(),
            },
            origin: None,
            quality: Quality::Usable,
            profile: SOC,
            rank,
        };
    }
    if is_arm_cpu_zone(zone_type) && host.single_package() && host.arm_cpu_zones == 1 {
        return package(None, Quality::Usable, ARM_CPU, 2);
    }
    Mapping {
        target: Target::Diagnostic {
            metric: ids::SENSOR_TEMPERATURE,
        },
        origin: None,
        quality: Quality::Unknown,
        profile: GENERIC,
        rank: 0,
    }
}

/// A GB10 zone with a meaning: a CPU cluster or the SoC.
fn gb10_zone(acpi_path: &str) -> Option<Mapping> {
    let (entity, metric, scope) = match gb10_cluster(acpi_path) {
        Some(scope) => ("cpu.package.0", ids::CPU_TEMPERATURE_CLUSTER, scope),
        None if acpi_path == GB10_SOC => ("soc", ids::SOC_TEMPERATURE, "soc"),
        None => return None,
    };
    Some(Mapping {
        target: Target::Proven {
            entity: entity.to_owned(),
            metric,
            scope: scope.to_owned(),
        },
        origin: None,
        quality: Quality::Usable,
        profile: GB10,
        rank: 0,
    })
}

fn package(origin: Option<String>, quality: Quality, profile: ProfileSpec, rank: u16) -> Mapping {
    Mapping {
        target: Target::Proven {
            entity: "cpu.package.0".to_owned(),
            metric: ids::CPU_TEMPERATURE_PACKAGE,
            scope: "package:0".to_owned(),
        },
        origin,
        quality,
        profile,
        rank,
    }
}
