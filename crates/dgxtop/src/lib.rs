//! # dgxtop (composition root)
//!
//! Assembles the crates of the D-79 architecture: the adapter hosts, the
//! collectors, the live state, the history (with its compression), the
//! stores, the Manager, the runtime and the terminal UI (Viewer and
//! Controller). Only this crate depends on every concrete implementation.

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
