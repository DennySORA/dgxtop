//! Catalog publication shared by the sysfs adapters and the replay adapter.
//!
//! Turns an adapter's current catalog into `CatalogDelta` pages relative to
//! the generation the parent last committed:
//!
//! * `previous_generation` equal to the last generation this publisher
//!   completed -> only changed upserts plus explicit removals;
//! * anything else (fresh parent, restarted host, lost commit) -> the full
//!   catalog plus bounded tombstones of sources removed earlier.
//!
//! Page 0 takes a snapshot; later pages serve that snapshot. Removals and
//! `complete == true` only appear on the final page, so an interrupted
//! discovery never deletes anything. The generation only changes when the
//! catalog content changed (or a resynchronisation is needed).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dgxtop_core::identity::{AdapterId, EntityId, SourceId};
use dgxtop_core::observation::{
    Capability, CatalogDelta, DiscoverRequest, EntityRecord, MAX_CATALOG_PAGE, SourceDescriptor,
};
use dgxtop_core::quality::Availability;

/// Tombstones kept for parents that resynchronise after missing removals.
const MAX_TOMBSTONES: usize = 4096;
/// Error code when a later page does not belong to the pending discovery.
pub(crate) const DISCOVERY_RESTART_REQUIRED: &str = "DISCOVERY_RESTART_REQUIRED";

/// The adapter's current catalog handed to [`CatalogPublisher::begin`].
#[derive(Debug, Clone)]
pub(crate) struct CatalogContent {
    pub sources: Vec<SourceDescriptor>,
    pub entities: Vec<EntityRecord>,
    /// Entity ids this adapter owns; only these may ever be removed.
    pub removable_entities: BTreeSet<EntityId>,
    pub capabilities: Vec<Capability>,
    pub availability: Availability,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Committed {
    generation: u32,
    sources: BTreeMap<SourceId, SourceDescriptor>,
    entities: BTreeMap<EntityId, EntityRecord>,
    removable: BTreeSet<EntityId>,
}

#[derive(Debug)]
struct PendingDiscovery {
    previous_generation: u32,
    next_page: u32,
    pages: VecDeque<CatalogDelta>,
    commit: Committed,
}

/// Delta/paging state of one adapter's catalog.
#[derive(Debug)]
pub(crate) struct CatalogPublisher {
    adapter_id: AdapterId,
    page_size: usize,
    committed: Committed,
    retired_sources: VecDeque<SourceId>,
    retired_entities: VecDeque<EntityId>,
    pending: Option<PendingDiscovery>,
}

/// Upserts and removals of one discovery answer.
struct Changes {
    entities: Vec<EntityRecord>,
    sources: Vec<SourceDescriptor>,
    removed_sources: Vec<SourceId>,
    removed_entities: Vec<EntityId>,
}

impl Changes {
    fn is_empty(&self) -> bool {
        self.entities.is_empty()
            && self.sources.is_empty()
            && self.removed_sources.is_empty()
            && self.removed_entities.is_empty()
    }
}

impl CatalogPublisher {
    pub(crate) fn new(adapter_id: AdapterId, page_size: usize) -> Self {
        Self {
            adapter_id,
            page_size: page_size.clamp(1, MAX_CATALOG_PAGE),
            committed: Committed::default(),
            retired_sources: VecDeque::new(),
            retired_entities: VecDeque::new(),
            pending: None,
        }
    }

    /// Whether the parent still holds `source_id` (removal not yet committed).
    pub(crate) fn holds(&self, source_id: &SourceId) -> bool {
        self.committed.sources.contains_key(source_id)
    }

    /// Answers page 0: snapshots `content` and returns the first page.
    pub(crate) fn begin(
        &mut self,
        request: &DiscoverRequest,
        content: CatalogContent,
    ) -> CatalogDelta {
        let sources: BTreeMap<SourceId, SourceDescriptor> = content
            .sources
            .into_iter()
            .map(|descriptor| (descriptor.source_id.clone(), descriptor))
            .collect();
        let entities: BTreeMap<EntityId, EntityRecord> = content
            .entities
            .into_iter()
            .map(|record| (record.entity_id.clone(), record))
            .collect();
        let in_sync = self.committed.generation != 0
            && request.previous_generation == self.committed.generation;
        let changes = if in_sync {
            self.delta_changes(&sources, &entities)
        } else {
            self.full_changes(&sources, &entities)
        };
        let generation = if in_sync && changes.is_empty() {
            self.committed.generation
        } else {
            let base = self.committed.generation.max(request.previous_generation);
            base.checked_add(1).unwrap_or(base)
        };
        let commit = Committed {
            generation,
            sources,
            entities,
            removable: content.removable_entities,
        };
        let pages = self.paginate(
            request,
            generation,
            changes,
            content.capabilities,
            content.availability,
            content.error_code,
        );
        self.pending = Some(PendingDiscovery {
            previous_generation: request.previous_generation,
            next_page: 0,
            pages,
            commit,
        });
        self.next(request)
    }

