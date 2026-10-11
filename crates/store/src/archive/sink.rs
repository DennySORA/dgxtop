//! [`SqliteArchive`], the `ArchiveSink` used by the archive worker.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dgxtop_core::action::{PersistenceKind, PersistenceState, PersistenceStatus};
use dgxtop_core::clock::{NANOS_PER_SECOND, TimeProvider, format_utc_ns};
use dgxtop_core::errors::InfraError;
use dgxtop_core::overview_history::ArchivePrune;
use dgxtop_core::ports::{ArchiveJournal, ArchiveReceipt, ArchiveSink};
use dgxtop_core::wire::format_fixed_width;
use rusqlite::{Connection, OptionalExtension, params};

use super::limits::{ArchiveLimits, ArchiveOpenError};
use super::queries::{
    delete_buckets_up_to, delete_empty_segments, delete_expired_buckets,
    delete_old_current_boot_gaps, delete_orphaned_gaps, delete_pruned_minutes,
    delete_stale_bridges, eviction_horizon, insert_row,
};
use super::rows::{ArchiveBatch, ArchiveRow, storage_estimate};
use crate::fs::{FileSystemOps, RealFileSystem};
use crate::sqlite::{
    CapacityPolicy, CapacityView, ConnectionProfile, DatabaseSpec, FaultKind, FreeSpaceProbe,
    PageUsage, Refusal, SchemaKind, StorageFootprint, TxError, VersionGate, WriteClass, Writer,
    drain, ensure_sqlite_version, fault_kind, fault_message, immediate_transaction, open_database,
    pages_and_backlog, path_problem, tx_sqlite_error, utc_cutoff,
};

/// Automatic retention runs at most every ten minutes (boot clock).
const RETENTION_INTERVAL_NS: u64 = 600 * NANOS_PER_SECOND;
/// Bounded transactions per retention run or eviction.
const MAX_MAINTENANCE_ROUNDS: u32 = 64;
/// Eviction rounds tried before an append gives up with `DiskFull`.
const MAX_EVICTION_ROUNDS: u32 = 16;
/// Status probes as if a small batch were about to be written.
const STATUS_PROBE_BYTES: u64 = 4096;
const SECONDS_PER_DAY: u64 = 86_400;
const SELECT_SEGMENT_EXISTS: &str = "SELECT 1 FROM history_segments WHERE segment_id = ?1";
/// Time-based retention protects no segment.
const NO_PROTECTED_SEGMENTS: &str = "[]";

/// Counters since open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchiveStats {
    pub rows_written: u64,
    /// Rows whose key already existed (re-appended journals).
    pub rows_already_present: u64,
    pub transactions: u64,
    pub buckets_expired: u64,
    pub buckets_evicted: u64,
    /// Minutes the history's compression asked to delete (D-82).
    pub buckets_pruned: u64,
    pub bridges_deleted: u64,
    pub segments_deleted: u64,
    pub gaps_deleted: u64,
}

/// Rows removed by one retention run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRetentionReport {
    pub cutoff_utc: String,
    pub buckets: u64,
    pub bridges: u64,
    pub segments: u64,
    pub gaps: u64,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct CleanupCounts {
    bridges: u64,
    segments: u64,
    gaps: u64,
    complete: bool,
}

/// The optional history archive on `history.sqlite3`.
///
/// Like the audit, a file that is not this build's format, or unreadable,
/// is deleted and created afresh at open (D-85); what cannot be fixed that
/// way still yields an archive whose status says why and whose appends fail
/// with `InfraError::Unavailable`.
pub struct SqliteArchive {
    path: PathBuf,
    limits: ArchiveLimits,
    policy: CapacityPolicy,
    clock: Arc<dyn TimeProvider>,
    free_space: Arc<dyn FreeSpaceProbe>,
    writer: Writer,
    last_issue: Option<String>,
    last_retention_boot_ns: Option<u64>,
    recreated: Option<String>,
    stats: ArchiveStats,
}

