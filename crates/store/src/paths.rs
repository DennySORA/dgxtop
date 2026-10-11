//! XDG path resolution and private directory handling (`OPERATIONS.md` §1,
//! `contracts/CONFIG.md`, `SECURITY.md` §5).
//!
//! * Config: `$XDG_CONFIG_HOME/dgxtop/v2/config.json`, falling back to
//!   `$HOME/.config/dgxtop/v2/config.json`.
//! * State: `$XDG_STATE_HOME/dgxtop/v2/` (fallback `$HOME/.local/state/dgxtop/v2/`)
//!   holding `audit.sqlite3`, `history.sqlite3`, `dgxtop.log`, `instance.lock`.
//! * Relative XDG values are ignored with a diagnostic note; explicit
//!   overrides must be absolute. The replay namespace lives under its own
//!   `replay/` root so simulated data never shares files with live data.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use dgxtop_core::quality::Namespace;
use thiserror::Error;

use crate::fs::{FileKind, FileSystemOps};

pub const APP_DIRECTORY: &str = "dgxtop";
pub const SCHEMA_DIRECTORY: &str = "v2";
pub const REPLAY_DIRECTORY: &str = "replay";
pub const CONFIG_FILE_NAME: &str = "config.json";
pub const AUDIT_DATABASE_NAME: &str = "audit.sqlite3";
pub const HISTORY_DATABASE_NAME: &str = "history.sqlite3";
/// The process history's database (D-76).
pub const PROCESS_HISTORY_DATABASE_NAME: &str = "process_history.sqlite3";
/// The process history's short-term file (D-76).
pub const PROCESS_RECENT_FILE_NAME: &str = "process_recent.json";
pub const LOG_FILE_NAME: &str = "dgxtop.log";
pub const INSTANCE_LOCK_NAME: &str = "instance.lock";

const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
const XDG_STATE_HOME: &str = "XDG_STATE_HOME";
const HOME: &str = "HOME";

/// Source of environment variables (injectable for tests).
pub trait EnvSource {
    /// Value of `name`; `None` when unset or not valid UTF-8.
    fn var(&self, name: &str) -> Option<String>;
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Fixed environment for tests and replay setups.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapEnv {
    values: BTreeMap<String, String>,
}

impl MapEnv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.values.insert(name.to_owned(), value.to_owned());
        self
    }
}

impl EnvSource for MapEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }
}

