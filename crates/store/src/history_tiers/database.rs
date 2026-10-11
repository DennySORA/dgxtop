//! Rows of the tiers database (D-82): series upserted by their identity,
//! closed buckets written as the history hands them, each (series, size)
//! cut to the latest the journal asks for, the series written longest ago
//! giving way past the size limit, and the read at startup. Only prepared
//! statements with bound parameters.

use std::collections::BTreeSet;
use std::time::Duration;

use dgxtop_core::errors::InfraError;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::{EntityId, HostId, MetricId, Scope};
use dgxtop_core::overview_history::{OverviewBucket, StoredTierSeries, TierJournal, TierRow};
use dgxtop_core::process_history::Tier;
use rusqlite::{Connection, Row, Transaction, params};

use crate::sqlite::{StepError, TxError, describe, immediate_transaction};

/// A transaction waits this long for the write lock at most.
const BUSY_DEADLINE: Duration = Duration::from_millis(500);
/// Series a pass over a database past its size gives way at once.
const SERIES_GIVING_WAY: i64 = 1;

/// Writes the journal's buckets (one written again replaces the earlier),
/// then keeps the latest `keep_latest` of every (series, size) it touched.
pub(super) fn append(connection: &mut Connection, journal: &TierJournal) -> Result<(), InfraError> {
    if journal.rows.is_empty() {
        return Ok(());
    }
    immediate_transaction(connection, BUSY_DEADLINE, |transaction| {
        let mut touched = BTreeSet::new();
        for row in &journal.rows {
            let Some(tier) = stored_tier(row.tier) else {
                continue;
            };
            let series_id = upsert_series(transaction, row)?;
            touched.insert((series_id, tier));
            insert_bucket(transaction, series_id, tier, &row.bucket)?;
        }
        for (series_id, tier) in touched {
            keep_latest(transaction, series_id, tier, journal.keep_latest)?;
        }
        Ok::<_, StepError<()>>(())
    })
    .map_err(|error| tx_error("history_tiers.append", error))?;
    // The archive profile leaves checkpoints to the writer.
    connection
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
        .map_err(|error| sqlite_error("history_tiers.checkpoint", &error))
}

/// The code of a size the database keeps (not the 5 s one).
fn stored_tier(tier: Tier) -> Option<i64> {
    (tier != Tier::Recent).then(|| tier.code())
}

fn upsert_series(transaction: &Transaction<'_>, row: &TierRow) -> rusqlite::Result<i64> {
    let series = &row.series;
    transaction
        .prepare_cached(
            "INSERT INTO tier_series (host_id, entity_id, metric_id, scope, last_utc_ns) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (host_id, entity_id, metric_id, scope) DO UPDATE SET \
             last_utc_ns = max(last_utc_ns, excluded.last_utc_ns) \
             RETURNING series_id",
        )?
        .query_row(
            params![
                series.host_id.as_str(),
                series.entity_id.as_str(),
                series.metric_id.as_str(),
                series.scope.as_str(),
                row.bucket.start_utc_ns,
            ],
            |found| found.get(0),
        )
}

