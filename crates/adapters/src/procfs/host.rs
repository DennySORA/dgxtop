//! Host-level facts: scheduler tick and page size (`sysconf`), the bounded
//! host metadata attributes, and the `host` entity with its `/proc/loadavg`
//! sources.

use dgxtop_core::identity::{EntityId, Scope};
use dgxtop_core::naming::{attributes, entity, groups};
use dgxtop_core::observation::{Attribute, EntityKind};
use dgxtop_core::quality::RawKind;
use nix::unistd::{SysconfVar, sysconf};

use super::catalog::{EntityParts, KeyContext, SnapshotBuilder, SourceSpec, source_draft};
use super::locator::{DRIVER, FAMILY_HOST, LOAD_FIELDS, ProcfsLocator, RANK_HOST_WIDE};
use super::mechanics::{invariant, namespace_link};
use super::parse::parse_cpu_model;
use crate::common::SystemFiles;

const HOSTNAME_PATH: &str = "/proc/sys/kernel/hostname";
const KERNEL_PATH: &str = "/proc/sys/kernel/osrelease";
const CPUINFO_PATH: &str = "/proc/cpuinfo";
const LOADAVG_PATH: &str = "/proc/loadavg";
pub(crate) const PID_NAMESPACE_PATH: &str = "/proc/self/ns/pid";

/// Per-process constants needed to interpret procfs values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostConstants {
    /// Scheduler clock ticks per second (`_SC_CLK_TCK`).
    pub clk_tck: Option<u64>,
    /// Memory page size in bytes (`_SC_PAGE_SIZE`).
    pub page_size: Option<u64>,
}

impl HostConstants {
    /// Values of this process; unavailable values stay `None`.
    pub fn from_sysconf() -> Self {
        Self {
            clk_tck: sysconf_value(SysconfVar::CLK_TCK),
            page_size: sysconf_value(SysconfVar::PAGE_SIZE),
        }
    }
}

fn sysconf_value(variable: SysconfVar) -> Option<u64> {
    let value = sysconf(variable).ok()??;
    u64::try_from(value).ok().filter(|value| *value > 0)
}

/// Host metadata read once per adapter lifetime (static for the boot).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct HostMetadata {
    pub hostname: Option<String>,
    pub kernel: Option<String>,
    pub cpu_model: Option<String>,
    pub pid_namespace: Option<String>,
}

pub(crate) fn load_host_metadata(files: &dyn SystemFiles) -> HostMetadata {
    HostMetadata {
        hostname: first_line(files, HOSTNAME_PATH),
        kernel: first_line(files, KERNEL_PATH),
        cpu_model: files
            .read_to_string(CPUINFO_PATH)
            .ok()
            .and_then(|text| parse_cpu_model(&text)),
        pid_namespace: namespace_link(files, PID_NAMESPACE_PATH),
    }
}

fn first_line(files: &dyn SystemFiles, path: &str) -> Option<String> {
    let text = files.read_to_string(path).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

fn host_attributes(metadata: &HostMetadata, constants: &HostConstants) -> Vec<Attribute> {
    let candidates = [
        (
            attributes::CLK_TCK,
            constants.clk_tck.map(|value| value.to_string()),
        ),
        (
            attributes::PAGE_SIZE,
            constants.page_size.map(|value| value.to_string()),
        ),
        (attributes::HOSTNAME, metadata.hostname.clone()),
        (attributes::KERNEL, metadata.kernel.clone()),
        (attributes::CPU_MODEL, metadata.cpu_model.clone()),
        (attributes::PID_NAMESPACE, metadata.pid_namespace.clone()),
    ];
    candidates
        .into_iter()
        .filter_map(|(key, value)| {
            value.map(|value| Attribute {
                key: key.to_owned(),
                value,
            })
        })
        .collect()
}

pub(crate) fn host_entity_id() -> EntityId {
    invariant(EntityId::new(entity::HOST), entity::HOST)
}

pub(crate) fn host_scope() -> Scope {
    invariant(Scope::new(entity::SCOPE_HOST), entity::SCOPE_HOST)
}

/// Adds the `host` entity (metadata attributes) and its load sources.
pub(crate) fn add_host(
    builder: &mut SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    metadata: &HostMetadata,
    constants: &HostConstants,
) {
    let entity_id = host_entity_id();
    let Some(incarnation) = builder.incarnation(&entity_id, entity::HOST) else {
        return;
    };
    let scope = host_scope();
    let sources = LOAD_FIELDS
        .into_iter()
        .filter_map(|field| {
            let origin = format!("procfs:{LOADAVG_PATH}:{}", field.channel());
            let spec = SourceSpec {
                device_id: "loadavg",
                channel: field.channel(),
                entity: &entity_id,
                scope: &scope,
                metric_id: field.metric_id(),
                group: groups::CPU,
                native_unit: field.native_unit(),
                raw_kind: RawKind::Gauge,
                driver: DRIVER,
                device_path: LOADAVG_PATH,
                origin: &origin,
                update_interval_ms: field.update_interval_ms(),
                label: None,
                rank: RANK_HOST_WIDE,
            };
            source_draft(builder.config(), keys, &spec, ProcfsLocator::Load(field))
        })
        .collect();
    let parts = EntityParts {
        entity_id,
        kind: EntityKind::Host,
        parent_id: None,
        display_name: metadata
            .hostname
            .clone()
            .unwrap_or_else(|| entity::HOST.to_owned()),
        attributes: host_attributes(metadata, constants),
    };
    builder.add(FAMILY_HOST, parts, incarnation, sources);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_skip_unknown_values() {
        let metadata = HostMetadata {
            hostname: Some("dgx".to_owned()),
            ..HostMetadata::default()
        };
        let constants = HostConstants {
            clk_tck: Some(100),
            page_size: None,
        };
        let attributes = host_attributes(&metadata, &constants);
        let keys: Vec<&str> = attributes
            .iter()
            .map(|attribute| attribute.key.as_str())
            .collect();
        assert_eq!(keys, vec!["clk_tck", "hostname"]);
    }

    #[test]
    fn compile_time_identities_are_valid() {
        assert_eq!(host_entity_id().as_str(), "host");
        assert_eq!(host_scope().as_str(), "host");
    }
}
