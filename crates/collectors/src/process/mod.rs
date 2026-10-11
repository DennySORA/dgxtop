//! Process part of the collectors (D-79): the process table with its GPU
//! process links, and the per-GPU grouping of NVML process records that the
//! GPU collector also sums.

pub(crate) mod gpu_processes;
pub mod table;

pub use table::ProcessCollector;
