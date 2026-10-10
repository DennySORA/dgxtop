//! Exclusive per-state-directory instance lock (`STORAGE.md` §1).
//!
//! Two dgxtop processes writing the same state directory would consume the
//! same audit tokens and fight over the SQLite writers, so the second one must
//! stop with `AlreadyRunning`. Two view sessions inside one process are not
//! two writers and never take this lock twice.

use std::fs::{File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use thiserror::Error;

use crate::fs::PRIVATE_FILE_MODE;
use crate::paths::INSTANCE_LOCK_NAME;

/// Bytes read back from a lock file to name the holder (a decimal pid).
const HOLDER_PID_MAX_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LockError {
    #[error("another dgxtop instance is already running on {path}{}", holder_suffix(*.holder_pid))]
    AlreadyRunning {
        path: String,
        holder_pid: Option<u32>,
    },
    #[error("unsafe instance lock file {path}: {reason}")]
    UnsafeLockFile { path: String, reason: String },
    #[error("cannot lock {path}: {message}")]
    Io { path: String, message: String },
}

fn holder_suffix(holder_pid: Option<u32>) -> String {
    holder_pid.map_or_else(String::new, |pid| format!(" (pid {pid})"))
}

/// Held exclusive `flock` on `<state_dir>/instance.lock`; released on drop.
#[derive(Debug)]
pub struct InstanceLock {
    _lock: Flock<File>,
    path: PathBuf,
}

impl InstanceLock {
    /// Takes the exclusive non-blocking lock. A second acquire, even from the
    /// same process through another open file description, fails with
    /// [`LockError::AlreadyRunning`].
    pub fn acquire(state_dir: &Path) -> Result<Self, LockError> {
        let path = state_dir.join(INSTANCE_LOCK_NAME);
        let file = open_lock_file(&path)?;
        verify_lock_file(&file, &path)?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => {
                record_holder_pid(&lock);
                Ok(Self { _lock: lock, path })
            }
            Err((file, Errno::EWOULDBLOCK)) => Err(LockError::AlreadyRunning {
                path: path.display().to_string(),
                holder_pid: read_holder_pid(&file),
            }),
            Err((_, errno)) => Err(io_error(&path, &io::Error::from(errno))),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn io_error(path: &Path, error: &io::Error) -> LockError {
    LockError::Io {
        path: path.display().to_string(),
        message: dgxtop_core::text::sanitize_message(&error.to_string()),
    }
}

fn open_lock_file(path: &Path) -> Result<File, LockError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                LockError::UnsafeLockFile {
                    path: path.display().to_string(),
                    reason: "is a symbolic link".to_owned(),
                }
            } else {
                io_error(path, &error)
            }
        })
}

fn verify_lock_file(file: &File, path: &Path) -> Result<(), LockError> {
    let metadata = file.metadata().map_err(|error| io_error(path, &error))?;
    let unsafe_file = |reason: String| LockError::UnsafeLockFile {
        path: path.display().to_string(),
        reason,
    };
    if !metadata.file_type().is_file() {
        return Err(unsafe_file("not a regular file".to_owned()));
    }
    let euid = nix::unistd::geteuid().as_raw();
    if metadata.uid() != euid {
        return Err(unsafe_file(format!(
            "owned by uid {}, expected {euid}",
            metadata.uid()
        )));
    }
    if metadata.mode() & 0o077 != 0 {
        // Our own file with loose bits (e.g. created under an old umask):
        // tighten instead of refusing, it carries no secret.
        file.set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))
            .map_err(|error| io_error(path, &error))?;
    }
    Ok(())
}

/// Best effort: the pid only improves the `AlreadyRunning` diagnostic, the
/// lock itself is what excludes other writers.
fn record_holder_pid(file: &File) {
    let pid = format!("{}\n", std::process::id());
    if file.set_len(0).is_ok() {
        let _ignored = file.write_all_at(pid.as_bytes(), 0);
    }
}

fn read_holder_pid(file: &File) -> Option<u32> {
    let mut buffer = [0u8; HOLDER_PID_MAX_BYTES];
    let read = file.read_at(&mut buffer, 0).ok()?;
    let text = std::str::from_utf8(buffer.get(..read)?).ok()?;
    text.trim().parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-lock-unit-{}-{label}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn second_acquire_is_already_running_until_release() {
        let dir = temp_dir("second");
        let first = InstanceLock::acquire(&dir).unwrap();
        let error = InstanceLock::acquire(&dir).unwrap_err();
        match error {
            LockError::AlreadyRunning { holder_pid, .. } => {
                assert_eq!(holder_pid, Some(std::process::id()));
            }
            other => panic!("unexpected {other:?}"),
        }
        drop(first);
        let again = InstanceLock::acquire(&dir).unwrap();
        assert_eq!(again.path(), dir.join(INSTANCE_LOCK_NAME));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lock_file_is_private_and_loose_modes_are_tightened() {
        let dir = temp_dir("mode");
        let path = dir.join(INSTANCE_LOCK_NAME);
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o666)).unwrap();
        let lock = InstanceLock::acquire(&dir).unwrap();
        let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, PRIVATE_FILE_MODE);
        drop(lock);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn symlinked_lock_file_is_refused() {
        let dir = temp_dir("symlink");
        let target = dir.join("elsewhere");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, dir.join(INSTANCE_LOCK_NAME)).unwrap();
        assert!(matches!(
            InstanceLock::acquire(&dir),
            Err(LockError::UnsafeLockFile { .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_state_dir_is_an_io_error() {
        let dir = temp_dir("missing");
        assert!(matches!(
            InstanceLock::acquire(&dir.join("absent")),
            Err(LockError::Io { .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
