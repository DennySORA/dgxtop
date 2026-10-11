//! System part of the collectors (D-79): one collector per device domain
//! (CPU, memory, disk/filesystem, network, GPU).

pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod memory;
pub mod network;

pub use cpu::CpuCollector;
pub use disk::DiskCollector;
pub use gpu::GpuCollector;
pub use memory::MemoryCollector;
pub use network::NetworkCollector;
