//! Versioned powercap (RAPL) profile. Only top-level `intel-rapl:N` zones are
//! mapped (AMD Zen uses the same driver name): `package-N` is package energy,
//! `psys` is platform energy — platform power is never CPU package power.
//! Subdomains (core/uncore/dram), MMIO duplicates and unknown names stay
//! `sensor.energy` diagnostics.

use dgxtop_core::metric::ids;
use dgxtop_core::quality::Quality;

use crate::hwmon::mapping::{Mapping, ProfileSpec, Target};

pub(crate) const RAPL: ProfileSpec = ProfileSpec {
    id: "powercap-rapl",
    revision: 1,
};
pub(crate) const GENERIC: ProfileSpec = ProfileSpec {
    id: "powercap-generic",
    revision: 1,
};

/// RAPL control type whose top-level zones are mapped.
const RAPL_CONTROL: &str = "intel-rapl";

/// Control type of a zone directory (`intel-rapl:0:1` -> `intel-rapl`).
pub(crate) fn control_type(zone: &str) -> &str {
    zone.split(':').next().unwrap_or(zone)
}

/// Parent zone directory of a subzone (`intel-rapl:0:1` -> `intel-rapl:0`).
pub(crate) fn parent_zone(zone: &str) -> Option<&str> {
    let (parent, _) = zone.rsplit_once(':')?;
    parent.contains(':').then_some(parent)
}

fn is_top_level(zone: &str) -> bool {
    zone.matches(':').count() == 1
}

/// `package-N` with a plain decimal N (`package-0-die-1` is not a package).
fn package_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("package-")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Classifies one powercap zone by directory and `name`.
pub(crate) fn classify(zone: &str, name: &str) -> Mapping {
    if control_type(zone) == RAPL_CONTROL && is_top_level(zone) {
        if let Some(package) = package_index(name) {
            return Mapping {
                target: Target::Proven {
                    entity: format!("cpu.package.{package}"),
                    metric: ids::CPU_ENERGY_PACKAGE,
                    scope: format!("package:{package}"),
                },
                origin: Some(format!("rapl:package:{package}")),
                quality: Quality::Verified,
                profile: RAPL,
                rank: 0,
            };
        }
        if name == "psys" {
            return Mapping {
                target: Target::Proven {
                    entity: "host".to_owned(),
                    metric: ids::CPU_ENERGY_PLATFORM,
                    scope: "platform".to_owned(),
                },
                origin: Some("rapl:psys".to_owned()),
                // Platform coverage differs between products.
                quality: Quality::Usable,
                profile: RAPL,
                rank: 0,
            };
        }
    }
    Mapping {
        target: Target::Diagnostic {
            metric: ids::SENSOR_ENERGY,
        },
        origin: None,
        quality: Quality::Unknown,
        profile: GENERIC,
        rank: 0,
    }
}
