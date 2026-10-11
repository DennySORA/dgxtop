//! The history of the GPU processes (D-64, D-76, D-77, D-79): every GPU
//! process followed as a run ([`runs`]) and what each GPU process held on
//! each GPU over the last day ([`memory`]), owned by one actor
//! ([`ProcessHistory`]) that the composition root runs on its own thread.
//!
//! The Manager hands it the committed process tables, the commits between
//! them, the signals dgxtop sent and the runs its sessions show
//! (`ProcessHistoryInput`); the probe and the stores answer it directly.
//! It publishes a bounded read model ([`PublishedProcessHistory`]): the
//! runs with their whole-life figures, the details and bars of the runs
//! asked for, and the processes holding a color. Its stores are reached
//! through the `ProcessHistoryStore` actor's `Dispatch` port; nothing here
//! performs I/O.

mod actor;
mod memory;
mod model;
mod published;
mod runs;
mod sample;

pub use self::actor::{ProcessHistory, ProcessHistoryPorts, ProcessHistorySettings};
pub use self::published::PublishedProcessHistory;

/// The boot and wall clock when an input is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Now {
    pub(crate) boot_ns: u64,
    pub(crate) utc_ns: i128,
}
