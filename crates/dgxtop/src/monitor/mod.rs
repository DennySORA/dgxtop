//! Monitor mode: wires every component and runs the terminal frontend.
//!
//! Startup order: CLI → safe paths and the
//! instance lock → configuration and admission → SQLite gate → store workers
//! → Manager and adapter hosts → frontend (published before slow sources
//! warm up). Shutdown is bounded: frontend, Manager, workers, hosts.
//!
//! * [`assemble`]: the shared pieces and the order everything is built in.
//! * [`workers`]: the state, store and control workers, tracked for the
//!   bounded stop.
//! * [`process`]: the history's process actor with its probe and stores.
//! * [`hosts`]: the adapter hosts of a live run or a replay.
//! * [`signals`]: termination signals and the bounded frontend wait.
//! * [`snapshot`]: the headless frontend of `--snapshot-json`.

mod assemble;
mod hosts;
mod process;
mod signals;
mod snapshot;
mod workers;

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::ports::{CatalogReader, LogLevel, Logger, SessionPort, SnapshotReader};
use dgxtop_core::quality::Namespace;
use dgxtop_core::snapshot_report::SnapshotReport;
use dgxtop_core::view::HostState;
use dgxtop_manager::presentation::HostFacts;
use dgxtop_manager::{SessionHandle, ShutdownTrigger};
use dgxtop_runtime::clock::BootClock;
use dgxtop_runtime::logger::FileLogger;
use dgxtop_runtime::supervisor::SupervisorHandle;
use dgxtop_runtime::terminal::{NonBlockingTerminal, TtyInputGate};
use dgxtop_store::config::copy_trusted_document;
use dgxtop_store::fs::RealFileSystem;
use dgxtop_store::lock::InstanceLock;
use dgxtop_store::paths::StatePaths;
use dgxtop_store::sqlite::ensure_sqlite_version;

use crate::cli::Cli;
use crate::fail_stop::FailStop;
use crate::modes::{EXIT_FAILURE, EXIT_INVALID, fail, print_stdout};
use crate::startup;

use self::assemble::{AssembleInputs, Wiring};
use self::snapshot::{Reporting, WaitError, Waiting};
use self::workers::Workers;

/// Bound of all application-controlled waits at shutdown together, after
/// the frontend exits.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(3);
/// Part of the budget kept for flushing the log after everything else.
const LOG_FLUSH_RESERVE: Duration = Duration::from_millis(250);
/// Longest wait for one idle worker pool.
const WORKER_STOP_CAP: Duration = Duration::from_millis(500);

/// Runs the monitor until the user quits, a signal arrives or the terminal hangs up.
pub fn run(cli: &Cli) -> ExitCode {
    let mut application = match Application::start(cli) {
        Ok(application) => application,
        Err(error) => return fail(error.exit_code, &error.message),
    };
    let frontend = application.run_frontend();
    application.finish().unwrap_or(frontend)
}

/// Prints one snapshot of what the monitor reads as JSON and exits
/// (`--snapshot-json`, D-140): the readers run headless in a private
/// state until their readings are complete, and standard output carries
/// the document alone, without what names the host, its hardware and its
/// users where `--redact` asks so (D-145).
pub fn snapshot(cli: &Cli) -> ExitCode {
    let mut application = match Application::start(cli) {
        Ok(application) => application,
        Err(error) => return fail(error.exit_code, &error.message),
    };
    let report = application.take_snapshot();
    if let Some(failed) = application.finish() {
        return failed;
    }
    let report = report.map(|report| {
        if cli.redact {
            report.redacted()
        } else {
            report
        }
    });
    match report.map(|report| serde_json::to_string_pretty(&report)) {
        Ok(Ok(json)) => print_stdout(&format!("{json}\n")),
        Ok(Err(error)) => fail(EXIT_FAILURE, &format!("snapshot: {error}")),
        Err(error) => fail(EXIT_FAILURE, error.message()),
    }
}

/// What a headless run reads the live state through (D-140).
struct LiveReaders {
    snapshots: Arc<dyn SnapshotReader>,
    catalog: Arc<dyn CatalogReader>,
    clock: Arc<dyn TimeProvider>,
    facts: HostFacts,
    namespace: Namespace,
    interval_ms: u32,
    /// The longest period of a read group in this run.
    slow_period_ms: u32,
}

