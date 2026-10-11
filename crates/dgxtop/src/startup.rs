//! Startup steps shared by every mode: path resolution, configuration load
//! with explicit CLI overrides, and the provenance of every setting
//! (spec/dgxtop-v2/OPERATIONS.md §1, contracts/CONFIG.md).

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::config::{Config, ValueOrigin};
use dgxtop_core::ports::{ConfigLoadError, ConfigRepository, LoadOrigin, LogLevel, Logger};
use dgxtop_core::quality::Namespace;
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::fs::{FileSystemOps, RealFileSystem};
use dgxtop_store::leftovers::{find_leftovers, remove_leftover};
use dgxtop_store::paths::{PathError, ProcessEnv, StatePaths};
use thiserror::Error;

use crate::cli::{Cli, CliError};

/// Fatal startup failures, reported with their location before anything runs.
#[derive(Debug, Error)]
pub enum StartupError {
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error("path error: {0}")]
    Path(#[from] PathError),
    #[error("configuration error: {0}")]
    Config(#[from] ConfigLoadError),
    #[error("configuration invalid after command-line overrides: {0}")]
    InvalidOverride(String),
    #[error("{0}")]
    Other(String),
}

/// Saved configuration, the effective configuration of this run (saved +
/// explicit CLI overrides) and the origin of every leaf setting.
#[derive(Debug, Clone)]
pub struct LoadedConfiguration {
    pub saved: Config,
    pub effective: Config,
    pub origins: BTreeMap<String, ValueOrigin>,
    pub load_origin: LoadOrigin,
    pub overridden: Vec<&'static str>,
}

/// Namespace of this run: replay fixtures never share live state.
pub fn namespace_of(cli: &Cli) -> Namespace {
    if cli.replay.is_some() {
        Namespace::Replay
    } else {
        Namespace::Live
    }
}

/// Resolves configuration and state paths (no I/O).
pub fn resolve_paths(cli: &Cli) -> Result<StatePaths, StartupError> {
    cli.validate_paths()?;
    Ok(StatePaths::resolve(
        &ProcessEnv,
        cli.config.as_deref(),
        cli.state_dir.as_deref(),
        namespace_of(cli),
    )?)
}

/// Real filesystem operations used by the persistent stores.
pub fn real_filesystem() -> Arc<dyn FileSystemOps> {
    Arc::new(RealFileSystem)
}

/// Loads the saved configuration (writing nothing) and applies explicit
/// command-line overrides to the effective copy only.
pub fn load_configuration(
    cli: &Cli,
    paths: &StatePaths,
    filesystem: Arc<dyn FileSystemOps>,
) -> Result<(LoadedConfiguration, FileConfigRepository), StartupError> {
    let mut repository = FileConfigRepository::new(paths.config_file.clone(), filesystem);
    let loaded = repository.load()?;
    let overrides = cli.overrides()?;
    let mut effective = loaded.config.clone();
    let overridden = overrides.apply(&mut effective);
    effective.validate().map_err(|issues| {
        StartupError::InvalidOverride(
            issues
                .iter()
                .map(|issue| format!("{}: {}", issue.path, issue.message))
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    let origins = value_origins(&loaded.config, &loaded.origin, &overridden);
    Ok((
        LoadedConfiguration {
            saved: loaded.config,
            effective,
            origins,
            load_origin: loaded.origin,
            overridden,
        },
        repository,
    ))
}

/// Origin of every leaf path: defaults when no file exists, saved otherwise,
/// CLI override for explicitly given flags.
pub fn value_origins(
    saved: &Config,
    load_origin: &LoadOrigin,
    overridden: &[&'static str],
) -> BTreeMap<String, ValueOrigin> {
    let base = match load_origin {
        LoadOrigin::Defaults => ValueOrigin::Default,
        LoadOrigin::File { .. } => ValueOrigin::Saved,
    };
    let mut origins = BTreeMap::new();
    if let Ok(document) = serde_json::to_value(saved) {
        collect_leaves(&document, String::new(), base, &mut origins);
    }
    for path in overridden {
        origins.insert((*path).to_owned(), ValueOrigin::CliOverride);
    }
    origins
}

fn collect_leaves(
    value: &serde_json::Value,
    prefix: String,
    origin: ValueOrigin,
    origins: &mut BTreeMap<String, ValueOrigin>,
) {
    match value {
        serde_json::Value::Object(fields) => {
            for (name, child) in fields {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                collect_leaves(child, path, origin, origins);
            }
        }
        _ => {
            origins.insert(prefix, origin);
        }
    }
}

/// Every leaf whose effective value differs from the saved value.
pub fn effective_differences(saved: &Config, effective: &Config) -> Vec<(String, String, String)> {
    let mut saved_leaves = BTreeMap::new();
    let mut effective_leaves = BTreeMap::new();
    if let (Ok(saved_document), Ok(effective_document)) =
        (serde_json::to_value(saved), serde_json::to_value(effective))
    {
        flatten(&saved_document, String::new(), &mut saved_leaves);
        flatten(&effective_document, String::new(), &mut effective_leaves);
    }
    effective_leaves
        .into_iter()
        .filter_map(|(path, value)| {
            let saved_value = saved_leaves.get(&path).cloned().unwrap_or_default();
            (saved_value != value).then_some((path, saved_value, value))
        })
        .collect()
}

fn flatten(value: &serde_json::Value, prefix: String, leaves: &mut BTreeMap<String, String>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (name, child) in fields {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                flatten(child, path, leaves);
            }
        }
        other => {
            leaves.insert(prefix, other.to_string());
        }
    }
}

/// One notice per retired key the saved configuration still holds (D-88).
pub fn retired_key_notices(saved: &Config) -> Vec<String> {
    saved
        .retired_keys_present()
        .into_iter()
        .map(|key| format!("{key} is retired and ignored (D-88); the next save leaves it out"))
        .collect()
}

/// Logs [`retired_key_notices`] at startup, one info line each.
pub fn log_retired_keys(saved: &Config, logger: &dyn Logger) {
    for notice in retired_key_notices(saved) {
        logger.log(LogLevel::Info, "config", &notice);
    }
}

/// Deletes what the state directory no longer needs (D-85): run under
/// the instance lock, before any partition opens; one log line a file.
pub fn remove_unneeded_files(fs: &dyn FileSystemOps, paths: &StatePaths, logger: &dyn Logger) {
    let leftovers = match find_leftovers(fs, paths) {
        Ok(found) => found,
        Err(error) => {
            logger.log(
                LogLevel::Warn,
                "store",
                &format!("state directory not cleaned: {error}"),
            );
            return;
        }
    };
    for leftover in leftovers {
        let path = leftover.path.display();
        let reason = leftover.kind.reason();
        match remove_leftover(fs, &leftover) {
            Ok(()) => logger.log(
                LogLevel::Info,
                "store",
                &format!("{path} deleted, {reason} (D-85)"),
            ),
            Err(error) => logger.log(
                LogLevel::Warn,
                "store",
                &format!("{path} not deleted ({reason}): {error}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use dgxtop_core::testing::MemoryLogger;
    use dgxtop_store::paths::MapEnv;

    use super::*;

    fn private_state_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dgxtop-startup-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn paths_in(dir: &std::path::Path) -> StatePaths {
        let env = MapEnv::new().with("HOME", "/home/acme");
        StatePaths::resolve(&env, None, Some(dir), Namespace::Live).unwrap()
    }

    /// Startup deletes the leftovers and logs one line for each; the
    /// databases in use stay.
    #[test]
    fn unneeded_files_are_deleted_with_one_line_each() {
        let dir = private_state_dir("leftovers");
        for name in [
            "history.sqlite3",
            "history.sqlite3.pre-v2-20260101T000000Z-1a2b3c4d.bak",
            "process_history.sqlite3-wal",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let logger = MemoryLogger::default();
        remove_unneeded_files(&RealFileSystem, &paths_in(&dir), &logger);
        let records = logger.records();
        assert_eq!(records.len(), 2, "{records:?}");
        assert!(records.iter().all(|(level, _, message)| {
            *level == LogLevel::Info && message.contains("deleted") && message.contains("D-85")
        }));
        assert!(dir.join("history.sqlite3").exists());
        assert!(!dir.join("process_history.sqlite3-wal").exists());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::write(dir.join("audit.sqlite3-wal"), b"x").unwrap();
        let refused = MemoryLogger::default();
        remove_unneeded_files(&RealFileSystem, &paths_in(&dir), &refused);
        assert_eq!(refused.records().len(), 1);
        assert_eq!(refused.records()[0].0, LogLevel::Warn);
        assert!(
            dir.join("audit.sqlite3-wal").exists(),
            "not private: untouched"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An old file's retired key is a notice and an info line; a file
    /// without one, none.
    #[test]
    fn retired_keys_are_noticed_and_logged() {
        let old = Config::reference().unwrap();
        let notices = retired_key_notices(&old);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].starts_with("store.raw_candidate_seconds is retired"));
        let logger = MemoryLogger::default();
        log_retired_keys(&old, &logger);
        assert_eq!(
            logger.records(),
            vec![(LogLevel::Info, "config", notices[0].clone())]
        );
        let current = Config::from_json(&old.to_pretty_json().unwrap()).unwrap();
        assert!(retired_key_notices(&current).is_empty());
        let silent = MemoryLogger::default();
        log_retired_keys(&current, &silent);
        assert!(silent.records().is_empty());
    }

    #[test]
    fn origins_mark_defaults_saved_and_overrides() {
        let config = Config::defaults().unwrap();
        let defaults = value_origins(&config, &LoadOrigin::Defaults, &[]);
        assert_eq!(
            defaults.get("sampling.interval_ms"),
            Some(&ValueOrigin::Default)
        );
        let saved = value_origins(
            &config,
            &LoadOrigin::File {
                path: "/x".into(),
                sha256: "0".repeat(64),
            },
            &["sampling.interval_ms"],
        );
        assert_eq!(
            saved.get("sampling.interval_ms"),
            Some(&ValueOrigin::CliOverride)
        );
        assert_eq!(saved.get("store.max_series"), Some(&ValueOrigin::Saved));
        assert!(saved.len() > 50, "every leaf has an origin");
    }
}
