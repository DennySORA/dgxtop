//! Startup steps shared by every mode: path resolution, configuration load
//! with explicit CLI overrides, and the provenance of every setting
//! (spec/dgxtop-v2/OPERATIONS.md §1, contracts/CONFIG.md).

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::config::{Config, ValueOrigin};
use dgxtop_core::ports::{ConfigLoadError, ConfigRepository, LoadOrigin};
use dgxtop_core::quality::Namespace;
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::fs::{FileSystemOps, RealFileSystem};
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

#[cfg(test)]
mod tests {
    use super::*;

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
