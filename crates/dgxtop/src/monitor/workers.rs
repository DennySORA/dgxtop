//! The workers beside the Manager: the live state's actor and the archive
//! it feeds, the history's tier store and read pool, the collector pool and
//! the configuration, audit and action actors. Each is spawned through
//! [`Workers`], which keeps it for the bounded stop at shutdown.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use dgxtop_core::action::{ActionOp, AuditOp};
use dgxtop_core::canonical::CollectionPlan;
use dgxtop_core::config::Config;
use dgxtop_core::identity::HostId;
use dgxtop_core::overview_history::{HistoryTierCompletion, HistoryTierJob};
use dgxtop_core::ports::{
    ArchiveGapCause, ArchiveJournal, ArchiveOutcome, ArchiveReader, ArchiveReceipt, ArchiveSink,
    AuditRepository, Collector, ConfigJob, ControlCompletion, Dispatch, HistoryJob,
    HistoryTierStore, IngestionCompletion, LogLevel, Logger, OverviewReader, StoreJob,
};
use dgxtop_history::OverviewTiers;
use dgxtop_runtime::queues::{PoolHandle, spawn_actor, spawn_pool};
use dgxtop_state::StateWriter;
use dgxtop_store::archive::{ArchiveLimits, SqliteArchive, SqliteArchiveReader};
use dgxtop_store::audit::{AuditLimits, SqliteAuditRepository};
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::history_tiers::SqliteHistoryTiers;
use dgxtop_store::paths::StatePaths;
use dgxtop_store::sqlite::StatvfsFreeSpace;

use super::StartError;
use super::assemble::Wiring;
use crate::collection_ops::run_collection;
use crate::control_ops::{run_action_op, run_audit_op, run_config_job, run_history_job};
use crate::store_actor::run_store_job;
use crate::worker_queues::{
    ACTION_QUEUE, ARCHIVE_QUEUE, AUDIT_QUEUE, CONFIG_QUEUE, HISTORY_TIERS_QUEUE, STATE_QUEUE,
};

/// Bounded wait for a worker to hand a completion to the Manager.
pub(super) const COMPLETION_SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// A worker whose bounded stop is part of shutdown.
trait StopOnShutdown {
    fn stop_within(&self, timeout: Duration) -> Option<String>;
}

impl<J: Send + 'static> StopOnShutdown for PoolHandle<J> {
    fn stop_within(&self, timeout: Duration) -> Option<String> {
        let report = self.stop(timeout);
        (report.detached > 0).then(|| {
            format!(
                "worker {} still busy at shutdown ({} detached)",
                self.name(),
                report.detached
            )
        })
    }
}

/// Every worker spawned for this run, in spawn order: each one spawned
/// here is stopped (bounded) at shutdown, so none can be forgotten.
#[derive(Default)]
pub(super) struct Workers {
    spawned: Vec<Box<dyn StopOnShutdown>>,
}

impl Workers {
    /// Spawns a single-threaded actor owning `state` and tracks it.
    pub(super) fn actor<S, J, H>(
        &mut self,
        name: &str,
        capacity: usize,
        state: S,
        handler: H,
    ) -> Result<PoolHandle<J>, StartError>
    where
        S: Send + 'static,
        J: Send + 'static,
        H: FnMut(&mut S, J) + Send + 'static,
    {
        let handle = spawn_actor(name, capacity, state, handler)?;
        self.spawned.push(Box::new(handle.clone()));
        Ok(handle)
    }

    /// Spawns a pool of `threads` sharing `handler` and tracks it.
    pub(super) fn pool<J: Send + 'static>(
        &mut self,
        name: &str,
        (threads, capacity): (u32, u32),
        handler: Arc<dyn Fn(J) + Send + Sync>,
    ) -> Result<PoolHandle<J>, StartError> {
        let handle = spawn_pool(
            name,
            usize::try_from(threads).unwrap_or(1),
            usize::try_from(capacity).unwrap_or(1),
            handler,
        )?;
        self.spawned.push(Box::new(handle.clone()));
        Ok(handle)
    }

    /// Stops every worker in spawn order, each within the time `timeout`
    /// leaves it; returns a warning per worker still busy.
    pub(super) fn stop_each(&self, timeout: impl Fn() -> Duration) -> Vec<String> {
        self.spawned
            .iter()
            .filter_map(|worker| worker.stop_within(timeout()))
            .collect()
    }
}

/// Completion senders shared by every worker; sends are bounded in time.
#[derive(Clone)]
pub(super) struct Completions {
    pub(super) ingestion: Sender<IngestionCompletion>,
    pub(super) control: Sender<ControlCompletion>,
    pub(super) logger: Arc<dyn Logger>,
}