/// A startup failure with its exit status.
#[derive(Debug)]
struct StartError {
    exit_code: u8,
    message: String,
}

impl StartError {
    /// What the user asked for cannot be run (bad paths, configuration or
    /// replay fixture).
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            exit_code: EXIT_INVALID,
            message: message.into(),
        }
    }

    fn failure(message: impl Into<String>) -> Self {
        Self {
            exit_code: EXIT_FAILURE,
            message: message.into(),
        }
    }
}

/// Any other error stops the start as a failure; the cases the user can fix
/// say so with [`StartError::invalid`].
impl<E: fmt::Display> From<E> for StartError {
    fn from(error: E) -> Self {
        Self::failure(error.to_string())
    }
}

struct Application {
    session: SessionHandle,
    trigger: ShutdownTrigger,
    frontend_finished: Arc<AtomicBool>,
    fail_stop: FailStop,
    /// `None` result: the control thread panicked (fail-stop).
    manager_thread: Option<std::thread::JoinHandle<Option<dgxtop_manager::ShutdownSummary>>>,
    supervisor: SupervisorHandle,
    workers: Workers,
    logger: Arc<FileLogger>,
    fps: u32,
    readers: LiveReaders,
    /// The state root of this run alone (a replay's or a snapshot's),
    /// removed when it ends.
    private_root: PrivateRoot,
    _lock: InstanceLock,
}

impl Application {
    fn start(cli: &Cli) -> Result<Self, StartError> {
        let namespace = startup::namespace_of(cli);
        let private_root = PrivateRoot::of(cli)?;
        let paths = resolve_paths(cli, private_root.path())?;
        let filesystem = startup::real_filesystem();
        paths.ensure_directories(filesystem.as_ref())?;
        if cli.snapshot_json && cli.replay.is_none() {
            copy_saved_configuration(cli, &paths)?;
        }
        let lock = InstanceLock::acquire(&paths.state_dir)?;
        let (mut loaded, config_repository) =
            startup::load_configuration(cli, &paths, Arc::clone(&filesystem))
                .map_err(|error| StartError::invalid(error.to_string()))?;
        if cli.snapshot_json {
            snapshot::read_slow_groups_soon(&mut loaded.effective.sampling);
        }
        let clock: Arc<dyn dgxtop_core::clock::TimeProvider> = Arc::new(BootClock::new()?);
        let logger = Arc::new(FileLogger::start(
            &paths.log_file,
            usize::try_from(loaded.effective.queues.log_bytes).unwrap_or(1 << 20),
            10 * 1024 * 1024,
            3,
            Arc::clone(&clock),
        )?);
        startup::remove_unneeded_files(filesystem.as_ref(), &paths, logger.as_ref());
        startup::log_retired_keys(&loaded.saved, logger.as_ref());
        let sqlite_ok = match ensure_sqlite_version() {
            Ok(_) => true,
            Err(error) => {
                logger.log(
                    LogLevel::Error,
                    "composition",
                    &format!("SQLite gate failed; audit and archive disabled: {error}"),
                );
                false
            }
        };
        let wiring = Wiring::new(
            &loaded,
            (clock, Arc::clone(&logger) as Arc<dyn Logger>),
            namespace,
            sqlite_ok,
        );
        wiring.assemble(AssembleInputs {
            cli,
            paths: &paths,
            loaded: &loaded,
            config_repository,
            lock,
            logger,
            private_root,
        })
    }

    /// Waits until the readings are complete (every rate, every read
    /// group, every host) and takes the document of that moment (D-140).
    fn take_snapshot(&self) -> Result<SnapshotReport, WaitError> {
        let readers = &self.readers;
        let stopped = || self.session.shutdown_requested() || self.fail_stop.reason().is_some();
        let starting = || {
            self.supervisor
                .statuses()
                .iter()
                .any(|host| host.state == HostState::Starting)
        };
        let snapshot = Waiting {
            snapshots: readers.snapshots.as_ref(),
            clock: readers.clock.as_ref(),
            interval_ms: readers.interval_ms,
            slow_period_ms: readers.slow_period_ms,
            starting: &starting,
            stopped: &stopped,
            pause: &mut std::thread::sleep,
        }
        .snapshot();
        // No frontend is left for a termination signal to wait for.
        self.frontend_finished.store(true, Ordering::Release);
        let adapters = self.supervisor.statuses();
        let reporting = Reporting {
            catalog: readers.catalog.as_ref(),
            clock: readers.clock.as_ref(),
            facts: &readers.facts,
            namespace: readers.namespace,
            interval_ms: readers.interval_ms,
            adapters: &adapters,
        };
        snapshot.map(|snapshot| reporting.report(&snapshot))
    }