/// Path resolution or directory safety failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PathError {
    #[error("HOME is not set; cannot derive the default {purpose} directory")]
    HomeMissing { purpose: &'static str },
    #[error("HOME must be an absolute path, got {0:?}")]
    HomeNotAbsolute(String),
    #[error("{flag} must be an absolute path, got {path:?}")]
    OverrideNotAbsolute { flag: &'static str, path: String },
    #[error("{flag} must name a file, got {path:?}")]
    OverrideWithoutFileName { flag: &'static str, path: String },
    #[error("{path}: directories must be absolute")]
    RelativeDirectory { path: String },
    #[error("{path}: is a symbolic link; refusing to use it")]
    Symlink { path: String },
    #[error("{path}: not a directory")]
    NotADirectory { path: String },
    #[error("{path}: owned by uid {owner}, expected uid {expected}")]
    ForeignOwner {
        path: String,
        owner: u32,
        expected: u32,
    },
    #[error("{path}: group/world-writable (mode {mode:o}); run `chmod go-w` on it")]
    GroupOrWorldWritable { path: String, mode: u32 },
    #[error("{path}: {operation} failed: {message}")]
    Io {
        path: String,
        operation: &'static str,
        message: String,
    },
}

impl PathError {
    fn io(path: &Path, operation: &'static str, error: &io::Error) -> Self {
        Self::Io {
            path: display(path),
            operation,
            message: dgxtop_core::text::sanitize_message(&error.to_string()),
        }
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

/// Every persistent location of one dgxtop data namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    pub namespace: Namespace,
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub audit_database: PathBuf,
    pub history_database: PathBuf,
    /// The process history's runs and buckets (D-76).
    pub process_history_database: PathBuf,
    /// The live runs' recent buckets, replaced every 30 s (D-76).
    pub process_recent_file: PathBuf,
    pub log_file: PathBuf,
    pub instance_lock: PathBuf,
    /// Default location of the v1 configuration (only read on an explicit
    /// `--import-legacy`); `None` when it cannot be derived.
    pub legacy_config_file: Option<PathBuf>,
    /// Diagnostics about ignored environment values.
    pub notes: Vec<String>,
}

impl StatePaths {
    /// Resolves every path; performs no I/O.
    pub fn resolve(
        env: &dyn EnvSource,
        config_override: Option<&Path>,
        state_override: Option<&Path>,
        namespace: Namespace,
    ) -> Result<Self, PathError> {
        let mut notes = Vec::new();
        let config_file = match config_override {
            Some(path) => validated_file_override("--config", path)?,
            None => {
                let base = base_directory(env, XDG_CONFIG_HOME, ".config", "config", &mut notes)?;
                namespace_root(&base, namespace).join(CONFIG_FILE_NAME)
            }
        };
        let state_dir = match state_override {
            Some(path) => {
                namespace_subdirectory(validated_dir_override("--state-dir", path)?, namespace)
            }
            None => {
                let base =
                    base_directory(env, XDG_STATE_HOME, ".local/state", "state", &mut notes)?;
                namespace_root(&base, namespace)
            }
        };
        let legacy_config_file = legacy_location(env);
        Ok(Self {
            namespace,
            audit_database: state_dir.join(AUDIT_DATABASE_NAME),
            history_database: state_dir.join(HISTORY_DATABASE_NAME),
            process_history_database: state_dir.join(PROCESS_HISTORY_DATABASE_NAME),
            process_recent_file: state_dir.join(PROCESS_RECENT_FILE_NAME),
            log_file: state_dir.join(LOG_FILE_NAME),
            instance_lock: state_dir.join(INSTANCE_LOCK_NAME),
            config_file,
            state_dir,
            legacy_config_file,
            notes,
        })
    }

    /// Directory holding the configuration file.
    pub fn config_dir(&self) -> &Path {
        self.config_file.parent().unwrap_or(Path::new("/"))
    }

    /// Creates (0700) and validates the config and state directories.
    pub fn ensure_directories(&self, fs: &dyn FileSystemOps) -> Result<(), PathError> {
        ensure_private_dir(fs, self.config_dir())?;
        ensure_private_dir(fs, &self.state_dir)
    }
}

fn validated_file_override(flag: &'static str, path: &Path) -> Result<PathBuf, PathError> {
    let path = validated_dir_override(flag, path)?;
    if path.file_name().is_none() || path.as_os_str().to_string_lossy().ends_with('/') {
        return Err(PathError::OverrideWithoutFileName {
            flag,
            path: display(&path),
        });
    }
    Ok(path)
}

fn validated_dir_override(flag: &'static str, path: &Path) -> Result<PathBuf, PathError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err(PathError::OverrideNotAbsolute {
            flag,
            path: display(path),
        })
    }
}

/// `$XDG_*` when absolute, else `$HOME/<fallback>` with a note when the XDG
/// value was present but relative (the XDG spec says to ignore it).
fn base_directory(
    env: &dyn EnvSource,
    variable: &str,
    fallback: &str,
    purpose: &'static str,
    notes: &mut Vec<String>,
) -> Result<PathBuf, PathError> {
    match env.var(variable).filter(|value| !value.is_empty()) {
        Some(value) if Path::new(&value).is_absolute() => return Ok(PathBuf::from(value)),
        Some(value) => notes.push(format!(
            "{variable}={value:?} is not absolute and was ignored; using $HOME/{fallback}"
        )),
        None => {}
    }
    Ok(home_directory(env, purpose)?.join(fallback))
}

fn home_directory(env: &dyn EnvSource, purpose: &'static str) -> Result<PathBuf, PathError> {
    let home = env
        .var(HOME)
        .filter(|value| !value.is_empty())
        .ok_or(PathError::HomeMissing { purpose })?;
    if Path::new(&home).is_absolute() {
        Ok(PathBuf::from(home))
    } else {
        Err(PathError::HomeNotAbsolute(home))
    }
}

