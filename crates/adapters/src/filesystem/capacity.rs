//! Capacity probing behind a small trait: the real probe calls statvfs on
//! `root.join(mount point)`; tests inject fakes.

use std::io;
use std::path::{Path, PathBuf};

/// Raw statvfs figures of one mounted filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityFigures {
    /// `f_blocks`: size in fragment units.
    pub blocks: u64,
    /// `f_bfree`: free fragments (including root-reserved ones).
    pub blocks_free: u64,
    /// `f_bavail`: fragments available to unprivileged users.
    pub blocks_available: u64,
    /// `f_frsize`: fragment size in bytes.
    pub fragment_size: u64,
}

/// Reads the capacity of a mounted filesystem.
pub trait CapacityProbe: Send {
    /// Capacity of the filesystem mounted at `mount_point` (absolute path as
    /// written in mountinfo).
    fn capacity(&self, mount_point: &str) -> io::Result<CapacityFigures>;
}

/// statvfs(2) under a configured root (`/` in production).
#[derive(Debug, Clone)]
pub struct StatvfsProbe {
    root: PathBuf,
}

impl StatvfsProbe {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn host() -> Self {
        Self::new("/")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl CapacityProbe for StatvfsProbe {
    // `fsblkcnt_t` and `c_ulong` are `u64` on 64-bit Linux but narrower on
    // some targets, so the widening conversions are kept explicit.
    #[allow(clippy::useless_conversion)]
    fn capacity(&self, mount_point: &str) -> io::Result<CapacityFigures> {
        let path = self.root.join(mount_point.trim_start_matches('/'));
        let stat = nix::sys::statvfs::statvfs(&path).map_err(io::Error::from)?;
        Ok(CapacityFigures {
            blocks: u64::from(stat.blocks()),
            blocks_free: u64::from(stat.blocks_free()),
            blocks_available: u64::from(stat.blocks_available()),
            fragment_size: u64::from(stat.fragment_size()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statvfs_of_the_root_filesystem_is_consistent() {
        let figures = StatvfsProbe::host().capacity("/").unwrap();
        assert!(figures.fragment_size > 0);
        assert!(figures.blocks_free <= figures.blocks);
        assert!(figures.blocks_available <= figures.blocks_free);
    }

    #[test]
    fn missing_mount_points_fail_with_not_found() {
        let error = StatvfsProbe::new("/nonexistent-dgxtop-root")
            .capacity("/mnt")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
