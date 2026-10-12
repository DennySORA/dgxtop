//! # dgxtop-history
//!
//! The history of dgxtop (D-79): it takes what the state committed, keeps
//! each series' recent steps in memory, decides what is saved and for how
//! long, answers the Manager's reads and follows GPU process runs (D-76).
//! The compression is `dgxtop-compression`'s; the SQLite files are the
//! store's, reached through ports.
//!
//! * Ingest: the state feeds it through the `HistoryIngest` port
//!   ([`HistoryRecorder`]) inside each commit, so the history never lags
//!   the snapshot. Per series (`MetricKey`) raw gauge
//!   steps and exact counter intervals are kept apart in fixed-width
//!   chunked logs (`raw_history_seconds`), segments are cut on
//!   source/generation/policy/kind changes and gaps merged into one index.
//! * Minutes: once the finalization horizon passes, minute rollups
//!   (covered time, time-weighted sum, min/max, sample count, exact `u128`
//!   counter totals) and the counter bridges of intervals crossing a minute
//!   boundary are cut from the raw steps and handed on as finished rows, to
//!   the Overview's sizes and to the archive (`ArchiveJournal`s drained from
//!   the recorder); the series keeps none of them (D-90).
//! * Memory: raw steps live in the `raw_history` partition, segments and
//!   gaps in `aggregates`; a full partition evicts its oldest data (raw
//!   already handed on first, else raw by boot time before its minutes are
//!   known, recorded as an `evicted` gap; the oldest gaps of the aggregates,
//!   reported as truncation).
//! * Overview sizes ([`OverviewTiers`], D-82): every Overview series in
//!   every bar size, fed by the same finished minutes, kept on disk through
//!   the store port and compressed from the archive at startup, so the
//!   Overview has no age limit from the archive.
//! * Reads: the Manager reads through `OverviewReader` ([`HistoryIndex`]):
//!   the bars and window statistics of the Overview's sizes, the minutes not
//!   finalized yet cut from the raw steps, each chart's window totals and,
//!   for a memory chart, the tracked processes' parts (D-99).
//! * Processes ([`ProcessHistory`]): the process actor keeps the GPU
//!   processes' runs and memory; the Manager hands it the committed process
//!   tables with the snapshot and catalog they came with and reads the model
//!   it publishes ([`PublishedProcessHistory`]) through
//!   `ProcessHistoryReader`; the history's readers join its
//!   `ProcessMemoryReader` record with the memory charts' bars (D-99).

#![forbid(unsafe_code)]

mod archive;
mod archive_expiry;
pub(crate) mod chunk;
pub(crate) mod gaps;
pub(crate) mod grid;
mod index;
pub(crate) mod output;
mod overview;
mod process;
mod recorder;
pub(crate) mod records;
pub(crate) mod rollup;
pub(crate) mod segment;
pub(crate) mod series;
mod settings;
pub(crate) mod slots;
pub(crate) mod state;
pub(crate) mod steps;
mod wall_clock;

use std::sync::{Arc, RwLock};

use dgxtop_core::budget::ByteLedger;
use dgxtop_core::config::{SamplingConfig, StoreConfig};
use dgxtop_core::sampling::SamplingTiming;

pub use index::HistoryIndex;
pub(crate) use index::HistoryWriter;
pub use overview::OverviewTiers;
pub use process::{
    ProcessHistory, ProcessHistoryPorts, ProcessHistorySettings, PublishedProcessHistory,
};
pub use recorder::HistoryRecorder;

use archive::ArchiveQueue;
use archive_expiry::ArchiveExpiryPolicy;
use state::HistoryState;

/// The two handles of one history, built together: `recorder` goes to the
/// live state (as its `HistoryIngest`), `index` to the history workers.
#[derive(Debug)]
pub struct History {
    pub recorder: HistoryRecorder,
    pub index: HistoryIndex,
}

impl History {
    /// Builds an empty history on the shared ledger. `archive_enabled`
    /// makes it queue finished minute buckets, bridges and gaps for
    /// [`dgxtop_core::ports::HistoryIngest::take_archive_journal`].
    pub fn new(
        store: &StoreConfig,
        sampling: &SamplingConfig,
        ledger: &ByteLedger,
        archive_enabled: bool,
    ) -> Self {
        let timing = SamplingTiming::from_sampling(sampling);
        let settings = settings::history_settings(store, &timing, archive_enabled);
        let shared = Arc::new(RwLock::new(HistoryState::new(settings, ledger)));
        let overview = OverviewTiers::new();
        let index = HistoryIndex::new(Arc::clone(&shared), overview.clone());
        let recorder = HistoryRecorder::new(
            HistoryWriter::new(shared),
            ArchiveQueue::new(ledger),
            ArchiveExpiryPolicy::new(store.archive_retention_days),
            archive_enabled,
            overview,
        );
        Self { recorder, index }
    }

    /// Splits the history into its recorder and its reader.
    pub fn into_parts(self) -> (HistoryRecorder, HistoryIndex) {
        (self.recorder, self.index)
    }
}
