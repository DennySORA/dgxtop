//! Shared handles over the history state: the single writer used by the
//! state actor (through the recorder) and the cloneable reader used by
//! history workers.

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use dgxtop_core::budget::ByteLedger;
use dgxtop_core::canonical::GapRecord;

use super::output::HistoryReport;
use super::state::{HistoryAppends, HistoryState};
use crate::query::QueryLimits;

/// Read side of the bounded history (`HistoryReader`). Cheap to clone; safe
/// to query from several history workers while the state keeps
/// committing. A query holds the lock only while pinning one series.
#[derive(Clone)]
pub struct HistoryIndex {
    shared: Arc<RwLock<HistoryState>>,
    ledger: ByteLedger,
    limits: QueryLimits,
}

impl fmt::Debug for HistoryIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoryIndex")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl HistoryIndex {
    pub(crate) fn new(
        shared: Arc<RwLock<HistoryState>>,
        ledger: ByteLedger,
        limits: QueryLimits,
    ) -> Self {
        Self {
            shared,
            ledger,
            limits,
        }
    }

    pub(crate) fn read(&self) -> RwLockReadGuard<'_, HistoryState> {
        self.shared.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn ledger(&self) -> &ByteLedger {
        &self.ledger
    }

    pub(crate) fn limits(&self) -> &QueryLimits {
        &self.limits
    }
}

/// Write side of the history, owned by the recorder on the state actor.
pub(crate) struct HistoryWriter {
    shared: Arc<RwLock<HistoryState>>,
}

impl HistoryWriter {
    pub(crate) fn new(shared: Arc<RwLock<HistoryState>>) -> Self {
        Self { shared }
    }

    fn write(&self) -> RwLockWriteGuard<'_, HistoryState> {
        self.shared.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn apply(
        &self,
        appends: HistoryAppends,
        now_ns: u64,
        revision: u64,
    ) -> HistoryReport {
        self.write().apply(appends, now_ns, revision)
    }

    pub(crate) fn maintain(&self, now_ns: u64) -> HistoryReport {
        self.write().maintain(now_ns)
    }

    /// Records a gap; the report carries it when it became visible.
    pub(crate) fn record_gap(&self, gap: GapRecord) -> HistoryReport {
        self.write().record_external_gap(gap)
    }

    pub(crate) fn shed_raw(&self, now_ns: u64, keep_ns: u64) -> HistoryReport {
        self.write().shed_raw(now_ns, keep_ns)
    }

    pub(crate) fn note_discontinuity(&self, boot_ns: u64) {
        self.write().note_discontinuity(boot_ns);
    }

    pub(crate) fn set_finalize_grace(&self, grace_ns: u64) {
        self.write().set_finalize_grace(grace_ns);
    }

    pub(crate) fn set_revision(&self, revision: u64) {
        self.write().set_revision(revision);
    }
}
