//! Disk footprint, page accounting, PASSIVE checkpoints and the pure
//! admission policy shared by the audit and the archive.
//!
//! Capacity counts the main file, `-wal` and `-shm`. Deleting rows does not
//! shrink the main file (no VACUUM on the hot path), so admission uses the
//! pages actually in use (`page_count - freelist_count`) for growth and the
//! real file sizes for the total; `max_page_count` is the hard cap of the
//! main file.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::errors::{FaultKind, fault_kind};

/// Bytes SQLite prepends to every WAL frame.
pub(crate) const WAL_FRAME_HEADER_BYTES: u64 = 24;
/// Floor of the `-shm` allowance (one 32 KiB wal-index block).
const SHM_ALLOWANCE_FLOOR_BYTES: u64 = 32 * 1024;

fn sibling_with_suffix(database_path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(database_path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// `<database>-wal`.
pub fn wal_path(database_path: &Path) -> PathBuf {
    sibling_with_suffix(database_path, "-wal")
}

/// `<database>-shm`.
pub fn shm_path(database_path: &Path) -> PathBuf {
    sibling_with_suffix(database_path, "-shm")
}

fn size_or_zero(path: &Path) -> io::Result<u64> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// On-disk bytes of one SQLite database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageFootprint {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
}

impl StorageFootprint {
    /// Sizes of the main file, `-wal` and `-shm` (missing files count 0).
    pub fn measure(database_path: &Path) -> io::Result<Self> {
        Ok(Self {
            database_bytes: size_or_zero(database_path)?,
            wal_bytes: size_or_zero(&wal_path(database_path))?,
            shm_bytes: size_or_zero(&shm_path(database_path))?,
        })
    }

    /// db + wal + shm.
    pub fn total(&self) -> u64 {
        self.database_bytes
            .saturating_add(self.wal_bytes)
            .saturating_add(self.shm_bytes)
    }
}

/// Page accounting of the main database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageUsage {
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_count: u64,
    pub max_page_count: u64,
}

fn pragma_u64(connection: &Connection, pragma: &str) -> rusqlite::Result<u64> {
    let value: i64 = connection.pragma_query_value(None, pragma, |row| row.get(0))?;
    Ok(u64::try_from(value).unwrap_or(0))
}

impl PageUsage {
    pub(crate) fn read(connection: &Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            page_size: pragma_u64(connection, "page_size")?.max(1),
            page_count: pragma_u64(connection, "page_count")?,
            freelist_count: pragma_u64(connection, "freelist_count")?,
            max_page_count: pragma_u64(connection, "max_page_count")?,
        })
    }

    /// Pages holding data (free pages are reused before the file grows).
    pub(crate) fn used_pages(&self) -> u64 {
        self.page_count.saturating_sub(self.freelist_count)
    }
}

/// Result of one `PRAGMA wal_checkpoint(PASSIVE)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CheckpointProgress {
    pub log_frames: u64,
    pub checkpointed_frames: u64,
}

impl CheckpointProgress {
    /// WAL content not yet copied into the main file (pinned by readers).
    pub(crate) fn backlog_bytes(&self, page_size: u64) -> u64 {
        self.log_frames
            .saturating_sub(self.checkpointed_frames)
            .saturating_mul(page_size.saturating_add(WAL_FRAME_HEADER_BYTES))
    }
}

/// PASSIVE checkpoint from the writer connection: never waits for readers.
pub(crate) fn passive_checkpoint(connection: &Connection) -> rusqlite::Result<CheckpointProgress> {
    connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        let log: i64 = row.get(1)?;
        let checkpointed: i64 = row.get(2)?;
        Ok(CheckpointProgress {
            log_frames: u64::try_from(log).unwrap_or(0),
            checkpointed_frames: u64::try_from(checkpointed).unwrap_or(0),
        })
    })
}

/// Page accounting plus the WAL backlog. When the WAL file reached
/// `wal_checkpoint_bytes`, a PASSIVE checkpoint runs first and the backlog is
/// what it could not copy (frames pinned by readers); below the threshold
/// the backlog is reported as zero.
pub(crate) fn pages_and_backlog(
    connection: &Connection,
    wal_bytes: u64,
    wal_checkpoint_bytes: u64,
) -> rusqlite::Result<(PageUsage, u64)> {
    let pages = PageUsage::read(connection)?;
    if wal_bytes < wal_checkpoint_bytes {
        return Ok((pages, 0));
    }
    match passive_checkpoint(connection) {
        Ok(progress) => Ok((pages, progress.backlog_bytes(pages.page_size))),
        // Another checkpointer holds the lock: assume nothing was copied.
        Err(error) if fault_kind(&error) == FaultKind::Busy => Ok((pages, wal_bytes)),
        Err(error) => Err(error),
    }
}

