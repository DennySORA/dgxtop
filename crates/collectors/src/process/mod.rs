//! Process part of the collectors (D-79): the process table with its GPU
//! process links and the GPU processes' descendants (from the tree of
//! every scanned process), the per-GPU grouping of NVML process records
//! that the GPU collector also sums, and the values the probe readings of
//! the followed GPU processes give their runs.

pub(crate) mod gpu_processes;
pub mod probe;
pub mod table;
pub(crate) mod tree;

pub use probe::ProcessProbeDeriver;
pub use table::ProcessCollector;