fn namespace_root(base: &Path, namespace: Namespace) -> PathBuf {
    namespace_subdirectory(base.join(APP_DIRECTORY).join(SCHEMA_DIRECTORY), namespace)
}

fn namespace_subdirectory(root: PathBuf, namespace: Namespace) -> PathBuf {
    match namespace {
        Namespace::Live => root,
        Namespace::Replay => root.join(REPLAY_DIRECTORY),
    }
}

fn legacy_location(env: &dyn EnvSource) -> Option<PathBuf> {
    let mut ignored = Vec::new();
    base_directory(env, XDG_CONFIG_HOME, ".config", "config", &mut ignored)
        .ok()
        .map(|base| base.join(APP_DIRECTORY).join(CONFIG_FILE_NAME))
}

/// Creates missing directories with mode 0700 and validates the final one.
///
/// Refuses a final component that is a symlink (we never create symlinks), a
/// directory owned by another uid, and a group/world-writable directory.
/// Every directory created here is validated the same way. Pre-existing
/// ancestors (such as `$HOME` or `/tmp`) are trusted as given.
pub fn ensure_private_dir(fs: &dyn FileSystemOps, path: &Path) -> Result<(), PathError> {
    if !path.is_absolute() {
        return Err(PathError::RelativeDirectory {
            path: display(path),
        });
    }
    let missing = missing_components(fs, path)?;
    for component in missing.iter().rev() {
        match fs.create_private_dir(component) {
            Ok(()) => {}
            // Created concurrently: validated below like any existing directory.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(PathError::io(component, "mkdir", &error)),
        }
        verify_private_directory(fs, component)?;
    }
    if missing.is_empty() {
        verify_private_directory(fs, path)?;
    }
    Ok(())
}

/// Deepest-first list of `path` and its ancestors that do not exist yet.
fn missing_components(fs: &dyn FileSystemOps, path: &Path) -> Result<Vec<PathBuf>, PathError> {
    let mut missing = Vec::new();
    let mut cursor = Some(path);
    while let Some(current) = cursor {
        match fs.symlink_metadata(current) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                cursor = current.parent();
            }
            Err(error) => return Err(PathError::io(current, "lstat", &error)),
        }
    }
    Ok(missing)
}

