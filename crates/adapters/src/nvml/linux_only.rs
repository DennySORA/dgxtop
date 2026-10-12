//! The NVML queries nvml-wrapper builds only for Linux, the one platform
//! whose NVML has them. On macOS, where dgxtop builds for development, they
//! answer `NotSupported`, as a GPU that lacks a query does.

pub(super) use platform::{
    common_ancestor, cpu_affinity, memory_affinity, minor_number, persistence_mode,
};

#[cfg(target_os = "linux")]
mod platform {
    use nvml_wrapper::Device;
    use nvml_wrapper::enum_wrappers::device::TopologyLevel;
    use nvml_wrapper::error::NvmlError;

    pub(crate) fn minor_number(device: &Device<'_>) -> Result<u32, NvmlError> {
        device.minor_number()
    }

    pub(crate) fn persistence_mode(device: &Device<'_>) -> Result<bool, NvmlError> {
        device.is_in_persistent_mode()
    }

    pub(crate) fn cpu_affinity(device: &Device<'_>, words: usize) -> Result<Vec<u64>, NvmlError> {
        device.cpu_affinity(words)
    }

    pub(crate) fn memory_affinity(
        device: &Device<'_>,
        words: usize,
        scope: u32,
    ) -> Result<Vec<u64>, NvmlError> {
        device.memory_affinity(words, scope)
    }

    pub(crate) fn common_ancestor<'nvml>(
        device: &Device<'nvml>,
        other: Device<'nvml>,
    ) -> Result<TopologyLevel, NvmlError> {
        device.topology_common_ancestor(other)
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use nvml_wrapper::Device;
    use nvml_wrapper::enum_wrappers::device::TopologyLevel;
    use nvml_wrapper::error::NvmlError;

    pub(crate) fn minor_number(_device: &Device<'_>) -> Result<u32, NvmlError> {
        Err(NvmlError::NotSupported)
    }

    pub(crate) fn persistence_mode(_device: &Device<'_>) -> Result<bool, NvmlError> {
        Err(NvmlError::NotSupported)
    }

    pub(crate) fn cpu_affinity(_device: &Device<'_>, _words: usize) -> Result<Vec<u64>, NvmlError> {
        Err(NvmlError::NotSupported)
    }

    pub(crate) fn memory_affinity(
        _device: &Device<'_>,
        _words: usize,
        _scope: u32,
    ) -> Result<Vec<u64>, NvmlError> {
        Err(NvmlError::NotSupported)
    }

    pub(crate) fn common_ancestor<'nvml>(
        _device: &Device<'nvml>,
        _other: Device<'nvml>,
    ) -> Result<TopologyLevel, NvmlError> {
        Err(NvmlError::NotSupported)
    }
}