impl fmt::Debug for SqliteArchive {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SqliteArchive")
            .field("path", &self.path)
            .field("limits", &self.limits)
            .field("writer", &self.writer)
            .field("last_issue", &self.last_issue)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

fn validate_path(path: &Path) -> Result<(), ArchiveOpenError> {
    match path_problem(path) {
        Some(reason) => Err(ArchiveOpenError::InvalidPath {
            path: path.display().to_string(),
            reason: reason.to_owned(),
        }),
        None => Ok(()),
    }
}

/// Splits rows into transactions of at most `max_rows` rows and about
/// `max_bytes` payload bytes (a single oversized row gets its own batch).
pub(crate) fn chunk_bounds(
    rows: &[ArchiveRow],
    max_rows: usize,
    max_bytes: u64,
) -> Vec<Range<usize>> {
    let mut bounds = Vec::new();
    let mut start = 0;
    let mut bytes = 0u64;
    for (index, row) in rows.iter().enumerate() {
        let size = row.payload_bytes();
        let count = index - start;
        if count > 0 && (count >= max_rows || bytes.saturating_add(size) > max_bytes) {
            bounds.push(start..index);
            start = index;
            bytes = 0;
        }
        bytes = bytes.saturating_add(size);
    }
    if start < rows.len() {
        bounds.push(start..rows.len());
    }
    bounds
}

fn referenced_segments(batch: &ArchiveBatch) -> (BTreeSet<&str>, BTreeSet<&str>) {
    let mut defined = BTreeSet::new();
    let mut referenced = BTreeSet::new();
    for row in &batch.rows {
        match row {
            ArchiveRow::Segment(segment) => {
                defined.insert(segment.segment_id.as_str());
            }
            ArchiveRow::Bucket(bucket) => {
                referenced.insert(bucket.segment_id.as_str());
            }
            ArchiveRow::Bridge(bridge) => {
                referenced.insert(bridge.segment_id.as_str());
            }
            ArchiveRow::Gap(_) => {}
        }
    }
    (defined, referenced)
}

/// Segment ids an append writes into, as the JSON array bound by the
/// empty-segment cleanup. Serializing strings cannot fail; an empty result
/// would make the cleanup statement fail rather than delete anything.
fn protected_segments_json(batch: &ArchiveBatch) -> String {
    let (defined, referenced) = referenced_segments(batch);
    let protected: Vec<&str> = defined.union(&referenced).copied().collect();
    serde_json::to_string(&protected).unwrap_or_default()
}

fn first_unknown_segment(
    connection: &Connection,
    batch: &ArchiveBatch,
) -> rusqlite::Result<Option<String>> {
    let (defined, referenced) = referenced_segments(batch);
    let mut statement = connection.prepare_cached(SELECT_SEGMENT_EXISTS)?;
    for segment_id in referenced.difference(&defined) {
        let exists = statement
            .query_row(params![segment_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            return Ok(Some((*segment_id).to_owned()));
        }
    }
    Ok(None)
}

impl SqliteArchive {
    /// Opens (creating with mode 0600 when missing) and verifies the
    /// archive. `Err` only for invalid arguments; see the type docs.
    pub fn open(
        path: &Path,
        limits: ArchiveLimits,
        clock: Arc<dyn TimeProvider>,
        free_space: Arc<dyn FreeSpaceProbe>,
    ) -> Result<Self, ArchiveOpenError> {
        Self::open_inner(
            path,
            limits,
            clock,
            free_space,
            &RealFileSystem,
            ensure_sqlite_version,
        )
    }

    pub(crate) fn open_inner(
        path: &Path,
        limits: ArchiveLimits,
        clock: Arc<dyn TimeProvider>,
        free_space: Arc<dyn FreeSpaceProbe>,
        fs: &dyn FileSystemOps,
        gate: VersionGate,
    ) -> Result<Self, ArchiveOpenError> {
        limits.validate()?;
        validate_path(path)?;
        let policy = limits.capacity_policy();
        let spec = DatabaseSpec {
            path,
            kind: SchemaKind::History,
            profile: ConnectionProfile::best_effort(limits.wal_checkpoint_bytes),
            policy,
        };
        let (writer, recreated) = Writer::new("archive", open_database(&spec, fs, gate));
        let mut archive = Self {
            path: path.to_path_buf(),
            limits,
            policy,
            clock,
            free_space,
            writer,
            last_issue: None,
            last_retention_boot_ns: None,
            recreated,
            stats: ArchiveStats::default(),
        };
        if archive.writer.unusable().is_none() {
            archive.maybe_run_retention();
        }
        Ok(archive)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn limits(&self) -> &ArchiveLimits {
        &self.limits
    }

    pub fn stats(&self) -> ArchiveStats {
        self.stats
    }

    /// Why the file found at open was deleted and created afresh (D-85),
    /// if it was.
    pub fn recreated(&self) -> Option<&str> {
        self.recreated.as_deref()
    }

    /// Age-based retention: buckets whose interval ended before
    /// `now - retention_days`, then stale bridges, empty segments (cascade)
    /// and gaps of boots without data. Also runs at open and at most every
    /// ten minutes from `append`; never from `status` (a read, D-96).
    pub fn run_retention(&mut self) -> Result<ArchiveRetentionReport, InfraError> {
        let cutoff_utc = utc_cutoff(self.clock.now_utc_ns(), self.limits.retention_days);
        let now_boot_ns = self.clock.now_boot_ns();
        self.last_retention_boot_ns = Some(now_boot_ns);
        let window_ns = u64::from(self.limits.retention_days)
            .saturating_mul(SECONDS_PER_DAY)
            .saturating_mul(NANOS_PER_SECOND);
        let cutoff_boot_ns = format_fixed_width(now_boot_ns.saturating_sub(window_ns));
        let batch = i64::from(self.limits.batch_rows);
        let mut rounds_left = MAX_MAINTENANCE_ROUNDS;
        let (buckets, buckets_done) = self.drain(&mut rounds_left, |connection| {
            delete_expired_buckets(connection, &cutoff_utc, batch)
        })?;
        self.stats.buckets_expired = self.stats.buckets_expired.saturating_add(buckets);
        let cleanup = if buckets_done {
            self.cleanup_dependents(
                &cutoff_utc,
                Some(&cutoff_boot_ns),
                NO_PROTECTED_SEGMENTS,
                &mut rounds_left,
            )?
        } else {
            CleanupCounts::default()
        };
        Ok(ArchiveRetentionReport {
            cutoff_utc,
            buckets,
            bridges: cleanup.bridges,
            segments: cleanup.segments,
            gaps: cleanup.gaps,
            complete: buckets_done && cleanup.complete,
        })
    }

    fn connection(&mut self) -> Result<&mut Connection, InfraError> {
        self.writer.connection().map_err(|unusable| {
            InfraError::Unavailable(format!("archive unavailable: {}", unusable.reason))
        })
    }

    fn maybe_run_retention(&mut self) {
        let now = self.clock.now_boot_ns();
        let due = self
            .last_retention_boot_ns
            .is_none_or(|last| now.saturating_sub(last) >= RETENTION_INTERVAL_NS);
        if due && let Err(error) = self.run_retention() {
            self.last_issue = Some(format!("archive retention failed: {error}"));
        }
    }

    /// A failed statement; corruption stops the archive (D-85).
    fn sqlite_failure(&mut self, operation: &'static str, error: &rusqlite::Error) -> InfraError {
        self.note_issue(operation, error);
        self.writer.statement_failed(operation, error)
    }

    /// A failed transaction; corruption stops the archive (D-85).
    fn tx_failure<E>(&mut self, operation: &'static str, error: &TxError<E>) -> InfraError {
        if let Some(sqlite) = tx_sqlite_error(error) {
            self.note_issue(operation, sqlite);
        }
        self.writer.transaction_failed(operation, error)
    }

    /// The status shows a failure until the next good append, except a
    /// held lock (it passes by itself) and corruption (the archive stops
    /// and says so).
    fn note_issue(&mut self, operation: &str, error: &rusqlite::Error) {
        if !matches!(fault_kind(error), FaultKind::Busy | FaultKind::Corrupt) {
            self.last_issue = Some(fault_message(operation, error));
        }
    }

    /// Runs one bounded statement in repeated transactions until it removes
    /// less than a batch or the round budget is spent. Returns the rows
    /// removed and whether it finished.
    fn drain(
        &mut self,
        rounds_left: &mut u32,
        work: impl Fn(&Connection) -> rusqlite::Result<u64>,
    ) -> Result<(u64, bool), InfraError> {
        let batch = u64::from(self.limits.batch_rows);
        let deadline = self.limits.busy_deadline();
        let drained = drain(self.connection()?, deadline, batch, rounds_left, work);
        let drained = drained.map_err(|error| self.tx_failure("archive.maintenance", &error))?;
        Ok((drained.removed, drained.finished))
    }

    /// Removes rows that only existed for buckets already gone. Segments in
    /// `protected_json` (JSON array) are kept because an append in progress
    /// still writes into them.
    fn cleanup_dependents(
        &mut self,
        segment_cutoff_utc: &str,
        current_boot_gap_cutoff: Option<&str>,
        protected_json: &str,
        rounds_left: &mut u32,
    ) -> Result<CleanupCounts, InfraError> {
        let batch = i64::from(self.limits.batch_rows);
        let boot_id = self.clock.boot_id();
        let mut counts = CleanupCounts::default();
        let (bridges, bridges_done) = self.drain(rounds_left, |connection| {
            delete_stale_bridges(connection, batch)
        })?;
        counts.bridges = bridges;
        let (segments, segments_done) = self.drain(rounds_left, |connection| {
            delete_empty_segments(connection, segment_cutoff_utc, protected_json, batch)
        })?;
        counts.segments = segments;
        let (orphaned, orphaned_done) = self.drain(rounds_left, |connection| {
            delete_orphaned_gaps(connection, boot_id.as_str(), batch)
        })?;
        let (old, old_done) = match current_boot_gap_cutoff {
            Some(cutoff) => self.drain(rounds_left, |connection| {
                delete_old_current_boot_gaps(connection, boot_id.as_str(), cutoff, batch)
            })?,
            None => (0, true),
        };
        counts.gaps = orphaned.saturating_add(old);
        counts.complete = bridges_done && segments_done && orphaned_done && old_done;
        self.stats.bridges_deleted = self.stats.bridges_deleted.saturating_add(counts.bridges);
        self.stats.segments_deleted = self.stats.segments_deleted.saturating_add(counts.segments);
        self.stats.gaps_deleted = self.stats.gaps_deleted.saturating_add(counts.gaps);
        Ok(counts)
    }

    /// Deletes the minutes the history's compression holds in its coarser
    /// sizes (D-82), then the rows that depended on them. Bounded like a
    /// retention run: what is left goes with the next prune.
    fn prune(&mut self, prunes: &[ArchivePrune], protected_json: &str) -> Result<(), InfraError> {
        if prunes.is_empty() {
            return Ok(());
        }
        let batch = i64::from(self.limits.batch_rows);
        let mut rounds_left = MAX_MAINTENANCE_ROUNDS;
        let mut latest_cutoff = String::new();
        for prune in prunes {
            let cutoff = format_utc_ns(i128::from(prune.before_utc_ns.max(0))).unwrap_or_default();
            let (removed, _) = self.drain(&mut rounds_left, |connection| {
                delete_pruned_minutes(connection, &prune.series, &cutoff, batch)
            })?;
            self.stats.buckets_pruned = self.stats.buckets_pruned.saturating_add(removed);
            latest_cutoff = latest_cutoff.max(cutoff);
        }
        self.cleanup_dependents(&latest_cutoff, None, protected_json, &mut rounds_left)?;
        Ok(())
    }

    /// Deletes the oldest whole minutes (up to one batch of buckets) and the
    /// rows that depended on them. Returns the buckets removed.
    fn evict_oldest(&mut self, protected_json: &str) -> Result<u64, InfraError> {
        let offset = i64::from(self.limits.batch_rows).saturating_sub(1);
        let horizon = eviction_horizon(self.connection()?, offset);
        let horizon = horizon.map_err(|error| self.sqlite_failure("archive.evict", &error))?;
        let Some(horizon) = horizon else {
            return Ok(0);
        };
        let batch = i64::from(self.limits.batch_rows);
        let mut rounds_left = MAX_MAINTENANCE_ROUNDS;
        let (evicted, _) = self.drain(&mut rounds_left, |connection| {
            delete_buckets_up_to(connection, &horizon, batch)
        })?;
        self.stats.buckets_evicted = self.stats.buckets_evicted.saturating_add(evicted);
        self.cleanup_dependents(&horizon, None, protected_json, &mut rounds_left)?;
        Ok(evicted)
    }

    fn capacity_view(&mut self) -> Result<CapacityView, InfraError> {
        let footprint = StorageFootprint::measure(&self.path)
            .map_err(|error| InfraError::from_io("archive.measure", &error))?;
        let wal_checkpoint_bytes = self.limits.wal_checkpoint_bytes;
        let measured = pages_and_backlog(
            self.connection()?,
            footprint.wal_bytes,
            wal_checkpoint_bytes,
        );
        let (pages, wal_backlog_bytes) =
            measured.map_err(|error| self.sqlite_failure("archive.capacity", &error))?;
        let directory = self.path.parent().unwrap_or(Path::new("/"));
        let free_disk_bytes = self.free_space.available_bytes(directory)?;
        Ok(CapacityView {
            footprint,
            pages,
            wal_backlog_bytes,
            free_disk_bytes,
        })
    }

    /// Admission before any row is written: low disk refuses, a pinned WAL
    /// pauses, the page cap evicts the oldest data first.
    fn admit_with_eviction(
        &mut self,
        estimate_bytes: u64,
        protected_json: &str,
    ) -> Result<(), InfraError> {
        for _ in 0..MAX_EVICTION_ROUNDS {
            let view = self.capacity_view()?;
            match self
                .policy
                .admit(&view, WriteClass::NewRecord, estimate_bytes)
            {
                Ok(()) => return Ok(()),
                Err(Refusal::LowDisk { available, minimum }) => {
                    self.last_issue = Some(format!(
                        "free disk {available} bytes below the {minimum} byte low-water mark"
                    ));
                    return Err(InfraError::DiskFull {
                        operation: "archive.free_space",
                    });
                }
                Err(refusal @ Refusal::WalBacklog { .. }) => {
                    let message = format!("archive writes paused: {refusal}");
                    self.last_issue = Some(message.clone());
                    return Err(InfraError::Unavailable(message));
                }
                Err(refusal @ (Refusal::OverLimit { .. } | Refusal::WalBudget { .. })) => {
                    self.last_issue = Some(format!("archive budget reached: {refusal}"));
                    return Err(InfraError::DiskFull {
                        operation: "archive.limit",
                    });
                }
                Err(Refusal::PageCap { .. }) => {
                    if self.evict_oldest(protected_json)? == 0 {
                        break;
                    }
                }
            }
        }
        self.last_issue = Some("archive budget cannot hold this batch".to_owned());
        Err(InfraError::DiskFull {
            operation: "archive.limit",
        })
    }

    /// Writes the rows in bounded transactions, each admitted on its own;
    /// returns (written, present). A refusal after some transactions leaves
    /// those committed (a retry skips them), so the caller records a gap.
    fn insert_batch(&mut self, batch: &ArchiveBatch) -> Result<(u64, u64), InfraError> {
        let protected_json = protected_segments_json(batch);
        let deadline = self.limits.busy_deadline();
        let max_rows = usize::try_from(self.limits.batch_rows).unwrap_or(usize::MAX);
        let mut written_total = 0u64;
        let mut present_total = 0u64;
        for bounds in chunk_bounds(&batch.rows, max_rows, self.limits.batch_bytes) {
            let chunk = batch.rows.get(bounds).unwrap_or_default();
            self.admit_with_eviction(storage_estimate(chunk), &protected_json)?;
            let result: Result<u64, TxError<()>> =
                immediate_transaction(self.connection()?, deadline, |transaction| {
                    let mut written = 0u64;
                    for row in chunk {
                        written = written.saturating_add(insert_row(transaction, row)?);
                    }
                    Ok(written)
                });
            let written = result.map_err(|error| self.tx_failure("archive.write", &error))?;
            let attempted = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            written_total = written_total.saturating_add(written);
            present_total = present_total.saturating_add(attempted.saturating_sub(written));
            self.stats.transactions = self.stats.transactions.saturating_add(1);
        }
        Ok((written_total, present_total))
    }

    /// A journal larger than an empty archive could hold is refused before
    /// anything is written (evicting its own earlier rows would be absurd).
    fn check_fits_budget(&mut self, batch: &ArchiveBatch) -> Result<(), InfraError> {
        let pages = PageUsage::read(self.connection()?);
        let page_size = pages
            .map_err(|error| self.sqlite_failure("archive.capacity", &error))?
            .page_size;
        let capacity = self
            .policy
            .database_page_cap(page_size)
            .saturating_mul(page_size);
        if storage_estimate(&batch.rows) > capacity {
            self.last_issue = Some(format!(
                "a journal of about {} bytes exceeds the {capacity} byte archive capacity",
                storage_estimate(&batch.rows)
            ));
            return Err(InfraError::DiskFull {
                operation: "archive.limit",
            });
        }
        Ok(())
    }

    fn check_segment_references(&mut self, batch: &ArchiveBatch) -> Result<(), InfraError> {
        let unknown = first_unknown_segment(self.connection()?, batch);
        let unknown = unknown.map_err(|error| self.sqlite_failure("archive.validate", &error))?;
        match unknown {
            Some(segment_id) => Err(InfraError::Database(format!(
                "archive journal rejected: rows reference unknown segment {segment_id}"
            ))),
            None => Ok(()),
        }
    }

    fn state_and_detail(&mut self) -> (PersistenceState, Option<String>) {
        if let Some(unusable) = self.writer.unusable() {
            return (PersistenceState::Failed, Some(unusable.reason.clone()));
        }
        let view = match self.capacity_view() {
            Ok(view) => view,
            Err(error) => return (PersistenceState::Failed, Some(error.to_string())),
        };
        match self
            .policy
            .admit(&view, WriteClass::NewRecord, STATUS_PROBE_BYTES)
        {
            Err(refusal @ Refusal::LowDisk { .. }) => (
                PersistenceState::Failed,
                Some(format!("archive writes refused: {refusal}")),
            ),
            Err(refusal) => (
                PersistenceState::Degraded,
                Some(format!("archive at a threshold: {refusal}")),
            ),
            Ok(()) if self.policy.is_near_limits(&view) => (
                PersistenceState::Degraded,
                Some("archive near a capacity threshold".to_owned()),
            ),
            Ok(()) => match &self.last_issue {
                Some(issue) => (PersistenceState::Degraded, Some(issue.clone())),
                None => (PersistenceState::Healthy, None),
            },
        }
    }
}

impl ArchiveSink for SqliteArchive {
    fn append(&mut self, journal: ArchiveJournal) -> Result<ArchiveReceipt, InfraError> {
        let batch = ArchiveBatch::from_journal(&journal).map_err(|reason| {
            InfraError::Database(format!("archive journal rejected: {reason}"))
        })?;
        self.connection()?;
        self.maybe_run_retention();
        self.check_segment_references(&batch)?;
        self.check_fits_budget(&batch)?;
        let (written, present) = self.insert_batch(&batch)?;
        self.prune(&journal.prunes, &protected_segments_json(&batch))?;
        self.stats.rows_written = self.stats.rows_written.saturating_add(written);
        self.stats.rows_already_present = self.stats.rows_already_present.saturating_add(present);
        self.last_issue = None;
        let revision = journal.revision;
        // The journal (and its byte permit) is released only once its rows
        // are committed.
        drop(journal);
        Ok(ArchiveReceipt {
            revision,
            rows: written,
        })
    }

    fn status(&mut self) -> PersistenceStatus {
        let (state, detail) = self.state_and_detail();
        let footprint = StorageFootprint::measure(&self.path).unwrap_or_default();
        PersistenceStatus {
            kind: PersistenceKind::Archive,
            state,
            detail,
            bytes_used: footprint.total(),
            bytes_limit: self.limits.limit_bytes,
            wal_bytes: footprint.wal_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::rows::{BridgeRow, GapRow};
    use crate::sqlite::{SqliteGateError, SqliteVersion, StatvfsFreeSpace};
    use dgxtop_core::testing::FakeClock;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dgxtop-archive-unit-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn integer_pragma(connection: &Connection, name: &str) -> i64 {
        connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn writer_connection_is_wal_normal_with_manual_checkpoints() {
        let dir = temp_dir("pragmas");
        let limits = ArchiveLimits::default();
        let mut archive = SqliteArchive::open(
            &dir.join("history.sqlite3"),
            limits,
            FakeClock::new(1),
            Arc::new(StatvfsFreeSpace),
        )
        .unwrap();
        let connection = archive.writer.connection().unwrap();
        let mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(integer_pragma(connection, "synchronous"), 1);
        assert_eq!(integer_pragma(connection, "cache_size"), -4096);
        assert_eq!(integer_pragma(connection, "busy_timeout"), 50);
        assert_eq!(integer_pragma(connection, "foreign_keys"), 1);
        assert_eq!(integer_pragma(connection, "wal_autocheckpoint"), 0);
        assert_eq!(integer_pragma(connection, "journal_size_limit"), 16 << 20);
        let cap = limits.capacity_policy().database_page_cap(4096);
        assert_eq!(
            integer_pragma(connection, "max_page_count"),
            i64::try_from(cap).unwrap()
        );
        drop(archive);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn failing_gate() -> Result<SqliteVersion, SqliteGateError> {
        Err(SqliteGateError::TooOld {
            found: 3_046_001,
            required: 3_051_003,
        })
    }

    #[test]
    fn version_gate_failure_disables_the_archive_without_creating_it() {
        let dir = temp_dir("gate");
        let path = dir.join("history.sqlite3");
        let mut archive = SqliteArchive::open_inner(
            &path,
            ArchiveLimits::default(),
            FakeClock::new(1),
            Arc::new(StatvfsFreeSpace),
            &RealFileSystem,
            failing_gate,
        )
        .unwrap();
        let status = archive.status();
        assert_eq!(status.state, PersistenceState::Failed);
        assert!(!path.exists());
        assert!(matches!(
            archive.run_retention(),
            Err(InfraError::Unavailable(_))
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn bridge(index: u64) -> ArchiveRow {
        ArchiveRow::Bridge(BridgeRow {
            segment_id: "s".to_owned(),
            start_boot_ns: format_fixed_width(index),
            end_boot_ns: format_fixed_width(index + 1),
            delta_decimal: "1".to_owned(),
        })
    }

    fn gap(reason_bytes: usize) -> ArchiveRow {
        ArchiveRow::Gap(GapRow {
            gap_id: "g".repeat(32),
            host_id: "h".to_owned(),
            boot_id: "b".to_owned(),
            start_boot_ns: format_fixed_width(1),
            end_boot_ns: format_fixed_width(2),
            reason_code: "r".repeat(reason_bytes),
            affected_count: 1,
        })
    }

    #[test]
    fn chunks_respect_row_and_byte_limits() {
        let rows: Vec<ArchiveRow> = (0..10).map(bridge).collect();
        let by_rows = chunk_bounds(&rows, 4, u64::MAX);
        assert_eq!(by_rows, vec![0..4, 4..8, 8..10]);
        let row_bytes = rows[0].payload_bytes();
        let by_bytes = chunk_bounds(&rows, 100, row_bytes * 3);
        assert_eq!(by_bytes, vec![0..3, 3..6, 6..9, 9..10]);
        assert!(chunk_bounds(&[], 4, 100).is_empty());
    }

    #[test]
    fn an_oversized_row_gets_its_own_chunk() {
        let rows = vec![gap(10), gap(100_000), gap(10)];
        let bounds = chunk_bounds(&rows, 100, 1000);
        assert_eq!(bounds, vec![0..1, 1..2, 2..3]);
    }

    fn sqlite_failure_of(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None)
    }

    /// The archive maps a failure like every partition (sqlite::Writer): a
    /// full database is `DiskFull` and shown until the next good append, a
    /// corrupt one stops the archive with its reason.
    #[test]
    fn failures_map_like_every_partition() {
        let dir = temp_dir("mapping");
        let mut archive = SqliteArchive::open(
            &dir.join("history.sqlite3"),
            ArchiveLimits::default(),
            FakeClock::new(1),
            Arc::new(StatvfsFreeSpace),
        )
        .unwrap();
        let full = archive.sqlite_failure(
            "archive.write",
            &sqlite_failure_of(rusqlite::ffi::SQLITE_FULL),
        );
        assert_eq!(
            full,
            InfraError::DiskFull {
                operation: "archive.write"
            }
        );
        let status = archive.status();
        assert_eq!(status.state, PersistenceState::Degraded);
        assert!(status.detail.unwrap().contains("archive.write"));
        let commit: TxError<()> =
            TxError::Commit(sqlite_failure_of(rusqlite::ffi::SQLITE_IOERR_FSYNC));
        assert!(matches!(
            archive.tx_failure("archive.write", &commit),
            InfraError::Uncertain { .. }
        ));
        let corrupt = archive.sqlite_failure(
            "archive.write",
            &sqlite_failure_of(rusqlite::ffi::SQLITE_CORRUPT),
        );
        assert!(matches!(corrupt, InfraError::Database(_)), "{corrupt:?}");
        let status = archive.status();
        assert_eq!(status.state, PersistenceState::Failed);
        assert!(status.detail.unwrap().contains("archive stopped"));
        assert!(matches!(
            archive.run_retention(),
            Err(InfraError::Unavailable(_))
        ));
        drop(archive);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
