//! The short-term file (D-76): the live runs' recent buckets as JSON,
//! written to a 0600 temporary file, synced and renamed over the old one,
//! so a crash leaves either the old file or the new one. Read back only
//! when it is a regular file of ours and not group/world-writable.

use std::io;
use std::path::Path;

use dgxtop_core::errors::InfraError;
use dgxtop_core::process_history::{MAX_LIVE_RUNS, RecentHistory, Tier};
use uuid::Uuid;

use crate::fs::{FileSystemOps, ReadLimitedError};

/// What the file may hold: the live runs' 5 s buckets and filling buckets
/// come to about 70 KB a run as JSON.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

pub(super) fn write(
    fs: &dyn FileSystemOps,
    path: &Path,
    recent: &RecentHistory,
) -> Result<(), InfraError> {
    let bytes = serde_json::to_vec(recent).map_err(|error| InfraError::Io {
        operation: "process_recent.encode",
        message: error.to_string(),
    })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return Err(InfraError::Io {
            operation: "process_recent.encode",
            message: format!(
                "{} bytes exceed the {MAX_FILE_BYTES} byte limit",
                bytes.len()
            ),
        });
    }
    let directory = path.parent().ok_or_else(|| InfraError::Io {
        operation: "process_recent.path",
        message: "the file has no directory".to_owned(),
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("process_recent.json");
    let temporary = directory.join(format!(".{file_name}.{}", Uuid::new_v4().simple()));
    let mut file = fs
        .create_private_file(&temporary)
        .map_err(|error| io_error("process_recent.create_temp", &error))?;
    let staged = fs
        .write_all(&mut file, &bytes)
        .and_then(|()| fs.sync_file(&file));
    drop(file);
    if let Err(error) = staged.and_then(|()| fs.rename(&temporary, path)) {
        let _ = fs.remove_file(&temporary);
        return Err(io_error("process_recent.write", &error));
    }
    fs.sync_directory(directory)
        .map_err(|error| io_error("process_recent.sync_directory", &error))
}

pub(super) fn read(fs: &dyn FileSystemOps, path: &Path) -> Result<RecentHistory, InfraError> {
    let read = fs
        .read_limited(path, MAX_FILE_BYTES)
        .map_err(|error| match error {
            ReadLimitedError::NotFound => InfraError::Unavailable("no short-term file".to_owned()),
            other => InfraError::Io {
                operation: "process_recent.read",
                message: other.to_string(),
            },
        })?;
    if read.facts.owner_uid != fs.effective_uid() || read.facts.is_group_or_world_writable() {
        return Err(InfraError::Io {
            operation: "process_recent.read",
            message: "the short-term file is not private to this user".to_owned(),
        });
    }
    let mut recent: RecentHistory =
        serde_json::from_slice(&read.bytes).map_err(|error| InfraError::Io {
            operation: "process_recent.decode",
            message: error.to_string(),
        })?;
    bound(&mut recent);
    Ok(recent)
}

/// Whatever the file says, no more than the history keeps.
fn bound(recent: &mut RecentHistory) {
    recent.runs.truncate(MAX_LIVE_RUNS);
    for run in &mut recent.runs {
        let excess = run.recent.len().saturating_sub(Tier::Recent.capacity());
        run.recent.drain(..excess);
        run.filling.truncate(Tier::ALL.len());
    }
}

fn io_error(operation: &'static str, error: &io::Error) -> InfraError {
    InfraError::from_io(operation, error)
}
