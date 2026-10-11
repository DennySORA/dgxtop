//! # dgxtop-history
//!
//! The history of dgxtop (D-79): it takes what the state committed, keeps
//! each series' recent samples and minutes in memory, decides what is
//! saved and for how long, answers the Manager's history reads, and
//! follows GPU process runs (D-76). Compression is the compression
//! component's; the SQLite files are the store's, reached through ports.
//!
//! Bounded canonical history (`STORAGE.md` §2–4). Per series (`MetricKey`):
//! raw gauge steps and exact counter intervals kept apart in fixed-width
//! chunked logs (`raw_history_seconds`), minute rollups derived from them
//! once the finalization horizon passes (covered time, time-weighted sum,
//! min/max, sample count, exact `u128` counter totals), counter bridges for
//! intervals crossing a minute boundary, segments cut on
//! source/generation/policy/kind changes, and a merged gap index. Raw
//! history lives in the `raw_history` partition, rollups/bridges/segments/
//! gaps in the `aggregates` partition; when a partition is full the oldest
//! data is evicted (raw first, then rollups are shortened) and the
//! truncation is recorded as an `evicted` gap.
//!
//! The state feeds it through the `HistoryIngest` port ([`HistoryRecorder`])
//! inside each commit, so history never lags the snapshot; the Manager reads
//! it through `HistoryReader` and `OverviewReader` ([`HistoryIndex`]); the
//! archive receives the finished rows as `ArchiveJournal`s drained from the
//! recorder.
//!
//! The Overview series are also kept in every bar size ([`OverviewTiers`],
//! D-82), fed by the same finished minutes, kept on disk through the store
//! port and compressed from the archive at startup, so the Overview has no
//! age limit from the archive.
//!
//! The GPU processes' runs and memory are its process actor's
//! ([`ProcessHistory`]): the Manager hands it the committed process tables
//! and reads the model it publishes ([`PublishedProcessHistory`]) through
//! `ProcessHistoryReader` and `ProcessMemoryReader`.

#![forbid(unsafe_code)]

mod archive;
pub(crate) mod chunk;
pub(crate) mod gaps;
pub(crate) mod grid;
mod index;
pub(crate) mod output;
mod overview;
pub(crate) mod pin;
mod process;
mod query;
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
use dgxtop_core::config::{RuntimeConfig, SamplingConfig, StoreConfig};
use dgxtop_core::sampling::SamplingTiming;

pub use index::HistoryIndex;
pub(crate) use index::HistoryWriter;
pub use overview::OverviewTiers;
pub use process::{
    ProcessHistory, ProcessHistoryPorts, ProcessHistorySettings, PublishedProcessHistory,
};
pub use recorder::HistoryRecorder;

use archive::ArchiveQueue;
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
        runtime: &RuntimeConfig,
        ledger: &ByteLedger,
        archive_enabled: bool,
    ) -> Self {
        let timing = SamplingTiming::from_sampling(sampling);
        let settings = settings::history_settings(store, &timing, archive_enabled);
        let shared = Arc::new(RwLock::new(HistoryState::new(settings, ledger)));
        let overview = OverviewTiers::new();
        let index = HistoryIndex::new(
            Arc::clone(&shared),
            ledger.clone(),
            settings::query_limits(store, runtime),
            overview.clone(),
        );
        let recorder = HistoryRecorder::new(
            HistoryWriter::new(shared),
            ArchiveQueue::new(ledger),
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
