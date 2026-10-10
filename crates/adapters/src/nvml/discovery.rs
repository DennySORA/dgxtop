//! Catalog delta pages. Upserts (entities, then sources) are paged at 256 per
//! page; removals and capabilities travel on the final page together with
//! `complete = true`, so a failed or half-delivered discovery never deletes
//! sources the parent still holds.

use std::collections::{BTreeSet, VecDeque};

use dgxtop_core::identity::{EntityId, IdentityError, SourceId};
use dgxtop_core::observation::{
    CatalogDelta, DiscoverRequest, EntityRecord, MAX_CATALOG_PAGE, SourceDescriptor,
};
use dgxtop_core::quality::Availability;

use super::capabilities::CapabilityTracker;
use super::catalog::{
    PublishedSources, device_capabilities, entity_record, unavailable_capabilities,
};
use super::inventory::Inventory;

/// Removed source ids remembered for parents that missed a generation.
pub const MAX_SOURCE_TOMBSTONES: usize = 2048;
pub const MAX_ENTITY_TOMBSTONES: usize = 128;

/// Bounded memory of removed sources/entities.
#[derive(Debug, Clone, Default)]
pub struct Tombstones {
    sources: VecDeque<SourceId>,
    entities: VecDeque<EntityId>,
}

impl Tombstones {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records removals; the oldest entries fall off beyond the bounds.
    pub fn bury(&mut self, sources: Vec<SourceId>, entities: Vec<EntityId>) {
        for source in sources {
            if !self.sources.contains(&source) {
                self.sources.push_back(source);
            }
        }
        for entity in entities {
            if !self.entities.contains(&entity) {
                self.entities.push_back(entity);
            }
        }
        while self.sources.len() > MAX_SOURCE_TOMBSTONES {
            self.sources.pop_front();
        }
        while self.entities.len() > MAX_ENTITY_TOMBSTONES {
            self.entities.pop_front();
        }
    }

    /// Drops tombstones of ids that are live again (a re-attached GPU).
    pub fn revive(
        &mut self,
        live_sources: &BTreeSet<SourceId>,
        live_entities: &BTreeSet<EntityId>,
    ) {
        self.sources.retain(|source| !live_sources.contains(source));
        self.entities
            .retain(|entity| !live_entities.contains(entity));
    }

    pub fn sources(&self) -> Vec<SourceId> {
        self.sources.iter().cloned().collect()
    }

    pub fn entities(&self) -> Vec<EntityId> {
        self.entities.iter().cloned().collect()
    }
}

/// Everything a catalog page is built from.
pub struct CatalogView<'a> {
    pub inventory: &'a Inventory,
    pub published: &'a PublishedSources,
    pub tracker: &'a CapabilityTracker,
    pub tombstones: &'a Tombstones,
}

/// One page of an available catalog. Upserts are sent when the parent's
/// generation differs from ours (or it asks for a later page).
pub fn available_page(
    view: &CatalogView<'_>,
    request: &DiscoverRequest,
    error_code: Option<String>,
) -> Result<CatalogDelta, IdentityError> {
    let generation = view.inventory.generation();
    let send_upserts = request.previous_generation != generation || request.page > 0;
    let (entities, sources) = if send_upserts {
        (
            entity_upserts(view)?,
            view.published.descriptors().collect(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let total = entities.len().saturating_add(sources.len());
    let page = usize::try_from(request.page).unwrap_or(usize::MAX);
    let start = page.saturating_mul(MAX_CATALOG_PAGE).min(total);
    let end = start.saturating_add(MAX_CATALOG_PAGE).min(total);
    let complete = end >= total;
    let mut delta = CatalogDelta {
        request_id: Some(request.request_id),
        adapter_id: Some(request.adapter_id.clone()),
        generation,
        availability: Some(Availability::Available),
        complete,
        next_page: (!complete).then(|| request.page.saturating_add(1)),
        error_code,
        ..CatalogDelta::default()
    };
    let entity_count = entities.len();
    delta.entities = entities
        .into_iter()
        .take(end.min(entity_count))
        .skip(start.min(entity_count))
        .collect();
    let source_start = start.saturating_sub(entity_count);
    let source_end = end.saturating_sub(entity_count);
    delta.sources = sources
        .into_iter()
        .take(source_end)
        .skip(source_start)
        .cloned()
        .collect::<Vec<SourceDescriptor>>();
    if complete {
        delta.removed_sources = view.tombstones.sources();
        delta.removed_entities = view.tombstones.entities();
        delta.capabilities = device_capabilities(view.inventory, view.tracker)?;
    }
    Ok(delta)
}

fn entity_upserts(view: &CatalogView<'_>) -> Result<Vec<EntityRecord>, IdentityError> {
    view.inventory
        .devices()
        .map(|device| {
            entity_record(
                device,
                view.inventory.software(),
                view.inventory.generation(),
            )
        })
        .collect()
}

/// Complete catalog page while NVML is unavailable: every capability is
/// unsupported with the reason, and previously known devices are removed.
pub fn unavailable_page(
    request: &DiscoverRequest,
    generation: u32,
    availability: Availability,
    reason: &str,
    tombstones: &Tombstones,
) -> CatalogDelta {
    CatalogDelta {
        request_id: Some(request.request_id),
        adapter_id: Some(request.adapter_id.clone()),
        generation,
        availability: Some(availability),
        removed_sources: tombstones.sources(),
        removed_entities: tombstones.entities(),
        capabilities: unavailable_capabilities(reason),
        complete: true,
        error_code: Some(reason.to_owned()),
        ..CatalogDelta::default()
    }
}

/// Incomplete page for a failed discovery step: nothing is removed.
pub fn failed_page(
    request: &DiscoverRequest,
    generation: u32,
    availability: Availability,
    reason: &str,
) -> CatalogDelta {
    CatalogDelta {
        request_id: Some(request.request_id),
        adapter_id: Some(request.adapter_id.clone()),
        generation,
        availability: Some(availability),
        complete: false,
        error_code: Some(reason.to_owned()),
        ..CatalogDelta::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::source_id;

    #[test]
    fn tombstones_are_bounded_unique_and_revivable() {
        let mut tombstones = Tombstones::new();
        let sources: Vec<SourceId> = (0..3000)
            .map(|index| source_id(&format!("s{index}")))
            .collect();
        tombstones.bury(sources.clone(), Vec::new());
        tombstones.bury(vec![sources[2999].clone()], Vec::new());
        assert_eq!(tombstones.sources().len(), MAX_SOURCE_TOMBSTONES);
        let live: BTreeSet<SourceId> = [sources[2999].clone()].into_iter().collect();
        tombstones.revive(&live, &BTreeSet::new());
        assert!(!tombstones.sources().contains(&sources[2999]));
        assert_eq!(tombstones.sources().len(), MAX_SOURCE_TOMBSTONES - 1);
    }
}
