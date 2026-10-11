//! Configuration repository: CAS, atomic commit protocol under injected
//! failures (AT08, AT36), load safety and legacy import (AT18).

// Test helpers panic on setup failures; the lints target production code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dgxtop_core::action::PersistenceState;
use dgxtop_core::config::{Config, ConfigError, LegacyMappingStatus, MAX_CONFIG_REVISION, Theme};
use dgxtop_core::digest::sha256_hex;
use dgxtop_core::errors::InfraError;
use dgxtop_core::ports::{ConfigLoadError, ConfigRepository, LoadOrigin, PersistError};
use dgxtop_store::config::{FileConfigRepository, LegacyImportError};
use dgxtop_store::fs::{Failpoint, FailpointFileSystem, FileSystemOps, RealFileSystem, Trigger};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-config-it-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn config_path(&self) -> PathBuf {
        self.0.join("dgxtop").join("v2").join("config.json")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn real() -> Arc<dyn FileSystemOps> {
    Arc::new(RealFileSystem)
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

fn directory_entries(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A repository with one committed revision (2) on disk.
fn committed(dir: &TempDir, fs: Arc<dyn FileSystemOps>) -> (FileConfigRepository, Config) {
    let mut repository = FileConfigRepository::new(dir.config_path(), fs);
    let mut candidate = repository.load().unwrap().config;
    candidate.sampling.interval_ms = 2000;
    let receipt = repository.compare_and_swap(1, &candidate).unwrap();
    assert_eq!(receipt.revision, 2);
    candidate.revision = 2;
    (repository, candidate)
}

#[test]
fn missing_file_loads_defaults_and_writes_nothing() {
    let dir = TempDir::new("missing");
    let mut repository = FileConfigRepository::new(dir.config_path(), real());
    let loaded = repository.load().unwrap();
    assert_eq!(loaded.origin, LoadOrigin::Defaults);
    assert_eq!(loaded.config, Config::defaults().unwrap());
    assert!(
        !dir.path().join("dgxtop").exists(),
        "load must not create anything"
    );
    assert_eq!(repository.status().state, PersistenceState::Healthy);
}

#[test]
fn cas_success_increments_revision_with_private_modes() {
    let dir = TempDir::new("success");
    let mut repository = FileConfigRepository::new(dir.config_path(), real());
    let mut candidate = repository.load().unwrap().config;
    candidate.sampling.interval_ms = 2500;
    candidate.viewer.theme = Theme::Amber;
    let receipt = repository.compare_and_swap(1, &candidate).unwrap();
    assert_eq!(receipt.revision, 2);
    assert_eq!(receipt.path, dir.config_path().display().to_string());
    let bytes = std::fs::read(dir.config_path()).unwrap();
    assert_eq!(receipt.sha256, sha256_hex(&bytes));
    assert_eq!(receipt.file_bytes, u64::try_from(bytes.len()).unwrap());
    assert_eq!(mode(&dir.config_path()), 0o600);
    assert_eq!(mode(dir.config_path().parent().unwrap()), 0o700);
    assert_eq!(mode(&dir.path().join("dgxtop")), 0o700);
    let reloaded = repository.load().unwrap();
    assert_eq!(reloaded.config.revision, 2);
    assert_eq!(reloaded.config.sampling.interval_ms, 2500);
    assert_eq!(
        reloaded.origin,
        LoadOrigin::File {
            path: receipt.path.clone(),
            sha256: receipt.sha256.clone()
        }
    );
    let status = repository.status();
    assert_eq!(status.state, PersistenceState::Healthy);
    assert_eq!(status.bytes_used, u64::try_from(bytes.len()).unwrap());
    let mut next = reloaded.config;
    next.viewer.fps = 2;
    assert_eq!(repository.compare_and_swap(2, &next).unwrap().revision, 3);
    assert_eq!(
        directory_entries(dir.config_path().parent().unwrap()),
        vec!["config.json"]
    );
}

#[test]
fn stale_revision_conflicts() {
    let dir = TempDir::new("stale");
    let (mut repository, current) = committed(&dir, real());
    let mut stale = current.clone();
    stale.revision = 1;
    assert_eq!(
        repository.compare_and_swap(1, &stale),
        Err(PersistError::Conflict { current: 2 })
    );
    // The candidate must carry the expected revision too.
    assert_eq!(
        repository.compare_and_swap(2, &stale),
        Err(PersistError::Conflict { current: 2 })
    );
    assert_eq!(repository.load().unwrap().config.revision, 2);
}

#[test]
fn external_concurrent_modification_conflicts() {
    let dir = TempDir::new("external");
    let (mut repository, current) = committed(&dir, real());
    let mut external = current.clone();
    external.revision = 7;
    std::fs::write(dir.config_path(), external.to_pretty_json().unwrap()).unwrap();
    let mut candidate = current;
    candidate.viewer.fps = 1;
    assert_eq!(
        repository.compare_and_swap(2, &candidate),
        Err(PersistError::Conflict { current: 7 })
    );
    assert_eq!(repository.load().unwrap().config, external);
}

#[test]
fn another_process_holding_the_commit_lock_times_out_cleanly() {
    let dir = TempDir::new("lock");
    let (repository, current) = committed(&dir, real());
    drop(repository);
    let held = RealFileSystem
        .try_lock_directory(dir.config_path().parent().unwrap())
        .unwrap()
        .unwrap();
    let mut repository = FileConfigRepository::new(dir.config_path(), real());
    let result = repository.compare_and_swap(2, &current);
    assert!(matches!(
        result,
        Err(PersistError::Io(InfraError::Timeout {
            operation: "config.lock",
            ..
        }))
    ));
    drop(held);
    assert_eq!(
        repository.compare_and_swap(2, &current).unwrap().revision,
        3
    );
}

#[test]
fn two_repositories_cannot_both_commit_the_same_revision() {
    let dir = TempDir::new("race");
    let mut first = FileConfigRepository::new(dir.config_path(), real());
    let mut second = FileConfigRepository::new(dir.config_path(), real());
    let base = first.load().unwrap().config;
    assert_eq!(second.load().unwrap().config, base);
    let mut one = base.clone();
    one.viewer.fps = 2;
    let mut two = base;
    two.viewer.fps = 3;
    assert_eq!(first.compare_and_swap(1, &one).unwrap().revision, 2);
    assert_eq!(
        second.compare_and_swap(1, &two),
        Err(PersistError::Conflict { current: 2 })
    );
    assert_eq!(second.load().unwrap().config.viewer.fps, 2);
}

fn assert_failed_before_rename(failpoint: Failpoint, expected: impl Fn(&PersistError) -> bool) {
    let dir = TempDir::new("before-rename");
    let fs = Arc::new(FailpointFileSystem::over_real());
    let (mut repository, current) = committed(&dir, fs.clone());
    let old_bytes = std::fs::read(dir.config_path()).unwrap();
    fs.arm(failpoint, Trigger::Once);
    let mut candidate = current.clone();
    candidate.viewer.fps = 1;
    let error = repository.compare_and_swap(2, &candidate).unwrap_err();
    assert!(expected(&error), "{failpoint:?}: unexpected {error:?}");
    assert_eq!(fs.fired(failpoint), 1);
    assert_eq!(
        std::fs::read(dir.config_path()).unwrap(),
        old_bytes,
        "old file intact"
    );
    assert_eq!(
        directory_entries(dir.config_path().parent().unwrap()),
        vec!["config.json"],
        "the temporary file was removed"
    );
    assert_eq!(repository.status().state, PersistenceState::Degraded);
    assert_eq!(repository.load().unwrap().config, current);
    // The failure was not sticky: the same commit succeeds afterwards.
    assert_eq!(
        repository.compare_and_swap(2, &candidate).unwrap().revision,
        3
    );
    assert_eq!(repository.status().state, PersistenceState::Healthy);
}

fn io_failure(operation: &'static str) -> impl Fn(&PersistError) -> bool {
    move |error| matches!(error, PersistError::Io(InfraError::Io { operation: failed, .. }) if *failed == operation)
}

/// Each step of the commit before the rename fails once (AT08, AT36).
#[test]
fn a_failure_before_the_rename_keeps_the_old_file() {
    assert_failed_before_rename(Failpoint::CreateFile, io_failure("config.create_temp"));
    assert_failed_before_rename(Failpoint::WriteNoSpace, |error| {
        matches!(
            error,
            PersistError::Io(InfraError::DiskFull {
                operation: "config.write"
            })
        )
    });
    assert_failed_before_rename(Failpoint::FileSync, io_failure("config.fsync"));
    assert_failed_before_rename(Failpoint::Rename, io_failure("config.rename"));
}

#[test]
fn directory_fsync_failure_after_rename_is_uncertain_with_the_new_content() {
    let dir = TempDir::new("uncertain");
    let fs = Arc::new(FailpointFileSystem::over_real());
    let (mut repository, current) = committed(&dir, fs.clone());
    fs.arm(Failpoint::DirectorySyncAfterRename, Trigger::Once);
    let mut candidate = current;
    candidate.viewer.fps = 1;
    let error = repository.compare_and_swap(2, &candidate).unwrap_err();
    let PersistError::Uncertain { message, on_disk } = error else {
        panic!("expected Uncertain, got {error:?}");
    };
    assert!(message.contains("directory fsync"));
    let on_disk = on_disk.expect("the repository reloads what is on disk");
    assert_eq!(
        on_disk.revision, 3,
        "the renamed (new) document is reported"
    );
    assert_eq!(on_disk.viewer.fps, 1);
    assert_eq!(repository.status().state, PersistenceState::Degraded);
    // A restart sees the complete new version.
    let mut restarted = FileConfigRepository::new(dir.config_path(), real());
    assert_eq!(restarted.load().unwrap().config, *on_disk);
}

#[test]
fn crash_after_persist_before_activation_reloads_the_new_version() {
    let dir = TempDir::new("crash");
    let (repository, current) = committed(&dir, real());
    // The process dies after the durable commit but before the runtime
    // swapped epochs: nothing but the file survives.
    drop(repository);
    let directory = dir.config_path().parent().unwrap().to_path_buf();
    // An earlier crash between temp creation and rename left a partial temp.
    let leftover = directory.join(format!(
        ".config.json.tmp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&leftover, b"{\"schema_version\": 2, \"revi").unwrap();
    let unrelated = directory.join("notes.txt");
    std::fs::write(&unrelated, b"keep me").unwrap();
    let mut restarted = FileConfigRepository::new(dir.config_path(), real());
    let loaded = restarted.load().unwrap();
    assert_eq!(loaded.config, current);
    assert_eq!(loaded.config.revision, 2);
    assert!(!leftover.exists(), "stale temporary removed at load");
    assert!(unrelated.exists(), "unrelated files are never touched");
}

#[test]
fn unknown_keys_and_nan_are_rejected_with_a_location() {
    let dir = TempDir::new("invalid");
    let (mut repository, current) = committed(&dir, real());
    let text = current.to_pretty_json().unwrap();
    let unknown = text.replacen(
        "\"revision\": 2,",
        "\"revision\": 2,\n  \"bogus\": true,",
        1,
    );
    assert_ne!(unknown, text);
    std::fs::write(dir.config_path(), &unknown).unwrap();
    match repository.load() {
        Err(ConfigLoadError::Invalid {
            error:
                ConfigError::Parse {
                    line,
                    column,
                    message,
                },
            ..
        }) => {
            assert!(line >= 3, "line {line}");
            assert!(column > 0);
            assert!(message.contains("bogus"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(repository.status().state, PersistenceState::Failed);
    let nan = text.replacen("8.0", "NaN", 1);
    assert_ne!(nan, text);
    std::fs::write(dir.config_path(), &nan).unwrap();
    assert!(matches!(
        repository.load(),
        Err(ConfigLoadError::Invalid {
            error: ConfigError::Parse { .. },
            ..
        })
    ));
    // An uninterpretable current file is never overwritten by a commit.
    let before = std::fs::read(dir.config_path()).unwrap();
    assert!(matches!(
        repository.compare_and_swap(2, &current),
        Err(PersistError::Io(InfraError::Unavailable(_)))
    ));
    assert_eq!(std::fs::read(dir.config_path()).unwrap(), before);
}

#[test]
fn future_schema_is_reported_and_never_rewritten() {
    let dir = TempDir::new("future");
    let (mut repository, current) = committed(&dir, real());
    let future = current.to_pretty_json().unwrap().replacen(
        "\"schema_version\": 2",
        "\"schema_version\": 3",
        1,
    );
    std::fs::write(dir.config_path(), &future).unwrap();
    let before = std::fs::read(dir.config_path()).unwrap();
    assert!(matches!(
        repository.load(),
        Err(ConfigLoadError::FutureSchema { found: 3, .. })
    ));
    assert_eq!(
        repository.status().state,
        PersistenceState::ReadOnlyDiagnostic
    );
    let mut candidate = current;
    candidate.revision = 2;
    assert!(matches!(
        repository.compare_and_swap(2, &candidate),
        Err(PersistError::Io(InfraError::Unavailable(_)))
    ));
    assert_eq!(std::fs::read(dir.config_path()).unwrap(), before);
}

#[test]
fn symlinked_config_is_refused_and_its_target_untouched() {
    let dir = TempDir::new("symlink");
    let (repository, current) = committed(&dir, real());
    drop(repository);
    let target = dir.path().join("elsewhere.json");
    std::fs::rename(dir.config_path(), &target).unwrap();
    std::os::unix::fs::symlink(&target, dir.config_path()).unwrap();
    let target_bytes = std::fs::read(&target).unwrap();
    let mut repository = FileConfigRepository::new(dir.config_path(), real());
    assert!(matches!(
        repository.load(),
        Err(ConfigLoadError::UnsafePath(_))
    ));
    assert_eq!(repository.status().state, PersistenceState::Failed);
    assert!(repository.compare_and_swap(2, &current).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), target_bytes);
    assert!(
        std::fs::symlink_metadata(dir.config_path())
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn group_writable_or_foreign_files_and_directories_are_refused() {
    let dir = TempDir::new("perms");
    let fs = Arc::new(FailpointFileSystem::over_real());
    let (mut repository, _current) = committed(&dir, fs.clone());
    std::fs::set_permissions(dir.config_path(), std::fs::Permissions::from_mode(0o664)).unwrap();
    assert!(
        matches!(repository.load(), Err(ConfigLoadError::UnsafePath(reason)) if reason.contains("writable"))
    );
    std::fs::set_permissions(dir.config_path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(repository.load().is_ok());
    fs.set_effective_uid(Some(fs.effective_uid().wrapping_add(1)));
    assert!(
        matches!(repository.load(), Err(ConfigLoadError::UnsafePath(reason)) if reason.contains("uid"))
    );
    fs.set_effective_uid(None);
    let directory = dir.config_path().parent().unwrap().to_path_buf();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(matches!(
        repository.load(),
        Err(ConfigLoadError::UnsafePath(_))
    ));
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(repository.load().is_ok());
}

#[test]
fn oversized_file_is_rejected() {
    let dir = TempDir::new("oversized");
    let (mut repository, _current) = committed(&dir, real());
    std::fs::write(dir.config_path(), vec![b' '; 1024 * 1024 + 1]).unwrap();
    match repository.load() {
        Err(ConfigLoadError::Invalid {
            error: ConfigError::Invalid(issues),
            ..
        }) => assert!(issues[0].message.contains("exceeds")),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn invalid_candidates_and_the_revision_ceiling_are_rejected_without_writing() {
    let dir = TempDir::new("candidate");
    let (mut repository, current) = committed(&dir, real());
    let before = std::fs::read(dir.config_path()).unwrap();
    let mut invalid = current.clone();
    invalid.quality.temperature_disagreement_deg_c = f64::NAN;
    invalid.privacy.persist_full_command_line = true;
    match repository.compare_and_swap(2, &invalid) {
        Err(PersistError::Invalid(issues)) => assert!(issues.len() >= 2),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(std::fs::read(dir.config_path()).unwrap(), before);
    let mut at_ceiling = current;
    at_ceiling.revision = MAX_CONFIG_REVISION;
    std::fs::write(dir.config_path(), at_ceiling.to_pretty_json().unwrap()).unwrap();
    assert!(matches!(
        repository.compare_and_swap(MAX_CONFIG_REVISION, &at_ceiling),
        Err(PersistError::Invalid(_))
    ));
    assert_eq!(
        repository.load().unwrap().config.revision,
        MAX_CONFIG_REVISION
    );
}

const LEGACY_V1: &str = r#"{
  "update_interval_secs": 0.25,
  "color_theme": "amber",
  "gpu_enabled": false,
  "redline_threshold": 90.0,
  "history_length": 600,
  "network_max_visible": 5,
  "mystery": true
}"#;

fn legacy_file(dir: &TempDir) -> PathBuf {
    let legacy_dir = dir.path().join("legacy");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let path = legacy_dir.join("config.json");
    std::fs::write(&path, LEGACY_V1).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    path
}

#[test]
fn legacy_dry_run_maps_everything_and_writes_nothing() {
    let dir = TempDir::new("legacy-dry");
    let legacy = legacy_file(&dir);
    let legacy_before = std::fs::read(&legacy).unwrap();
    let repository = FileConfigRepository::new(dir.config_path(), real());
    let report = repository.import_legacy(&legacy, false).unwrap();
    assert_eq!(report.candidate.sampling.interval_ms, 250);
    assert_eq!(report.candidate.viewer.theme, Theme::Amber);
    assert!(!report.candidate.sampling.gpu_enabled);
    assert_eq!(report.mappings.len(), 7);
    assert!(
        report
            .mappings
            .iter()
            .any(|mapping| mapping.status == LegacyMappingStatus::Unsupported)
    );
    assert!(!report.warnings.is_empty());
    assert!(!dir.config_path().exists());
    assert!(!dir.path().join("dgxtop").exists());
    assert_eq!(std::fs::read(&legacy).unwrap(), legacy_before);
}

#[test]
fn legacy_apply_writes_v2_once_and_refuses_an_existing_v2() {
    let dir = TempDir::new("legacy-apply");
    let legacy = legacy_file(&dir);
    let legacy_before = std::fs::read(&legacy).unwrap();
    let mut repository = FileConfigRepository::new(dir.config_path(), real());
    let report = repository.import_legacy(&legacy, true).unwrap();
    assert_eq!(report.candidate.revision, 2);
    assert_eq!(mode(&dir.config_path()), 0o600);
    let loaded = repository.load().unwrap();
    assert_eq!(loaded.config, report.candidate);
    assert_eq!(loaded.config.sampling.interval_ms, 250);
    let second = repository.import_legacy(&legacy, true);
    assert!(matches!(second, Err(LegacyImportError::V2Exists { .. })));
    assert_eq!(repository.load().unwrap().config, report.candidate);
    assert_eq!(
        std::fs::read(&legacy).unwrap(),
        legacy_before,
        "legacy file never modified"
    );
    assert_eq!(
        directory_entries(dir.config_path().parent().unwrap()),
        vec!["config.json"]
    );
}

#[test]
fn legacy_input_errors_are_reported() {
    let dir = TempDir::new("legacy-errors");
    let repository = FileConfigRepository::new(dir.config_path(), real());
    assert!(matches!(
        repository.import_legacy(&dir.path().join("absent.json"), false),
        Err(LegacyImportError::NotFound { .. })
    ));
    let broken = dir.path().join("broken.json");
    std::fs::write(&broken, "{\n \"update_interval_secs\": \"fast\"\n}").unwrap();
    match repository.import_legacy(&broken, false) {
        Err(LegacyImportError::Parse { line, column, .. }) => {
            assert_eq!(line, 2);
            assert!(column > 0);
        }
        other => panic!("unexpected {other:?}"),
    }
    let legacy = legacy_file(&dir);
    let link = dir.path().join("link.json");
    std::os::unix::fs::symlink(&legacy, &link).unwrap();
    assert!(matches!(
        repository.import_legacy(&link, false),
        Err(LegacyImportError::UnsafePath(_))
    ));
    assert!(!dir.config_path().exists());
}

#[test]
fn failed_legacy_publish_leaves_no_v2_file() {
    let dir = TempDir::new("legacy-fail");
    let legacy = legacy_file(&dir);
    let fs = Arc::new(FailpointFileSystem::over_real());
    fs.arm(Failpoint::Rename, Trigger::Once);
    let repository = FileConfigRepository::new(dir.config_path(), fs.clone());
    match repository.import_legacy(&legacy, true) {
        Err(LegacyImportError::Persist(PersistError::Io(InfraError::Io {
            operation: "config.rename",
            ..
        }))) => {}
        other => panic!("unexpected {other:?}"),
    }
    assert!(!dir.config_path().exists());
    assert!(directory_entries(dir.config_path().parent().unwrap()).is_empty());
    // Once the failure is gone the import succeeds exactly once.
    assert_eq!(
        repository
            .import_legacy(&legacy, true)
            .unwrap()
            .candidate
            .revision,
        2
    );
    fs.arm(Failpoint::DirectorySyncAfterRename, Trigger::Once);
    let second =
        FileConfigRepository::new(dir.path().join("other").join("config.json"), fs.clone());
    match second.import_legacy(&legacy, true) {
        Err(LegacyImportError::Persist(PersistError::Uncertain { on_disk, .. })) => {
            assert_eq!(on_disk.unwrap().revision, 2);
        }
        other => panic!("unexpected {other:?}"),
    }
}