impl Completions {
    fn ingestion(&self, completion: IngestionCompletion) {
        if self
            .ingestion
            .send_timeout(completion, COMPLETION_SEND_TIMEOUT)
            .is_err()
        {
            self.logger.log(
                LogLevel::Warn,
                "composition",
                "manager ingestion inbox full or closed; completion dropped",
            );
        }
    }

    fn control(&self, completion: ControlCompletion) {
        if self
            .control
            .send_timeout(completion, COMPLETION_SEND_TIMEOUT)
            .is_err()
        {
            self.logger.log(
                LogLevel::Error,
                "composition",
                "manager control inbox full or closed; completion dropped",
            );
        }
    }
}

impl Wiring {
    pub(super) fn spawn_archive(
        &self,
        effective: &Config,
        paths: &StatePaths,
        completions: Completions,
        workers: &mut Workers,
    ) -> Result<Option<PoolHandle<ArchiveJournal>>, StartError> {
        if !self.archive_on(effective) {
            return Ok(None);
        }
        let archive = SqliteArchive::open(
            &paths.history_database,
            ArchiveLimits::from_config(effective),
            Arc::clone(&self.clock),
            Arc::new(StatvfsFreeSpace),
        )?;
        log_recreated(
            self.logger.as_ref(),
            &paths.history_database,
            archive.recreated(),
        );
        let logger = Arc::clone(&self.logger);
        let handle = workers.actor(
            "archive",
            ARCHIVE_QUEUE,
            archive,
            move |archive: &mut SqliteArchive, journal: ArchiveJournal| {
                let revision = journal.revision;
                let outcome = match archive.append(journal) {
                    Ok(receipt) => {
                        log_archive_deletion_failure(logger.as_ref(), &receipt);
                        ArchiveOutcome::Committed { rows: receipt.rows }
                    }
                    Err(error) => {
                        logger.log(
                            LogLevel::Warn,
                            "archive",
                            &format!(
                                "archive append failed (in-memory history continues): {error}"
                            ),
                        );
                        ArchiveOutcome::Gap {
                            cause: ArchiveGapCause::WriteFailed,
                            detail: error.to_string(),
                        }
                    }
                };
                completions.control(ControlCompletion::Archive {
                    revision,
                    outcome,
                    status: Some(archive.status()),
                });
            },
        )?;
        Ok(Some(handle))
    }

    pub(super) fn spawn_store(
        &self,
        writer: StateWriter,
        archive: Option<PoolHandle<ArchiveJournal>>,
        (completions, archive_bytes): (Completions, u64),
        workers: &mut Workers,
    ) -> Result<PoolHandle<StoreJob>, StartError> {
        let clock = Arc::clone(&self.clock);
        let fail_stop = self.fail_stop.clone();
        workers.actor(
            "hot-store",
            STATE_QUEUE,
            writer,
            move |writer: &mut StateWriter, job: StoreJob| {
                // After a fail-stop the writer's state is not trusted again.
                if fail_stop.is_halted() {
                    return;
                }
                fail_stop.guard("hot store", || {
                    if let Some(ack) = run_store_job(writer, job, clock.as_ref()) {
                        completions.ingestion(IngestionCompletion::Committed(ack));
                    }
                });
                if fail_stop.is_halted() {
                    return;
                }
                if let Some(archive) = &archive
                    && let Some(journal) = writer.take_archive_journal(archive_bytes)
                {
                    offer_archive_journal(archive, journal, &completions);
                }
            },
        )
    }

    /// The history's store of the Overview's sizes (D-82), on with the
    /// archive: one actor with the tiers database and a read-only reader of
    /// the archive, answering the history directly (the Manager never sees
    /// these jobs). Attached before the hot store starts, so live minutes
    /// wait for the startup load.
    pub(super) fn spawn_history_tiers(
        &self,
        effective: &Config,
        paths: &StatePaths,
        (overview, host_id): (OverviewTiers, HostId),
        workers: &mut Workers,
    ) -> Result<(), StartError> {
        if !self.archive_on(effective) {
            return Ok(());
        }
        let tiers = SqliteHistoryTiers::open(&paths.history_tiers_database);
        log_recreated(
            self.logger.as_ref(),
            &paths.history_tiers_database,
            tiers.recreated(),
        );
        let files = HistoryTierFiles {
            tiers,
            archive: SqliteArchiveReader::new(&paths.history_database),
        };
        let answer = overview.clone();
        let handle = workers.actor(
            "history-tiers",
            HISTORY_TIERS_QUEUE,
            files,
            move |files: &mut HistoryTierFiles, job: HistoryTierJob| {
                answer.on_store(files.run(job));
            },
        )?;
        let run_start_utc_ns = i64::try_from(self.clock.now_utc_ns()).unwrap_or(i64::MAX);
        overview.attach_store(
            Box::new(handle),
            host_id,
            run_start_utc_ns,
            Arc::clone(&self.logger),
        );
        Ok(())
    }

