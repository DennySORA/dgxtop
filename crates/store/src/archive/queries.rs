//! Prepared statements of the archive tables: idempotent inserts, age-based
//! retention, oldest-first capacity eviction and the dependent cleanup of
//! bridges, empty segments and orphaned gaps. Every value is bound.

use rusqlite::{Connection, OptionalExtension, params};

use super::rows::{ArchiveRow, BridgeRow, BucketRow, GapRow, SegmentRow};

const INSERT_SEGMENT: &str = "INSERT INTO history_segments (segment_id, host_id, boot_id, \
     entity_id, metric_id, scope, source_id, source_generation, policy_revision, unit, \
     sample_kind, started_utc) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
     ON CONFLICT(segment_id) DO NOTHING";
const INSERT_BUCKET: &str = "INSERT INTO history_buckets (segment_id, start_boot_ns, \
     end_boot_ns, start_utc, end_utc, covered_ns, weighted_sum, minimum, maximum, \
     counter_delta_decimal, sample_count, method, reason_code) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
     ON CONFLICT(segment_id, start_boot_ns) DO NOTHING";
const INSERT_BRIDGE: &str = "INSERT INTO counter_bridges (segment_id, start_boot_ns, \
     end_boot_ns, delta_decimal) VALUES (?1, ?2, ?3, ?4) \
     ON CONFLICT(segment_id, start_boot_ns, end_boot_ns) DO NOTHING";
const INSERT_GAP: &str = "INSERT INTO history_gaps (gap_id, host_id, boot_id, start_boot_ns, \
     end_boot_ns, reason_code, affected_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
     ON CONFLICT(gap_id) DO NOTHING";

// `start_utc <= end_utc`, so the extra start bound lets idx_history_utc
// narrow the scan.
const DELETE_EXPIRED_BUCKETS: &str = "DELETE FROM history_buckets WHERE rowid IN (
    SELECT rowid FROM history_buckets WHERE start_utc < ?1 AND end_utc < ?1 LIMIT ?2)";
// The `offset`-th oldest bucket start, or the newest one when fewer exist
// (a negative OFFSET behaves like zero on an empty table: no row).
const SELECT_EVICTION_HORIZON: &str = "SELECT start_utc FROM history_buckets \
     ORDER BY start_utc LIMIT 1 \
     OFFSET (SELECT min(?1, count(*) - 1) FROM history_buckets)";
// One series' minutes in every boot that started before ?5 (D-82: the
// history's compression holds them in its coarser sizes).
const DELETE_PRUNED_MINUTES: &str = "DELETE FROM history_buckets WHERE rowid IN (
    SELECT b.rowid FROM history_buckets AS b
    WHERE b.segment_id IN (SELECT s.segment_id FROM history_segments AS s
        WHERE s.host_id = ?1 AND s.entity_id = ?2 AND s.metric_id = ?3 AND s.scope = ?4)
      AND b.start_utc < ?5
    LIMIT ?6)";
const DELETE_BUCKETS_UP_TO: &str = "DELETE FROM history_buckets WHERE rowid IN (
    SELECT rowid FROM history_buckets WHERE start_utc <= ?1 LIMIT ?2)";
// Bridges that end before the earliest remaining bucket of their segment
// can no longer join any bucket total.
const DELETE_STALE_BRIDGES: &str = "DELETE FROM counter_bridges WHERE rowid IN (
    SELECT b.rowid FROM counter_bridges AS b
    WHERE b.end_boot_ns <= (SELECT MIN(k.start_boot_ns) FROM history_buckets AS k
                            WHERE k.segment_id = b.segment_id)
    LIMIT ?1)";
// ON DELETE CASCADE removes the remaining bridges of these segments. ?3 is a
// JSON array of segment ids an append in progress still writes into.
const DELETE_EMPTY_SEGMENTS: &str = "DELETE FROM history_segments WHERE rowid IN (
    SELECT s.rowid FROM history_segments AS s
    WHERE s.started_utc < ?1
      AND NOT EXISTS (SELECT 1 FROM history_buckets AS k WHERE k.segment_id = s.segment_id)
      AND s.segment_id NOT IN (SELECT value FROM json_each(?3))
    LIMIT ?2)";
const DELETE_ORPHANED_GAPS: &str = "DELETE FROM history_gaps WHERE rowid IN (
    SELECT g.rowid FROM history_gaps AS g
    WHERE g.boot_id <> ?1
      AND NOT EXISTS (SELECT 1 FROM history_segments AS s
                      WHERE s.host_id = g.host_id AND s.boot_id = g.boot_id)
    LIMIT ?2)";
const DELETE_OLD_CURRENT_BOOT_GAPS: &str = "DELETE FROM history_gaps WHERE rowid IN (
    SELECT rowid FROM history_gaps WHERE boot_id = ?1 AND end_boot_ns < ?2 LIMIT ?3)";

fn changed(rows: usize) -> u64 {
    u64::try_from(rows).unwrap_or(u64::MAX)
}

