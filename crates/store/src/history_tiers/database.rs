//! Rows of the tiers database (D-82): series the history let go deleted,
//! series upserted by their identity, closed buckets written as the history
//! hands them, each (series, size) cut to the latest the journal asks for,
//! the series written longest ago
//! giving way past the size limit, and the read at startup. Only prepared
//! statements with bound parameters.

use std::collections::BTreeSet;
use std::time::Duration;

use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::{EntityId, HostId, MetricId, Scope};
use dgxtop_core::overview_history::{OverviewBucket, StoredTierSeries, TierJournal, TierRow};
use dgxtop_core::process_history::Tier;
use rusqlite::{Connection, Row, Transaction, params};

use crate::sqlite::{
    BucketTable, DatabaseFailure, StepError, checkpoint, immediate_transaction, used_bytes,
};

/// A transaction waits this long for the write lock at most.
const BUSY_DEADLINE: Duration = Duration::from_millis(500);
const TIER_BUCKETS: BucketTable = BucketTable {
    table: "tier_buckets",
    owner_column: "series_id",
};
/// Series a pass over a database past its size gives way at once.
const SERIES_GIVING_WAY: i64 = 1;

/// Deletes the series the journal forgets, writes its buckets (one written
/// again replaces the earlier), then keeps the latest `keep_latest` of
/// every (series, size) it touched.
pub(super) fn append(
    connection: &mut Connection,
    journal: &TierJournal,
) -> Result<(), DatabaseFailure> {
    if journal.rows.is_empty() && journal.forget.is_empty() {
        return Ok(());
    }
    immediate_transaction(connection, BUSY_DEADLINE, |transaction| {
        for series in &journal.forget {
            delete_series(transaction, series)?;
        }
        let mut touched = BTreeSet::new();
        for row in &journal.rows {
            let Some(tier) = stored_tier(row.tier) else {
                continue;
            };
            let series_id = upsert_series(transaction, row)?;
            touched.insert((series_id, tier));
            insert_bucket(transaction, series_id, tier, &row.bucket)?;
        }
        // The older buckets are in the coarser sizes already.
        let keep = u64::from(journal.keep_latest);
        for (series_id, tier) in touched {
            TIER_BUCKETS.keep_latest(transaction, series_id, tier, keep)?;
        }
        Ok::<_, StepError<()>>(())
    })
    .map_err(DatabaseFailure::transaction("history_tiers.append"))?;
    checkpoint(connection).map_err(DatabaseFailure::statement("history_tiers.checkpoint"))
}

/// Deletes one series and every bucket of it.
fn delete_series(
    transaction: &Transaction<'_>,
    series: &SeriesInEveryBoot,
) -> rusqlite::Result<()> {
    let identity = params![
        series.host_id.as_str(),
        series.entity_id.as_str(),
        series.metric_id.as_str(),
        series.scope.as_str(),
    ];
    transaction
        .prepare_cached(
            "DELETE FROM tier_buckets WHERE series_id IN (SELECT series_id FROM tier_series \
             WHERE host_id = ?1 AND entity_id = ?2 AND metric_id = ?3 AND scope = ?4)",
        )?
        .execute(identity)?;
    transaction
        .prepare_cached(
            "DELETE FROM tier_series \
             WHERE host_id = ?1 AND entity_id = ?2 AND metric_id = ?3 AND scope = ?4",
        )?
        .execute(identity)?;
    Ok(())
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

/// While the database holds more than `max_used_bytes`, the series written
/// longest ago give way (D-82: none expires by age).
pub(super) fn retain(
    connection: &mut Connection,
    max_used_bytes: u64,
) -> Result<(), DatabaseFailure> {
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
    .map_err(DatabaseFailure::transaction("history_tiers.retain"))
}

/// Every series with its buckets, each size oldest first. A series whose
/// identity no longer parses is left out (kept on disk).
pub(super) fn load(connection: &mut Connection) -> Result<Vec<StoredTierSeries>, DatabaseFailure> {
    let mut statement = connection
        .prepare("SELECT series_id, host_id, entity_id, metric_id, scope FROM tier_series")
        .map_err(DatabaseFailure::statement("history_tiers.load"))?;
    let series: Vec<(i64, SeriesInEveryBoot)> = statement
        .query_map([], series_of)
        .map_err(DatabaseFailure::statement("history_tiers.load"))?
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
) -> Result<Vec<(Tier, OverviewBucket)>, DatabaseFailure> {
    let mut statement = connection
        .prepare_cached(
            "SELECT tier, start_utc_ns, covered_ns, weighted_sum, minimum, maximum, \
             sample_count, total FROM tier_buckets WHERE series_id = ?1 \
             ORDER BY tier, start_utc_ns",
        )
        .map_err(DatabaseFailure::statement("history_tiers.buckets"))?;
    let rows = statement
        .query_map(params![series_id], bucket_of)
        .map_err(DatabaseFailure::statement("history_tiers.buckets"))?;
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
