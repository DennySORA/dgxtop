//! Linux hwmon adapter (`/sys/class/hwmon`).
//!
//! Emits temperatures (`mdegC`) and power (`uW`, instantaneous `input` and
//! driver-averaged `average` as separate sources) in read group `fast`. A
//! channel is catalogued only when a versioned profile proves what it
//! measures (coretemp, AMD k10temp/zenpower, NVIDIA GB10); every other
//! channel, every fan and every energy counter is left unread (D-92). A
//! driver fault flag keeps the value but marks it invalid (`SENSOR_FAULT`);
//! an alarm flag only adds `ALARM`. The kernel's hwmon mirror of thermal
//! zones (a device whose `device` is a `thermal_zoneN`, such as `acpitz`) is
//! skipped: the thermal adapter reads those sensors (D-81).
//!
//! The submodules `sysfs`, `reader`, `mapping`, `catalog`, `table` and
//! `engine` are the sysfs mechanics shared with the thermal, powercap (and,
//! for catalog publication, replay) adapters; they live here only because of
//! module ownership and are candidates for `common.rs`.

pub(crate) mod catalog;
pub(crate) mod engine;
pub(crate) mod mapping;
mod profile;
pub(crate) mod reader;
mod scan;
pub(crate) mod sysfs;
pub(crate) mod table;

use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::HostId;
use dgxtop_core::naming;
use dgxtop_core::observation::AdapterManifest;

use self::engine::{SysfsAdapter, sysfs_telemetry_port};
use self::scan::HwmonScanner;
use crate::common::SystemFiles;
use crate::registry::AdapterOptions;

/// Static manifest of the hwmon adapter.
pub fn manifest() -> AdapterManifest {
    sysfs::live_manifest(naming::adapters::HWMON, "HwmonAdapter")
}

/// hwmon telemetry adapter.
pub struct HwmonAdapter {
    inner: SysfsAdapter<HwmonScanner>,
}

impl HwmonAdapter {
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
                HwmonScanner::new(),
            ),
        }
    }

    /// Number of currently catalogued sources.
    pub fn source_count(&self) -> usize {
        self.inner.source_count()
    }
}

sysfs_telemetry_port!(HwmonAdapter);
