//! Adapter-side contracts: catalog (entities, sources, capabilities), read
//! plans, raw observations, observation batches and the adapter-host IPC
//! envelopes (`observation*.schema.json`, `ipc-request.schema.json`,
//! `adapter-manifest.schema.json`).

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::identity::{
    AdapterId, EntityId, MetricKey, OriginId, OriginIdentity, OriginRelation, ProfileId, ReadGroup,
    SourceId,
};
use crate::metric::{NativeUnit, Scalar};
use crate::quality::{Availability, Quality, RawKind, ReadStatus, SampleKind, Support};
use crate::shared_text::SharedText;

/// Observation schema version.
pub const OBSERVATION_SCHEMA_VERSION: u32 = 1;
/// Adapter-host IPC protocol version.
pub const IPC_PROTOCOL_VERSION: u32 = 1;
/// Hard cap on records in one batch (schema `maxItems`).
pub const MAX_RECORDS_PER_BATCH: usize = 256;
/// Hard cap on source slots in one read request.
pub const MAX_SOURCE_SLOTS: usize = 256;
/// Hard cap on reason codes per record.
pub const MAX_REASON_CODES: usize = 32;
/// Hard cap on catalog upserts per discovery page.
pub const MAX_CATALOG_PAGE: usize = 256;

/// Contract violations detected while validating adapter output.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContractError {
    #[error("{0}")]
    Invalid(String),
}

fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::Invalid(message.into())
}

/// Where a raw value came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub adapter_id: AdapterId,
    pub source_id: SourceId,
    pub origin_id: Option<OriginId>,
    pub origin_relation: OriginRelation,
    pub driver: SharedText,
    pub device_path: SharedText,
    pub channel: SharedText,
    pub generation: u32,
    pub profile_id: ProfileId,
    pub profile_revision: u32,
}

impl Provenance {
    pub fn origin(&self) -> OriginIdentity {
        OriginIdentity {
            origin_id: self.origin_id.clone(),
            relation: self.origin_relation,
        }
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        let bounded = |name: &str, value: &str, max: usize| {
            if value.is_empty() || value.len() > max {
                Err(invalid(format!(
                    "provenance.{name} must be 1..={max} bytes"
                )))
            } else {
                Ok(())
            }
        };
        bounded("driver", &self.driver, 256)?;
        bounded("device_path", &self.device_path, 1024)?;
        bounded("channel", &self.channel, 256)?;
        if i32::try_from(self.generation).is_err() {
            return Err(invalid("provenance.generation exceeds i32::MAX"));
        }
        if self.profile_revision == 0 || i32::try_from(self.profile_revision).is_err() {
            return Err(invalid(
                "provenance.profile_revision must be 1..=2147483647",
            ));
        }
        self.origin()
            .validate()
            .map_err(|error| invalid(error.to_string()))
    }
}

/// One raw source reading (`observation.schema.json`). `value` is present
/// exactly when `read_status == ok`; failures never become 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub schema_version: u32,
    #[serde(with = "crate::wire::u64_decimal")]
    pub cycle_id: u64,
    pub config_revision: u32,
    pub key: MetricKey,
    pub provenance: Provenance,
    pub sample_kind: SampleKind,
    pub read_status: ReadStatus,
    pub quality: Quality,
    pub native_unit: NativeUnit,
    pub value: Option<Scalar>,
    #[serde(with = "crate::wire::u64_decimal")]
    pub read_started_boot_ns: u64,
    #[serde(with = "crate::wire::u64_decimal")]
    pub read_finished_boot_ns: u64,
    #[serde(with = "crate::wire::opt_u64_decimal")]
    pub device_sample_boot_ns: Option<u64>,
    pub source_update_interval_ms: Option<u32>,
    pub reason_codes: Vec<String>,
}

