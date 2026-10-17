//! Rows of the process history database (D-76, D-77): runs upserted by
//! their key, closed buckets packed into 12 bytes a metric, the deletions
//! the history's journal asks for, and the reads at startup and for one
//! run as its requests say (D-94: no policy here). Only prepared
//! statements with bound parameters.

use std::time::Duration;

use dgxtop_core::action::Signal;
use dgxtop_core::identity::{BootId, HostId, Pid, PidNamespace, ProcessKey};
use dgxtop_core::process_history::{
    Aggregate, BucketRequest, EndCause, KeepLatest, ProcessHistoryJournal,
    ProcessHistoryLoadRequest, RUN_METRICS, RunBucket, RunBucketRow, RunDetails, RunEnd, RunInfo,
    StoreUsage,
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use crate::sqlite::{
    BucketTable, DatabaseFailure, StepError, checkpoint, immediate_transaction, used_bytes,
};

/// A transaction waits this long for the write lock at most.
const BUSY_DEADLINE: Duration = Duration::from_millis(500);
const PROCESS_BUCKETS: BucketTable = BucketTable {
    table: "process_buckets",
    owner_column: "run_id",
};
const AGGREGATE_BYTES: usize = 12;
const PACKED_BYTES: usize = AGGREGATE_BYTES * RUN_METRICS;

const RUN_COLUMNS: &str = "run_id, host_id, boot_id, pid_namespace, pid, start_time_ticks, \
    name, command, cwd, uid, gpus, started_utc_ns, last_seen_utc_ns, ended_utc_ns, \
    end_cause, end_code, parent_pid, parent_name, executable, cgroup, nice";

/// Applies the journal in one transaction: upserts its runs, inserts its
/// buckets (a bucket written again replaces the earlier one; one of a run
/// not stored is left out), keeps the latest buckets it names, deletes the
/// runs it forgets and those beyond the runs it keeps. Returns what the
/// database holds then.
pub(super) fn append(
    connection: &mut Connection,
    journal: &ProcessHistoryJournal,
) -> Result<StoreUsage, DatabaseFailure> {
    let usage = immediate_transaction(connection, BUSY_DEADLINE, |transaction| {
        for run in &journal.runs {
            upsert_run(transaction, run)?;
        }
        for row in &journal.buckets {
            insert_bucket(transaction, row)?;
        }
        for keep in &journal.keep_latest {
            keep_latest(transaction, keep)?;
        }
        for key in &journal.forget {
            forget(transaction, key)?;
        }
        if let Some(runs) = journal.keep_runs {
            keep_runs(transaction, runs)?;
        }
        Ok::<_, StepError<()>>(usage(transaction)?)
    })
    .map_err(DatabaseFailure::transaction("process_history.append"))?;
    checkpoint(connection).map_err(DatabaseFailure::statement("process_history.checkpoint"))?;
    Ok(usage)
}

fn insert_bucket(transaction: &Transaction<'_>, row: &RunBucketRow) -> rusqlite::Result<()> {
    let Some(run_id) = run_id(transaction, &row.key)? else {
        return Ok(());
    };
    transaction
        .prepare_cached(
            "INSERT OR REPLACE INTO process_buckets \
             (run_id, tier, start_utc_ns, aggregates) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            run_id,
            row.tier.code(),
            row.bucket.start_utc_ns,
            pack(&row.bucket)
        ])?;
    Ok(())
}

fn keep_latest(transaction: &Transaction<'_>, keep: &KeepLatest) -> rusqlite::Result<()> {
    let Some(run_id) = run_id(transaction, &keep.key)? else {
        return Ok(());
    };
    let buckets = u64::try_from(keep.buckets).unwrap_or(u64::MAX);
    PROCESS_BUCKETS.keep_latest(transaction, run_id, keep.tier.code(), buckets)
}

/// Deletes a run; its buckets go with it (`ON DELETE CASCADE`).
fn forget(transaction: &Transaction<'_>, key: &ProcessKey) -> rusqlite::Result<()> {
    transaction
        .prepare_cached(
            "DELETE FROM process_runs WHERE host_id = ?1 AND boot_id = ?2 \
             AND pid_namespace = ?3 AND pid = ?4 AND start_time_ticks = ?5",
        )?
        .execute(key_params(key))?;
    Ok(())
}

/// Deletes every run but the `runs` last seen.
fn keep_runs(transaction: &Transaction<'_>, runs: usize) -> rusqlite::Result<()> {
    transaction
        .prepare_cached(
            "DELETE FROM process_runs WHERE run_id NOT IN \
             (SELECT run_id FROM process_runs ORDER BY last_seen_utc_ns DESC LIMIT ?1)",
        )?
        .execute(params![i64::try_from(runs).unwrap_or(i64::MAX)])?;
    Ok(())
}

