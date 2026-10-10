//! Audit capacity and timing limits (`STORAGE.md` §6 policy table).

use std::time::Duration;

use dgxtop_core::config::Config;
use thiserror::Error;

use crate::sqlite::CapacityPolicy;

const MIB: u64 = 1024 * 1024;
/// Page size assumed when validating limits (SQLite's default).
const ASSUMED_PAGE_SIZE: u64 = 4096;
/// Smallest main-file cap that still holds the schema and some history.
const MIN_DATABASE_PAGES: u64 = 64;
/// WAL room one intent transaction needs between the two WAL thresholds.
const MIN_WAL_HEADROOM_BYTES: u64 = 32 * 1024;

/// Budget and deadlines of the audit partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditLimits {
    /// db + wal + shm hard budget (default 128 MiB).
    pub limit_bytes: u64,
    /// Rows older than this may be deleted; younger rows never are.
    pub retention_days: u32,
    /// WAL size that triggers a PASSIVE checkpoint (default 8 MiB).
    pub wal_checkpoint_bytes: u64,
    /// WAL backlog after a checkpoint attempt that stops new intents
    /// (default 16 MiB).
    pub wal_stop_bytes: u64,
    /// Free-disk low-water mark for new intents (default 64 MiB).
    pub min_free_disk_bytes: u64,
    /// Total time an intent may wait for the write lock (default 1000 ms).
    pub busy_deadline_ms: u64,
    /// Budget kept for outcomes of committed intents (default 1 MiB).
    pub outcome_reserve_bytes: u64,
}

impl Default for AuditLimits {
    fn default() -> Self {
        Self {
            limit_bytes: 128 * MIB,
            retention_days: 30,
            wal_checkpoint_bytes: 8 * MIB,
            wal_stop_bytes: 16 * MIB,
            min_free_disk_bytes: 64 * MIB,
            busy_deadline_ms: 1000,
            outcome_reserve_bytes: MIB,
        }
    }
}

/// Invalid arguments to [`crate::audit::SqliteAuditRepository::open`].
/// Database problems are not errors here: they open a repository whose
/// status is `Failed`/`ReadOnlyDiagnostic` and which refuses every write.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuditOpenError {
    #[error("invalid audit limits: {0}")]
    InvalidLimits(String),
    #[error("invalid audit database path {path}: {reason}")]
    InvalidPath { path: String, reason: String },
}

impl AuditLimits {
    /// Limits from `store.audit_*` and `runtime.audit_intent_deadline_ms`.
    /// The WAL thresholds shrink with small budgets (at most a quarter of the
    /// budget may be WAL) so the main file always keeps most of the space.
    pub fn from_config(config: &Config) -> Self {
        let defaults = Self::default();
        let limit_bytes = u64::from(config.store.audit_limit_mib).saturating_mul(MIB);
        let wal_stop_bytes = defaults.wal_stop_bytes.min(limit_bytes / 4);
        Self {
            limit_bytes,
            retention_days: config.store.audit_retention_days,
            wal_checkpoint_bytes: defaults.wal_checkpoint_bytes.min(wal_stop_bytes / 2),
            wal_stop_bytes,
            min_free_disk_bytes: defaults.min_free_disk_bytes,
            busy_deadline_ms: u64::from(config.runtime.audit_intent_deadline_ms),
            outcome_reserve_bytes: defaults.outcome_reserve_bytes.min(limit_bytes / 16),
        }
    }

    pub fn validate(&self) -> Result<(), AuditOpenError> {
        let invalid = |message: &str| Err(AuditOpenError::InvalidLimits(message.to_owned()));
        if self.retention_days == 0 {
            return invalid("retention_days must be at least 1");
        }
        if self.busy_deadline_ms == 0 {
            return invalid("busy_deadline_ms must be at least 1");
        }
        if self.wal_checkpoint_bytes == 0 || self.wal_checkpoint_bytes >= self.wal_stop_bytes {
            return invalid("wal_checkpoint_bytes must be positive and below wal_stop_bytes");
        }
        // One intent transaction must fit between the checkpoint and stop
        // thresholds, or no intent could ever be admitted.
        if self.wal_stop_bytes - self.wal_checkpoint_bytes < MIN_WAL_HEADROOM_BYTES {
            return invalid("wal_stop_bytes - wal_checkpoint_bytes must be at least 32 KiB");
        }
        let cap_pages = self.capacity_policy().database_page_cap(ASSUMED_PAGE_SIZE);
        if cap_pages < MIN_DATABASE_PAGES {
            return invalid("limit_bytes leaves too little room next to the WAL budget");
        }
        let reserve_pages = self.outcome_reserve_bytes.div_ceil(ASSUMED_PAGE_SIZE);
        if reserve_pages.saturating_mul(2) > cap_pages {
            return invalid("outcome_reserve_bytes must stay below half of the database cap");
        }
        Ok(())
    }

    pub fn busy_deadline(&self) -> Duration {
        Duration::from_millis(self.busy_deadline_ms)
    }

    pub(crate) fn capacity_policy(&self) -> CapacityPolicy {
        CapacityPolicy {
            limit_bytes: self.limit_bytes,
            reserve_bytes: self.outcome_reserve_bytes,
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
        let limits = AuditLimits::default();
        assert_eq!(limits.limit_bytes, 128 * MIB);
        assert_eq!(limits.retention_days, 30);
        assert_eq!(limits.wal_checkpoint_bytes, 8 * MIB);
        assert_eq!(limits.wal_stop_bytes, 16 * MIB);
        assert_eq!(limits.min_free_disk_bytes, 64 * MIB);
        assert_eq!(limits.busy_deadline(), Duration::from_millis(1000));
        assert_eq!(limits.validate(), Ok(()));
    }

    #[test]
    fn inconsistent_limits_are_rejected() {
        let base = AuditLimits::default();
        let cases = [
            AuditLimits {
                retention_days: 0,
                ..base
            },
            AuditLimits {
                busy_deadline_ms: 0,
                ..base
            },
            AuditLimits {
                wal_checkpoint_bytes: base.wal_stop_bytes,
                ..base
            },
            AuditLimits {
                limit_bytes: base.wal_stop_bytes,
                ..base
            },
            AuditLimits {
                outcome_reserve_bytes: base.limit_bytes,
                ..base
            },
            AuditLimits {
                wal_checkpoint_bytes: base.wal_stop_bytes - 1024,
                ..base
            },
        ];
        for case in cases {
            assert!(case.validate().is_err(), "{case:?}");
        }
    }
}
