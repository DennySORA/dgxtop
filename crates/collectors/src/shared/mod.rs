//! Helpers shared by the system and process collectors (D-79): the
//! collection session, source arbitration, exact counter derivation, the
//! per-GPU grouping of NVML process records and the value/output plumbing
//! every collector uses.

pub(crate) mod arbitration;
pub(crate) mod counter;
pub(crate) mod reasons;

pub(crate) mod context;
pub(crate) mod counter_inputs;
pub(crate) mod formulas;
pub(crate) mod gpu_processes;
pub(crate) mod output;
pub(crate) mod state_bounds;
pub(crate) mod values;

use dgxtop_core::identity::AdapterId;
use dgxtop_core::naming;
use dgxtop_core::observation::split_page;

/// Whether batches of `(adapter_id, read_group)` feed `collector`
/// (page suffixes and replay mirrors resolved by `naming::collectors_for`).
pub(crate) fn routes_to(adapter_id: &AdapterId, read_group: &str, collector: &str) -> bool {
    let (base_group, _) = split_page(read_group);
    naming::collectors_for(adapter_id.as_str(), base_group).contains(&collector)
}
