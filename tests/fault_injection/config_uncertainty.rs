//! AT36 (versioned configuration persistence with uncertain outcomes): the
//! Manager's ConfigurationService driving the real `FileConfigRepository`
//! through the config actor, over a failpoint filesystem.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dgxtop_core::action::PersistenceState;
use dgxtop_core::budget::ByteLedger;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::commands::{CommandBody, CommandResult, CommandStatus, ViewKind};
use dgxtop_core::config::Config;
use dgxtop_core::errors::{ErrorCode, ResultCode};
use dgxtop_core::ports::ConfigRepository;
use dgxtop_core::quality::Namespace;
use dgxtop_core::view::{SettingsView, ViewModel};
use dgxtop_manager::{ManagerChannels, ManagerPorts};
use dgxtop_runtime::clock::BootClock;
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::fs::{Failpoint, FailpointFileSystem, FileSystemOps, RealFileSystem, Trigger};

use crate::support::filesystem::FaultyFileSystem;
use crate::support::manager_rig::{
    Identity, ProcessTableSnapshots, RunningManager, inert_ports, ingress_gate, manager_setup,
};
use crate::support::probes::{TempDir, serial};
use crate::support::workers::{Completions, WorkerSet, spawn_config_worker};

const PROJECTION_WAIT: Duration = Duration::from_secs(5);

/// Manager + config actor on `config.json` inside a private directory.
struct ConfigRig {
    manager: RunningManager,
    _workers: WorkerSet,
    config_path: PathBuf,
    _dir: TempDir,
}

impl ConfigRig {
    fn start(dir: TempDir, filesystem: Arc<dyn FileSystemOps>) -> Self {
        let config_path = config_path(&dir);
        let mut repository = FileConfigRepository::new(config_path.clone(), filesystem);
        let saved = repository.load().expect("defaults load").config;
        let clock: Arc<dyn TimeProvider> = Arc::new(BootClock::new().expect("boot clock"));
        let identity = Identity::fault_host(clock.as_ref());
        let ledger = ByteLedger::for_memory_limit_mib(u64::from(saved.store.memory_limit_mib));
        let channels = ManagerChannels::create();
        let mut workers = WorkerSet::default();
        let config_worker =
            spawn_config_worker(repository, Completions::from_inboxes(&channels.inboxes()));
        workers.track(&config_worker);
        let ports = ManagerPorts {
            config: Box::new(config_worker),
            ..inert_ports(
                Arc::clone(&clock),
                ProcessTableSnapshots::with_processes(Vec::new()),
                &ledger,
                ingress_gate(&saved, &ledger),
            )
        };
        let setup = manager_setup(&saved, &identity, Vec::new(), Namespace::Live);
        let manager = RunningManager::start(setup, ports, channels);
        manager.show(ViewKind::Settings);
        Self {
            manager,
            _workers: workers,
            config_path,
            _dir: dir,
        }
    }

    fn set_interval(&self, interval_ms: u32, expected_config_revision: u32) -> CommandResult {
        self.manager.execute(CommandBody::SetSamplingInterval {
            interval_ms,
            expected_config_revision,
        })
    }

    /// Newest projection whose settings view shows `revision`.
    fn settings_at(&self, revision: u32) -> (Arc<ViewModel>, Arc<SettingsView>) {
        let projection = self.manager.projection_where(
            &format!("settings revision {revision}"),
            PROJECTION_WAIT,
            |model| {
                model
                    .modules
                    .settings
                    .as_deref()
                    .is_some_and(|settings| settings.revision == revision)
            },
        );
        let settings = projection.modules.settings.clone().expect("checked");
        (projection, settings)
    }

    /// What a restarted dgxtop would load from disk.
    fn reload_from_disk(&self) -> Config {
        FileConfigRepository::new(self.config_path.clone(), Arc::new(RealFileSystem))
            .load()
            .expect("the file on disk is complete and valid")
            .config
    }
}

fn config_path(dir: &TempDir) -> PathBuf {
    dir.join("dgxtop").join("v2").join("config.json")
}

