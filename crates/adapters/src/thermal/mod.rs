//! Linux thermal-zone adapter (`/sys/class/thermal`).
//!
//! Emits one `mdegC` gauge per zone in read group `fast`. `x86_pkg_temp`
//! maps to `cpu.temperature.package` only on single-package hosts with a
//! single such zone, with confirmed origin `x86_pkg_therm:0` (the same
//! physical sensor as coretemp `Package id 0`, so the collector treats both
//! as aliases); SoC zones map to `soc.temperature`; every other zone is a
//! `sensor.temperature` diagnostics source. Trip points are bounded entity
//! attributes; a `disabled` zone is unsupported (re-probed with throttling).

mod profile;
mod scan;

use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::HostId;
use dgxtop_core::naming;
use dgxtop_core::observation::AdapterManifest;

use self::scan::ThermalScanner;
use crate::common::SystemFiles;
use crate::hwmon::engine::{SysfsAdapter, sysfs_telemetry_port};
use crate::hwmon::sysfs::live_manifest;
use crate::registry::AdapterOptions;

/// Static manifest of the thermal adapter.
pub fn manifest() -> AdapterManifest {
    live_manifest(naming::adapters::THERMAL, "ThermalAdapter")
}

/// Thermal-zone telemetry adapter.
pub struct ThermalAdapter {
    inner: SysfsAdapter<ThermalScanner>,
}

impl ThermalAdapter {
    /// `files` is rooted at `/` in production and at a fake tree in tests.
    pub fn new(
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        files: Arc<dyn SystemFiles>,
        options: AdapterOptions,
    ) -> Self {
        Self {
            inner: SysfsAdapter::new(
                manifest(),
                clock,
                host_id,
                files,
                &options,
                ThermalScanner::new(),
            ),
        }
    }

    /// Number of currently catalogued sources.
    pub fn source_count(&self) -> usize {
        self.inner.source_count()
    }
}

sysfs_telemetry_port!(ThermalAdapter);
