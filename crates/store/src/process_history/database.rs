//! Rows of the process history database (D-76, D-77): runs upserted by
//! their key, closed buckets packed into 12 bytes a metric, retention, and
//! the reads at startup and for one run. Only prepared statements with
//! bound parameters.

use std::time::Duration;

use dgxtop_core::action::Signal;
use dgxtop_core::errors::InfraError;
use dgxtop_core::identity::{BootId, HostId, Pid, PidNamespace, ProcessKey};
use dgxtop_core::process_history::{
    Aggregate, EndCause, MAX_LISTED_RUNS, ProcessHistoryJournal, RUN_METRICS, RunBucket,
    RunBucketRow, RunDetails, RunEnd, RunInfo, Tier,
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use crate::sqlite::{StepError, TxError, describe, immediate_transaction};

/// A transaction waits this long for the write lock at most.
const BUSY_DEADLINE: Duration = Duration::from_millis(500);
const AGGREGATE_BYTES: usize = 12;
const PACKED_BYTES: usize = AGGREGATE_BYTES * RUN_METRICS;
/// Ended runs whose buckets the startup load returns, the latest first.
const ENDED_RUNS_WITH_BUCKETS: i64 = 32;
/// Runs kept at most; the ones last seen longest ago give way.
const MAX_STORED_RUNS: i64 = 2000;
/// Ended runs a pass over a database near its size gives way at once.
const RUNS_GIVING_WAY: i64 = 50;

const RUN_COLUMNS: &str = "run_id, host_id, boot_id, pid_namespace, pid, start_time_ticks, \
    name, command, cwd, uid, gpus, started_utc_ns, last_seen_utc_ns, ended_utc_ns, \
    end_cause, end_code, parent_pid, parent_name, executable, cgroup, nice";

/// Upserts the journal's runs, then inserts its buckets (a bucket written
/// again replaces the earlier one).
pub(super) fn append(
    connection: &mut Connection,
    journal: &ProcessHistoryJournal,
) -> Result<(), InfraError> {
    if journal.is_empty() {
        return Ok(());
    }
    immediate_transaction(connection, BUSY_DEADLINE, |transaction| {
        for run in &journal.runs {
            upsert_run(transaction, run)?;
        }
        let mut touched = std::collections::BTreeSet::new();
        for row in &journal.buckets {
            let Some(tier) = stored_tier(row.tier) else {
                continue;
            };
            let Some(run_id) = run_id(transaction, &row.key)? else {
                continue;
            };
            touched.insert((run_id, row.tier));
            transaction
                .prepare_cached(
                    "INSERT OR REPLACE INTO process_buckets \
                     (run_id, tier, start_utc_ns, aggregates) VALUES (?1, ?2, ?3, ?4)",
                )?
                .execute(params![
                    run_id,
                    tier,
                    row.bucket.start_utc_ns,
                    pack(&row.bucket)
                ])?;
        }
        for (run_id, tier) in touched {
            keep_latest(transaction, run_id, tier)?;
        }
        Ok::<_, StepError<()>>(())
    })
    .map_err(|error| tx_error("process_history.append", error))?;
    // The archive profile leaves checkpoints to the writer.
    connection
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
        .map_err(|error| sqlite_error("process_history.checkpoint", &error))
}

/// Each size keeps a run's latest [`Tier::capacity`] buckets (D-77): the
/// older ones are in the coarser sizes already.
fn keep_latest(transaction: &Transaction<'_>, run_id: i64, tier: Tier) -> rusqlite::Result<()> {
    let keep = i64::try_from(tier.capacity()).unwrap_or(i64::MAX);
    transaction
        .prepare_cached(
            "DELETE FROM process_buckets WHERE run_id = ?1 AND tier = ?2 AND start_utc_ns < \
             (SELECT start_utc_ns FROM process_buckets WHERE run_id = ?1 AND tier = ?2 \
              ORDER BY start_utc_ns DESC LIMIT 1 OFFSET ?3)",
        )?
        .execute(params![run_id, tier.code(), keep - 1])?;
    Ok(())
}

/// Runs beyond the cap, and while the database holds more than
/// `max_used_bytes`, the ended runs last seen longest ago (D-77: no run
/// expires by age).
pub(super) fn retain(connection: &mut Connection, max_used_bytes: u64) -> Result<(), InfraError> {
    immediate_transaction(connection, BUSY_DEADLINE, |transaction| {
        transaction.execute(
            "DELETE FROM process_runs WHERE run_id NOT IN \
             (SELECT run_id FROM process_runs ORDER BY last_seen_utc_ns DESC LIMIT ?1)",
            params![MAX_STORED_RUNS],
        )?;
        while used_bytes(transaction)? > max_used_bytes {
            let removed = transaction.execute(
                "DELETE FROM process_runs WHERE run_id IN (SELECT run_id FROM process_runs \
                 WHERE ended_utc_ns IS NOT NULL ORDER BY last_seen_utc_ns LIMIT ?1)",
                params![RUNS_GIVING_WAY],
            )?;
            if removed == 0 {
                break;
            }
        }
        Ok::<_, StepError<()>>(())
    })
    .map_err(|error| tx_error("process_history.retain", error))
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

/// The runs the history lists (the last seen first), and the buckets of
/// those still unfinished and of the latest ended ones.
pub(super) fn load(
    connection: &mut Connection,
) -> Result<(Vec<RunInfo>, Vec<RunBucketRow>), InfraError> {
    let limit = i64::try_from(MAX_LISTED_RUNS).unwrap_or(i64::MAX);
    let mut statement = connection
        .prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM process_runs ORDER BY last_seen_utc_ns DESC LIMIT ?1"
        ))
        .map_err(|error| sqlite_error("process_history.load", &error))?;
    let runs: Vec<(i64, RunInfo)> = statement
        .query_map(params![limit], run_of)
        .map_err(|error| sqlite_error("process_history.load", &error))?
        .filter_map(Result::ok)
        .flatten()
        .collect();
    drop(statement);
    let mut ended = 0;
    let mut buckets = Vec::new();
    for (run_id, info) in &runs {
        if info.end.is_some() {
            ended += 1;
            if ended > ENDED_RUNS_WITH_BUCKETS {
                continue;
            }
        }
        buckets.extend(buckets_of(connection, *run_id, &info.key)?);
    }
    Ok((runs.into_iter().map(|(_, info)| info).collect(), buckets))
}

