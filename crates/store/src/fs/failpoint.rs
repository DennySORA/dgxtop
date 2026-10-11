//! Failure injection over a real filesystem.
//!
//! [`FailpointFileSystem`] delegates to an inner [`FileSystemOps`] and fails
//! chosen operations on demand. It exists so tests (unit, integration and the
//! repository-level fault-injection suite) can prove the commit protocol:
//! failures before the rename leave the old file intact, and a directory fsync
//! failure after the rename is reported as uncertain.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{
    DirectoryLock, FileFacts, FileSystemOps, LimitedRead, PrivateFile, ReadLimitedError,
    RealFileSystem,
};

/// Operations that can be made to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Failpoint {
    /// `create_private_file` fails with `EACCES`.
    CreateFile,
    /// `write_all` writes the first half of the bytes, then fails with `ENOSPC`.
    WriteNoSpace,
    /// `sync_file` fails with `EIO`.
    FileSync,
    /// `rename` fails with `EIO` without renaming anything.
    Rename,
    /// `sync_directory` fails with `EIO`, but only when a rename or link
    /// succeeded since the previous directory sync (the commit is published
    /// but its durability is unknown).
    DirectorySyncAfterRename,
}

/// How often an armed failpoint fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Once,
    Always,
}

#[derive(Debug, Default)]
struct FailpointState {
    armed: BTreeMap<Failpoint, Trigger>,
    fired: BTreeMap<Failpoint, u64>,
    published_since_directory_sync: bool,
    effective_uid_override: Option<u32>,
}

impl FailpointState {
    fn take(&mut self, failpoint: Failpoint) -> bool {
        let Some(trigger) = self.armed.get(&failpoint).copied() else {
            return false;
        };
        if trigger == Trigger::Once {
            self.armed.remove(&failpoint);
        }
        let counter = self.fired.entry(failpoint).or_insert(0);
        *counter = counter.saturating_add(1);
        true
    }
}

/// [`FileSystemOps`] wrapper with injectable failures.
#[derive(Debug)]
pub struct FailpointFileSystem {
    inner: Arc<dyn FileSystemOps>,
    state: Mutex<FailpointState>,
}

impl FailpointFileSystem {
    pub fn new(inner: Arc<dyn FileSystemOps>) -> Self {
        Self {
            inner,
            state: Mutex::new(FailpointState::default()),
        }
    }

    /// Wraps the real filesystem.
    pub fn over_real() -> Self {
        Self::new(Arc::new(RealFileSystem))
    }

    pub fn arm(&self, failpoint: Failpoint, trigger: Trigger) {
        self.lock_state().armed.insert(failpoint, trigger);
    }

    /// How many times `failpoint` injected a failure.
    pub fn fired(&self, failpoint: Failpoint) -> u64 {
        self.lock_state()
            .fired
            .get(&failpoint)
            .copied()
            .unwrap_or(0)
    }

    /// Pretends to run as another uid, so files we own look foreign-owned
    /// (owner checks cannot otherwise be exercised without root).
    pub fn set_effective_uid(&self, uid: Option<u32>) {
        self.lock_state().effective_uid_override = uid;
    }

    fn lock_state(&self) -> MutexGuard<'_, FailpointState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn should_fail(&self, failpoint: Failpoint) -> bool {
        self.lock_state().take(failpoint)
    }

    fn mark_published(&self) {
        self.lock_state().published_since_directory_sync = true;
    }

    fn take_directory_sync_failure(&self) -> bool {
        let mut state = self.lock_state();
        let published = std::mem::take(&mut state.published_since_directory_sync);
        published && state.take(Failpoint::DirectorySyncAfterRename)
    }
}

fn injected(errno: i32) -> io::Error {
    io::Error::from_raw_os_error(errno)
}

impl FileSystemOps for FailpointFileSystem {
    fn create_private_file(&self, path: &Path) -> io::Result<PrivateFile> {
        if self.should_fail(Failpoint::CreateFile) {
            return Err(injected(libc::EACCES));
        }
        self.inner.create_private_file(path)
    }

