//! Where a GPU sits (D-54): the CPUs and NUMA nodes nearest it, and the
//! PCIe path and direct peer (P2P) access to each other GPU, read once per
//! generation like the other static facts. A fact the driver cannot answer
//! stays unknown; only a lost library session aborts.

use dgxtop_core::gpu_state::GpuPeer;

use super::backend::{BackendError, BackendResult, GpuInventory};

/// Other GPUs a device's peer list covers (the attribute holds about 20).
pub const MAX_PEERS: usize = 16;
/// Bits of one affinity mask word.
const WORD_BITS: usize = 64;

/// The topology of one device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceTopology {
    /// The nearest CPUs as a Linux CPU list (`0-31`).
    pub cpus: Option<String>,
    /// The NUMA nodes nearest its memory (`0`).
    pub numa_nodes: Option<String>,
    /// The other GPUs, in enumeration order, at most [`MAX_PEERS`].
    pub peers: Vec<GpuPeer>,
}

impl DeviceTopology {
    /// Reads the topology of `uuid` among `devices` (`(index, uuid)`
    /// pairs, `uuid` among them).
    pub fn read(
        backend: &dyn GpuInventory,
        uuid: &str,
        devices: &[(u32, String)],
    ) -> Result<Self, BackendError> {
        let mut peers = Vec::new();
        for (index, peer) in devices
            .iter()
            .filter(|(_, peer)| peer != uuid)
            .take(MAX_PEERS)
        {
            peers.push(GpuPeer {
                index: *index,
                link: known(backend.peer_link(uuid, peer))?,
                access: known(backend.peer_access(uuid, peer))?,
            });
        }
        Ok(Self {
            cpus: known(backend.cpu_affinity(uuid))?.and_then(|mask| mask_list(&mask)),
            numa_nodes: known(backend.numa_affinity(uuid))?.and_then(|mask| mask_list(&mask)),
            peers,
        })
    }
}

/// The value, `None` for a fact the driver cannot answer, or the error of
/// a lost library session.
fn known<T>(result: BackendResult<T>) -> Result<Option<T>, BackendError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(BackendError::Uninitialized) => Err(BackendError::Uninitialized),
        Err(_) => Ok(None),
    }
}

/// The set bits of `mask` (bit `n` of the words is item `n`) as a Linux
/// list (`0-31,64-95`); `None` when no bit is set.
pub fn mask_list(mask: &[u64]) -> Option<String> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (word_index, word) in mask.iter().enumerate() {
        for bit in 0..WORD_BITS {
            if word & (1 << bit) == 0 {
                continue;
            }
            let item = word_index * WORD_BITS + bit;
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == item => *end = item,
                _ => ranges.push((item, item)),
            }
        }
    }
    if ranges.is_empty() {
        return None;
    }
    let parts: Vec<String> = ranges
        .into_iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect();
    Some(parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_become_linux_lists() {
        assert_eq!(mask_list(&[0xffff_ffff, 0, 0]).as_deref(), Some("0-31"));
        assert_eq!(mask_list(&[0b1]).as_deref(), Some("0"));
        // A range across a word boundary, a lone bit, the top bit.
        assert_eq!(
            mask_list(&[0xc000_0000_0000_0000, 0b1001, 1 << 63]).as_deref(),
            Some("62-64,67,191")
        );
        assert_eq!(mask_list(&[0, 0]), None);
        assert_eq!(mask_list(&[]), None);
    }
}