/// Checks one directory: real directory, owned by us, not writable by others.
pub fn verify_private_directory(fs: &dyn FileSystemOps, path: &Path) -> Result<(), PathError> {
    let facts = fs
        .symlink_metadata(path)
        .map_err(|error| PathError::io(path, "lstat", &error))?;
    match facts.kind {
        FileKind::Directory => {}
        FileKind::Symlink => {
            return Err(PathError::Symlink {
                path: display(path),
            });
        }
        FileKind::Regular | FileKind::Other => {
            return Err(PathError::NotADirectory {
                path: display(path),
            });
        }
    }
    let expected = fs.effective_uid();
    if facts.owner_uid != expected {
        return Err(PathError::ForeignOwner {
            path: display(path),
            owner: facts.owner_uid,
            expected,
        });
    }
    if facts.is_group_or_world_writable() {
        return Err(PathError::GroupOrWorldWritable {
            path: display(path),
            mode: facts.permissions,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> MapEnv {
        MapEnv::new().with(HOME, "/home/alice")
    }

    #[test]
    fn xdg_values_win_over_home() {
        let env = env()
            .with(XDG_CONFIG_HOME, "/xdg/config")
            .with(XDG_STATE_HOME, "/xdg/state");
        let paths = StatePaths::resolve(&env, None, None, Namespace::Live).unwrap();
        assert_eq!(
            paths.config_file,
            PathBuf::from("/xdg/config/dgxtop/v2/config.json")
        );
        assert_eq!(paths.state_dir, PathBuf::from("/xdg/state/dgxtop/v2"));
        assert_eq!(
            paths.audit_database,
            PathBuf::from("/xdg/state/dgxtop/v2/audit.sqlite3")
        );
        assert_eq!(
            paths.history_database,
            PathBuf::from("/xdg/state/dgxtop/v2/history.sqlite3")
        );
        assert_eq!(
            paths.log_file,
            PathBuf::from("/xdg/state/dgxtop/v2/dgxtop.log")
        );
        assert_eq!(
            paths.instance_lock,
            PathBuf::from("/xdg/state/dgxtop/v2/instance.lock")
        );
        assert_eq!(
            paths.legacy_config_file,
            Some(PathBuf::from("/xdg/config/dgxtop/config.json"))
        );
        assert!(paths.notes.is_empty());
    }

    #[test]
    fn missing_or_empty_xdg_falls_back_to_home() {
        let env = env().with(XDG_STATE_HOME, "");
        let paths = StatePaths::resolve(&env, None, None, Namespace::Live).unwrap();
        assert_eq!(
            paths.config_file,
            PathBuf::from("/home/alice/.config/dgxtop/v2/config.json")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/home/alice/.local/state/dgxtop/v2")
        );
        assert!(paths.notes.is_empty());
    }

    #[test]
    fn relative_xdg_is_ignored_with_a_note() {
        let env = env()
            .with(XDG_CONFIG_HOME, "relative/config")
            .with(XDG_STATE_HOME, "./state");
        let paths = StatePaths::resolve(&env, None, None, Namespace::Live).unwrap();
        assert_eq!(
            paths.config_file,
            PathBuf::from("/home/alice/.config/dgxtop/v2/config.json")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/home/alice/.local/state/dgxtop/v2")
        );
        assert_eq!(paths.notes.len(), 2);
        assert!(paths.notes[0].contains("XDG_CONFIG_HOME"));
        assert!(paths.notes[1].contains("XDG_STATE_HOME"));
    }

    #[test]
    fn replay_namespace_uses_a_separate_root() {
        let live = StatePaths::resolve(&env(), None, None, Namespace::Live).unwrap();
        let replay = StatePaths::resolve(&env(), None, None, Namespace::Replay).unwrap();
        assert_eq!(
            replay.state_dir,
            PathBuf::from("/home/alice/.local/state/dgxtop/v2/replay")
        );
        assert_eq!(
            replay.config_file,
            PathBuf::from("/home/alice/.config/dgxtop/v2/replay/config.json")
        );
        assert_ne!(live.audit_database, replay.audit_database);
        assert_ne!(live.instance_lock, replay.instance_lock);
        let overridden = StatePaths::resolve(
            &env(),
            None,
            Some(Path::new("/srv/state")),
            Namespace::Replay,
        )
        .unwrap();
        assert_eq!(overridden.state_dir, PathBuf::from("/srv/state/replay"));
    }

    #[test]
    fn overrides_must_be_absolute() {
        let error = StatePaths::resolve(
            &env(),
            Some(Path::new("config.json")),
            None,
            Namespace::Live,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PathError::OverrideNotAbsolute {
                flag: "--config",
                ..
            }
        ));
        let error = StatePaths::resolve(&env(), None, Some(Path::new("state")), Namespace::Live)
            .unwrap_err();
        assert!(matches!(
            error,
            PathError::OverrideNotAbsolute {
                flag: "--state-dir",
                ..
            }
        ));
        let error =
            StatePaths::resolve(&env(), Some(Path::new("/")), None, Namespace::Live).unwrap_err();
        assert!(matches!(error, PathError::OverrideWithoutFileName { .. }));
    }

    #[test]
    fn home_is_required_and_absolute_when_needed() {
        assert!(matches!(
            StatePaths::resolve(&MapEnv::new(), None, None, Namespace::Live),
            Err(PathError::HomeMissing { purpose: "config" })
        ));
        let relative_home = MapEnv::new().with(HOME, "home/alice");
        assert!(matches!(
            StatePaths::resolve(&relative_home, None, None, Namespace::Live),
            Err(PathError::HomeNotAbsolute(_))
        ));
    }
}
