//! [`FileConfigRepository`]: the `ConfigRepository` used by the config worker.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dgxtop_core::action::{PersistenceKind, PersistenceState, PersistenceStatus};
use dgxtop_core::config::{Config, MAX_CONFIG_REVISION, ValidationIssue};
use dgxtop_core::digest::sha256_hex;
use dgxtop_core::errors::InfraError;
use dgxtop_core::ports::{
    ConfigLoadError, ConfigRepository, LoadOrigin, LoadedConfig, PersistError, PersistReceipt,
};
use dgxtop_core::text::sanitize_message;
use uuid::Uuid;

use super::document::{CurrentDocument, MAX_CONFIG_FILE_BYTES, display, read_current};
use crate::fs::{DirectoryLock, FileSystemOps};
use crate::paths::{PathError, ensure_private_dir, verify_private_directory};

/// Wait for another dgxtop process's commit to finish.
const LOCK_WAIT: Duration = Duration::from_millis(500);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Directory entries examined when removing stale temporary files.
const MAX_DIRECTORY_SCAN: usize = 1024;
const TEMP_MARKER: &str = ".tmp-";

/// How a staged document becomes visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Publish {
    /// Atomic replace of an existing (or missing) file.
    Replace,
    /// Only if no file exists (legacy import); `Conflict` otherwise.
    CreateNew,
}

/// Versioned atomic JSON configuration file.
pub struct FileConfigRepository {
    path: PathBuf,
    fs: Arc<dyn FileSystemOps>,
    state: PersistenceState,
    detail: Option<String>,
    file_bytes: u64,
}

impl fmt::Debug for FileConfigRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileConfigRepository")
            .field("path", &self.path)
            .field("state", &self.state)
            .field("detail", &self.detail)
            .finish_non_exhaustive()
    }
}

fn persist_io(operation: &'static str, error: &io::Error) -> PersistError {
    PersistError::Io(InfraError::from_io(operation, error))
}

fn unavailable(message: impl AsRef<str>) -> PersistError {
    PersistError::Io(InfraError::Unavailable(sanitize_message(message.as_ref())))
}

fn path_failure(error: &PathError) -> PersistError {
    match error {
        PathError::Io { .. } => PersistError::Io(InfraError::Io {
            operation: "config.directory",
            message: sanitize_message(&error.to_string()),
        }),
        _ => unavailable(format!("unsafe config directory: {error}")),
    }
}

/// Built-in defaults; the embedded document is validated by tests, so a
/// failure is a build defect surfaced as an error rather than a panic.
pub(crate) fn default_config() -> Result<Config, PersistError> {
    Config::defaults()
        .map_err(|error| unavailable(format!("embedded defaults are invalid: {error}")))
}

/// Candidate with `expected + 1`; the revision never wraps or passes the
/// schema maximum (that requires a controlled migration).
pub(crate) fn next_document(
    candidate: &Config,
    expected_revision: u32,
) -> Result<Config, PersistError> {
    let next = expected_revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_CONFIG_REVISION)
        .ok_or_else(|| {
            PersistError::Invalid(vec![ValidationIssue {
                path: "revision".to_owned(),
                message: "revision limit reached; a controlled migration is required".to_owned(),
            }])
        })?;
    let mut stored = candidate.clone();
    stored.revision = next;
    stored.validate().map_err(PersistError::Invalid)?;
    Ok(stored)
}

/// Persisted form: pretty JSON with a trailing newline, at most 1 MiB.
pub(crate) fn encode(stored: &Config) -> Result<Vec<u8>, PersistError> {
    let text = stored
        .to_pretty_json()
        .map_err(|error| unavailable(format!("config encoding failed: {error}")))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_CONFIG_FILE_BYTES {
        return Err(PersistError::Invalid(vec![ValidationIssue {
            path: "$".to_owned(),
            message: format!("encoded config exceeds {MAX_CONFIG_FILE_BYTES} bytes"),
        }]));
    }
    Ok(text.into_bytes())
}

fn is_temporary_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

