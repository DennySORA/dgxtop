//! Linux powercap adapter (`/sys/class/powercap`, RAPL energy counters).
//!
//! Emits `energy_uj` as exact `u64` counters (native `uJ`) in read group
//! `fast`, with the driver-declared `max_energy_range_uj` as counter modulus.
//! `package-N` -> `cpu.energy.package`, `psys` -> `cpu.energy.platform`,
//! subdomains -> `sensor.energy` diagnostics. `energy_uj` is often root-only:
//! a permission error is reported as `permission_denied` (record and
//! capability), never as 0, and re-probed with throttling.

mod profile;
mod scan;

use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::HostId;
use dgxtop_core::naming;
use dgxtop_core::observation::AdapterManifest;

use self::scan::PowercapScanner;
use crate::common::SystemFiles;
use crate::hwmon::engine::{SysfsAdapter, sysfs_telemetry_port};
use crate::hwmon::sysfs::live_manifest;
use crate::registry::AdapterOptions;

/// Static manifest of the powercap adapter.
pub fn manifest() -> AdapterManifest {
    live_manifest(naming::adapters::POWERCAP, "PowercapAdapter")
}

/// Powercap (RAPL) telemetry adapter.
pub struct PowercapAdapter {
    inner: SysfsAdapter<PowercapScanner>,
}

impl PowercapAdapter {
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
                PowercapScanner::new(),
            ),
        }
    }

    /// Number of currently catalogued sources.
    pub fn source_count(&self) -> usize {
        self.inner.source_count()
    }
}

sysfs_telemetry_port!(PowercapAdapter);
