//! Compile-time registry of the live telemetry adapters for the composition
//! root: the construction options shared by every adapter, the static
//! manifests and construction by id. There is no dynamic loading: an id that
//! is not compiled in is rejected.

use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{AdapterId, HostId};
use dgxtop_core::naming::{self, adapters};
use dgxtop_core::observation::{AdapterManifest, AdapterOperation, Isolation, MAX_CATALOG_PAGE};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::Namespace;
use thiserror::Error;

use crate::common::{RootedFiles, SystemFiles};

pub use dgxtop_core::naming::adapters::LIVE as LIVE_ADAPTERS;

/// Schema upper bound of `AdapterManifest::source_limit`.
pub const MAX_SOURCE_LIMIT: u32 = 2048;

/// Construction options shared by every live adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterOptions {
    /// Minimum interval between re-probes of an unsupported or
    /// permission-denied capability.
    pub unsupported_probe_ms: u64,
    /// Requested source limit; the manifest clamps it to 1..=2048.
    pub max_sources: u32,
    /// Upserts per discovery page and records per read page (1..=256).
    pub page_size: usize,
    /// The thermal adapter runs too and reads every thermal zone, so hwmon
    /// leaves the zones' hwmon mirrors to it (D-81); without it hwmon reads
    /// them.
    pub thermal_reads_zones: bool,
}

impl Default for AdapterOptions {
    fn default() -> Self {
        Self {
            unsupported_probe_ms: 60_000,
            max_sources: MAX_SOURCE_LIMIT,
            page_size: MAX_CATALOG_PAGE,
            thermal_reads_zones: true,
        }
    }
}

impl AdapterOptions {
    /// Source limit announced in the manifest and enforced by discovery.
    pub fn source_limit(&self) -> u32 {
        self.max_sources.clamp(1, MAX_SOURCE_LIMIT)
    }

    /// Page size clamped to the protocol limit.
    pub fn effective_page_size(&self) -> usize {
        self.page_size.clamp(1, MAX_CATALOG_PAGE)
    }
}

/// Why the registry could not build an adapter.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegistryError {
    #[error("unknown adapter id {adapter_id:?}: only compiled-in live adapters can be built")]
    UnknownAdapter { adapter_id: String },
    #[error("adapter {adapter_id:?} is not a live adapter; replay adapters are built separately")]
    NotLive { adapter_id: String },
}

/// Static manifest of one of the seven live adapters (default options).
pub fn live_manifest(adapter_id: &str) -> Option<AdapterManifest> {
    match adapter_id {
        adapters::PROCFS => Some(crate::procfs::manifest()),
        adapters::HWMON => Some(crate::hwmon::manifest()),
        adapters::THERMAL => Some(crate::thermal::manifest()),
        adapters::POWERCAP => Some(crate::powercap::manifest()),
        adapters::NVML => Some(crate::nvml::manifest()),
        adapters::NETWORK => Some(crate::network::manifest()),
        adapters::FILESYSTEM => Some(crate::filesystem::manifest()),
        _ => None,
    }
}

/// Builds one live adapter reading the real host (`/`).
pub fn build_live_adapter(
    adapter_id: &str,
    clock: Arc<dyn TimeProvider>,
    host_id: HostId,
    options: AdapterOptions,
) -> Result<Box<dyn TelemetryPort>, RegistryError> {
    let files: Arc<dyn SystemFiles> = Arc::new(RootedFiles::host());
    let adapter: Box<dyn TelemetryPort> = match adapter_id {
        adapters::PROCFS => Box::new(crate::procfs::ProcfsAdapter::new(
            clock, host_id, files, options,
        )),
        adapters::HWMON => Box::new(crate::hwmon::HwmonAdapter::new(
            clock, host_id, files, options,
        )),
        adapters::THERMAL => Box::new(crate::thermal::ThermalAdapter::new(
            clock, host_id, files, options,
        )),
        adapters::POWERCAP => Box::new(crate::powercap::PowercapAdapter::new(
            clock, host_id, files, options,
        )),
        adapters::NVML => Box::new(crate::nvml::NvmlAdapter::new(clock, host_id)),
        adapters::NETWORK => Box::new(crate::network::NetworkAdapter::new(
            clock, host_id, files, options,
        )),
        adapters::FILESYSTEM => Box::new(crate::filesystem::FilesystemAdapter::new(
            clock, host_id, files, options,
        )),
        other if naming::is_replay_adapter(other) => {
            return Err(RegistryError::NotLive {
                adapter_id: other.to_owned(),
            });
        }
        other => {
            return Err(RegistryError::UnknownAdapter {
                adapter_id: other.to_owned(),
            });
        }
    };
    Ok(adapter)
}

/// Manifest of a persistent-process adapter reading kernel files
/// (no runtime library, live namespace, one request in flight).
pub(crate) fn kernel_file_manifest(
    adapter_id: &'static str,
    implementation: &'static str,
    minimum_sample_ms: u32,
    options: &AdapterOptions,
) -> AdapterManifest {
    AdapterManifest {
        api_version: 1,
        adapter_id: compile_time_adapter_id(adapter_id),
        implementation: implementation.to_owned(),
        isolation: Isolation::PersistentProcess,
        operations: vec![
            AdapterOperation::Discover,
            AdapterOperation::Read,
            AdapterOperation::Shutdown,
        ],
        max_inflight: 1,
        runtime_library: None,
        minimum_sample_ms,
        source_limit: options.source_limit(),
        sample_namespace: Namespace::Live,
    }
}

/// Typed id of a compiled-in adapter; the names are literals covered by
/// tests, so a failure is a programming error.
pub(crate) fn compile_time_adapter_id(adapter_id: &'static str) -> AdapterId {
    crate::procfs::mechanics::invariant(AdapterId::new(adapter_id), adapter_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_limit_and_page_size_are_clamped() {
        let options = AdapterOptions {
            unsupported_probe_ms: 1,
            max_sources: 0,
            page_size: 0,
            ..AdapterOptions::default()
        };
        assert_eq!(options.source_limit(), 1);
        assert_eq!(options.effective_page_size(), 1);
        let options = AdapterOptions {
            unsupported_probe_ms: 1,
            max_sources: 100_000,
            page_size: 100_000,
            ..AdapterOptions::default()
        };
        assert_eq!(options.source_limit(), 2048);
        assert_eq!(options.effective_page_size(), 256);
    }
}