impl FileConfigRepository {
    pub fn new(path: PathBuf, fs: Arc<dyn FileSystemOps>) -> Self {
        Self {
            path,
            fs,
            state: PersistenceState::Healthy,
            detail: None,
            file_bytes: 0,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn filesystem(&self) -> &dyn FileSystemOps {
        self.fs.as_ref()
    }

    pub(crate) fn directory(&self) -> Result<&Path, PersistError> {
        self.path
            .parent()
            .filter(|directory| !directory.as_os_str().is_empty())
            .ok_or_else(|| unavailable("config path has no parent directory"))
    }

    fn file_name(&self) -> String {
        self.path.file_name().map_or_else(
            || "config.json".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    fn temporary_prefix(&self) -> String {
        format!(".{}{TEMP_MARKER}", self.file_name())
    }

    /// Creates/validates the private directory and takes the cross-process
    /// commit lock, waiting at most [`LOCK_WAIT`].
    pub(crate) fn lock_directory(&self) -> Result<DirectoryLock, PersistError> {
        let directory = self.directory()?;
        ensure_private_dir(self.fs.as_ref(), directory).map_err(|error| path_failure(&error))?;
        let started = Instant::now();
        loop {
            match self.fs.try_lock_directory(directory) {
                Ok(Some(lock)) => return Ok(lock),
                Ok(None) if started.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(LOCK_POLL_INTERVAL);
                }
                Ok(None) => {
                    return Err(PersistError::Io(InfraError::Timeout {
                        operation: "config.lock",
                        elapsed_ms: u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    }));
                }
                Err(error) => return Err(persist_io("config.lock", &error)),
            }
        }
    }

    /// Revision currently on disk (defaults' revision when missing). A file
    /// we cannot interpret is never overwritten.
    fn current_revision(&self) -> Result<u32, PersistError> {
        match read_current(self.fs.as_ref(), &self.path) {
            Ok(CurrentDocument::Missing) => Ok(default_config()?.revision),
            Ok(CurrentDocument::Present { config, .. }) => Ok(config.revision),
            Err(ConfigLoadError::FutureSchema { found, .. }) => Err(unavailable(format!(
                "current config has schema_version {found}; it is never rewritten"
            ))),
            Err(ConfigLoadError::Invalid { error, .. }) => Err(unavailable(format!(
                "current config is invalid ({error}); not overwritten"
            ))),
            Err(ConfigLoadError::UnsafePath(reason)) => {
                Err(unavailable(format!("unsafe config path: {reason}")))
            }
            Err(ConfigLoadError::Io(error)) => Err(PersistError::Io(error)),
        }
    }

    /// Configuration on disk right now, for an uncertain commit.
    fn reload_on_disk(&self) -> Option<Box<Config>> {
        match read_current(self.fs.as_ref(), &self.path) {
            Ok(CurrentDocument::Present { config, .. }) => Some(config),
            _ => None,
        }
    }

    fn discard(&self, temporary: &Path) {
        // Best effort: a leftover 0600 temp file in our private directory is
        // harmless and removed by the next load.
        let _ignored = self.fs.remove_file(temporary);
    }

    /// temp (0600, O_EXCL) -> write -> fsync -> rename/link -> dir fsync.
    pub(crate) fn publish(&self, bytes: &[u8], publish: Publish) -> Result<(), PersistError> {
        let directory = self.directory()?;
        let temporary = directory.join(format!(
            "{}{}",
            self.temporary_prefix(),
            Uuid::new_v4().simple()
        ));
        let mut file = self
            .fs
            .create_private_file(&temporary)
            .map_err(|error| persist_io("config.create_temp", &error))?;
        let staged = self
            .fs
            .write_all(&mut file, bytes)
            .map_err(|error| persist_io("config.write", &error))
            .and_then(|()| {
                self.fs
                    .sync_file(&file)
                    .map_err(|error| persist_io("config.fsync", &error))
            });
        drop(file);
        if let Err(error) = staged {
            self.discard(&temporary);
            return Err(error);
        }
        let published = match publish {
            Publish::Replace => self.fs.rename(&temporary, &self.path),
            Publish::CreateNew => self.fs.link_new(&temporary, &self.path),
        };
        if let Err(error) = published {
            self.discard(&temporary);
            return Err(match (publish, error.kind()) {
                (Publish::CreateNew, io::ErrorKind::AlreadyExists) => {
                    PersistError::Conflict { current: 0 }
                }
                _ => persist_io("config.rename", &error),
            });
        }
        if publish == Publish::CreateNew {
            self.discard(&temporary);
        }
        self.fs.sync_directory(directory).map_err(|error| {
            // The rename happened; whether it survives a crash is unknown, so
            // report what is on disk now and never claim the old file exists.
            PersistError::Uncertain {
                message: sanitize_message(&format!(
                    "directory fsync failed after the rename: {error}"
                )),
                on_disk: self.reload_on_disk(),
            }
        })
    }

    fn commit(
        &self,
        expected_revision: u32,
        candidate: &Config,
    ) -> Result<PersistReceipt, PersistError> {
        let _lock = self.lock_directory()?;
        let current = self.current_revision()?;
        if candidate.revision != expected_revision || current != expected_revision {
            return Err(PersistError::Conflict { current });
        }
        candidate.validate().map_err(PersistError::Invalid)?;
        let stored = next_document(candidate, expected_revision)?;
        let bytes = encode(&stored)?;
        self.publish(&bytes, Publish::Replace)?;
        Ok(PersistReceipt {
            revision: stored.revision,
            sha256: sha256_hex(&bytes),
            path: display(&self.path),
            file_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        })
    }

    /// Removes temporaries left by a crash between create and rename, but
    /// only while no other process is committing.
    fn remove_stale_temporaries(&self, directory: &Path) {
        let Ok(Some(_lock)) = self.fs.try_lock_directory(directory) else {
            return;
        };
        let Ok(names) = self.fs.list_directory(directory, MAX_DIRECTORY_SCAN) else {
            return;
        };
        let prefix = self.temporary_prefix();
        for name in names {
            if let Some(name) = name.to_str()
                && is_temporary_name(name, &prefix)
            {
                self.discard(&directory.join(name));
            }
        }
    }

    fn load_document(&self) -> Result<(LoadedConfig, u64), ConfigLoadError> {
        if let Ok(directory) = self.directory()
            && self.fs.symlink_metadata(directory).is_ok()
        {
            verify_private_directory(self.fs.as_ref(), directory)
                .map_err(|error| ConfigLoadError::UnsafePath(error.to_string()))?;
            self.remove_stale_temporaries(directory);
        }
        match read_current(self.fs.as_ref(), &self.path)? {
            CurrentDocument::Missing => {
                let config = Config::defaults().map_err(|error| ConfigLoadError::Invalid {
                    path: "<embedded defaults>".to_owned(),
                    error,
                })?;
                Ok((
                    LoadedConfig {
                        config,
                        origin: LoadOrigin::Defaults,
                    },
                    0,
                ))
            }
            CurrentDocument::Present {
                config,
                sha256,
                len,
            } => Ok((
                LoadedConfig {
                    config: *config,
                    origin: LoadOrigin::File {
                        path: display(&self.path),
                        sha256,
                    },
                },
                len,
            )),
        }
    }

    fn set_state(&mut self, state: PersistenceState, detail: Option<String>) {
        self.state = state;
        self.detail = detail.map(|text| sanitize_message(&text));
    }
}

impl ConfigRepository for FileConfigRepository {
    fn load(&mut self) -> Result<LoadedConfig, ConfigLoadError> {
        match self.load_document() {
            Ok((loaded, len)) => {
                self.file_bytes = len;
                self.set_state(PersistenceState::Healthy, None);
                Ok(loaded)
            }
            Err(error) => {
                let state = match error {
                    ConfigLoadError::FutureSchema { .. } => PersistenceState::ReadOnlyDiagnostic,
                    _ => PersistenceState::Failed,
                };
                self.set_state(state, Some(error.to_string()));
                Err(error)
            }
        }
    }

    fn compare_and_swap(
        &mut self,
        expected_revision: u32,
        candidate: &Config,
    ) -> Result<PersistReceipt, PersistError> {
        let result = self.commit(expected_revision, candidate);
        match &result {
            Ok(_) => {
                self.file_bytes = self
                    .fs
                    .symlink_metadata(&self.path)
                    .map_or(self.file_bytes, |facts| facts.len);
                self.set_state(PersistenceState::Healthy, None);
            }
            Err(PersistError::Io(error)) => self.set_state(
                PersistenceState::Degraded,
                Some(format!("last commit failed, previous file kept: {error}")),
            ),
            Err(PersistError::Uncertain { message, .. }) => self.set_state(
                PersistenceState::Degraded,
                Some(format!("last commit outcome uncertain: {message}")),
            ),
            Err(PersistError::Conflict { .. } | PersistError::Invalid(_)) => {}
        }
        result
    }

    fn status(&self) -> PersistenceStatus {
        PersistenceStatus {
            kind: PersistenceKind::Config,
            state: self.state,
            detail: self.detail.clone(),
            bytes_used: self.file_bytes,
            bytes_limit: MAX_CONFIG_FILE_BYTES,
            wal_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_never_wraps() {
        let candidate = Config::defaults().unwrap();
        assert_eq!(next_document(&candidate, 1).unwrap().revision, 2);
        assert_eq!(
            next_document(&candidate, MAX_CONFIG_REVISION - 1)
                .unwrap()
                .revision,
            MAX_CONFIG_REVISION
        );
        assert!(matches!(
            next_document(&candidate, MAX_CONFIG_REVISION),
            Err(PersistError::Invalid(_))
        ));
        assert!(matches!(
            next_document(&candidate, u32::MAX),
            Err(PersistError::Invalid(_))
        ));
    }

    #[test]
    fn only_our_temporary_names_match() {
        let prefix = ".config.json.tmp-";
        let name = format!("{prefix}{}", Uuid::new_v4().simple());
        assert!(is_temporary_name(&name, prefix));
        assert!(!is_temporary_name("config.json", prefix));
        assert!(!is_temporary_name(".config.json.tmp-notes", prefix));
        assert!(!is_temporary_name(
            &format!("{prefix}{}x", "a".repeat(31)),
            prefix
        ));
    }
}