/// Whether a write may consume the reserve kept for pending outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteClass {
    /// New intents, attempts and archive rows: never use the reserve and are
    /// refused at the free-disk and WAL stop thresholds.
    NewRecord,
    /// Outcomes of already committed intents: may use the reserve.
    ReservedOutcome,
}

/// Measurements an admission decision is made on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapacityView {
    pub footprint: StorageFootprint,
    pub pages: PageUsage,
    pub wal_backlog_bytes: u64,
    pub free_disk_bytes: u64,
}

/// Why a write was refused before its transaction started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    LowDisk {
        available: u64,
        minimum: u64,
    },
    /// Readers pin WAL frames a checkpoint could not copy; new records wait
    /// until they finish.
    WalBacklog {
        backlog: u64,
        stop: u64,
    },
    /// The write alone would push the WAL past its stop threshold.
    WalBudget {
        projected: u64,
        stop: u64,
    },
    OverLimit {
        projected: u64,
        allowed: u64,
    },
    PageCap {
        needed: u64,
        available: u64,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LowDisk { available, minimum } => write!(
                formatter,
                "free disk {available} bytes is below the {minimum} byte low-water mark"
            ),
            Self::WalBacklog { backlog, stop } => write!(
                formatter,
                "WAL backlog {backlog} bytes pinned by readers reaches the {stop} byte stop threshold after a checkpoint"
            ),
            Self::WalBudget { projected, stop } => write!(
                formatter,
                "the write would grow the WAL to {projected} bytes, above the {stop} byte stop threshold"
            ),
            Self::OverLimit { projected, allowed } => write!(
                formatter,
                "db+wal+shm would reach {projected} bytes, above the {allowed} byte budget"
            ),
            Self::PageCap { needed, available } => write!(
                formatter,
                "database needs {needed} pages but only {available} are allowed"
            ),
        }
    }
}

/// Byte budget of one partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapacityPolicy {
    pub limit_bytes: u64,
    /// Kept free for outcomes of committed intents (0 for the archive).
    pub reserve_bytes: u64,
    /// PASSIVE checkpoint threshold; also the connection's
    /// `journal_size_limit`, i.e. what a restarted WAL is truncated back to.
    pub wal_checkpoint_bytes: u64,
    pub wal_stop_bytes: u64,
    pub min_free_disk_bytes: u64,
}

impl CapacityPolicy {
    /// `-shm` allowance: 32 KiB covers 4096 WAL frames; scale with the WAL.
    fn shm_allowance(&self) -> u64 {
        (self.wal_stop_bytes / 256).max(SHM_ALLOWANCE_FLOOR_BYTES)
    }

    /// Hard cap of the main file in pages, leaving room for the WAL (up to
    /// its stop threshold) and the shm inside `limit_bytes`.
    pub(crate) fn database_page_cap(&self, page_size: u64) -> u64 {
        let journal = self.wal_stop_bytes.saturating_add(self.shm_allowance());
        self.limit_bytes.saturating_sub(journal) / page_size.max(1)
    }

    /// WAL bytes a write of `estimate_bytes` may add (one frame per page).
    pub(crate) fn wal_growth(page_size: u64, estimate_bytes: u64) -> u64 {
        let page_size = page_size.max(1);
        estimate_bytes
            .div_ceil(page_size)
            .max(1)
            .saturating_mul(page_size.saturating_add(WAL_FRAME_HEADER_BYTES))
    }

    /// Decides whether a write of about `estimate_bytes` may start.
    ///
    /// New records must leave the WAL within `wal_stop_bytes`, keep the
    /// outcome reserve free in pages and bytes, and need the free-disk
    /// low-water mark. Outcomes may use the reserve and are only bound by
    /// the hard page cap and `limit_bytes`.
    pub(crate) fn admit(
        &self,
        view: &CapacityView,
        class: WriteClass,
        estimate_bytes: u64,
    ) -> Result<(), Refusal> {
        let page_size = view.pages.page_size.max(1);
        let wal_after = self
            .wal_before_next_write(view)
            .saturating_add(Self::wal_growth(page_size, estimate_bytes));
        let reserve = match class {
            WriteClass::NewRecord => {
                self.admit_new_record(view, wal_after)?;
                self.reserve_bytes
            }
            WriteClass::ReservedOutcome => 0,
        };
        let estimate_pages = estimate_bytes.div_ceil(page_size).max(1);
        let pages_after = view.pages.used_pages().saturating_add(estimate_pages);
        let page_budget = view
            .pages
            .max_page_count
            .saturating_sub(reserve.div_ceil(page_size));
        if pages_after > page_budget {
            return Err(Refusal::PageCap {
                needed: pages_after,
                available: page_budget,
            });
        }
        let projected_database = view
            .footprint
            .database_bytes
            .max(pages_after.saturating_mul(page_size));
        let projected = projected_database
            .saturating_add(wal_after)
            .saturating_add(view.footprint.shm_bytes);
        let allowed = self.limit_bytes.saturating_sub(reserve);
        if projected > allowed {
            return Err(Refusal::OverLimit { projected, allowed });
        }
        Ok(())
    }

