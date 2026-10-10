//! Retained-size estimates of decoded IPC messages.
//!
//! The estimate counts inline struct sizes, vector capacities, string
//! capacities and the reference-count header of every interned `Arc<str>`
//! identity. It is what an ingress permit is shrunk to once the wire buffer
//! is gone, and what the 4 MiB decoded budget is checked against.

use std::mem::size_of;

use dgxtop_core::identity::{MetricKey, OriginIdentity};
use dgxtop_core::observation::{
    AdapterManifest, Attribute, Capability, CatalogDelta, CatalogReply, EntityRecord, HostHello,
    IpcRequest, Observation, ObservationBatch, Provenance, ShutdownAck, SourceDescriptor,
};

/// Strong + weak counters stored in front of every `Arc<str>` payload.
const ARC_HEADER_BYTES: u64 = 16;

/// Estimated heap + inline bytes retained by a decoded value.
pub trait DecodedSize {
    fn decoded_bytes(&self) -> u64;
}

fn bytes_of(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

fn arc_text(text: &str) -> u64 {
    ARC_HEADER_BYTES.saturating_add(bytes_of(text.len()))
}

fn owned_text(text: &String) -> u64 {
    bytes_of(text.capacity())
}

fn optional_text(text: &Option<String>) -> u64 {
    text.as_ref().map_or(0, owned_text)
}

fn vector_inline<T>(items: &Vec<T>) -> u64 {
    bytes_of(items.capacity()).saturating_mul(bytes_of(size_of::<T>()))
}

fn metric_key_heap(key: &MetricKey) -> u64 {
    [
        arc_text(key.host_id.as_str()),
        arc_text(key.boot_id.as_str()),
        arc_text(key.entity_id.as_str()),
        arc_text(key.metric_id.as_str()),
        arc_text(key.scope.as_str()),
    ]
    .iter()
    .fold(0u64, |sum, part| sum.saturating_add(*part))
}

fn origin_heap(origin: &OriginIdentity) -> u64 {
    origin
        .origin_id
        .as_ref()
        .map_or(0, |id| arc_text(id.as_str()))
}

fn provenance_heap(provenance: &Provenance) -> u64 {
    [
        arc_text(provenance.adapter_id.as_str()),
        arc_text(provenance.source_id.as_str()),
        provenance
            .origin_id
            .as_ref()
            .map_or(0, |id| arc_text(id.as_str())),
        arc_text(provenance.driver.as_str()),
        arc_text(provenance.device_path.as_str()),
        arc_text(provenance.channel.as_str()),
        arc_text(provenance.profile_id.as_str()),
    ]
    .iter()
    .fold(0u64, |sum, part| sum.saturating_add(*part))
}

fn observation_heap(record: &Observation) -> u64 {
    let reason_codes = record
        .reason_codes
        .iter()
        .fold(vector_inline(&record.reason_codes), |sum, code| {
            sum.saturating_add(owned_text(code))
        });
    metric_key_heap(&record.key)
        .saturating_add(provenance_heap(&record.provenance))
        .saturating_add(bytes_of(record.native_unit.as_str().len()))
        .saturating_add(reason_codes)
}

impl DecodedSize for ObservationBatch {
    fn decoded_bytes(&self) -> u64 {
        let records = self
            .records
            .iter()
            .fold(vector_inline(&self.records), |sum, record| {
                sum.saturating_add(observation_heap(record))
            });
        bytes_of(size_of::<Self>())
            .saturating_add(arc_text(self.adapter_id.as_str()))
            .saturating_add(arc_text(self.read_group.as_str()))
            .saturating_add(optional_text(&self.error_code))
            .saturating_add(records)
    }
}

fn entity_heap(entity: &EntityRecord) -> u64 {
    let attributes = entity
        .attributes
        .iter()
        .fold(vector_inline(&entity.attributes), |sum, attribute| {
            sum.saturating_add(attribute_heap(attribute))
        });
    arc_text(entity.entity_id.as_str())
        .saturating_add(
            entity
                .parent_id
                .as_ref()
                .map_or(0, |id| arc_text(id.as_str())),
        )
        .saturating_add(owned_text(&entity.display_name))
        .saturating_add(attributes)
}

fn attribute_heap(attribute: &Attribute) -> u64 {
    owned_text(&attribute.key).saturating_add(owned_text(&attribute.value))
}

fn source_heap(source: &SourceDescriptor) -> u64 {
    [
        arc_text(source.source_id.as_str()),
        arc_text(source.adapter_id.as_str()),
        arc_text(source.entity_id.as_str()),
        owned_text(&source.device_id),
        owned_text(&source.channel),
        source.metric.as_ref().map_or(0, metric_key_heap),
        origin_heap(&source.origin),
        owned_text(&source.driver),
        optional_text(&source.api_version),
        owned_text(&source.device_path),
        bytes_of(source.native_unit.as_str().len()),
        arc_text(source.read_group.as_str()),
        arc_text(source.profile_id.as_str()),
        optional_text(&source.label),
    ]
    .iter()
    .fold(0u64, |sum, part| sum.saturating_add(*part))
}

fn capability_heap(capability: &Capability) -> u64 {
    owned_text(&capability.capability_id)
        .saturating_add(
            capability
                .entity_id
                .as_ref()
                .map_or(0, |id| arc_text(id.as_str())),
        )
        .saturating_add(optional_text(&capability.reason))
}

impl DecodedSize for CatalogDelta {
    fn decoded_bytes(&self) -> u64 {
        let entities = self
            .entities
            .iter()
            .fold(vector_inline(&self.entities), |sum, entity| {
                sum.saturating_add(entity_heap(entity))
            });
        let sources = self
            .sources
            .iter()
            .fold(vector_inline(&self.sources), |sum, source| {
                sum.saturating_add(source_heap(source))
            });
        let removed_sources = self
            .removed_sources
            .iter()
            .fold(vector_inline(&self.removed_sources), |sum, id| {
                sum.saturating_add(arc_text(id.as_str()))
            });
        let removed_entities = self
            .removed_entities
            .iter()
            .fold(vector_inline(&self.removed_entities), |sum, id| {
                sum.saturating_add(arc_text(id.as_str()))
            });
        let capabilities = self
            .capabilities
            .iter()
            .fold(vector_inline(&self.capabilities), |sum, capability| {
                sum.saturating_add(capability_heap(capability))
            });
        bytes_of(size_of::<Self>())
            .saturating_add(
                self.adapter_id
                    .as_ref()
                    .map_or(0, |id| arc_text(id.as_str())),
            )
            .saturating_add(optional_text(&self.error_code))
            .saturating_add(entities)
            .saturating_add(sources)
            .saturating_add(removed_sources)
            .saturating_add(removed_entities)
            .saturating_add(capabilities)
    }
}

impl DecodedSize for CatalogReply {
    fn decoded_bytes(&self) -> u64 {
        bytes_of(size_of::<Self>() - size_of::<CatalogDelta>())
            .saturating_add(self.delta.decoded_bytes())
    }
}

fn manifest_heap(manifest: &AdapterManifest) -> u64 {
    arc_text(manifest.adapter_id.as_str())
        .saturating_add(owned_text(&manifest.implementation))
        .saturating_add(vector_inline(&manifest.operations))
        .saturating_add(optional_text(&manifest.runtime_library))
}

impl DecodedSize for HostHello {
    fn decoded_bytes(&self) -> u64 {
        bytes_of(size_of::<Self>())
            .saturating_add(arc_text(self.adapter_id.as_str()))
            .saturating_add(owned_text(&self.nonce))
            .saturating_add(manifest_heap(&self.manifest))
    }
}

impl DecodedSize for ShutdownAck {
    fn decoded_bytes(&self) -> u64 {
        bytes_of(size_of::<Self>())
    }
}

impl DecodedSize for IpcRequest {
    fn decoded_bytes(&self) -> u64 {
        bytes_of(size_of::<Self>())
            .saturating_add(arc_text(self.adapter_id.as_str()))
            .saturating_add(arc_text(self.read_group.as_str()))
            .saturating_add(vector_inline(&self.source_slots))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::{ObservationBuilder, batch_of, metric_key};

    fn batch(records: usize, reason_codes: usize) -> ObservationBatch {
        let records = (0..records)
            .map(|index| {
                let mut builder = ObservationBuilder::new(
                    metric_key("gpu0", "gpu.temperature", "device"),
                    "nvml",
                    &format!("nvml:gpu0:temp{index}"),
                )
                .f64(41.5);
                for code in 0..reason_codes {
                    builder = builder.reason(&format!("R{code}"));
                }
                builder.build()
            })
            .collect();
        batch_of("nvml", "fast", 9, records)
    }

    #[test]
    fn decoded_estimate_stays_within_three_times_the_wire_size() {
        // The supervisor reserves `3 x wire` of decoded credit; realistic
        // batches (short identities, up to 32 reason codes) must fit.
        for (records, codes) in [(1, 0), (16, 0), (256, 0), (64, 32), (256, 4)] {
            let value = batch(records, codes);
            let wire = u64::try_from(serde_json::to_vec(&value).unwrap().len()).unwrap();
            let decoded = value.decoded_bytes();
            assert!(
                decoded <= wire * 3,
                "{records} records/{codes} codes: decoded {decoded} > 3 x wire {wire}"
            );
        }
    }
}
