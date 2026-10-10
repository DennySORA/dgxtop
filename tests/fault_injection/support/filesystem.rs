//! `FileSystemOps` double: the store's failpoint filesystem plus two faults
//! its failpoints cannot express — a file fsync that hangs (the disk stopped
//! answering mid-commit) and a configuration file that cannot be read back
//! once a rename replaced it (EIO), so an uncertain commit cannot be reloaded.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use dgxtop_store::fs::{
    DirectoryLock, FailpointFileSystem, FileFacts, FileSystemOps, LimitedRead, PrivateFile,
    ReadLimitedError,
};

use super::probes::Gate;

#[derive(Debug)]
pub struct FaultyFileSystem {
    pub failpoints: FailpointFileSystem,
    fsync_gate: Gate,
    unreadable_after_rename: Option<PathBuf>,
    unreadable: AtomicBool,
}

impl FaultyFileSystem {
    fn build(fsync_gate: Gate, unreadable_after_rename: Option<PathBuf>) -> Self {
        Self {
            failpoints: FailpointFileSystem::over_real(),
            fsync_gate,
            unreadable_after_rename,
            unreadable: AtomicBool::new(false),
        }
    }

    /// Every file fsync first passes `gate`.
    pub fn with_fsync_gate(gate: Gate) -> Self {
        Self::build(gate, None)
    }

    /// `path` reads fail with EIO once a rename replaced it.
    pub fn unreadable_after_rename_of(path: PathBuf) -> Self {
        Self::build(Gate::open(), Some(path))
    }
}

impl FileSystemOps for FaultyFileSystem {
    fn create_private_file(&self, path: &Path) -> io::Result<PrivateFile> {
        self.failpoints.create_private_file(path)
    }

    fn write_all(&self, file: &mut PrivateFile, bytes: &[u8]) -> io::Result<()> {
        self.failpoints.write_all(file, bytes)
    }

    fn sync_file(&self, file: &PrivateFile) -> io::Result<()> {
        self.fsync_gate.pass();
        self.failpoints.sync_file(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.failpoints.rename(from, to)?;
        if self.unreadable_after_rename.as_deref() == Some(to) {
            self.unreadable.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn link_new(&self, existing: &Path, new_path: &Path) -> io::Result<()> {
        self.failpoints.link_new(existing, new_path)
    }

    fn sync_directory(&self, directory: &Path) -> io::Result<()> {
        self.failpoints.sync_directory(directory)
    }

    fn read_limited(&self, path: &Path, max_bytes: u64) -> Result<LimitedRead, ReadLimitedError> {
        let replaced = self.unreadable_after_rename.as_deref() == Some(path)
            && self.unreadable.load(Ordering::SeqCst);
        if replaced {
            return Err(ReadLimitedError::Io(io::Error::from_raw_os_error(
                libc::EIO,
            )));
        }
        self.failpoints.read_limited(path, max_bytes)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileFacts> {
        self.failpoints.symlink_metadata(path)
    }

    fn list_directory(&self, directory: &Path, max_entries: usize) -> io::Result<Vec<OsString>> {
        self.failpoints.list_directory(directory, max_entries)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.failpoints.remove_file(path)
    }

    fn create_private_dir(&self, path: &Path) -> io::Result<()> {
        self.failpoints.create_private_dir(path)
    }

    fn try_lock_directory(&self, directory: &Path) -> io::Result<Option<DirectoryLock>> {
        self.failpoints.try_lock_directory(directory)
    }

    fn effective_uid(&self) -> u32 {
        self.failpoints.effective_uid()
    }
}