fn insert_bucket(
    transaction: &Transaction<'_>,
    series_id: i64,
    tier: i64,
    bucket: &OverviewBucket,
) -> rusqlite::Result<()> {
    if bucket.is_empty() {
        return Ok(());
    }
    let finite = |value: f64| value.is_finite().then_some(value);
    let total = bucket.total_exact.then(|| bucket.total.to_string());
    transaction
        .prepare_cached(
            "INSERT OR REPLACE INTO tier_buckets (series_id, tier, start_utc_ns, covered_ns, \
             weighted_sum, minimum, maximum, sample_count, total) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?
        .execute(params![
            series_id,
            tier,
            bucket.start_utc_ns,
            i64::try_from(bucket.covered_ns).unwrap_or(i64::MAX),
            bucket.weighted_sum,
            finite(bucket.minimum),
            finite(bucket.maximum),
            bucket.samples,
            total,
        ])?;
    Ok(())
}

/// Each (series, size) keeps its latest `keep` buckets: the older ones are
/// in the coarser sizes already.
fn keep_latest(
    transaction: &Transaction<'_>,
    series_id: i64,
    tier: i64,
    keep: u32,
) -> rusqlite::Result<()> {
    transaction
        .prepare_cached(
            "DELETE FROM tier_buckets WHERE series_id = ?1 AND tier = ?2 AND start_utc_ns < \
             (SELECT start_utc_ns FROM tier_buckets WHERE series_id = ?1 AND tier = ?2 \
              ORDER BY start_utc_ns DESC LIMIT 1 OFFSET ?3)",
        )?
        .execute(params![
            series_id,
            tier,
            i64::from(keep.max(1)).saturating_sub(1)
        ])?;
    Ok(())
}

/// While the database holds more than `max_used_bytes`, the series written
/// longest ago give way (D-82: none expires by age).
pub(super) fn retain(connection: &mut Connection, max_used_bytes: u64) -> Result<(), InfraError> {
    immediate_transaction(connection, BUSY_DEADLINE, |transaction| {
        while used_bytes(transaction)? > max_used_bytes {
            let removed = transaction.execute(
                "DELETE FROM tier_series WHERE series_id IN (SELECT series_id FROM tier_series \
                 ORDER BY last_utc_ns LIMIT ?1)",
                params![SERIES_GIVING_WAY],
            )?;
            if removed == 0 {
                break;
            }
        }
        Ok::<_, StepError<()>>(())
    })
    .map_err(|error| tx_error("history_tiers.retain", error))
}

/// Bytes of the pages in use (the free ones are reused before it grows).
fn used_bytes(connection: &Connection) -> rusqlite::Result<u64> {
    let pragma = |name: &str| -> rusqlite::Result<u64> {
        connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
            .map(|value| u64::try_from(value).unwrap_or(0))
    };
    let pages = pragma("page_count")?.saturating_sub(pragma("freelist_count")?);
    Ok(pages.saturating_mul(pragma("page_size")?))
}

/// Every series with its buckets, each size oldest first. A series whose
/// identity no longer parses is left out (kept on disk).
pub(super) fn load(connection: &mut Connection) -> Result<Vec<StoredTierSeries>, InfraError> {
    let mut statement = connection
        .prepare("SELECT series_id, host_id, entity_id, metric_id, scope FROM tier_series")
        .map_err(|error| sqlite_error("history_tiers.load", &error))?;
    let series: Vec<(i64, SeriesInEveryBoot)> = statement
        .query_map([], series_of)
        .map_err(|error| sqlite_error("history_tiers.load", &error))?
        .filter_map(Result::ok)
        .flatten()
        .collect();
    drop(statement);
    series
        .into_iter()
        .map(|(series_id, series)| {
            Ok(StoredTierSeries {
                series,
                buckets: buckets_of(connection, series_id)?,
            })
        })
        .collect()
}

fn series_of(row: &Row<'_>) -> rusqlite::Result<Option<(i64, SeriesInEveryBoot)>> {
    let series_id: i64 = row.get(0)?;
    let text = |index: usize| row.get::<_, String>(index);
    let (host, entity, metric, scope) = (text(1)?, text(2)?, text(3)?, text(4)?);
    let parsed = (|| {
        Some(SeriesInEveryBoot {
            host_id: HostId::new(host).ok()?,
            entity_id: EntityId::new(entity).ok()?,
            metric_id: MetricId::new(metric).ok()?,
            scope: Scope::new(scope).ok()?,
        })
    })();
    Ok(parsed.map(|series| (series_id, series)))
}

fn buckets_of(
    connection: &Connection,
    series_id: i64,
) -> Result<Vec<(Tier, OverviewBucket)>, InfraError> {
    let mut statement = connection
        .prepare_cached(
            "SELECT tier, start_utc_ns, covered_ns, weighted_sum, minimum, maximum, \
             sample_count, total FROM tier_buckets WHERE series_id = ?1 \
             ORDER BY tier, start_utc_ns",
        )
        .map_err(|error| sqlite_error("history_tiers.buckets", &error))?;
    let rows = statement
        .query_map(params![series_id], bucket_of)
        .map_err(|error| sqlite_error("history_tiers.buckets", &error))?;
    Ok(rows.filter_map(Result::ok).flatten().collect())
}

fn bucket_of(row: &Row<'_>) -> rusqlite::Result<Option<(Tier, OverviewBucket)>> {
    let tier: i64 = row.get(0)?;
    let covered: i64 = row.get(2)?;
    let minimum: Option<f64> = row.get(4)?;
    let maximum: Option<f64> = row.get(5)?;
    let total: Option<String> = row.get(7)?;
    let exact_total = total.as_deref().and_then(|text| text.parse::<u64>().ok());
    let bucket = OverviewBucket {
        start_utc_ns: row.get(1)?,
        covered_ns: u64::try_from(covered).unwrap_or(0),
        weighted_sum: row.get(3)?,
        minimum: minimum.unwrap_or(f64::INFINITY),
        maximum: maximum.unwrap_or(f64::NEG_INFINITY),
        samples: row.get(6)?,
        total: exact_total.unwrap_or(0),
        total_exact: exact_total.is_some(),
    };
    Ok(Tier::from_code(tier).map(|tier| (tier, bucket)))
}

/// Whether a series is stored (tests).
#[cfg(test)]
pub(super) fn series_count(connection: &Connection) -> rusqlite::Result<i64> {
    connection.query_row("SELECT count(*) FROM tier_series", [], |row| row.get(0))
}

fn sqlite_error(operation: &'static str, error: &rusqlite::Error) -> InfraError {
    InfraError::Database(format!("{operation}: {}", describe(error)))
}

fn tx_error(operation: &'static str, error: TxError<()>) -> InfraError {
    match error {
        TxError::Sqlite(error) | TxError::Commit(error) => sqlite_error(operation, &error),
        TxError::BusyTimeout { elapsed_ms } => InfraError::Timeout {
            operation,
            elapsed_ms,
        },
        TxError::Abort(()) => InfraError::Database(format!("{operation}: aborted")),
    }
}
