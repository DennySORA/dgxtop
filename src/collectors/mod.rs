pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod gpu_process;
pub mod memory;
pub mod network;

use crate::error::Result;

/// Trait for all system metric collectors.
/// Each collector reads from a specific data source and produces domain-level stats.
pub trait Collector {
    type Output;

    /// Collect the current snapshot of metrics.
    fn collect(&mut self) -> Result<Self::Output>;

    /// Whether this collector's data source is available on this system.
    fn is_available(&self) -> bool;
}