impl Observation {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != OBSERVATION_SCHEMA_VERSION {
            return Err(invalid("observation schema_version must be 1"));
        }
        if self.config_revision == 0 || i32::try_from(self.config_revision).is_err() {
            return Err(invalid("config_revision must be 1..=2147483647"));
        }
        self.provenance.validate()?;
        if self.read_status.is_ok() != self.value.is_some() {
            return Err(invalid(
                "value must be present exactly when read_status is ok",
            ));
        }
        if self.read_finished_boot_ns < self.read_started_boot_ns {
            return Err(invalid(
                "read_finished_boot_ns precedes read_started_boot_ns",
            ));
        }
        if let Some(interval) = self.source_update_interval_ms
            && !(1..=3_600_000).contains(&interval)
        {
            return Err(invalid("source_update_interval_ms must be 1..=3600000"));
        }
        if self.reason_codes.len() > MAX_REASON_CODES {
            return Err(invalid("at most 32 reason codes"));
        }
        if self
            .reason_codes
            .iter()
            .any(|code| code.is_empty() || code.len() > 80)
        {
            return Err(invalid("reason codes must be 1..=80 bytes"));
        }
        Ok(())
    }
}

/// Batch completion status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    Ok,
    /// Some sources failed or more pages follow (see `error_code`).
    Partial,
    Failed,
}

/// `error_code` marking a non-final page of a paged read group.
pub const PAGE_CONTINUES: &str = "PAGE_CONTINUES";

/// One adapter reply to a read plan (`observation-batch.schema.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationBatch {
    pub protocol_version: u32,
    pub adapter_id: AdapterId,
    pub host_generation: u32,
    pub catalog_revision: u32,
    pub config_revision: u32,
    pub request_id: Uuid,
    #[serde(with = "crate::wire::u64_decimal")]
    pub cycle_id: u64,
    pub read_group: ReadGroup,
    #[serde(with = "crate::wire::u64_decimal")]
    pub started_boot_ns: u64,
    #[serde(with = "crate::wire::u64_decimal")]
    pub finished_boot_ns: u64,
    pub records: Vec<Observation>,
    pub status: BatchStatus,
    pub error_code: Option<String>,
}

impl ObservationBatch {
    /// Validates shape and consistency with the plan that requested it.
    pub fn validate_against(
        &self,
        plan: &ReadPlan,
        max_records: usize,
    ) -> Result<(), ContractError> {
        if self.protocol_version != IPC_PROTOCOL_VERSION {
            return Err(invalid("unsupported protocol_version"));
        }
        if self.adapter_id != plan.adapter_id {
            return Err(invalid("batch adapter_id differs from the plan"));
        }
        if self.request_id != plan.request_id {
            return Err(invalid("batch request_id differs from the plan"));
        }
        if self.host_generation != plan.host_generation {
            return Err(invalid("batch host_generation differs from the plan"));
        }
        if self.cycle_id != plan.cycle_id || self.read_group != plan.read_group {
            return Err(invalid("batch cycle/read_group differs from the plan"));
        }
        if self.config_revision != plan.config_revision
            || self.catalog_revision != plan.catalog_revision
        {
            return Err(invalid("batch revisions differ from the plan"));
        }
        if self.records.len() > max_records.min(MAX_RECORDS_PER_BATCH) {
            return Err(invalid("too many records in one batch"));
        }
        if self.finished_boot_ns < self.started_boot_ns {
            return Err(invalid("batch finished before it started"));
        }
        if let Some(code) = &self.error_code
            && code.len() > 128
        {
            return Err(invalid("error_code exceeds 128 bytes"));
        }
        for record in &self.records {
            record.validate()?;
            if record.provenance.adapter_id != self.adapter_id {
                return Err(invalid("record adapter_id differs from batch"));
            }
            if record.cycle_id != self.cycle_id || record.config_revision != self.config_revision {
                return Err(invalid("record cycle/config differs from batch"));
            }
        }
        Ok(())
    }

    /// Whether more pages of this read group follow.
    pub fn continues(&self) -> bool {
        self.status == BatchStatus::Partial && self.error_code.as_deref() == Some(PAGE_CONTINUES)
    }
}

/// Request to read sources of one read group. Built by the Manager scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadPlan {
    pub request_id: Uuid,
    pub adapter_id: AdapterId,
    pub host_generation: u32,
    pub catalog_revision: u32,
    pub config_revision: u32,
    pub cycle_id: u64,
    pub read_group: ReadGroup,
    /// Catalog slots to read; empty means "all sources of the read group".
    pub source_slots: Vec<u32>,
    pub deadline_boot_ns: u64,
    /// Config epoch the plan belongs to; late replies of older epochs are dropped.
    pub epoch: u64,
    /// Page index for paged read groups (0 = first page).
    pub page: u32,
}