fn insert_segment(connection: &Connection, row: &SegmentRow) -> rusqlite::Result<usize> {
    connection.prepare_cached(INSERT_SEGMENT)?.execute(params![
        row.segment_id,
        row.host_id,
        row.boot_id,
        row.entity_id,
        row.metric_id,
        row.scope,
        row.source_id,
        row.source_generation,
        row.policy_revision,
        row.unit,
        row.sample_kind,
        row.started_utc,
    ])
}

fn insert_bucket(connection: &Connection, row: &BucketRow) -> rusqlite::Result<usize> {
    connection.prepare_cached(INSERT_BUCKET)?.execute(params![
        row.segment_id,
        row.start_boot_ns,
        row.end_boot_ns,
        row.start_utc,
        row.end_utc,
        row.covered_ns,
        row.weighted_sum,
        row.minimum,
        row.maximum,
        row.counter_delta_decimal,
        row.sample_count,
        row.method,
        row.reason_code,
    ])
}

fn insert_bridge(connection: &Connection, row: &BridgeRow) -> rusqlite::Result<usize> {
    connection.prepare_cached(INSERT_BRIDGE)?.execute(params![
        row.segment_id,
        row.start_boot_ns,
        row.end_boot_ns,
        row.delta_decimal,
    ])
}

fn insert_gap(connection: &Connection, row: &GapRow) -> rusqlite::Result<usize> {
    connection.prepare_cached(INSERT_GAP)?.execute(params![
        row.gap_id,
        row.host_id,
        row.boot_id,
        row.start_boot_ns,
        row.end_boot_ns,
        row.reason_code,
        row.affected_count,
    ])
}

/// Inserts one row; returns 1 when written, 0 when an identical key already
/// existed (re-appended journal).
pub(crate) fn insert_row(connection: &Connection, row: &ArchiveRow) -> rusqlite::Result<u64> {
    let written = match row {
        ArchiveRow::Segment(segment) => insert_segment(connection, segment)?,
        ArchiveRow::Bucket(bucket) => insert_bucket(connection, bucket)?,
        ArchiveRow::Bridge(bridge) => insert_bridge(connection, bridge)?,
        ArchiveRow::Gap(gap) => insert_gap(connection, gap)?,
    };
    Ok(changed(written))
}

pub(crate) fn delete_expired_buckets(
    connection: &Connection,
    cutoff_utc: &str,
    limit: i64,
) -> rusqlite::Result<u64> {
    connection
        .prepare_cached(DELETE_EXPIRED_BUCKETS)?
        .execute(params![cutoff_utc, limit])
        .map(changed)
}

/// `start_utc` of the `offset`-th oldest bucket (the newest when fewer
/// exist); `None` when the table is empty.
/// Deletes up to `limit` of `series`' minutes that started before
/// `cutoff_utc`.
pub(crate) fn delete_pruned_minutes(
    connection: &Connection,
    series: &dgxtop_core::history::SeriesInEveryBoot,
    cutoff_utc: &str,
    limit: i64,
) -> rusqlite::Result<u64> {
    connection
        .prepare_cached(DELETE_PRUNED_MINUTES)?
        .execute(params![
            series.host_id.as_str(),
            series.entity_id.as_str(),
            series.metric_id.as_str(),
            series.scope.as_str(),
            cutoff_utc,
            limit
        ])
        .map(changed)
}

pub(crate) fn eviction_horizon(
    connection: &Connection,
    offset: i64,
) -> rusqlite::Result<Option<String>> {
    connection
        .prepare_cached(SELECT_EVICTION_HORIZON)?
        .query_row(params![offset], |row| row.get(0))
        .optional()
}

pub(crate) fn delete_buckets_up_to(
    connection: &Connection,
    horizon_utc: &str,
    limit: i64,
) -> rusqlite::Result<u64> {
    connection
        .prepare_cached(DELETE_BUCKETS_UP_TO)?
        .execute(params![horizon_utc, limit])
        .map(changed)
}

pub(crate) fn delete_stale_bridges(connection: &Connection, limit: i64) -> rusqlite::Result<u64> {
    connection
        .prepare_cached(DELETE_STALE_BRIDGES)?
        .execute(params![limit])
        .map(changed)
}

/// Deletes segments without buckets that started before `cutoff_utc`,
/// except those in `protected_json` (a JSON array of segment ids).
pub(crate) fn delete_empty_segments(
    connection: &Connection,
    cutoff_utc: &str,
    protected_json: &str,
    limit: i64,
) -> rusqlite::Result<u64> {
    connection
        .prepare_cached(DELETE_EMPTY_SEGMENTS)?
        .execute(params![cutoff_utc, limit, protected_json])
        .map(changed)
}

pub(crate) fn delete_orphaned_gaps(
    connection: &Connection,
    current_boot_id: &str,
    limit: i64,
) -> rusqlite::Result<u64> {
    connection
        .prepare_cached(DELETE_ORPHANED_GAPS)?
        .execute(params![current_boot_id, limit])
        .map(changed)
}

pub(crate) fn delete_old_current_boot_gaps(
    connection: &Connection,
    current_boot_id: &str,
    cutoff_boot_ns: &str,
    limit: i64,
) -> rusqlite::Result<u64> {
    connection
        .prepare_cached(DELETE_OLD_CURRENT_BOOT_GAPS)?
        .execute(params![current_boot_id, cutoff_boot_ns, limit])
        .map(changed)
}