    pub(super) fn spawn_history(
        &self,
        effective: &Config,
        history: Arc<dyn OverviewReader>,
        completions: Completions,
        workers: &mut Workers,
    ) -> Result<PoolHandle<HistoryJob>, StartError> {
        let clock = Arc::clone(&self.clock);
        workers.pool(
            "history",
            (
                effective.runtime.history_workers,
                effective.queues.history_requests,
            ),
            Arc::new(move |job: HistoryJob| {
                completions.control(run_history_job(history.as_ref(), job, clock.as_ref()));
            }),
        )
    }

    pub(super) fn spawn_collectors(
        &self,
        effective: &Config,
        registry: Vec<Arc<dyn Collector>>,
        completions: Completions,
        workers: &mut Workers,
    ) -> Result<PoolHandle<CollectionPlan>, StartError> {
        workers.pool(
            "collector",
            (
                effective.runtime.compute_workers,
                effective.queues.collector_jobs,
            ),
            Arc::new(move |plan: CollectionPlan| {
                completions.ingestion(run_collection(&registry, &plan));
            }),
        )
    }

    pub(super) fn spawn_config(
        &self,
        repository: FileConfigRepository,
        completions: Completions,
        workers: &mut Workers,
    ) -> Result<PoolHandle<ConfigJob>, StartError> {
        workers.actor(
            "config",
            CONFIG_QUEUE,
            repository,
            move |repository: &mut FileConfigRepository, job: ConfigJob| {
                completions.control(run_config_job(repository, job));
            },
        )
    }

    pub(super) fn spawn_audit(
        &self,
        effective: &Config,
        paths: &StatePaths,
        completions: Completions,
        workers: &mut Workers,
    ) -> Result<PoolHandle<AuditOp>, StartError> {
        let repository: Box<dyn AuditRepository> = if self.sqlite_ok {
            let audit = SqliteAuditRepository::open(
                &paths.audit_database,
                AuditLimits::from_config(effective),
                Arc::clone(&self.clock),
                Arc::new(StatvfsFreeSpace),
            )?;
            log_recreated(
                self.logger.as_ref(),
                &paths.audit_database,
                audit.recreated(),
            );
            Box::new(audit)
        } else {
            Box::new(crate::audit_unavailable::UnavailableAudit::new(
                "SQLite runtime gate failed",
            ))
        };
        workers.actor(
            "audit",
            AUDIT_QUEUE,
            repository,
            move |repository: &mut Box<dyn AuditRepository>, op: AuditOp| {
                let completion = run_audit_op(repository.as_mut(), op);
                completions.control(ControlCompletion::Audit(completion));
            },
        )
    }

    pub(super) fn spawn_actions(
        &self,
        completions: Completions,
        workers: &mut Workers,
    ) -> Result<PoolHandle<ActionOp>, StartError> {
        let clock = Arc::clone(&self.clock);
        let adapter = dgxtop_adapters::process_action::PidfdActionAdapter::new(Arc::clone(&clock));
        let fail_stop = self.fail_stop.clone();
        workers.actor(
            "action",
            ACTION_QUEUE,
            adapter,
            move |port: &mut dgxtop_adapters::process_action::PidfdActionAdapter, op: ActionOp| {
                let halted = fail_stop.reason();
                let completion = run_action_op(port, op, clock.as_ref(), halted.as_deref());
                completions.control(ControlCompletion::Action(completion));
            },
        )
    }
}

/// Hands `journal` to the archive without waiting: a busy archive turns it
/// into an archive gap the Manager shows, never a stall of the state.
fn offer_archive_journal(
    archive: &PoolHandle<ArchiveJournal>,
    journal: ArchiveJournal,
    completions: &Completions,
) {
    let revision = journal.revision;
    if archive.try_dispatch(journal).is_ok() {
        return;
    }
    completions.logger.log(
        LogLevel::Warn,
        "archive",
        "archive worker busy; journal dropped and reported as an archive gap",
    );
    completions.control(ControlCompletion::Archive {
        revision,
        outcome: ArchiveOutcome::Gap {
            cause: ArchiveGapCause::WorkerBusy,
            detail: "archive worker busy".to_owned(),
        },
        status: None,
    });
}

/// The history tier actor's state (D-82): the tiers database and the
/// archive's reader, each answering the job the history sent.
struct HistoryTierFiles {
    tiers: SqliteHistoryTiers,
    archive: SqliteArchiveReader,
}

