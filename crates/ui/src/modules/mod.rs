//! The built-in display modules. Each renders exactly one core view type
//! from the immutable projection; a missing topic renders "no data" text.

mod common;
mod cpu;
mod disk;
pub(crate) mod folds;
mod gpu;
mod history;
mod host_status;
mod memory;
mod network;
mod process_memory;
mod processes;
mod settings;

use dgxtop_core::identity::IdentityError;
use thiserror::Error;

pub use cpu::{CPU_MODULE_ID, CpuModule};
pub use disk::DiskModule;
pub use gpu::{GPU_MODULE_ID, GpuModule};
pub use history::HISTORY_MODULE_ID;
pub use history::HistoryModule;
pub(crate) use history::history_gpus;
pub use memory::MEMORY_MODULE_ID;
pub use memory::MemoryModule;
pub use network::NetworkModule;
pub use processes::ProcessesModule;
pub use settings::SettingsModule;

use crate::registry::{DisplayModule, ManifestError};

/// A built-in module whose static manifest failed validation (a build defect).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModuleBuildError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

fn boxed<M: DisplayModule + 'static>(module: M) -> Box<dyn DisplayModule> {
    Box::new(module)
}

/// Constructs the built-ins in `config::BUILTIN_MODULES` order.
pub fn builtin_modules() -> Vec<Result<Box<dyn DisplayModule>, ModuleBuildError>> {
    vec![
        CpuModule::new().map(boxed),
        GpuModule::new().map(boxed),
        MemoryModule::new().map(boxed),
        DiskModule::new().map(boxed),
        NetworkModule::new().map(boxed),
        ProcessesModule::new().map(boxed),
        HistoryModule::new().map(boxed),
        SettingsModule::new().map(boxed),
    ]
}
