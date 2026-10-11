//! What the process actor publishes (D-79): its read model, swapped whole
//! like the state's snapshot so a reader never waits on a sample, and the
//! process memory record, which a reader asks for the parts of a chart's
//! exact window (the window moves with every history refresh, so the
//! parts cannot be laid out before it is known).

use std::collections::BTreeSet;
use std::sync::{Arc, PoisonError, RwLock};

use dgxtop_core::identity::EntityId;
use dgxtop_core::ports::{ProcessHistoryReader, ProcessMemoryReader};
use dgxtop_core::process_model::{MemoryPart, ProcessHistoryModel};

use super::memory::ProcessMemoryBook;

/// The readers' side of one process actor; cheap to clone.
#[derive(Debug, Clone)]
pub struct PublishedProcessHistory {
    model: Arc<RwLock<Arc<ProcessHistoryModel>>>,
    memory: Arc<RwLock<ProcessMemoryBook>>,
}

impl PublishedProcessHistory {
    pub(super) fn new(memory: Arc<RwLock<ProcessMemoryBook>>) -> Self {
        Self {
            model: Arc::new(RwLock::new(Arc::new(ProcessHistoryModel::default()))),
            memory,
        }
    }

    pub(super) fn publish(&self, model: ProcessHistoryModel) {
        let retired = {
            let mut latest = self.model.write().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *latest, Arc::new(model))
        };
        // The retired model is dropped outside the lock.
        drop(retired);
    }
}

impl ProcessHistoryReader for PublishedProcessHistory {
    fn latest(&self) -> Arc<ProcessHistoryModel> {
        Arc::clone(&self.model.read().unwrap_or_else(PoisonError::into_inner))
    }
}

impl ProcessMemoryReader for PublishedProcessHistory {
    fn parts(
        &self,
        gpus: &BTreeSet<EntityId>,
        window: (u64, u64),
        cells: usize,
    ) -> Vec<MemoryPart> {
        self.memory
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .parts(gpus, window, cells)
    }
}