    /// Stops everything within the shutdown budget and tells what was
    /// left behind; the failure's exit status where the run fail-stopped.
    fn finish(&mut self) -> Option<ExitCode> {
        let summary = self.shutdown();
        // The terminal may be gone (hang-up), so stderr failures are ignored.
        let mut stderr = std::io::stderr();
        for warning in summary {
            let _ = writeln!(
                stderr,
                "dgxtop: {}",
                dgxtop_core::text::sanitize_message(&warning)
            );
        }
        self.fail_stop
            .reason()
            .map(|reason| fail(EXIT_FAILURE, &format!("fail-stop: {reason}")))
    }

    fn run_frontend(&mut self) -> ExitCode {
        dgxtop_ui::install_panic_hook();
        let terminal = NonBlockingTerminal::from_borrowed(std::io::stdout())
            .and_then(|transport| Ok((transport, TtyInputGate::controlling_terminal()?)));
        let (transport, input_gate) = match terminal {
            Ok(terminal) => terminal,
            Err(error) => {
                self.frontend_finished.store(true, Ordering::Release);
                return fail(EXIT_FAILURE, &format!("terminal unavailable: {error}"));
            }
        };
        let options = dgxtop_ui::TuiOptions {
            fps: self.fps,
            ..dgxtop_ui::TuiOptions::default()
        };
        let outcome = dgxtop_ui::run_tui(
            Box::new(self.session.clone()),
            Box::new(transport),
            Box::new(input_gate),
            options,
        );
        self.frontend_finished.store(true, Ordering::Release);
        dgxtop_ui::restore_terminal();
        match outcome {
            Ok(_) => ExitCode::SUCCESS,
            Err(error) => fail(EXIT_FAILURE, &error.to_string()),
        }
    }

    /// Bounded shutdown; returns warnings about anything left behind.
    fn shutdown(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        let total = ShutdownDeadline::start(SHUTDOWN_BUDGET);
        let control = ShutdownDeadline::start(SHUTDOWN_BUDGET.saturating_sub(LOG_FLUSH_RESERVE));
        self.trigger.trigger("frontend exited");
        if let Some(thread) = self.manager_thread.take() {
            match join_within(thread, control.remaining(SHUTDOWN_BUDGET)) {
                Some(Some(summary)) => {
                    if summary.timed_out {
                        warnings.push(
                            "manager shutdown ended before every control completion arrived"
                                .to_owned(),
                        );
                    }
                    warnings.extend(
                        summary
                            .unfinished
                            .into_iter()
                            .map(|item| format!("unfinished at shutdown: {item}")),
                    );
                }
                Some(None) => warnings.push("manager stopped by a fail-stop".to_owned()),
                None => {
                    warnings.push("manager did not stop within the 3 s shutdown budget".to_owned())
                }
            }
        }
        warnings.extend(
            self.workers
                .stop_each(|| control.remaining(WORKER_STOP_CAP)),
        );
        let report = self.supervisor.shutdown(control.remaining(SHUTDOWN_BUDGET));
        if !report.completed_within_deadline {
            warnings
                .push("adapter hosts did not all stop within the 3 s shutdown budget".to_owned());
        }
        for host in report.unreaped() {
            warnings.push(format!("adapter host may still be running: {host:?}"));
        }
        let _ = self.logger.flush(total.remaining(LOG_FLUSH_RESERVE));
        let _ = self.logger.stop(total.remaining(LOG_FLUSH_RESERVE));
        self.private_root.remove();
        warnings
    }
}

/// One deadline shared by consecutive shutdown waits, so their sum stays
/// within the budget instead of each wait getting the whole budget.
struct ShutdownDeadline {
    until: Instant,
}

impl ShutdownDeadline {
    fn start(budget: Duration) -> Self {
        Self {
            until: Instant::now() + budget,
        }
    }