/// One run's buckets, oldest first.
pub(super) fn read_run(
    connection: &mut Connection,
    key: &ProcessKey,
) -> Result<Vec<RunBucketRow>, InfraError> {
    let found = connection
        .prepare_cached(
            "SELECT run_id FROM process_runs WHERE host_id = ?1 AND boot_id = ?2 \
             AND pid_namespace = ?3 AND pid = ?4 AND start_time_ticks = ?5",
        )
        .and_then(|mut statement| {
            statement
                .query_row(key_params(key), |row| row.get::<_, i64>(0))
                .optional()
        })
        .map_err(|error| sqlite_error("process_history.read_run", &error))?;
    match found {
        Some(run_id) => buckets_of(connection, run_id, key),
        None => Ok(Vec::new()),
    }
}

fn buckets_of(
    connection: &Connection,
    run_id: i64,
    key: &ProcessKey,
) -> Result<Vec<RunBucketRow>, InfraError> {
    let read = |sql: &str, (limit, tier): (i64, i64)| -> Result<Vec<RunBucketRow>, InfraError> {
        let mut statement = connection
            .prepare_cached(sql)
            .map_err(|error| sqlite_error("process_history.buckets", &error))?;
        let rows = statement
            .query_map(params![run_id, limit, tier], |row| {
                let tier: i64 = row.get(0)?;
                let start: i64 = row.get(1)?;
                let packed: Vec<u8> = row.get(2)?;
                Ok(Tier::from_code(tier).zip(unpack(start, &packed)))
            })
            .map_err(|error| sqlite_error("process_history.buckets", &error))?;
        Ok(rows
            .filter_map(Result::ok)
            .flatten()
            .map(|(tier, bucket)| RunBucketRow {
                key: key.clone(),
                tier,
                bucket,
            })
            .collect())
    };
    let mut rows = Vec::new();
    // Coarsest first, each oldest first.
    for tier in Tier::ALL.into_iter().skip(1).rev() {
        // The latest of each tier, as many as the history keeps in memory.
        let mut latest = read(
            "SELECT tier, start_utc_ns, aggregates FROM process_buckets \
             WHERE run_id = ?1 AND tier = ?3 ORDER BY start_utc_ns DESC LIMIT ?2",
            (
                i64::try_from(tier.capacity()).unwrap_or(i64::MAX),
                tier.code(),
            ),
        )?;
        latest.reverse();
        rows.extend(latest);
    }
    Ok(rows)
}

