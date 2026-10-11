//! Shared handles over the history state: the single writer used by the
//! state actor (through the recorder) and the cloneable reader used by
//! history workers (`OverviewReader`, D-82).

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use dgxtop_core::canonical::GapRecord;

use super::grid::MinuteGrid;
use super::output::HistoryReport;
use super::state::{HistoryAppends, HistoryState};
use crate::overview::OverviewTiers;

/// Read side of the history (`OverviewReader`). Cheap to clone; safe to
/// read from several history workers while the state keeps committing. A
/// read holds the state's lock only while it cuts one series' minutes not
/// finalized yet.
#[derive(Clone)]
pub struct HistoryIndex {
    shared: Arc<RwLock<HistoryState>>,
    overview: OverviewTiers,
}

impl fmt::Debug for HistoryIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoryIndex")
            .field("overview", &self.overview)
            .finish_non_exhaustive()
    }
}

impl HistoryIndex {
    pub(crate) fn new(shared: Arc<RwLock<HistoryState>>, overview: OverviewTiers) -> Self {
        Self { shared, overview }
    }

    /// The Overview series in every size (D-82): composition sets its local
    /// clock and attaches its store; the readers ask it.
    pub fn overview(&self) -> &OverviewTiers {
        &self.overview
    }

    pub(crate) fn read(&self) -> RwLockReadGuard<'_, HistoryState> {
        self.shared.read().unwrap_or_else(PoisonError::into_inner)
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

    pub(crate) fn apply(&self, appends: HistoryAppends, now_ns: u64) -> HistoryReport {
        self.write().apply(appends, now_ns)
    }

    pub(crate) fn maintain(&self, now_ns: u64) -> HistoryReport {
        self.write().maintain(now_ns)
    }

    /// Records a gap; the report carries it when it became visible.
    pub(crate) fn record_gap(&self, gap: GapRecord) -> HistoryReport {
        self.write().record_external_gap(gap)
    }

    /// Called only from the recorder's first boot-to-UTC pairing (the
    /// recorder owns this writer), so minutes are never finished before
    /// the wall clock can place them.
    pub(crate) fn set_minute_grid(&self, grid: MinuteGrid) {
        self.write().set_minute_grid(grid);
    }

    pub(crate) fn set_finalize_grace(&self, grace_ns: u64) {
        self.write().set_finalize_grace(grace_ns);
    }
}