fn usage(transaction: &Transaction<'_>) -> rusqlite::Result<StoreUsage> {
    let runs: i64 = transaction
        .prepare_cached("SELECT count(*) FROM process_runs")?
        .query_row([], |row| row.get(0))?;
    Ok(StoreUsage {
        used_bytes: used_bytes(transaction)?,
        runs: u64::try_from(runs).unwrap_or(0),
    })
}

/// The runs `request` asks for (the last seen first), and the buckets of
/// those still unfinished and of the ended ones it asks buckets of.
pub(super) fn load(
    connection: &mut Connection,
    request: &ProcessHistoryLoadRequest,
) -> Result<(Vec<RunInfo>, Vec<RunBucketRow>), DatabaseFailure> {
    let limit = i64::try_from(request.runs).unwrap_or(i64::MAX);
    let mut statement = connection
        .prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM process_runs ORDER BY last_seen_utc_ns DESC LIMIT ?1"
        ))
        .map_err(DatabaseFailure::statement("process_history.load"))?;
    let failed = DatabaseFailure::statement("process_history.load");
    let runs: Vec<(i64, RunInfo)> = statement
        .query_map(params![limit], run_of)
        .map_err(&failed)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(&failed)?
        .into_iter()
        .flatten()
        .collect();
    drop(statement);
    let mut ended = 0;
    let mut buckets = Vec::new();
    for (run_id, info) in &runs {
        if info.end.is_some() {
            ended += 1;
            if ended > request.ended_runs_with_buckets {
                continue;
            }
        }
        buckets.extend(buckets_of(
            connection,
            (*run_id, &info.key),
            &request.buckets,
        )?);
    }
    Ok((runs.into_iter().map(|(_, info)| info).collect(), buckets))
}

/// One run's buckets, as `request` asks.
pub(super) fn read_run(
    connection: &mut Connection,
    key: &ProcessKey,
    request: &BucketRequest,
) -> Result<Vec<RunBucketRow>, DatabaseFailure> {
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
        .map_err(DatabaseFailure::statement("process_history.read_run"))?;
    match found {
        Some(run_id) => buckets_of(connection, (run_id, key), request),
        None => Ok(Vec::new()),
    }
}

/// One run's buckets: of each size `request` names, in its order, the
/// latest it asks for, oldest first. A bucket this format never writes
/// fails the read.
fn buckets_of(
    connection: &Connection,
    (run_id, key): (i64, &ProcessKey),
    request: &BucketRequest,
) -> Result<Vec<RunBucketRow>, DatabaseFailure> {
    let failed = DatabaseFailure::statement("process_history.buckets");
    let mut statement = connection
        .prepare_cached(
            "SELECT start_utc_ns, aggregates FROM process_buckets \
             WHERE run_id = ?1 AND tier = ?2 ORDER BY start_utc_ns DESC LIMIT ?3",
        )
        .map_err(&failed)?;
    let mut rows = Vec::new();
    for (tier, latest) in &request.latest {
        let (tier, limit) = (*tier, i64::try_from(*latest).unwrap_or(i64::MAX));
        let latest = statement
            .query_map(params![run_id, tier.code(), limit], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(&failed)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(&failed)?;
        for (start, packed) in latest.into_iter().rev() {
            let bucket = unpack(start, &packed).ok_or_else(|| DatabaseFailure::Row {
                operation: "process_history.buckets",
                detail: format!(
                    "run {run_id} holds a {tier:?} bucket of {} bytes, not a whole number of \
                     metrics up to {PACKED_BYTES}",
                    packed.len()
                ),
            })?;
            rows.push(RunBucketRow {
                key: key.clone(),
                tier,
                bucket,
            });
        }
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

/// Per metric: count, mean and peak, little-endian, through the last
/// metric with a sample: a run on one GPU leaves the other GPUs' figures
/// out (D-120).
pub(super) fn pack(bucket: &RunBucket) -> Vec<u8> {
    let used = bucket
        .values
        .iter()
        .rposition(|aggregate| aggregate.count > 0)
        .map_or(1, |last| last + 1);
    let mut packed = Vec::with_capacity(used * AGGREGATE_BYTES);
    for aggregate in &bucket.values[..used] {
        #[allow(clippy::cast_possible_truncation)]
        let mean = aggregate.mean().unwrap_or(0.0) as f32;
        packed.extend(aggregate.count.to_le_bytes());
        packed.extend(mean.to_le_bytes());
        packed.extend(aggregate.max.to_le_bytes());
    }
    packed
}

/// A stored bucket, its metrics in [`RunMetric`] order, those it leaves
/// out (or a version before them did not know) empty; `None` for any other
/// layout, more metrics than this version knows among them: the load then
/// fails with its reason and nothing is deleted (D-120).
///
/// [`RunMetric`]: dgxtop_core::process_history::RunMetric
pub(super) fn unpack(start_utc_ns: i64, packed: &[u8]) -> Option<RunBucket> {
    if packed.is_empty()
        || packed.len() > PACKED_BYTES
        || !packed.len().is_multiple_of(AGGREGATE_BYTES)
    {
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
