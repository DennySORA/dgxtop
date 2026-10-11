//! What the history's compression reads of the archive at startup (D-82):
//! the series earlier runs left, and one series' gauge minutes from every
//! boot, oldest first, a bounded page at a time. Only segments in the
//! metric's canonical unit count (units never mix), and only segments that
//! started before this run (its own minutes reach the history live).

use dgxtop_core::clock::format_utc_ns;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::{EntityId, MetricId, Scope};
use dgxtop_core::metric::descriptor;
use dgxtop_core::overview_history::{
    ArchivedMinutes, ArchivedMinutesRead, ArchivedSeriesList, OverviewBucket,
};
use rusqlite::{Connection, params};

use crate::sqlite::parse_utc_rfc3339_ns;

/// Series one listing returns at most.
const MAX_SERIES: i64 = 4096;
/// Minutes one read returns at most, whatever it asks.
const MAX_PAGE_MINUTES: u32 = 50_000;

const SELECT_SERIES: &str = "SELECT DISTINCT entity_id, metric_id, scope, unit \
     FROM history_segments \
     WHERE host_id = ?1 AND started_utc < ?2 \
     AND metric_id IN (SELECT value FROM json_each(?3)) \
     LIMIT ?4";
/// `+start_utc` keeps the planner on each segment's primary key.
const SELECT_MINUTES: &str = "SELECT b.start_utc, b.covered_ns, b.weighted_sum, b.minimum, \
     b.maximum, b.sample_count \
     FROM history_buckets AS b \
     WHERE b.segment_id IN (SELECT s.segment_id FROM history_segments AS s \
         WHERE s.host_id = ?1 AND s.entity_id = ?2 AND s.metric_id = ?3 AND s.scope = ?4 \
         AND s.unit = ?5 AND s.started_utc < ?6) \
     AND +b.start_utc >= ?7 AND b.method = 'time_weighted_step' \
     ORDER BY b.start_utc LIMIT ?8";

fn utc_text(utc_ns: i64) -> String {
    format_utc_ns(i128::from(utc_ns.max(0))).unwrap_or_default()
}

/// The series of the listed metrics, each in its canonical unit.
pub(super) fn series(
    connection: &Connection,
    list: &ArchivedSeriesList,
) -> rusqlite::Result<Vec<SeriesInEveryBoot>> {
    let metrics = serde_json::to_string(&list.metric_ids).unwrap_or_default();
    let mut statement = connection.prepare_cached(SELECT_SERIES)?;
    let rows = statement.query_map(
        params![
            list.host_id.as_str(),
            utc_text(list.before_utc_ns),
            metrics,
            MAX_SERIES
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    )?;
    let mut found = Vec::new();
    for row in rows {
        let (entity, metric, scope, unit) = row?;
        let canonical = descriptor(&metric).is_some_and(|known| known.unit.to_string() == unit);
        let parsed = (|| {
            Some(SeriesInEveryBoot {
                host_id: list.host_id.clone(),
                entity_id: EntityId::new(entity).ok()?,
                metric_id: MetricId::new(metric).ok()?,
                scope: Scope::new(scope).ok()?,
            })
        })();
        if let Some(series) = parsed.filter(|_| canonical)
            && !found.contains(&series)
        {
            found.push(series);
        }
    }
    Ok(found)
}

/// One page of a series' minutes, oldest first, each a bucket at its
/// minute's UTC start.
pub(super) fn minutes(
    connection: &Connection,
    read: &ArchivedMinutesRead,
) -> rusqlite::Result<ArchivedMinutes> {
    let series = &read.series;
    let Some(known) = descriptor(series.metric_id.as_str()) else {
        return Ok(ArchivedMinutes {
            minutes: Vec::new(),
            more: false,
        });
    };
    let page = read.max_minutes.clamp(1, MAX_PAGE_MINUTES);
    let mut statement = connection.prepare_cached(SELECT_MINUTES)?;
    let rows = statement.query_map(
        params![
            series.host_id.as_str(),
            series.entity_id.as_str(),
            series.metric_id.as_str(),
            series.scope.as_str(),
            known.unit.to_string(),
            utc_text(read.segments_before_utc_ns),
            utc_text(read.from_utc_ns),
            i64::from(page) + 1,
        ],
        |row| {
            Ok(minute_of(
                &row.get::<_, String>(0)?,
                &row.get::<_, String>(1)?,
                row.get::<_, Option<f64>>(2)?,
                (row.get::<_, Option<f64>>(3)?, row.get::<_, Option<f64>>(4)?),
                row.get::<_, u32>(5)?,
            ))
        },
    )?;
    let mut minutes: Vec<OverviewBucket> = rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let more = minutes.len() > usize::try_from(page).unwrap_or(usize::MAX);
    minutes.truncate(usize::try_from(page).unwrap_or(usize::MAX));
    Ok(ArchivedMinutes { minutes, more })
}

/// A stored minute as a bucket; `None` for one without time held or with a
/// malformed stamp or sum.
fn minute_of(
    start_utc: &str,
    covered_ns: &str,
    weighted_sum: Option<f64>,
    (minimum, maximum): (Option<f64>, Option<f64>),
    samples: u32,
) -> Option<OverviewBucket> {
    let start = i64::try_from(parse_utc_rfc3339_ns(start_utc)?).ok()?;
    let covered = covered_ns
        .parse::<u64>()
        .ok()
        .filter(|covered| *covered > 0)?;
    let weighted_sum = weighted_sum.filter(|sum| sum.is_finite())?;
    let minimum = minimum.filter(|value| value.is_finite())?;
    let maximum = maximum.filter(|value| value.is_finite())?;
    Some(OverviewBucket::gauge(
        start,
        covered,
        weighted_sum,
        (minimum, maximum),
        samples,
    ))
}
