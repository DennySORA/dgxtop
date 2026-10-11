//! Archive capacity, batching and timing limits (`STORAGE.md` §6 policy
//! table).

use std::time::Duration;

use dgxtop_core::config::Config;
use thiserror::Error;

use crate::sqlite::CapacityPolicy;

const MIB: u64 = 1024 * 1024;
const ASSUMED_PAGE_SIZE: u64 = 4096;
const MIN_DATABASE_PAGES: u64 = 64;
/// WAL headroom required per `batch_bytes` (payload, indexes and slack).
const WAL_HEADROOM_BATCHES: u64 = 4;

/// Budget, batching and deadlines of the optional history archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    /// db + wal + shm hard budget (default 1 GiB); the oldest data is
    /// evicted first when an append would exceed it.
    pub limit_bytes: u64,
    /// Soft retention target (default 7 days).
    pub retention_days: u32,
    /// WAL size that triggers a PASSIVE checkpoint (default 16 MiB).
    pub wal_checkpoint_bytes: u64,
    /// WAL backlog after a checkpoint attempt that pauses archive writes
    /// (default 32 MiB).
    pub wal_stop_bytes: u64,
    /// Rows per transaction (default 512).
    pub batch_rows: u32,
    /// Estimated payload bytes per transaction (default 1 MiB).
    pub batch_bytes: u64,
    /// Free-disk low-water mark (default 64 MiB).
    pub min_free_disk_bytes: u64,
    /// Total time one batch may wait for the write lock (default 500 ms).
    pub busy_deadline_ms: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            limit_bytes: 1024 * MIB,
            retention_days: 7,
            wal_checkpoint_bytes: 16 * MIB,
            wal_stop_bytes: 32 * MIB,
            batch_rows: 512,
            batch_bytes: MIB,
            min_free_disk_bytes: 64 * MIB,
            busy_deadline_ms: 500,
        }
    }
}

/// Invalid arguments to [`crate::archive::SqliteArchive::open`]. Database
/// problems open an archive whose status is `Failed` (a file that is merely
/// not this build's is recreated instead, D-85).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ArchiveOpenError {
    #[error("invalid archive limits: {0}")]
    InvalidLimits(String),
    #[error("invalid archive database path {path}: {reason}")]
    InvalidPath { path: String, reason: String },
}

impl ArchiveLimits {
    /// Limits from `store.archive_limit_mib` and `store.archive_retention_days`;
    /// WAL thresholds shrink with small budgets (at most a quarter is WAL).
    pub fn from_config(config: &Config) -> Self {
        let defaults = Self::default();
        let limit_bytes = u64::from(config.store.archive_limit_mib).saturating_mul(MIB);
        let wal_stop_bytes = defaults.wal_stop_bytes.min(limit_bytes / 4);
        Self {
            limit_bytes,
            retention_days: config.store.archive_retention_days,
            wal_checkpoint_bytes: defaults.wal_checkpoint_bytes.min(wal_stop_bytes / 2),
            wal_stop_bytes,
            ..defaults
        }
    }

    pub fn validate(&self) -> Result<(), ArchiveOpenError> {
        let invalid = |message: &str| Err(ArchiveOpenError::InvalidLimits(message.to_owned()));
        if self.retention_days == 0 {
            return invalid("retention_days must be at least 1");
        }
        if self.batch_rows == 0 || self.batch_bytes == 0 {
            return invalid("batch_rows and batch_bytes must be positive");
        }
        if self.busy_deadline_ms == 0 {
            return invalid("busy_deadline_ms must be at least 1");
        }
        if self.wal_checkpoint_bytes == 0 || self.wal_checkpoint_bytes >= self.wal_stop_bytes {
            return invalid("wal_checkpoint_bytes must be positive and below wal_stop_bytes");
        }
        // One transaction (payload plus index growth) must fit between the
        // checkpoint and stop thresholds, or it could never be admitted.
        let headroom = self.wal_stop_bytes - self.wal_checkpoint_bytes;
        if headroom < self.batch_bytes.saturating_mul(WAL_HEADROOM_BATCHES) {
            return invalid("wal_stop_bytes - wal_checkpoint_bytes must hold 4 x batch_bytes");
        }
        if self.capacity_policy().database_page_cap(ASSUMED_PAGE_SIZE) < MIN_DATABASE_PAGES {
            return invalid("limit_bytes leaves too little room next to the WAL budget");
        }
        Ok(())
    }

    pub fn busy_deadline(&self) -> Duration {
        Duration::from_millis(self.busy_deadline_ms)
    }

    pub(crate) fn capacity_policy(&self) -> CapacityPolicy {
        CapacityPolicy {
            limit_bytes: self.limit_bytes,
            reserve_bytes: 0,
            wal_checkpoint_bytes: self.wal_checkpoint_bytes,
            wal_stop_bytes: self.wal_stop_bytes,
            min_free_disk_bytes: self.min_free_disk_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_storage_policy_table() {
        let limits = ArchiveLimits::default();
        assert_eq!(limits.limit_bytes, 1024 * MIB);
        assert_eq!(limits.retention_days, 7);
        assert_eq!(limits.wal_checkpoint_bytes, 16 * MIB);
        assert_eq!(limits.wal_stop_bytes, 32 * MIB);
        assert_eq!(limits.batch_rows, 512);
        assert_eq!(limits.batch_bytes, MIB);
        assert_eq!(limits.min_free_disk_bytes, 64 * MIB);
        assert_eq!(limits.busy_deadline(), Duration::from_millis(500));
        assert_eq!(limits.validate(), Ok(()));
    }

    #[test]
    fn inconsistent_limits_are_rejected() {
        let base = ArchiveLimits::default();
        for case in [
            ArchiveLimits {
                retention_days: 0,
                ..base
            },
            ArchiveLimits {
                batch_rows: 0,
                ..base
            },
            ArchiveLimits {
                batch_bytes: 0,
                ..base
            },
            ArchiveLimits {
                busy_deadline_ms: 0,
                ..base
            },
            ArchiveLimits {
                wal_checkpoint_bytes: base.wal_stop_bytes,
                ..base
            },
            ArchiveLimits {
                limit_bytes: base.wal_stop_bytes,
                ..base
            },
            ArchiveLimits {
                batch_bytes: 8 * MIB,
                ..base
            },
        ] {
            assert!(case.validate().is_err(), "{case:?}");
        }
    }
}