    /// WAL bytes the next write starts from. A fully checkpointed WAL is
    /// restarted by that write and truncated to `journal_size_limit`, so its
    /// stale high-water size must not count; a WAL with frames still pinned
    /// keeps growing from its current size.
    fn wal_before_next_write(&self, view: &CapacityView) -> u64 {
        if view.wal_backlog_bytes == 0 {
            view.footprint.wal_bytes.min(self.wal_checkpoint_bytes)
        } else {
            view.footprint.wal_bytes
        }
    }

    fn admit_new_record(&self, view: &CapacityView, wal_after: u64) -> Result<(), Refusal> {
        if view.free_disk_bytes < self.min_free_disk_bytes {
            return Err(Refusal::LowDisk {
                available: view.free_disk_bytes,
                minimum: self.min_free_disk_bytes,
            });
        }
        let pinned = view.wal_backlog_bytes > 0;
        if view.wal_backlog_bytes >= self.wal_stop_bytes
            || (pinned && wal_after > self.wal_stop_bytes)
        {
            return Err(Refusal::WalBacklog {
                backlog: view.wal_backlog_bytes,
                stop: self.wal_stop_bytes,
            });
        }
        if wal_after > self.wal_stop_bytes {
            return Err(Refusal::WalBudget {
                projected: wal_after,
                stop: self.wal_stop_bytes,
            });
        }
        Ok(())
    }