/// Request to (re)discover the adapter's entities, sources and capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverRequest {
    pub request_id: Uuid,
    pub adapter_id: AdapterId,
    pub host_generation: u32,
    pub catalog_revision: u32,
    pub config_revision: u32,
    pub deadline_boot_ns: u64,
    /// Catalog generation the parent already holds (deltas are relative to it).
    pub previous_generation: u32,
    pub page: u32,
}

/// Kind of a discovered entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Host,
    CpuPackage,
    CpuCore,
    Soc,
    SystemMemory,
    Gpu,
    GpuLink,
    BlockDevice,
    Filesystem,
    NetworkInterface,
    PowerDomain,
    Sensor,
    Process,
}

/// Bounded metadata attribute (UUID, driver version, mount point, uid ...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Attribute {
    pub key: String,
    pub value: String,
}

/// Maximum attributes kept per entity: a GPU carries its identity, its
/// firmware and the driver and CUDA stack it runs on (D-48).
pub const MAX_ATTRIBUTES: usize = 32;

/// One discovered entity; metadata is bounded and never a time series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRecord {
    pub entity_id: EntityId,
    pub kind: EntityKind,
    pub parent_id: Option<EntityId>,
    pub generation: u32,
    pub discovered_boot_ns: u64,
    pub removed_boot_ns: Option<u64>,
    /// Sanitized display name (driver/product label).
    pub display_name: String,
    pub attributes: Vec<Attribute>,
}

impl EntityRecord {
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|attribute| attribute.key == key)
            .map(|attribute| attribute.value.as_str())
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.attributes.len() > MAX_ATTRIBUTES {
            return Err(invalid("too many entity attributes"));
        }
        if self.display_name.len() > 256
            || self
                .attributes
                .iter()
                .any(|attribute| attribute.key.len() > 64 || attribute.value.len() > 256)
        {
            return Err(invalid("entity metadata exceeds size limits"));
        }
        Ok(())
    }
}

/// One discovered source channel: identity, origin, units and the MetricKey it
/// is a candidate for. `metric` is `None` when the purpose is unproven; such a
/// source is shown in diagnostics only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDescriptor {
    pub source_id: SourceId,
    pub adapter_id: AdapterId,
    /// Interned catalog slot; never reused for another source in the same catalog.
    pub slot: u32,
    pub generation: u32,
    pub entity_id: EntityId,
    pub device_id: String,
    pub channel: String,
    pub metric: Option<MetricKey>,
    pub origin: OriginIdentity,
    pub driver: String,
    pub api_version: Option<String>,
    pub device_path: String,
    pub native_unit: NativeUnit,
    pub raw_kind: RawKind,
    pub sample_kind: SampleKind,
    pub read_group: ReadGroup,
    pub update_interval_ms: Option<u32>,
    pub profile_id: ProfileId,
    pub profile_revision: u32,
    /// Policy order from the profile (lower first); not a probability.
    pub profile_rank: u16,
    /// Driver label if any (e.g. hwmon `temp1_label`), sanitized.
    pub label: Option<String>,
    /// Counter modulus when the driver declares one (e.g. powercap max_energy_range_uj).
    pub counter_modulus: Option<u64>,
}

/// Capability of one metric/action on one scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub capability_id: String,
    pub entity_id: Option<EntityId>,
    pub support: Support,
    pub reason: Option<String>,
    pub min_interval_ms: Option<u32>,
}

/// One catalog change page from discovery. A page never deletes sources that
/// it did not report; removals are explicit and only committed with
/// `complete == true` together with the generation.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CatalogDelta {
    pub request_id: Option<Uuid>,
    pub adapter_id: Option<AdapterId>,
    pub generation: u32,
    pub availability: Option<Availability>,
    pub entities: Vec<EntityRecord>,
    pub sources: Vec<SourceDescriptor>,
    pub removed_sources: Vec<SourceId>,
    pub removed_entities: Vec<EntityId>,
    pub capabilities: Vec<Capability>,
    pub complete: bool,
    pub next_page: Option<u32>,
    pub error_code: Option<String>,
}

impl CatalogDelta {
    pub fn validate(&self, max_page: usize) -> Result<(), ContractError> {
        let upserts = self.entities.len() + self.sources.len();
        if upserts > max_page.min(MAX_CATALOG_PAGE) {
            return Err(invalid("catalog page exceeds 256 upserts"));
        }
        for entity in &self.entities {
            entity.validate()?;
        }
        if self.complete && self.next_page.is_some() {
            return Err(invalid("a complete catalog cannot announce another page"));
        }
        Ok(())
    }
}

