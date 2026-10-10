//! Free disk space probe for the 64 MiB low-water mark (`STORAGE.md` §6).
//!
//! The probe runs on the I/O worker that owns the database and is not a
//! substitute for handling `ENOSPC`/`SQLITE_FULL`; it only refuses new
//! intents and archive rows early, while outcomes keep working.

use std::fmt;
use std::path::Path;

use dgxtop_core::errors::InfraError;

/// Bytes available to unprivileged writers on the filesystem of `path`.
pub trait FreeSpaceProbe: Send + Sync + fmt::Debug {
    fn available_bytes(&self, path: &Path) -> Result<u64, InfraError>;
}

/// `statvfs(2)`: `f_bavail * f_frsize`.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatvfsFreeSpace;

impl FreeSpaceProbe for StatvfsFreeSpace {
    fn available_bytes(&self, path: &Path) -> Result<u64, InfraError> {
        let stats = nix::sys::statvfs::statvfs(path).map_err(|errno| InfraError::Io {
            operation: "statvfs",
            message: errno.desc().to_owned(),
        })?;
        Ok(available(stats.blocks_available(), stats.fragment_size()))
    }
}

// `fsblkcnt_t` and `c_ulong` are already u64 on the 64-bit GNU targets but
// narrower elsewhere, so the conversions are identity on some platforms.
#[allow(clippy::useless_conversion)]
fn available(blocks: libc::fsblkcnt_t, fragment_size: libc::c_ulong) -> u64 {
    u64::from(blocks).saturating_mul(u64::from(fragment_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statvfs_reports_space_for_an_existing_directory() {
        let bytes = StatvfsFreeSpace
            .available_bytes(&std::env::temp_dir())
            .unwrap();
        assert!(bytes > 0);
    }

    #[test]
    fn statvfs_failure_is_an_infra_error() {
        let error = StatvfsFreeSpace
            .available_bytes(Path::new("/nonexistent/dgxtop/probe"))
            .unwrap_err();
        assert!(matches!(
            error,
            InfraError::Io {
                operation: "statvfs",
                ..
            }
        ));
    }
}
