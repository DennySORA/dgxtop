//! GPU identities and display labels. A GPU is identified by its
//! `gpu:<UUID>` entity; the NVML index only orders and labels it.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::canonical::CatalogSnapshot;
use dgxtop_core::identity::EntityId;
use dgxtop_core::naming::{attributes, entity};
use dgxtop_core::observation::EntityRecord;

use super::index::SnapshotLookup;
use super::labels::label_text;

/// GPUs kept per projection (DGX-class hosts have at most 16).
pub(crate) const MAX_GPUS: usize = 64;
const GPU_ENTITY_PREFIX: &str = "gpu:";
const SHORT_UUID_CHARS: usize = 8;
/// `memory_model` attribute value of a GPU that allocates from system RAM.
const UNIFIED_MEMORY_MODEL: &str = "unified";

pub(crate) struct GpuIdentity<'a> {
    pub(crate) entity_id: EntityId,
    pub(crate) label: String,
    pub(crate) record: Option<&'a EntityRecord>,
    nvml_index: Option<u32>,
}

impl GpuIdentity<'_> {
    /// The adapter reports this GPU's memory model as unified: it allocates
    /// from system RAM and has no framebuffer of its own.
    pub(crate) fn has_unified_memory(&self) -> bool {
        self.record.is_some_and(is_unified_memory)
    }
}

/// The adapter reports the GPU of `record` as allocating from system RAM.
fn is_unified_memory(record: &EntityRecord) -> bool {
    record
        .attribute(attributes::MEMORY_MODEL)
        .is_some_and(|model| model.trim().eq_ignore_ascii_case(UNIFIED_MEMORY_MODEL))
}

pub(crate) struct GpuDirectory<'a> {
    gpus: Vec<GpuIdentity<'a>>,
}

impl<'a> GpuDirectory<'a> {
    /// GPUs known to the catalog or reporting samples, ordered by NVML
    /// index (unknown index last), then by entity id.
    pub(crate) fn new(catalog: &'a CatalogSnapshot, index: &SnapshotLookup<'a>) -> Self {
        let mut records: BTreeMap<EntityId, Option<&'a EntityRecord>> = BTreeMap::new();
        for (entity_id, record) in &catalog.entities {
            if is_gpu_entity(entity_id.as_str()) {
                records.insert(entity_id.clone(), Some(record.as_ref()));
            }
        }
        for entity_id in index.entities_with_prefix(GPU_ENTITY_PREFIX) {
            if is_gpu_entity(entity_id.as_str()) {
                records.entry(entity_id.clone()).or_insert(None);
            }
        }
        let mut gpus: Vec<GpuIdentity<'a>> = records
            .into_iter()
            .map(|(entity_id, record)| identity(entity_id, record))
            .collect();
        gpus.sort_by(|left, right| {
            left.nvml_index
                .is_none()
                .cmp(&right.nvml_index.is_none())
                .then(left.nvml_index.cmp(&right.nvml_index))
                .then_with(|| left.entity_id.cmp(&right.entity_id))
        });
        gpus.truncate(MAX_GPUS);
        Self { gpus }
    }

    pub(crate) fn gpus(&self) -> &[GpuIdentity<'a>] {
        &self.gpus
    }

    pub(crate) fn contains(&self, entity_id: &EntityId) -> bool {
        self.gpus.iter().any(|gpu| &gpu.entity_id == entity_id)
    }

    /// Display label of a GPU; GPUs that vanished keep a UUID-based label.
    pub(crate) fn label_of(&self, entity_id: &EntityId) -> String {
        self.gpus
            .iter()
            .find(|gpu| &gpu.entity_id == entity_id)
            .map_or_else(|| uuid_label(entity_id.as_str()), |gpu| gpu.label.clone())
    }

    /// The GPUs that allocate from system RAM (D-36): the one place this
    /// set is made.
    pub(crate) fn unified_gpus(&self) -> BTreeSet<EntityId> {
        self.gpus
            .iter()
            .filter(|gpu| gpu.has_unified_memory())
            .map(|gpu| gpu.entity_id.clone())
            .collect()
    }

    pub(crate) fn choices(&self) -> Vec<(EntityId, String)> {
        self.gpus
            .iter()
            .map(|gpu| (gpu.entity_id.clone(), gpu.label.clone()))
            .collect()
    }
}

pub(crate) fn is_gpu_entity(entity_id: &str) -> bool {
    entity::parse_gpu(entity_id).is_some_and(|uuid| !uuid.is_empty())
}

fn identity(entity_id: EntityId, record: Option<&EntityRecord>) -> GpuIdentity<'_> {
    let nvml_index = record
        .and_then(|record| record.attribute(attributes::NVML_INDEX))
        .and_then(|index| index.trim().parse::<u32>().ok());
    let label = nvml_index.map_or_else(
        || uuid_label(entity_id.as_str()),
        |index| format!("GPU {index}"),
    );
    GpuIdentity {
        entity_id,
        label,
        record,
        nvml_index,
    }
}

/// "GPU 1a2b3c4d" from `gpu:GPU-1a2b3c4d-...` when no NVML index is known.
fn uuid_label(entity_id: &str) -> String {
    let uuid = entity::parse_gpu(entity_id).unwrap_or(entity_id);
    let short: String = uuid
        .strip_prefix("GPU-")
        .unwrap_or(uuid)
        .chars()
        .take(SHORT_UUID_CHARS)
        .collect();
    label_text(&format!("GPU {short}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, entity_record, sample};
    use dgxtop_core::observation::EntityKind;

    fn directory_fixture() -> Fixture {
        let mut fixture = Fixture::new();
        fixture.add_entity(entity_record(
            "gpu:GPU-bbbb2222",
            EntityKind::Gpu,
            "NVIDIA H100",
            &[(attributes::NVML_INDEX, "1")],
        ));
        fixture.add_entity(entity_record(
            "gpu:GPU-aaaa1111",
            EntityKind::Gpu,
            "NVIDIA H100",
            &[(attributes::NVML_INDEX, "0")],
        ));
        fixture.add(
            "gpu",
            sample("gpu.temperature", "gpu:GPU-cccc3333-x", "gpu").f64(40.0),
        );
        fixture.add(
            "gpu",
            sample("gpu.temperature", "gpupid:GPU-a:7", "gpu").f64(40.0),
        );
        fixture
    }

    #[test]
    fn gpus_are_ordered_by_nvml_index_but_identified_by_uuid() {
        let fixture = directory_fixture();
        let index = SnapshotLookup::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        let labels: Vec<(&str, &str)> = directory
            .gpus()
            .iter()
            .map(|gpu| (gpu.entity_id.as_str(), gpu.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("gpu:GPU-aaaa1111", "GPU 0"),
                ("gpu:GPU-bbbb2222", "GPU 1"),
                ("gpu:GPU-cccc3333-x", "GPU cccc3333"),
            ]
        );
        assert!(directory.contains(&EntityId::new("gpu:GPU-bbbb2222").unwrap()));
        assert_eq!(
            directory.label_of(&EntityId::new("gpu:GPU-dddd4444").unwrap()),
            "GPU dddd4444"
        );
        assert_eq!(directory.choices().len(), 3);
    }
}