impl HistoryTierFiles {
    fn run(&mut self, job: HistoryTierJob) -> HistoryTierCompletion {
        match job {
            HistoryTierJob::Load => HistoryTierCompletion::Loaded(self.tiers.load()),
            HistoryTierJob::Append(journal) => {
                HistoryTierCompletion::Appended(self.tiers.append(&journal))
            }
            HistoryTierJob::ListArchived(list) => {
                HistoryTierCompletion::Listed(self.archive.archived_series(&list))
            }
            HistoryTierJob::ReadArchived(read) => {
                let result = self.archive.archived_minutes(&read);
                HistoryTierCompletion::ArchivedRead { read, result }
            }
        }
    }
}

/// One line for a database found unusable and recreated at open (D-85).
pub(super) fn log_recreated(logger: &dyn Logger, database: &Path, reason: Option<&str>) {
    if let Some(reason) = reason {
        logger.log(
            LogLevel::Info,
            "store",
            &format!(
                "{} deleted and created afresh (D-85): {reason}",
                database.display()
            ),
        );
    }
}

/// One warning when the deletions a committed archive journal asked for
/// failed: the rows are durable, the deletions wait for the next request
/// (D-96).
fn log_archive_deletion_failure(logger: &dyn Logger, receipt: &ArchiveReceipt) {
    if let Some(error) = &receipt.deletion_failure {
        logger.log(
            LogLevel::Warn,
            "archive",
            &format!(
                "ARCHIVE_DELETION_FAILED ({}): journal {} committed, its deletions wait for the next request: {error}",
                error.code().as_str(),
                receipt.revision
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A committed journal whose deletions failed is one warning with the
    /// error code; a clean receipt says nothing.
    #[test]
    fn a_failed_archive_deletion_is_logged_with_its_code() {
        let logger = dgxtop_core::testing::MemoryLogger::default();
        let mut receipt = ArchiveReceipt {
            revision: 7,
            rows: 3,
            deletion_failure: None,
        };
        log_archive_deletion_failure(&logger, &receipt);
        assert!(logger.records().is_empty());
        receipt.deletion_failure = Some(dgxtop_core::errors::InfraError::Database(
            "archive.maintenance: locked".to_owned(),
        ));
        log_archive_deletion_failure(&logger, &receipt);
        let records = logger.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, LogLevel::Warn);
        assert!(
            records[0]
                .2
                .starts_with("ARCHIVE_DELETION_FAILED (PersistFailed): journal 7 committed"),
            "{records:?}"
        );
    }

    /// A recreated database is one info line naming the file and the
    /// reason; a database opened as it was says nothing.
    #[test]
    fn a_recreated_database_is_logged_once() {
        let logger = dgxtop_core::testing::MemoryLogger::default();
        let path = Path::new("/state/acme/history.sqlite3");
        log_recreated(&logger, path, None);
        assert!(logger.records().is_empty());
        log_recreated(&logger, path, Some("no dgxtop format version"));
        let records = logger.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, LogLevel::Info);
        assert!(
            records[0]
                .2
                .contains("/state/acme/history.sqlite3 deleted and created afresh")
        );
        assert!(records[0].2.contains("no dgxtop format version"));
    }

    /// Every worker spawned through `Workers` is stopped at shutdown, an
    /// actor and a pool alike, each asked for its own share of the time.
    #[test]
    fn every_spawned_worker_is_stopped_at_shutdown() {
        let mut workers = Workers::default();
        let actor = workers
            .actor("acme-actor", 2, (), |_: &mut (), _: u8| {})
            .unwrap();
        let pool = workers
            .pool("acme-pool", (1, 2), Arc::new(|_: u8| {}))
            .unwrap();
        let asked = AtomicUsize::new(0);
        let warnings = workers.stop_each(|| {
            asked.fetch_add(1, Ordering::SeqCst);
            Duration::from_millis(500)
        });
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        assert!(actor.try_dispatch(1).is_err(), "the actor still takes jobs");
        assert!(pool.try_dispatch(1).is_err(), "the pool still takes jobs");
    }

    /// A worker still busy when its time runs out is named in a warning.
    #[test]
    fn a_worker_busy_at_shutdown_is_named() {
        let mut workers = Workers::default();
        let (started, running) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded::<()>(1);
        let actor = workers
            .actor("acme-busy", 2, (), move |_: &mut (), _: u8| {
                let _ = started.send(());
                let _ = wait.recv();
            })
            .unwrap();
        actor.try_dispatch(1).unwrap();
        running.recv().unwrap();
        let warnings = workers.stop_each(|| Duration::from_millis(20));
        let _ = release.send(());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("acme-busy"), "{warnings:?}");
    }
}
