//! # dgxtop (composition root)
//!
//! Assembles the six components of the v2 architecture (spec/dgxtop-v2/):
//! adapter hosts, collectors, the Manager, the stores, the Viewer and the
//! Controller. Only this crate depends on every concrete implementation.

pub mod audit_unavailable;
pub mod cli;
pub mod collection_ops;
pub mod control_ops;
pub mod diagnostics;
pub mod fail_stop;
pub mod host_mode;
pub mod live_store;
pub mod modes;
pub mod monitor;
#[cfg(any(test, feature = "bench-support"))]
pub mod perf;
#[cfg(test)]
mod provenance;
pub mod replay_unavailable;
pub mod startup;
pub mod state_actor;
pub mod version;
pub mod worker_queues;