fn upsert_run(transaction: &Transaction<'_>, run: &RunInfo) -> rusqlite::Result<()> {
    let (cause, code) = run.end.map_or((None, None), |end| cause_columns(end.cause));
    let key = &run.key;
    let details = &run.details;
    transaction
        .prepare_cached(
            "INSERT INTO process_runs (host_id, boot_id, pid_namespace, pid, start_time_ticks, \
             name, command, cwd, uid, gpus, started_utc_ns, last_seen_utc_ns, ended_utc_ns, \
             end_cause, end_code, parent_pid, parent_name, executable, cgroup, nice) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19, ?20) \
             ON CONFLICT (host_id, boot_id, pid_namespace, pid, start_time_ticks) DO UPDATE SET \
             name = excluded.name, command = excluded.command, cwd = excluded.cwd, \
             uid = excluded.uid, gpus = excluded.gpus, started_utc_ns = excluded.started_utc_ns, \
             last_seen_utc_ns = excluded.last_seen_utc_ns, ended_utc_ns = excluded.ended_utc_ns, \
             end_cause = excluded.end_cause, end_code = excluded.end_code, \
             parent_pid = excluded.parent_pid, parent_name = excluded.parent_name, \
             executable = excluded.executable, cgroup = excluded.cgroup, nice = excluded.nice",
        )?
        .execute(params![
            key.host_id.as_str(),
            key.boot_id.as_str(),
            key.pid_namespace.as_str(),
            key.pid.get(),
            i64::try_from(key.start_time_ticks).unwrap_or(i64::MAX),
            run.name,
            run.command,
            run.cwd,
            run.uid,
            run.gpus.join("\n"),
            run.started_utc_ns,
            run.last_seen_utc_ns,
            run.end.map(|end| end.utc_ns),
            cause,
            code,
            details.parent_pid,
            details.parent_name,
            details.executable,
            details.cgroup,
            details.nice,
        ])?;
    Ok(())
}

fn run_id(transaction: &Transaction<'_>, key: &ProcessKey) -> rusqlite::Result<Option<i64>> {
    transaction
        .prepare_cached(
            "SELECT run_id FROM process_runs WHERE host_id = ?1 AND boot_id = ?2 \
             AND pid_namespace = ?3 AND pid = ?4 AND start_time_ticks = ?5",
        )?
        .query_row(key_params(key), |row| row.get(0))
        .optional()
}

fn key_params(key: &ProcessKey) -> (&str, &str, &str, i32, i64) {
    (
        key.host_id.as_str(),
        key.boot_id.as_str(),
        key.pid_namespace.as_str(),
        key.pid.get(),
        i64::try_from(key.start_time_ticks).unwrap_or(i64::MAX),
    )
}

/// A run row; `None` for one whose identity no longer parses (kept on
/// disk, left out of the history).
fn run_of(row: &Row<'_>) -> rusqlite::Result<Option<(i64, RunInfo)>> {
    let identity = (|| -> Option<ProcessKey> {
        Some(ProcessKey {
            host_id: HostId::new(row.get::<_, String>(1).ok()?).ok()?,
            boot_id: BootId::new(row.get::<_, String>(2).ok()?).ok()?,
            pid_namespace: PidNamespace::new(row.get::<_, String>(3).ok()?).ok()?,
            pid: Pid::new(row.get::<_, i64>(4).ok()?).ok()?,
            start_time_ticks: u64::try_from(row.get::<_, i64>(5).ok()?).ok()?,
        })
    })();
    let Some(key) = identity else {
        return Ok(None);
    };
    let gpus: String = row.get(10)?;
    let ended: Option<i64> = row.get(13)?;
    let cause: Option<String> = row.get(14)?;
    let code: Option<i64> = row.get(15)?;
    let end = ended
        .zip(cause.and_then(|cause| cause_of(&cause, code)))
        .map(|(utc_ns, cause)| RunEnd { utc_ns, cause });
    Ok(Some((
        row.get(0)?,
        RunInfo {
            key,
            name: row.get(6)?,
            command: row.get(7)?,
            cwd: row.get(8)?,
            uid: row
                .get::<_, Option<i64>>(9)?
                .and_then(|uid| u32::try_from(uid).ok()),
            gpus: gpus
                .split('\n')
                .filter(|gpu| !gpu.is_empty())
                .map(str::to_owned)
                .collect(),
            started_utc_ns: row.get(11)?,
            last_seen_utc_ns: row.get(12)?,
            end,
            details: RunDetails {
                parent_pid: row
                    .get::<_, Option<i64>>(16)?
                    .and_then(|pid| i32::try_from(pid).ok()),
                parent_name: row.get(17)?,
                executable: row.get(18)?,
                cgroup: row.get(19)?,
                nice: row
                    .get::<_, Option<i64>>(20)?
                    .and_then(|nice| i8::try_from(nice).ok()),
            },
        },
    )))
}