fn assert_code(result: &CommandResult, code: ErrorCode) {
    assert_eq!(result.code, ResultCode::from(code), "{result:?}");
}

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at36_uncertain_commit_adopts_the_disk_version_and_cas_blocks_concurrent_writers() {
    let _serial = serial();
    let failpoints = Arc::new(FailpointFileSystem::over_real());
    let rig = ConfigRig::start(TempDir::new("config-cas"), failpoints.clone());

    // The rename lands but the directory fsync fails: the outcome is
    // uncertain, so the Manager reloads what is on disk instead of claiming
    // either the old or the new file.
    failpoints.arm(Failpoint::DirectorySyncAfterRename, Trigger::Once);
    let uncertain = rig.set_interval(2_000, 1);
    assert_eq!(failpoints.fired(Failpoint::DirectorySyncAfterRename), 1);
    assert_code(&uncertain, ErrorCode::PersistFailed);
    assert!(
        uncertain.message.contains("commit_uncertain"),
        "{uncertain:?}"
    );
    assert!(
        uncertain
            .message
            .contains("adopted the on-disk configuration")
    );
    assert!(!uncertain.message.contains("unchanged"));
    assert_eq!(uncertain.config_revision, Some(2));
    let (projection, settings) = rig.settings_at(2);
    assert_eq!(projection.header.configured_interval_ms, 2_000);
    assert_eq!(settings.config_status.state, PersistenceState::Degraded);
    assert!(
        settings
            .last_result
            .as_deref()
            .is_some_and(|text| text.contains("adopted on-disk revision 2")),
        "{settings:?}"
    );
    let on_disk = rig.reload_from_disk();
    assert_eq!((on_disk.revision, on_disk.sampling.interval_ms), (2, 2_000));

    // CAS: a command still based on revision 1 cannot overwrite revision 2.
    assert_code(&rig.set_interval(3_000, 1), ErrorCode::Conflict);
    let committed = rig.set_interval(3_000, 2);
    assert!(committed.code.is_ok(), "{committed:?}");
    assert_eq!(committed.config_revision, Some(3));

    // A concurrent writer (another dgxtop on the same file) commits first:
    // the Manager's next commit is rejected and the file keeps the other
    // writer's version.
    let mut other_process =
        FileConfigRepository::new(rig.config_path.clone(), Arc::new(RealFileSystem));
    let mut theirs = other_process.load().expect("load").config;
    assert_eq!(theirs.revision, 3);
    theirs.viewer.fps = 2;
    assert_eq!(
        other_process
            .compare_and_swap(3, &theirs)
            .expect("the other writer commits")
            .revision,
        4
    );
    let lost_race = rig.set_interval(4_000, 3);
    assert_eq!(lost_race.status, CommandStatus::Rejected);
    assert_code(&lost_race, ErrorCode::Conflict);
    let on_disk = rig.reload_from_disk();
    assert_eq!(on_disk.revision, 4);
    assert_eq!(on_disk.viewer.fps, 2, "the other writer's version survives");
    assert_eq!(on_disk.sampling.interval_ms, 3_000, "never overwritten");
}

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at36_uncertain_commit_that_cannot_be_reloaded_never_claims_the_old_file() {
    let _serial = serial();
    let dir = TempDir::new("config-unreadable");
    let filesystem = Arc::new(FaultyFileSystem::unreadable_after_rename_of(config_path(
        &dir,
    )));
    filesystem
        .failpoints
        .arm(Failpoint::DirectorySyncAfterRename, Trigger::Once);
    let rig = ConfigRig::start(dir, filesystem.clone());

    let uncertain = rig.set_interval(2_000, 1);
    assert_eq!(
        filesystem
            .failpoints
            .fired(Failpoint::DirectorySyncAfterRename),
        1
    );
    assert_code(&uncertain, ErrorCode::PersistFailed);
    assert!(
        uncertain.message.contains("commit_uncertain"),
        "{uncertain:?}"
    );
    // The new document may well be in place: it is, and a restart loads it.
    let on_disk = rig.reload_from_disk();
    assert_eq!((on_disk.revision, on_disk.sampling.interval_ms), (2, 2_000));
    // So the answer must not claim the old configuration or revision is
    // what is stored: it must never claim the old version is still there.
    assert!(
        !uncertain.message.contains("unchanged"),
        "claims an unchanged configuration: {uncertain:?}"
    );
    assert_eq!(
        uncertain.config_revision, None,
        "the stored revision is unknown, not the old one"
    );
}