    fn write_all(&self, file: &mut PrivateFile, bytes: &[u8]) -> io::Result<()> {
        if self.should_fail(Failpoint::WriteNoSpace) {
            let half = bytes.len() / 2;
            let partial = bytes.get(..half).unwrap_or_default();
            self.inner.write_all(file, partial)?;
            return Err(injected(libc::ENOSPC));
        }
        self.inner.write_all(file, bytes)
    }

    fn sync_file(&self, file: &PrivateFile) -> io::Result<()> {
        if self.should_fail(Failpoint::FileSync) {
            return Err(injected(libc::EIO));
        }
        self.inner.sync_file(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.should_fail(Failpoint::Rename) {
            return Err(injected(libc::EIO));
        }
        self.inner.rename(from, to)?;
        self.mark_published();
        Ok(())
    }

    fn link_new(&self, existing: &Path, new_path: &Path) -> io::Result<()> {
        if self.should_fail(Failpoint::Rename) {
            return Err(injected(libc::EIO));
        }
        self.inner.link_new(existing, new_path)?;
        self.mark_published();
        Ok(())
    }

    fn sync_directory(&self, directory: &Path) -> io::Result<()> {
        if self.take_directory_sync_failure() {
            return Err(injected(libc::EIO));
        }
        self.inner.sync_directory(directory)
    }

    fn read_limited(&self, path: &Path, max_bytes: u64) -> Result<LimitedRead, ReadLimitedError> {
        self.inner.read_limited(path, max_bytes)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileFacts> {
        self.inner.symlink_metadata(path)
    }

    fn list_directory(&self, directory: &Path, max_entries: usize) -> io::Result<Vec<OsString>> {
        self.inner.list_directory(directory, max_entries)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }

    fn create_private_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.create_private_dir(path)
    }

    fn try_lock_directory(&self, directory: &Path) -> io::Result<Option<DirectoryLock>> {
        self.inner.try_lock_directory(directory)
    }

    fn effective_uid(&self) -> u32 {
        self.lock_state()
            .effective_uid_override
            .unwrap_or_else(|| self.inner.effective_uid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-failpoint-unit-{}-{label}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn write_failure_leaves_a_partial_file_and_reports_enospc() {
        let dir = temp_dir("write");
        let fs = FailpointFileSystem::over_real();
        fs.arm(Failpoint::WriteNoSpace, Trigger::Once);
        let path = dir.join("partial");
        let mut file = fs.create_private_file(&path).unwrap();
        let error = fs.write_all(&mut file, b"0123456789").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(std::fs::read(&path).unwrap(), b"01234");
        assert_eq!(fs.fired(Failpoint::WriteNoSpace), 1);
        fs.write_all(&mut file, b"!").unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directory_sync_fails_only_after_a_successful_rename() {
        let dir = temp_dir("dirsync");
        let fs = FailpointFileSystem::over_real();
        fs.arm(Failpoint::DirectorySyncAfterRename, Trigger::Always);
        fs.sync_directory(&dir).unwrap();
        let from = dir.join("a");
        let to = dir.join("b");
        std::fs::write(&from, b"x").unwrap();
        fs.rename(&from, &to).unwrap();
        assert_eq!(
            fs.sync_directory(&dir).unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
        assert!(to.exists(), "the rename itself really happened");
        fs.sync_directory(&dir).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rename_failure_does_not_rename() {
        let dir = temp_dir("rename");
        let fs = FailpointFileSystem::over_real();
        fs.arm(Failpoint::Rename, Trigger::Once);
        let from = dir.join("a");
        std::fs::write(&from, b"x").unwrap();
        assert!(fs.rename(&from, &dir.join("b")).is_err());
        assert!(from.exists());
        assert!(!dir.join("b").exists());
        fs.rename(&from, &dir.join("b")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