    /// Time left before the deadline, at most `cap` (zero once it passed).
    fn remaining(&self, cap: Duration) -> Duration {
        self.until
            .saturating_duration_since(Instant::now())
            .min(cap)
    }
}

/// Joins a thread or gives up after `timeout` (the thread is left detached).
fn join_within<T: Send + 'static>(
    thread: std::thread::JoinHandle<T>,
    timeout: Duration,
) -> Option<T> {
    let (sender, receiver) = bounded(1);
    let joiner = std::thread::Builder::new()
        .name("dgxtop-join".to_owned())
        .spawn(move || {
            let _ = sender.send(thread.join());
        })
        .ok()?;
    let joined = receiver.recv_timeout(timeout).ok()?.ok();
    drop(joiner);
    joined
}

/// The private temporary state root of a replay or a snapshot (never the
/// live state, so neither meets the monitor's lock or its files); `None`
/// for the monitor, which keeps its state. It is removed when the run
/// ends, and when a start fails after it was made.
#[derive(Debug)]
struct PrivateRoot(Option<PathBuf>);

impl PrivateRoot {
    fn of(cli: &Cli) -> Result<Self, StartError> {
        cli.keeps_private_state()
            .then(|| private_state_root(cli))
            .transpose()
            .map(Self)
    }

    fn path(&self) -> Option<&Path> {
        self.0.as_deref()
    }

    fn remove(&mut self) {
        if let Some(root) = self.0.take() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

impl Drop for PrivateRoot {
    fn drop(&mut self) {
        self.remove();
    }
}

fn private_state_root(cli: &Cli) -> Result<PathBuf, StartError> {
    let label = if cli.replay.is_some() {
        "replay"
    } else {
        "snapshot"
    };
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let root = std::env::temp_dir().join(format!("dgxtop-{label}-{}-{unique}", std::process::id()));
    let failed =
        |error: std::io::Error| StartError::failure(format!("{label} state directory: {error}"));
    std::fs::create_dir(&root).map_err(failed)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).map_err(failed)?;
    Ok(root)
}

/// A snapshot samples as the saved configuration says (a host whose GPU
/// adapter is turned off stays unread), from a copy in its private root:
/// the owner's file is only ever read, under the rules a load trusts it
/// by. Without a saved file the defaults stand.
fn copy_saved_configuration(cli: &Cli, private: &StatePaths) -> Result<(), StartError> {
    let saved = startup::resolve_paths(cli)
        .map_err(|error| StartError::invalid(error.to_string()))?
        .config_file;
    copy_trusted_document(&RealFileSystem, &saved, &private.config_file)
        .map(|_copied| ())
        .map_err(|error| StartError::invalid(format!("configuration error: {error}")))
}

fn resolve_paths(cli: &Cli, private_root: Option<&Path>) -> Result<StatePaths, StartError> {
    match private_root {
        Some(root) => {
            let mut private_cli = cli.clone();
            private_cli.config = Some(root.join("config/config.json"));
            private_cli.state_dir = Some(root.join("state"));
            startup::resolve_paths(&private_cli)
                .map_err(|error| StartError::invalid(error.to_string()))
        }
        None => startup::resolve_paths(cli).map_err(|error| StartError::invalid(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_waits_share_one_deadline() {
        let deadline = ShutdownDeadline::start(Duration::from_millis(200));
        assert!(deadline.remaining(Duration::from_secs(3)) <= Duration::from_millis(200));
        assert_eq!(
            deadline.remaining(Duration::from_millis(10)),
            Duration::from_millis(10)
        );
        std::thread::sleep(Duration::from_millis(220));
        assert_eq!(deadline.remaining(Duration::from_secs(3)), Duration::ZERO);
        let spent = ShutdownDeadline::start(Duration::ZERO);
        assert_eq!(spent.remaining(Duration::from_secs(1)), Duration::ZERO);
    }

    /// An error without a cause the user can fix is a failure, its message
    /// kept; a user's mistake is invalid.
    #[test]
    fn a_start_error_is_a_failure_unless_said_invalid() {
        let failure = StartError::from(std::io::Error::other("lock held by acme"));
        assert_eq!(failure.exit_code, EXIT_FAILURE);
        assert_eq!(failure.message, "lock held by acme");
        let invalid = StartError::invalid("--replay requires a fixture");
        assert_eq!(invalid.exit_code, EXIT_INVALID);
    }
}