fn cause_columns(cause: EndCause) -> (Option<&'static str>, Option<i64>) {
    let (text, code) = match cause {
        EndCause::SignaledByDgxtop(Signal::Sigterm) => ("dgxtop_sigterm", None),
        EndCause::SignaledByDgxtop(Signal::Sigkill) => ("dgxtop_sigkill", None),
        EndCause::Signal(signal) => ("signal", Some(i64::from(signal))),
        EndCause::Exit(code) => ("exit", Some(i64::from(code))),
        EndCause::OutOfMemory => ("oom", None),
        EndCause::NotVisible => ("not_visible", None),
        EndCause::WhileAway => ("while_away", None),
        EndCause::HostRestarted => ("host_restarted", None),
    };
    (Some(text), code)
}

fn cause_of(text: &str, code: Option<i64>) -> Option<EndCause> {
    let code = || code.and_then(|code| u8::try_from(code).ok());
    Some(match text {
        "dgxtop_sigterm" => EndCause::SignaledByDgxtop(Signal::Sigterm),
        "dgxtop_sigkill" => EndCause::SignaledByDgxtop(Signal::Sigkill),
        "signal" => EndCause::Signal(code()?),
        "exit" => EndCause::Exit(code()?),
        "oom" => EndCause::OutOfMemory,
        "not_visible" => EndCause::NotVisible,
        "while_away" => EndCause::WhileAway,
        "host_restarted" => EndCause::HostRestarted,
        _ => return None,
    })
}

/// Every size of a minute and more is stored; the 5 s buckets are the
/// short-term file's.
fn stored_tier(tier: Tier) -> Option<i64> {
    (tier != Tier::Recent).then(|| tier.code())
}

/// Per metric: count, mean and peak, little-endian.
pub(super) fn pack(bucket: &RunBucket) -> Vec<u8> {
    let mut packed = Vec::with_capacity(PACKED_BYTES);
    for aggregate in &bucket.values {
        #[allow(clippy::cast_possible_truncation)]
        let mean = aggregate.mean().unwrap_or(0.0) as f32;
        packed.extend(aggregate.count.to_le_bytes());
        packed.extend(mean.to_le_bytes());
        packed.extend(aggregate.max.to_le_bytes());
    }
    packed
}

/// A stored bucket, every metric in [`RunMetric`] order; `None` for any
/// other layout (a file of another format is recreated, D-85).
///
/// [`RunMetric`]: dgxtop_core::process_history::RunMetric
pub(super) fn unpack(start_utc_ns: i64, packed: &[u8]) -> Option<RunBucket> {
    if packed.len() != PACKED_BYTES {
        return None;
    }
    let mut bucket = RunBucket::new(start_utc_ns);
    for (value, chunk) in bucket
        .values
        .iter_mut()
        .zip(packed.chunks_exact(AGGREGATE_BYTES))
    {
        let count = u32::from_le_bytes(chunk[0..4].try_into().ok()?);
        let mean = f32::from_le_bytes(chunk[4..8].try_into().ok()?);
        let max = f32::from_le_bytes(chunk[8..12].try_into().ok()?);
        *value = if count == 0 {
            Aggregate::default()
        } else {
            Aggregate::from_parts(count, f64::from(mean), f64::from(max))
        };
    }
    Some(bucket)
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
