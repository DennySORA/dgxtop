//! Wires the live state and the history into one store (D-79).
//!
//! The state owns the history's recorder as its `HistoryIngest` port and
//! calls it inside every commit; the history's index goes to the history
//! workers. Only this composition root knows both concrete crates.

use dgxtop_core::budget::ByteLedger;
use dgxtop_core::config::{RuntimeConfig, SamplingConfig, StoreConfig};
use dgxtop_core::quality::Namespace;
use dgxtop_history::{History, HistoryIndex};
use dgxtop_state::{LiveState, SharedSnapshot, StateWriter};

/// The three handles of one live store, built together: `writer` belongs
/// to the HotStore actor, `snapshots` to presentation and `history` to the
/// history workers.
#[derive(Debug)]
pub struct LiveStore {
    pub writer: StateWriter,
    pub snapshots: SharedSnapshot,
    pub history: HistoryIndex,
}

impl LiveStore {
    /// Builds an empty store (snapshot revision 0) on the shared ledger.
    ///
    /// `namespace` is the only data namespace accepted (live, or replay in
    /// `--replay`); `archive_enabled` makes the history queue finished
    /// minute buckets, bridges and gaps for
    /// [`StateWriter::take_archive_journal`].
    pub fn new(
        store: &StoreConfig,
        sampling: &SamplingConfig,
        runtime: &RuntimeConfig,
        ledger: ByteLedger,
        namespace: Namespace,
        archive_enabled: bool,
    ) -> Self {
        let (recorder, history) =
            History::new(store, sampling, runtime, &ledger, archive_enabled).into_parts();
        let (writer, snapshots) = LiveState::new(
            store,
            sampling,
            ledger,
            namespace,
            archive_enabled,
            Box::new(recorder),
        )
        .into_parts();
        Self {
            writer,
            snapshots,
            history,
        }
    }

    /// Splits the store into its writer, snapshot reader and history reader.
    pub fn into_parts(self) -> (StateWriter, SharedSnapshot, HistoryIndex) {
        (self.writer, self.snapshots, self.history)
    }
}
