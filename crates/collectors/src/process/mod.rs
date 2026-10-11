//! Process part of the collectors (D-79): the process table with its GPU
//! process links and the GPU processes' descendants (from the tree of
//! every scanned process), and the values the probe readings of the
//! followed GPU processes give their runs.

pub mod probe;
pub mod table;
pub(crate) mod tree;

pub use probe::ProcessProbeDeriver;
pub use table::ProcessCollector;
