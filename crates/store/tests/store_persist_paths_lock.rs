//! XDG paths, private directories and the instance lock, plus the startup
//! order (safe paths and lock before config and SQLite).

// Test helpers panic on setup failures; the lints target production code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dgxtop_core::action::PersistenceState;
use dgxtop_core::ports::{AuditRepository, ConfigRepository, LoadOrigin};
use dgxtop_core::quality::Namespace;
use dgxtop_core::testing::FakeClock;
use dgxtop_store::audit::{AuditLimits, SqliteAuditRepository};
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::fs::{FailpointFileSystem, FileSystemOps, RealFileSystem};
use dgxtop_store::lock::InstanceLock;
use dgxtop_store::paths::{MapEnv, PathError, StatePaths, ensure_private_dir};
use dgxtop_store::sqlite::StatvfsFreeSpace;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-paths-it-{label}-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn ensure_private_dir_creates_every_missing_level_with_0700() {
    let dir = TempDir::new("create");
    let deep = dir.path().join("a").join("b").join("c");
    ensure_private_dir(&RealFileSystem, &deep).unwrap();
    for level in [dir.path().join("a"), dir.path().join("a/b"), deep.clone()] {
        assert_eq!(mode(&level), 0o700, "{}", level.display());
    }
    // Idempotent on an existing private directory.
    ensure_private_dir(&RealFileSystem, &deep).unwrap();
}

#[test]
fn ensure_private_dir_refuses_unsafe_final_components() {
    let dir = TempDir::new("unsafe");
    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(
        ensure_private_dir(&RealFileSystem, &link),
        Err(PathError::Symlink { .. })
    ));
    let shared = dir.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        ensure_private_dir(&RealFileSystem, &shared),
        Err(PathError::GroupOrWorldWritable { mode: 0o777, .. })
    ));
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(matches!(
        ensure_private_dir(&RealFileSystem, &shared),
        Err(PathError::GroupOrWorldWritable { .. })
    ));
    let file = dir.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(matches!(
        ensure_private_dir(&RealFileSystem, &file),
        Err(PathError::NotADirectory { .. })
    ));
    assert!(matches!(
        ensure_private_dir(&RealFileSystem, Path::new("relative/dir")),
        Err(PathError::RelativeDirectory { .. })
    ));
    // Nothing was created behind the symlink.
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
}

#[test]
fn ensure_private_dir_refuses_directories_owned_by_another_uid() {
    let dir = TempDir::new("foreign");
    let existing = dir.path().join("existing");
    std::fs::create_dir(&existing).unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o700)).unwrap();
    let fs = FailpointFileSystem::over_real();
    fs.set_effective_uid(Some(fs.effective_uid().wrapping_add(1)));
    assert!(matches!(
        ensure_private_dir(&fs, &existing),
        Err(PathError::ForeignOwner { .. })
    ));
}

#[test]
fn startup_order_paths_lock_config_then_audit() {
    let dir = TempDir::new("startup");
    let env = MapEnv::new().with("HOME", dir.path().to_str().unwrap());
    let paths = StatePaths::resolve(&env, None, None, Namespace::Live).unwrap();
    let fs: Arc<dyn FileSystemOps> = Arc::new(RealFileSystem);
    paths.ensure_directories(fs.as_ref()).unwrap();
    let _lock = InstanceLock::acquire(&paths.state_dir).unwrap();
    let mut config = FileConfigRepository::new(paths.config_file.clone(), fs.clone());
    let loaded = config.load().unwrap();
    assert_eq!(loaded.origin, LoadOrigin::Defaults);
    let mut audit = SqliteAuditRepository::open(
        &paths.audit_database,
        AuditLimits::from_config(&loaded.config),
        FakeClock::new(1),
        Arc::new(StatvfsFreeSpace),
    )
    .unwrap();
    assert_eq!(audit.status().state, PersistenceState::Healthy);
    assert!(paths.audit_database.exists());
    assert_eq!(mode(&paths.audit_database), 0o600);
    assert!(
        !paths.history_database.exists(),
        "the archive is off by default"
    );
}