/// Adapter registration (`adapter-manifest.schema.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterManifest {
    pub api_version: u32,
    pub adapter_id: AdapterId,
    pub implementation: String,
    pub isolation: Isolation,
    pub operations: Vec<AdapterOperation>,
    pub max_inflight: u32,
    pub runtime_library: Option<String>,
    pub minimum_sample_ms: u32,
    pub source_limit: u32,
    pub sample_namespace: crate::quality::Namespace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    PersistentProcess,
    InProcessReplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterOperation {
    Discover,
    Read,
    Shutdown,
}

impl AdapterManifest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.api_version != 1 {
            return Err(invalid("adapter api_version must be 1"));
        }
        if self.max_inflight != 1 {
            return Err(invalid("max_inflight must be 1"));
        }
        if !(100..=60_000).contains(&self.minimum_sample_ms) {
            return Err(invalid("minimum_sample_ms must be 100..=60000"));
        }
        if !(1..=2048).contains(&self.source_limit) {
            return Err(invalid("source_limit must be 1..=2048"));
        }
        if self.operations.is_empty() || self.operations.len() > 3 {
            return Err(invalid("operations must list 1..=3 entries"));
        }
        Ok(())
    }
}

/// Read group spellings shared by adapters, scheduler and collectors.
pub mod groups {
    /// 1 s counters/gauges (cpu, memory, diskstats, network, GPU fast, sensors).
    pub const FAST: &str = "fast";
    /// 5 s filesystem capacity.
    pub const CAPACITY: &str = "capacity";
    /// 5 s slow health (GPU ECC, energy, retired pages).
    pub const SLOW_HEALTH: &str = "slow_health";
    /// 1 s paged process resources (procfs) / GPU process lists (nvml).
    pub const PROCESS: &str = "process";
}

/// IPC message kinds (requests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcRequestKind {
    Discover,
    Read,
    Shutdown,
}

/// Parent -> host request (`ipc-request.schema.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcRequest {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub adapter_id: AdapterId,
    pub host_generation: u32,
    #[serde(rename = "type")]
    pub kind: IpcRequestKind,
    #[serde(with = "crate::wire::u64_decimal")]
    pub deadline_boot_ns: u64,
    pub catalog_revision: u32,
    pub config_revision: u32,
    #[serde(with = "crate::wire::u64_decimal")]
    pub cycle_id: u64,
    pub read_group: ReadGroup,
    pub source_slots: Vec<u32>,
}

impl IpcRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.protocol_version != IPC_PROTOCOL_VERSION {
            return Err(invalid("unsupported protocol_version"));
        }
        if self.source_slots.len() > MAX_SOURCE_SLOTS {
            return Err(invalid("more than 256 source slots"));
        }
        if self.kind != IpcRequestKind::Read && !self.source_slots.is_empty() {
            return Err(invalid("discover/shutdown carry no source slots"));
        }
        let mut seen = std::collections::BTreeSet::new();
        if !self.source_slots.iter().all(|slot| seen.insert(*slot)) {
            return Err(invalid("duplicate source slot"));
        }
        if self.catalog_revision == 0 || self.config_revision == 0 {
            return Err(invalid("revisions start at 1"));
        }
        Ok(())
    }

    /// Page index encoded in the read group (`process`, `process:1`, ...).
    pub fn page(&self) -> u32 {
        split_page(self.read_group.as_str()).1
    }
}

/// Splits `group:page` into (`group`, page); a bare group is page 0.
pub fn split_page(read_group: &str) -> (&str, u32) {
    match read_group.rsplit_once(':') {
        Some((group, page)) => match page.parse::<u32>() {
            Ok(index) => (group, index),
            Err(_) => (read_group, 0),
        },
        None => (read_group, 0),
    }
}

/// Encodes `group:page` (page 0 stays the bare group name).
pub fn paged_group(group: &str, page: u32) -> String {
    if page == 0 {
        group.to_owned()
    } else {
        format!("{group}:{page}")
    }
}

/// Host -> parent first frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostHello {
    #[serde(rename = "type")]
    pub kind: HelloTag,
    pub protocol_version: u32,
    pub adapter_id: AdapterId,
    pub generation: u32,
    /// Echo of the parent-provided nonce for this child incarnation.
    pub nonce: String,
    pub manifest: AdapterManifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelloTag {
    Hello,
}

