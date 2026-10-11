//! # dgxtop-collectors
//!
//! Pure domain collectors of dgxtop v2 (ARCHITECTURE.md §5): normalization,
//! validation, origin de-duplication, source arbitration, exact counter
//! derivation and the six domain collectors, in two parts (D-79):
//! [`system`] (CPU, GPU, memory, disk/filesystem, network) and [`process`]
//! (the process table with its GPU links and descendants, and the probe
//! deriver of the followed GPU processes), over helpers in a shared module.
//!
//! Collectors implement [`dgxtop_core::ports::Collector`]: they read only the
//! `CollectionPlan` (admitted adapter batches, catalog, policy and the
//! committed state), perform no I/O, start no threads, read no clock other
//! than `plan.now_boot_ns`, and return a `CollectionBatch` whose `next_state`
//! becomes the delta base only after the state acknowledges the commit
//! (`commit_collection`).

#![forbid(unsafe_code)]

pub mod process;
pub mod registry;
pub mod system;

mod shared;

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

pub use process::ProcessCollector;
pub use registry::{collector_for, default_collectors};
pub use system::{CpuCollector, DiskCollector, GpuCollector, MemoryCollector, NetworkCollector};
