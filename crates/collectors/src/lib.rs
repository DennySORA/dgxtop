//! # dgxtop-collectors
//!
//! Pure domain collectors of dgxtop v2 (ARCHITECTURE.md §5): normalization,
//! validation, origin de-duplication, source arbitration, exact counter
//! derivation and the six domain collectors (CPU, GPU, memory, disk/
//! filesystem, network, process).
//!
//! Collectors implement [`dgxtop_core::ports::Collector`]: they read only the
//! `CollectionPlan` (admitted adapter batches, catalog, policy and the
//! committed state), perform no I/O, start no threads, read no clock other
//! than `plan.now_boot_ns`, and return a `CollectionBatch` whose `next_state`
//! becomes the delta base only after the store acknowledges the commit.

#![forbid(unsafe_code)]

pub mod arbitration;
pub mod counter;
pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod memory;
pub mod network;
pub mod process;
pub mod reasons;
pub mod registry;

mod context;
mod counter_inputs;
mod formulas;
mod gpu_processes;
mod output;
mod state_bounds;
mod values;

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

pub use cpu::CpuCollector;
pub use disk::DiskCollector;
pub use gpu::GpuCollector;
pub use memory::MemoryCollector;
pub use network::NetworkCollector;
pub use process::ProcessCollector;
pub use registry::{collector_for, default_collectors};

use dgxtop_core::identity::AdapterId;
use dgxtop_core::naming;
use dgxtop_core::observation::split_page;

/// Whether batches of `(adapter_id, read_group)` feed `collector`
/// (page suffixes and replay mirrors resolved by `naming::collectors_for`).
pub(crate) fn routes_to(adapter_id: &AdapterId, read_group: &str, collector: &str) -> bool {
    let (base_group, _) = split_page(read_group);
    naming::collectors_for(adapter_id.as_str(), base_group).contains(&collector)
}