/// Host -> parent reply to `discover`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogReply {
    #[serde(rename = "type")]
    pub kind: CatalogTag,
    pub request_id: Uuid,
    pub generation: u32,
    pub delta: CatalogDelta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogTag {
    Catalog,
}

/// Host -> parent acknowledgement of `shutdown`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownAck {
    #[serde(rename = "type")]
    pub kind: ShutdownTag,
    pub request_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownTag {
    ShutdownAck,
}

/// Health reported by an adapter implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterHealth {
    pub availability: Availability,
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gpu_entity_keeps_up_to_thirty_two_attributes() {
        let record = |count: usize| EntityRecord {
            entity_id: EntityId::new("gpu:GPU-1").unwrap(),
            kind: EntityKind::Gpu,
            parent_id: None,
            generation: 1,
            discovered_boot_ns: 1,
            removed_boot_ns: None,
            display_name: "NVIDIA GB10".to_owned(),
            attributes: (0..count)
                .map(|index| Attribute {
                    key: format!("key_{index}"),
                    value: "value".to_owned(),
                })
                .collect(),
        };
        // Identity, firmware and the driver and CUDA stack (D-48).
        assert!(record(32).validate().is_ok());
        assert!(record(33).validate().is_err());
    }

    #[test]
    fn page_encoding_round_trips() {
        assert_eq!(split_page("process"), ("process", 0));
        assert_eq!(split_page("process:3"), ("process", 3));
        assert_eq!(split_page("weird:x"), ("weird:x", 0));
        assert_eq!(paged_group("process", 0), "process");
        assert_eq!(paged_group("process", 2), "process:2");
    }

    #[test]
    fn spec_observation_example_parses_and_validates() {
        let text = include_str!("../../../spec/dgxtop-v2/examples/observation.cpu-package.json");
        let observation: Observation = serde_json::from_str(text).unwrap();
        observation.validate().unwrap();
        assert_eq!(observation.value.unwrap().to_f64_lossy(), 68000.0);
        let reencoded: serde_json::Value = serde_json::to_value(&observation).unwrap();
        let original: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(reencoded, original);
    }

    #[test]
    fn spec_batch_example_parses_with_null_failure_value() {
        let text = include_str!("../../../spec/dgxtop-v2/examples/observation-batch.partial.json");
        let batch: ObservationBatch = serde_json::from_str(text).unwrap();
        assert_eq!(batch.records.len(), 2);
        assert_eq!(batch.records[1].read_status, ReadStatus::Timeout);
        assert!(batch.records[1].value.is_none());
        for record in &batch.records {
            record.validate().unwrap();
        }
    }

    #[test]
    fn failed_read_with_value_is_rejected() {
        let text = include_str!("../../../spec/dgxtop-v2/examples/observation.cpu-package.json");
        let mut observation: Observation = serde_json::from_str(text).unwrap();
        observation.read_status = ReadStatus::Timeout;
        assert!(observation.validate().is_err());
        observation.read_status = ReadStatus::Ok;
        observation.value = None;
        assert!(observation.validate().is_err());
    }

    #[test]
    fn spec_ipc_request_and_manifest_parse() {
        let request: IpcRequest = serde_json::from_str(include_str!(
            "../../../spec/dgxtop-v2/examples/ipc-request.read.json"
        ))
        .unwrap();
        request.validate().unwrap();
        let manifest: AdapterManifest = serde_json::from_str(include_str!(
            "../../../spec/dgxtop-v2/examples/adapter-manifest.nvml.json"
        ))
        .unwrap();
        manifest.validate().unwrap();
    }

    #[test]
    fn ipc_request_rejects_slots_on_discover_and_duplicates() {
        let mut request: IpcRequest = serde_json::from_str(include_str!(
            "../../../spec/dgxtop-v2/examples/ipc-request.read.json"
        ))
        .unwrap();
        request.source_slots = vec![1, 1];
        assert!(request.validate().is_err());
        request.source_slots = vec![1];
        request.kind = IpcRequestKind::Discover;
        assert!(request.validate().is_err());
        request.source_slots = (0..257).collect();
        request.kind = IpcRequestKind::Read;
        assert!(request.validate().is_err());
    }
}