    /// Serves the next page of the pending discovery.
    pub(crate) fn next(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        let matches = self.pending.as_ref().is_some_and(|pending| {
            pending.next_page == request.page
                && pending.previous_generation == request.previous_generation
        });
        if !matches {
            return self.failure(request, DISCOVERY_RESTART_REQUIRED, None);
        }
        let Some(mut pending) = self.pending.take() else {
            return self.failure(request, DISCOVERY_RESTART_REQUIRED, None);
        };
        let Some(page) = pending.pages.pop_front() else {
            return self.failure(request, DISCOVERY_RESTART_REQUIRED, None);
        };
        if page.complete {
            self.commit(pending.commit);
        } else {
            pending.next_page = pending.next_page.saturating_add(1);
            self.pending = Some(pending);
        }
        page
    }

    /// A page reporting that discovery could not run; nothing changes.
    pub(crate) fn failure(
        &self,
        request: &DiscoverRequest,
        error_code: &str,
        availability: Option<Availability>,
    ) -> CatalogDelta {
        CatalogDelta {
            request_id: Some(request.request_id),
            adapter_id: Some(self.adapter_id.clone()),
            generation: self.committed.generation,
            availability,
            complete: false,
            next_page: None,
            error_code: Some(error_code.to_owned()),
            ..CatalogDelta::default()
        }
    }

    fn delta_changes(
        &self,
        sources: &BTreeMap<SourceId, SourceDescriptor>,
        entities: &BTreeMap<EntityId, EntityRecord>,
    ) -> Changes {
        Changes {
            entities: entities
                .iter()
                .filter(|(id, record)| self.committed.entities.get(*id) != Some(*record))
                .map(|(_, record)| record.clone())
                .collect(),
            sources: sources
                .iter()
                .filter(|(id, descriptor)| self.committed.sources.get(*id) != Some(*descriptor))
                .map(|(_, descriptor)| descriptor.clone())
                .collect(),
            removed_sources: self
                .committed
                .sources
                .keys()
                .filter(|id| !sources.contains_key(*id))
                .cloned()
                .collect(),
            removed_entities: self
                .committed
                .removable
                .iter()
                .filter(|id| !entities.contains_key(*id))
                .cloned()
                .collect(),
        }
    }

    fn full_changes(
        &self,
        sources: &BTreeMap<SourceId, SourceDescriptor>,
        entities: &BTreeMap<EntityId, EntityRecord>,
    ) -> Changes {
        let removed_sources: BTreeSet<SourceId> = self
            .committed
            .sources
            .keys()
            .chain(self.retired_sources.iter())
            .filter(|id| !sources.contains_key(*id))
            .cloned()
            .collect();
        let removed_entities: BTreeSet<EntityId> = self
            .committed
            .removable
            .iter()
            .chain(self.retired_entities.iter())
            .filter(|id| !entities.contains_key(*id))
            .cloned()
            .collect();
        Changes {
            entities: entities.values().cloned().collect(),
            sources: sources.values().cloned().collect(),
            removed_sources: removed_sources.into_iter().collect(),
            removed_entities: removed_entities.into_iter().collect(),
        }
    }

    fn paginate(
        &self,
        request: &DiscoverRequest,
        generation: u32,
        changes: Changes,
        capabilities: Vec<Capability>,
        availability: Availability,
        error_code: Option<String>,
    ) -> VecDeque<CatalogDelta> {
        let size = self.page_size;
        let upserts = changes.entities.len() + changes.sources.len();
        let page_count = upserts
            .div_ceil(size)
            .max(capabilities.len().div_ceil(size))
            .max(1);
        let mut entities = changes.entities.into_iter();
        let mut sources = changes.sources.into_iter();
        let mut capabilities = capabilities.into_iter();
        let mut removed_sources = Some(changes.removed_sources);
        let mut removed_entities = Some(changes.removed_entities);
        let mut pages = VecDeque::with_capacity(page_count);
        for index in 0..page_count {
            let page_entities: Vec<EntityRecord> = entities.by_ref().take(size).collect();
            let room = size - page_entities.len();
            let page_sources: Vec<SourceDescriptor> = sources.by_ref().take(room).collect();
            let complete = index + 1 == page_count;
            pages.push_back(CatalogDelta {
                request_id: Some(request.request_id),
                adapter_id: Some(self.adapter_id.clone()),
                generation,
                availability: Some(availability),
                entities: page_entities,
                sources: page_sources,
                removed_sources: if complete {
                    removed_sources.take().unwrap_or_default()
                } else {
                    Vec::new()
                },
                removed_entities: if complete {
                    removed_entities.take().unwrap_or_default()
                } else {
                    Vec::new()
                },
                capabilities: capabilities.by_ref().take(size).collect(),
                complete,
                next_page: if complete {
                    None
                } else {
                    u32::try_from(index + 1).ok()
                },
                error_code: error_code.clone(),
            });
        }
        pages
    }

    fn commit(&mut self, commit: Committed) {
        for id in self.committed.sources.keys() {
            if !commit.sources.contains_key(id) {
                push_bounded(&mut self.retired_sources, id.clone());
            }
        }
        for id in &self.committed.removable {
            if !commit.entities.contains_key(id) {
                push_bounded(&mut self.retired_entities, id.clone());
            }
        }
        self.retired_sources
            .retain(|id| !commit.sources.contains_key(id));
        self.retired_entities
            .retain(|id| !commit.entities.contains_key(id));
        self.committed = commit;
    }
}

fn push_bounded<T: PartialEq>(queue: &mut VecDeque<T>, value: T) {
    if queue.contains(&value) {
        return;
    }
    if queue.len() >= MAX_TOMBSTONES {
        queue.pop_front();
    }
    queue.push_back(value);
}
