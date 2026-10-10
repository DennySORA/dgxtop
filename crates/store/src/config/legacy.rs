//! Explicit import of the v1 configuration (`OPERATIONS.md` §4, AT18).
//!
//! The dry run reads and maps the legacy file and writes nothing. Applying
//! writes only a new v2 file through the same atomic publish as a commit
//! and refuses when a v2 file already exists. The legacy file is never
//! modified.

use std::io;
use std::path::Path;

use dgxtop_core::config::{
    Config, ConfigError, LegacyConfig, LegacyImportReport, ValidationIssue, map_legacy_config,
};
use dgxtop_core::errors::InfraError;
use dgxtop_core::ports::PersistError;
use dgxtop_core::text::sanitize_message;
use thiserror::Error;

use super::document::{MAX_CONFIG_FILE_BYTES, display};
use super::repository::{FileConfigRepository, Publish, encode, next_document};
use crate::fs::{FileSystemOps, ReadLimitedError};

/// Why a legacy import produced no report or wrote nothing.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum LegacyImportError {
    #[error("legacy config {path} does not exist")]
    NotFound { path: String },
    #[error("unsafe legacy config path: {0}")]
    UnsafePath(String),
    #[error("legacy config {path} exceeds {limit} bytes")]
    TooLarge { path: String, limit: u64 },
    #[error("legacy config {path} is invalid at line {line}, column {column}: {message}")]
    Parse {
        path: String,
        line: usize,
        column: usize,
        message: String,
    },
    #[error(transparent)]
    Read(InfraError),
    #[error("embedded defaults are invalid: {0}")]
    Defaults(ConfigError),
    #[error("mapped candidate is invalid: {}", describe_issues(.0))]
    InvalidCandidate(Vec<ValidationIssue>),
    #[error("a v2 config already exists at {path}; refusing to overwrite it")]
    V2Exists { path: String },
    #[error("writing the imported config failed: {0}")]
    Persist(PersistError),
}

fn describe_issues(issues: &[ValidationIssue]) -> String {
    issues
        .iter()
        .map(|issue| format!("{}: {}", issue.path, issue.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Legacy bytes plus warnings about who else could have edited the file.
///
/// The v1 app wrote its file with the default umask (often 0664), and the
/// file is only an explicitly named, fully validated input shown in a dry
/// run, so ownership and mode are reported instead of refused. Symlinks,
/// non-regular files and oversized files are still refused.
fn read_legacy(
    fs: &dyn FileSystemOps,
    legacy_path: &Path,
) -> Result<(Vec<u8>, Vec<String>), LegacyImportError> {
    let label = display(legacy_path);
    let read = match fs.read_limited(legacy_path, MAX_CONFIG_FILE_BYTES) {
        Ok(read) => read,
        Err(ReadLimitedError::NotFound) => return Err(LegacyImportError::NotFound { path: label }),
        Err(ReadLimitedError::Symlink) => {
            return Err(LegacyImportError::UnsafePath(format!(
                "{label} is a symbolic link"
            )));
        }
        Err(ReadLimitedError::NotRegular(kind)) => {
            return Err(LegacyImportError::UnsafePath(format!(
                "{label} is not a regular file ({kind:?})"
            )));
        }
        Err(ReadLimitedError::TooLarge { limit }) => {
            return Err(LegacyImportError::TooLarge { path: label, limit });
        }
        Err(ReadLimitedError::Io(error)) => {
            return Err(LegacyImportError::Read(InfraError::from_io(
                "legacy.read",
                &error,
            )));
        }
    };
    let mut warnings = Vec::new();
    let euid = fs.effective_uid();
    if read.facts.owner_uid != euid {
        warnings.push(format!(
            "legacy file is owned by uid {}, not by this user (uid {euid}); review every mapped value",
            read.facts.owner_uid
        ));
    }
    if read.facts.is_group_or_world_writable() {
        warnings.push(format!(
            "legacy file is group/world-writable (mode {:o}); review every mapped value",
            read.facts.permissions
        ));
    }
    Ok((read.bytes, warnings))
}

fn parse_legacy(label: &str, bytes: &[u8]) -> Result<LegacyConfig, LegacyImportError> {
    serde_json::from_slice(bytes).map_err(|error| LegacyImportError::Parse {
        path: label.to_owned(),
        line: error.line(),
        column: error.column(),
        message: sanitize_message(&error.to_string()),
    })
}

impl FileConfigRepository {
    /// Maps the v1 file at `legacy_path` onto the defaults. With
    /// `apply = false` nothing is written. With `apply = true` the mapped
    /// candidate is stored as the first v2 document (revision = defaults + 1)
    /// and the returned report's candidate is exactly what was written.
    pub fn import_legacy(
        &self,
        legacy_path: &Path,
        apply: bool,
    ) -> Result<LegacyImportReport, LegacyImportError> {
        let (bytes, file_warnings) = read_legacy(self.filesystem(), legacy_path)?;
        let legacy = parse_legacy(&display(legacy_path), &bytes)?;
        let defaults = Config::defaults().map_err(LegacyImportError::Defaults)?;
        let mut report = map_legacy_config(&legacy, &defaults);
        report.warnings.extend(file_warnings);
        report
            .candidate
            .validate()
            .map_err(LegacyImportError::InvalidCandidate)?;
        if apply {
            report.candidate = self.write_imported(&report.candidate, defaults.revision)?;
        }
        Ok(report)
    }

    fn write_imported(
        &self,
        candidate: &Config,
        base_revision: u32,
    ) -> Result<Config, LegacyImportError> {
        let v2_exists = || LegacyImportError::V2Exists {
            path: display(self.path()),
        };
        let _lock = self.lock_directory().map_err(LegacyImportError::Persist)?;
        match self.filesystem().symlink_metadata(self.path()) {
            Ok(_) => return Err(v2_exists()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(LegacyImportError::Read(InfraError::from_io(
                    "config.inspect",
                    &error,
                )));
            }
        }
        let stored = next_document(candidate, base_revision).map_err(LegacyImportError::Persist)?;
        let bytes = encode(&stored).map_err(LegacyImportError::Persist)?;
        match self.publish(&bytes, Publish::CreateNew) {
            Ok(()) => Ok(stored),
            Err(PersistError::Conflict { .. }) => Err(v2_exists()),
            Err(error) => Err(LegacyImportError::Persist(error)),
        }
    }
}