    /// Near a cap: worth a `Degraded` status before writes are refused.
    /// The WAL counts by the frames readers keep from the checkpoint, not
    /// by its file, which a restart leaves at `journal_size_limit` (the
    /// checkpoint threshold) even when everything was copied.
    pub(crate) fn is_near_limits(&self, view: &CapacityView) -> bool {
        let used_bytes = view
            .pages
            .used_pages()
            .saturating_mul(view.pages.page_size)
            .saturating_add(view.footprint.wal_bytes)
            .saturating_add(view.footprint.shm_bytes);
        let ninety_percent = self.limit_bytes / 10 * 9;
        view.wal_backlog_bytes >= self.wal_checkpoint_bytes
            || used_bytes >= ninety_percent
            || view.free_disk_bytes < self.min_free_disk_bytes.saturating_mul(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: u64 = 4096;

    fn policy() -> CapacityPolicy {
        CapacityPolicy {
            limit_bytes: 1 << 20,
            reserve_bytes: 64 * 1024,
            wal_checkpoint_bytes: 64 * 1024,
            wal_stop_bytes: 128 * 1024,
            min_free_disk_bytes: 1 << 20,
        }
    }

    fn view(used_pages: u64, wal_bytes: u64) -> CapacityView {
        let max_page_count = policy().database_page_cap(PAGE);
        CapacityView {
            footprint: StorageFootprint {
                database_bytes: used_pages * PAGE,
                wal_bytes,
                shm_bytes: 32 * 1024,
            },
            pages: PageUsage {
                page_size: PAGE,
                page_count: used_pages,
                freelist_count: 0,
                max_page_count,
            },
            wal_backlog_bytes: 0,
            free_disk_bytes: 10 << 30,
        }
    }

    #[test]
    fn page_cap_leaves_room_for_wal_and_shm() {
        // 1 MiB - 128 KiB WAL stop - 32 KiB shm = 864 KiB = 216 pages.
        assert_eq!(policy().database_page_cap(PAGE), 216);
        let tiny = CapacityPolicy {
            limit_bytes: 1000,
            ..policy()
        };
        assert_eq!(tiny.database_page_cap(PAGE), 0);
    }

    #[test]
    fn new_records_keep_the_reserve_but_outcomes_may_use_it() {
        let policy = policy();
        // 216-page cap, 16-page reserve: 200 usable pages for new records.
        let near_reserve = view(199, 0);
        assert_eq!(
            policy.admit(&near_reserve, WriteClass::NewRecord, PAGE),
            Ok(())
        );
        let at_reserve = view(200, 0);
        assert!(matches!(
            policy.admit(&at_reserve, WriteClass::NewRecord, PAGE),
            Err(Refusal::PageCap { .. })
        ));
        assert_eq!(
            policy.admit(&at_reserve, WriteClass::ReservedOutcome, PAGE),
            Ok(())
        );
        let full = view(216, 0);
        assert!(
            policy
                .admit(&full, WriteClass::ReservedOutcome, PAGE)
                .is_err()
        );
    }

    #[test]
    fn free_pages_are_reused_before_the_file_grows() {
        let policy = policy();
        let mut reused = view(200, 0);
        reused.pages.page_count = 216;
        reused.pages.freelist_count = 100;
        reused.footprint.database_bytes = 216 * PAGE;
        assert_eq!(policy.admit(&reused, WriteClass::NewRecord, PAGE), Ok(()));
    }

    #[test]
    fn low_disk_and_wal_backlog_refuse_only_new_records() {
        let policy = policy();
        let mut low_disk = view(10, 0);
        low_disk.free_disk_bytes = 1000;
        assert!(matches!(
            policy.admit(&low_disk, WriteClass::NewRecord, PAGE),
            Err(Refusal::LowDisk {
                available: 1000,
                ..
            })
        ));
        assert_eq!(
            policy.admit(&low_disk, WriteClass::ReservedOutcome, PAGE),
            Ok(())
        );
        let mut pinned = view(10, 200 * 1024);
        pinned.wal_backlog_bytes = 128 * 1024;
        assert!(matches!(
            policy.admit(&pinned, WriteClass::NewRecord, PAGE),
            Err(Refusal::WalBacklog { .. })
        ));
        assert_eq!(
            policy.admit(&pinned, WriteClass::ReservedOutcome, PAGE),
            Ok(())
        );
    }

    #[test]
    fn a_pinned_wal_counts_toward_the_total() {
        let policy = policy();
        // 100 pages = 400 KiB db with a pinned 600 KiB WAL: new records stop
        // at the WAL threshold and even outcomes exceed the 1 MiB budget.
        let mut big_wal = view(100, 600 * 1024);
        big_wal.wal_backlog_bytes = 4096;
        assert!(matches!(
            policy.admit(&big_wal, WriteClass::NewRecord, PAGE),
            Err(Refusal::WalBacklog { .. })
        ));
        assert!(matches!(
            policy.admit(&big_wal, WriteClass::ReservedOutcome, PAGE),
            Err(Refusal::OverLimit { .. })
        ));
        assert!(policy.is_near_limits(&big_wal));
        assert!(!policy.is_near_limits(&view(10, 0)));
    }

    #[test]
    fn a_write_larger_than_the_wal_budget_is_refused() {
        let policy = policy();
        // 200 KiB of pages would grow an empty WAL past the 128 KiB stop.
        assert!(matches!(
            policy.admit(&view(10, 0), WriteClass::NewRecord, 200 * 1024),
            Err(Refusal::WalBudget { .. })
        ));
        assert_eq!(
            policy.admit(&view(10, 0), WriteClass::NewRecord, 32 * 1024),
            Ok(())
        );
    }

    #[test]
    fn a_checkpointed_wal_left_at_its_size_limit_is_not_near_a_threshold() {
        let policy = policy();
        // A restarted WAL is truncated back to `journal_size_limit`, the
        // checkpoint threshold, so a healthy file stays at that size.
        let restarted = view(10, policy.wal_checkpoint_bytes);
        assert!(!policy.is_near_limits(&restarted));
        // Frames that readers keep from the checkpoint are the threshold.
        let pinned = CapacityView {
            wal_backlog_bytes: policy.wal_checkpoint_bytes,
            ..restarted
        };
        assert!(policy.is_near_limits(&pinned));
    }

    #[test]
    fn a_fully_checkpointed_wal_counts_as_truncated() {
        let policy = policy();
        // Same 600 KiB file, but nothing is pinned: the next write restarts
        // the WAL and truncates it to the 64 KiB journal size limit.
        let checkpointed = view(100, 600 * 1024);
        assert_eq!(
            policy.admit(&checkpointed, WriteClass::NewRecord, PAGE),
            Ok(())
        );
    }

    #[test]
    fn checkpoint_backlog_counts_frame_headers() {
        let progress = CheckpointProgress {
            log_frames: 10,
            checkpointed_frames: 4,
        };
        assert_eq!(progress.backlog_bytes(PAGE), 6 * (PAGE + 24));
        let complete = CheckpointProgress {
            log_frames: 10,
            checkpointed_frames: 10,
        };
        assert_eq!(complete.backlog_bytes(PAGE), 0);
    }
}
